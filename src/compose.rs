use crate::config::{ProjectConfig, ServiceConfig, service_from_image};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::{
    fs,
    io::AsyncWriteExt,
    process::Command,
    time::{Instant, sleep, timeout},
};

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub success: bool,
    pub log: String,
}
#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(
        &self,
        program: &str,
        args: &[String],
        cwd: &Path,
        timeout_for: Duration,
    ) -> anyhow::Result<CommandOutput>;
}
#[derive(Default)]
pub struct ProcessRunner;
#[async_trait]
impl CommandRunner for ProcessRunner {
    async fn run(
        &self,
        program: &str,
        args: &[String],
        cwd: &Path,
        timeout_for: Duration,
    ) -> anyhow::Result<CommandOutput> {
        let child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let output = timeout(timeout_for, child.wait_with_output())
            .await
            .map_err(|_| anyhow::anyhow!("命令执行超时"))??;
        let mut log = String::from_utf8_lossy(&output.stdout).into_owned();
        log.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok(CommandOutput {
            success: output.status.success(),
            log,
        })
    }
}

#[derive(Serialize)]
struct Override {
    services: BTreeMap<String, OverrideService>,
}
#[derive(Serialize)]
struct OverrideService {
    image: String,
}
pub async fn write_image_override(
    path: &Path,
    services: &[ServiceConfig],
    images: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    let managed = services
        .iter()
        .map(|service| service.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut map = BTreeMap::new();
    for (id, image) in images {
        if !managed.contains(id.as_str()) {
            anyhow::bail!("未知服务状态 {id}");
        }
        map.insert(
            id.clone(),
            OverrideService {
                image: image.clone(),
            },
        );
    }
    write_override_file(path, map).await
}

async fn write_override_file(
    path: &Path,
    services: BTreeMap<String, OverrideService>,
) -> anyhow::Result<()> {
    let bytes = serde_yaml::to_string(&Override { services })?.into_bytes();
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("override 路径缺少父目录"))?;
    fs::create_dir_all(parent).await?;
    let tmp = parent.join(format!(".compose.deploy.{}.tmp", uuid::Uuid::new_v4()));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp)
        .await?;
    file.write_all(&bytes).await?;
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    fs::rename(&tmp, path).await?;
    if let Ok(dir) = fs::File::open(parent).await {
        let _ = dir.sync_all().await;
    }
    Ok(())
}

#[derive(Clone)]
pub struct Compose {
    cfg: Arc<RwLock<ProjectConfig>>,
    override_file: PathBuf,
    runner: Arc<dyn CommandRunner>,
}
#[derive(Debug, Clone, Serialize, Default)]
pub struct RuntimeState {
    pub actual_image: Option<String>,
    pub actual_image_id: Option<String>,
    pub replicas: usize,
    pub mixed_images: bool,
    pub container_status: String,
}
#[derive(Debug, Clone)]
pub struct ImageArtifact {
    pub image: String,
    pub pinned_image: String,
    pub digest: String,
    pub image_id: String,
}
#[derive(Deserialize)]
struct Inspect {
    #[serde(rename = "Image", default)]
    image: String,
    #[serde(rename = "Config")]
    config: InspectConfig,
    #[serde(rename = "State")]
    state: InspectState,
}
#[derive(Deserialize)]
struct ImageInspect {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "RepoDigests", default)]
    repo_digests: Vec<String>,
}
#[derive(Deserialize)]
struct InspectConfig {
    #[serde(rename = "Image")]
    image: String,
    #[serde(rename = "Healthcheck")]
    healthcheck: Option<serde_json::Value>,
    #[serde(rename = "Labels", default)]
    labels: BTreeMap<String, String>,
}
#[derive(Deserialize)]
struct InspectState {
    #[serde(rename = "Status")]
    status: String,
    #[serde(rename = "Health")]
    health: Option<InspectHealth>,
}
#[derive(Deserialize)]
struct InspectHealth {
    #[serde(rename = "Status")]
    status: String,
}

impl Compose {
    pub fn new(cfg: ProjectConfig, override_file: PathBuf, runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            cfg: Arc::new(RwLock::new(cfg)),
            override_file,
            runner,
        }
    }
    pub fn project(&self) -> ProjectConfig {
        self.cfg
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    pub fn replace_project(&self, project: ProjectConfig) {
        *self.cfg.write().unwrap_or_else(|error| error.into_inner()) = project;
    }
    pub async fn resolve_candidate(&self) -> anyhow::Result<ProjectConfig> {
        let mut candidate = self.project();
        resolve_project(&mut candidate, self.runner.clone()).await?;
        Ok(candidate)
    }
    fn base_args(&self) -> (Vec<String>, PathBuf) {
        let cfg = self.project();
        let cwd = cfg.project_dir().to_path_buf();
        let mut args = vec![
            "compose".into(),
            "--project-name".into(),
            cfg.id.clone(),
            "--project-directory".into(),
            cwd.display().to_string(),
        ];
        for file in &cfg.compose_files {
            args.extend(["-f".into(), file.display().to_string()]);
        }
        args.extend(["-f".into(), self.override_file.display().to_string()]);
        (args, cwd)
    }
    pub async fn pull(&self, service: &str, limit: Duration) -> anyhow::Result<String> {
        let (mut args, cwd) = self.base_args();
        args.extend(["pull".into(), service.into()]);
        let out = self.runner.run("docker", &args, &cwd, limit).await?;
        if !out.success {
            anyhow::bail!("docker compose pull 执行失败\n{}", out.log);
        }
        Ok(out.log)
    }
    pub async fn up(&self, service: &str, limit: Duration) -> anyhow::Result<String> {
        let (mut args, cwd) = self.base_args();
        args.extend(["up".into(), "-d".into(), "--no-deps".into(), service.into()]);
        let out = self.runner.run("docker", &args, &cwd, limit).await?;
        if !out.success {
            anyhow::bail!("docker compose up 执行失败\n{}", out.log);
        }
        Ok(out.log)
    }
    pub async fn image_artifact(
        &self,
        image: &str,
        repository: &str,
        limit: Duration,
    ) -> anyhow::Result<ImageArtifact> {
        let args = vec!["image".into(), "inspect".into(), image.into()];
        let cwd = self.project().project_dir().to_path_buf();
        let out = self.runner.run("docker", &args, &cwd, limit).await?;
        if !out.success {
            anyhow::bail!("无法检查已拉取镜像\n{}", out.log);
        }
        let mut values: Vec<ImageInspect> = serde_json::from_str(&out.log)?;
        let value = values
            .pop()
            .ok_or_else(|| anyhow::anyhow!("docker image inspect 未返回镜像"))?;
        let digest = value
            .repo_digests
            .iter()
            .filter_map(|value| value.rsplit_once('@'))
            .find(|(repo, _)| repositories_match(repo, repository))
            .map(|(_, digest)| digest)
            .filter(|digest| digest.starts_with("sha256:"))
            .ok_or_else(|| anyhow::anyhow!("已拉取镜像缺少目标仓库的 RepoDigest"))?
            .to_owned();
        Ok(ImageArtifact {
            image: image.into(),
            pinned_image: format!("{repository}@{digest}"),
            digest,
            image_id: value.id,
        })
    }
    pub async fn recreate(&self, service: &str, limit: Duration) -> anyhow::Result<String> {
        let (mut args, cwd) = self.base_args();
        args.extend([
            "up".into(),
            "-d".into(),
            "--force-recreate".into(),
            "--no-deps".into(),
            service.into(),
        ]);
        let out = self.runner.run("docker", &args, &cwd, limit).await?;
        if !out.success {
            anyhow::bail!("docker compose 重建服务失败\n{}", out.log);
        }
        Ok(out.log)
    }
    pub async fn stop(&self, service: &str, limit: Duration) -> anyhow::Result<String> {
        let (mut args, cwd) = self.base_args();
        args.extend(["stop".into(), service.into()]);
        let out = self.runner.run("docker", &args, &cwd, limit).await?;
        if !out.success {
            anyhow::bail!("docker compose 停止服务失败\n{}", out.log);
        }
        Ok(out.log)
    }
    pub async fn remove(&self, service: &str, limit: Duration) -> anyhow::Result<String> {
        let (mut args, cwd) = self.base_args();
        args.extend([
            "rm".into(),
            "--stop".into(),
            "--force".into(),
            service.into(),
        ]);
        let out = self.runner.run("docker", &args, &cwd, limit).await?;
        if !out.success {
            anyhow::bail!("docker compose 下线服务失败\n{}", out.log);
        }
        Ok(out.log)
    }
    pub async fn logs(&self, service: &str, tail: u32, limit: Duration) -> anyhow::Result<String> {
        let (mut args, cwd) = self.base_args();
        args.extend([
            "logs".into(),
            "--no-color".into(),
            "--tail".into(),
            tail.to_string(),
            service.into(),
        ]);
        let out = self.runner.run("docker", &args, &cwd, limit).await?;
        if !out.success {
            anyhow::bail!("docker compose logs 执行失败\n{}", out.log);
        }
        Ok(out.log)
    }
    async fn ids(&self, service: &str, limit: Duration) -> anyhow::Result<(Vec<String>, String)> {
        let (mut args, cwd) = self.base_args();
        args.extend(["ps".into(), "-q".into(), service.into()]);
        let out = self.runner.run("docker", &args, &cwd, limit).await?;
        if !out.success {
            anyhow::bail!("docker compose ps 执行失败\n{}", out.log);
        }
        Ok((
            out.log
                .lines()
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(str::to_owned)
                .collect(),
            out.log,
        ))
    }
    async fn inspect(
        &self,
        ids: &[String],
        limit: Duration,
    ) -> anyhow::Result<(RuntimeState, bool)> {
        if ids.is_empty() {
            anyhow::bail!("Compose 未返回容器");
        }
        let mut args = vec!["inspect".into()];
        args.extend_from_slice(ids);
        let cwd = self.project().project_dir().to_path_buf();
        let out = self.runner.run("docker", &args, &cwd, limit).await?;
        if !out.success {
            anyhow::bail!("docker inspect 执行失败");
        }
        let values: Vec<Inspect> = serde_json::from_str(&out.log)?;
        if values.len() != ids.len() {
            anyhow::bail!("部分容器无法检查");
        }
        Ok(summarize(values))
    }
    pub async fn runtimes(
        &self,
        limit: Duration,
    ) -> anyhow::Result<BTreeMap<String, RuntimeState>> {
        let (mut args, cwd) = self.base_args();
        args.extend(["ps".into(), "-q".into()]);
        let out = self.runner.run("docker", &args, &cwd, limit).await?;
        if !out.success {
            anyhow::bail!("docker compose ps 执行失败");
        }
        let ids = out
            .log
            .lines()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut inspect_args = vec!["inspect".into()];
        inspect_args.extend_from_slice(&ids);
        let out = self
            .runner
            .run("docker", &inspect_args, &cwd, limit)
            .await?;
        if !out.success {
            anyhow::bail!("docker inspect 执行失败");
        }
        let values: Vec<Inspect> = serde_json::from_str(&out.log)?;
        let mut grouped: BTreeMap<String, Vec<Inspect>> = BTreeMap::new();
        for value in values {
            if let Some(service) = value
                .config
                .labels
                .get("com.docker.compose.service")
                .cloned()
            {
                grouped.entry(service).or_default().push(value);
            }
        }
        Ok(grouped
            .into_iter()
            .map(|(service, values)| (service, summarize(values).0))
            .collect())
    }
}

pub(crate) fn repositories_match(left: &str, right: &str) -> bool {
    fn normalized(value: &str) -> String {
        let value = value
            .trim_start_matches("docker.io/")
            .trim_start_matches("index.docker.io/")
            .trim_start_matches("registry-1.docker.io/");
        if value.contains('/') {
            value.to_owned()
        } else {
            format!("library/{value}")
        }
    }
    left == right || normalized(left) == normalized(right)
}

fn summarize(values: Vec<Inspect>) -> (RuntimeState, bool) {
    let mut ready = true;
    let mut worst = "healthy".to_owned();
    let mut images = BTreeSet::new();
    let mut image_ids = BTreeSet::new();
    let replicas = values.len();
    for v in values {
        images.insert(v.config.image);
        if !v.image.is_empty() {
            image_ids.insert(v.image);
        }
        if v.config.healthcheck.is_some() {
            let health = v
                .state
                .health
                .map(|h| h.status)
                .unwrap_or_else(|| "starting".into());
            if health != "healthy" {
                ready = false;
                worst = health;
            }
        } else if v.state.status != "running" {
            ready = false;
            worst = v.state.status;
        } else if worst == "healthy" {
            worst = "running".into();
        }
    }
    let mixed_images = images.len() > 1 || image_ids.len() > 1;
    let actual_image = (images.len() == 1).then(|| images.into_iter().next().unwrap());
    let actual_image_id = (image_ids.len() == 1).then(|| image_ids.into_iter().next().unwrap());
    (
        RuntimeState {
            actual_image,
            actual_image_id,
            replicas,
            mixed_images,
            container_status: worst,
        },
        ready,
    )
}

impl Compose {
    pub async fn wait_healthy(
        &self,
        service: &str,
        health_limit: Duration,
        command_limit: Duration,
    ) -> anyhow::Result<String> {
        self.wait_healthy_image(service, None, health_limit, command_limit)
            .await
    }
    pub async fn wait_healthy_image(
        &self,
        service: &str,
        expected_image_id: Option<&str>,
        health_limit: Duration,
        command_limit: Duration,
    ) -> anyhow::Result<String> {
        let deadline = Instant::now() + health_limit;
        loop {
            let (ids, _) = self.ids(service, command_limit).await?;
            match self.inspect(&ids, command_limit).await {
                Ok((state, true)) => {
                    if state.mixed_images {
                        anyhow::bail!("服务的多个副本运行了不同镜像");
                    }
                    if let Some(expected) = expected_image_id.filter(|value| !value.is_empty())
                        && state.actual_image_id.as_deref() != Some(expected)
                    {
                        anyhow::bail!("容器运行的镜像 ID 与已验证候选镜像不一致");
                    }
                    return Ok(format!("容器状态：{}\n", state.container_status));
                }
                Ok((state, false)) => {
                    if matches!(
                        state.container_status.as_str(),
                        "unhealthy" | "exited" | "dead"
                    ) {
                        anyhow::bail!("容器状态异常: {}", state.container_status);
                    }
                }
                Err(e) => {
                    if Instant::now() >= deadline {
                        return Err(e);
                    }
                }
            }
            if Instant::now() >= deadline {
                anyhow::bail!("等待容器健康状态超时");
            }
            sleep(Duration::from_secs(2)).await;
        }
    }
    pub async fn runtime(&self, service: &str, limit: Duration) -> RuntimeState {
        let Ok((ids, _)) = self.ids(service, limit).await else {
            return RuntimeState {
                actual_image: None,
                actual_image_id: None,
                replicas: 0,
                mixed_images: false,
                container_status: "unknown".into(),
            };
        };
        if ids.is_empty() {
            return RuntimeState {
                actual_image: None,
                actual_image_id: None,
                replicas: 0,
                mixed_images: false,
                container_status: "down".into(),
            };
        }
        self.inspect(&ids, limit)
            .await
            .map(|(state, _)| state)
            .unwrap_or(RuntimeState {
                actual_image: None,
                actual_image_id: None,
                replicas: 0,
                mixed_images: false,
                container_status: "unknown".into(),
            })
    }
}

#[derive(Deserialize)]
struct CanonicalCompose {
    name: String,
    services: BTreeMap<String, CanonicalService>,
}

#[derive(Deserialize)]
struct CanonicalService {
    image: Option<String>,
}

pub async fn resolve_project(
    project: &mut ProjectConfig,
    runner: std::sync::Arc<dyn CommandRunner>,
) -> anyhow::Result<()> {
    let cwd = project.project_dir().to_path_buf();
    let mut args = vec![
        "compose".into(),
        "--project-directory".into(),
        cwd.display().to_string(),
    ];
    for file in &project.compose_files {
        args.extend(["-f".into(), file.display().to_string()]);
    }
    args.extend(["config".into(), "--format".into(), "json".into()]);
    let out = runner
        .run("docker", &args, &cwd, project.command_timeout())
        .await?;
    if !out.success {
        anyhow::bail!(
            "无法解析 Compose 项目 {}\n{}",
            project.compose_files[0].display(),
            out.log
        );
    }
    let canonical: CanonicalCompose = serde_json::from_str(&out.log)
        .map_err(|error| anyhow::anyhow!("docker compose config 返回了无效 JSON: {error}"))?;
    if canonical.name.trim().is_empty() {
        anyhow::bail!("docker compose config 未返回项目名");
    }

    let mut services = Vec::new();
    for (id, service) in canonical.services {
        let Some(image) = service.image.filter(|image| !image.trim().is_empty()) else {
            tracing::warn!(project=%canonical.name, service=%id, "忽略没有 image 的 Compose 服务");
            continue;
        };
        services.push(service_from_image(id, image)?);
    }
    project.id = canonical.name;
    project.services = services;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    struct FakeRunner(Mutex<Vec<CommandOutput>>);
    #[async_trait]
    impl CommandRunner for FakeRunner {
        async fn run(
            &self,
            _program: &str,
            _args: &[String],
            _cwd: &Path,
            _timeout_for: Duration,
        ) -> anyhow::Result<CommandOutput> {
            Ok(self.0.lock().unwrap().remove(0))
        }
    }

    #[derive(Default)]
    struct RecordingRunner(Mutex<Vec<(Vec<String>, PathBuf)>>);
    #[async_trait]
    impl CommandRunner for RecordingRunner {
        async fn run(
            &self,
            _program: &str,
            args: &[String],
            cwd: &Path,
            _timeout_for: Duration,
        ) -> anyhow::Result<CommandOutput> {
            self.0
                .lock()
                .unwrap()
                .push((args.to_vec(), cwd.to_path_buf()));
            Ok(CommandOutput {
                success: true,
                log: String::new(),
            })
        }
    }
    #[tokio::test]
    async fn stable_override() {
        let d = tempdir().unwrap();
        let p = d.path().join("o.yaml");
        let s = ServiceConfig {
            id: "a".into(),
            image: "ghcr.io/o/i".into(),
            tag_pattern: ".*".into(),
            version_source: crate::config::VersionSourceConfig::OciRegistry {
                registry: "ghcr.io".into(),
                repository: "o/i".into(),
            },
        };
        let mut images = BTreeMap::new();
        images.insert("a".into(), "ghcr.io/o/i@sha256:digest".into());
        write_image_override(&p, &[s], &images).await.unwrap();
        let text = fs::read_to_string(p).await.unwrap();
        assert!(text.contains("ghcr.io/o/i@sha256:digest"));
    }

    #[tokio::test]
    async fn parses_healthcheck_and_no_healthcheck_states() {
        let dir = tempdir().unwrap();
        let compose_file = dir.path().join("compose.yaml");
        fs::write(&compose_file, "services: {}\n").await.unwrap();
        let output = |log: &str| CommandOutput {
            success: true,
            log: log.into(),
        };
        let runner = Arc::new(FakeRunner(Mutex::new(vec![
            output("one\n"),
            output(
                r#"[{"Config":{"Image":"image:1","Healthcheck":{"Test":["CMD","true"]}},"State":{"Status":"running","Health":{"Status":"healthy"}}}]"#,
            ),
            output("two\n"),
            output(
                r#"[{"Config":{"Image":"image:2","Healthcheck":null},"State":{"Status":"running","Health":null}}]"#,
            ),
            output("three\n"),
            output(
                r#"[{"Config":{"Image":"image:3","Healthcheck":{"Test":["CMD","false"]}},"State":{"Status":"running","Health":{"Status":"unhealthy"}}}]"#,
            ),
            output(""),
        ])));
        let compose = Compose::new(
            ProjectConfig {
                compose_files: vec![compose_file],
                health_timeout_seconds: 1,
                command_timeout_seconds: 1,
                id: "test".into(),
                services: Vec::new(),
            },
            dir.path().join("override.yaml"),
            runner,
        );
        assert_eq!(
            compose
                .runtime("svc", Duration::from_secs(1))
                .await
                .container_status,
            "healthy"
        );
        assert_eq!(
            compose
                .runtime("svc", Duration::from_secs(1))
                .await
                .container_status,
            "running"
        );
        assert_eq!(
            compose
                .runtime("svc", Duration::from_secs(1))
                .await
                .container_status,
            "unhealthy"
        );
        assert_eq!(
            compose
                .runtime("svc", Duration::from_secs(1))
                .await
                .container_status,
            "down"
        );
    }

    #[tokio::test]
    async fn batches_project_runtime_and_detects_mixed_replicas() {
        let dir = tempdir().unwrap();
        let compose_file = dir.path().join("compose.yaml");
        fs::write(&compose_file, "services: {}\n").await.unwrap();
        let runner = Arc::new(FakeRunner(Mutex::new(vec![
            CommandOutput {
                success: true,
                log: "one\ntwo\n".into(),
            },
            CommandOutput {
                success: true,
                log: r#"[
                    {"Image":"sha256:one","Config":{"Image":"repo/app@sha256:one","Healthcheck":null,"Labels":{"com.docker.compose.service":"api"}},"State":{"Status":"running","Health":null}},
                    {"Image":"sha256:two","Config":{"Image":"repo/app@sha256:two","Healthcheck":null,"Labels":{"com.docker.compose.service":"api"}},"State":{"Status":"running","Health":null}}
                ]"#
                .into(),
            },
        ])));
        let compose = Compose::new(
            ProjectConfig {
                compose_files: vec![compose_file],
                health_timeout_seconds: 1,
                command_timeout_seconds: 1,
                id: "test".into(),
                services: Vec::new(),
            },
            dir.path().join("override.yaml"),
            runner,
        );
        let states = compose.runtimes(Duration::from_secs(1)).await.unwrap();
        let api = &states["api"];
        assert_eq!(api.replicas, 2);
        assert!(api.mixed_images);
        assert!(api.actual_image.is_none());
        assert!(api.actual_image_id.is_none());
    }

    #[tokio::test]
    async fn resolves_compose_project_and_image_services() {
        let dir = tempdir().unwrap();
        let compose_file = dir.path().join("compose.yaml");
        fs::write(&compose_file, "services: {}\n").await.unwrap();
        let json = r#"{"name":"demo","services":{"api":{"image":"ghcr.io/me/api:1.2.3"},"local":{"build":{"context":"."}}}}"#;
        let runner = Arc::new(FakeRunner(Mutex::new(vec![CommandOutput {
            success: true,
            log: json.into(),
        }])));
        let mut project = ProjectConfig {
            compose_files: vec![compose_file],
            health_timeout_seconds: 1,
            command_timeout_seconds: 1,
            id: String::new(),
            services: Vec::new(),
        };
        resolve_project(&mut project, runner).await.unwrap();
        assert_eq!(project.id, "demo");
        assert_eq!(project.services.len(), 1);
        assert_eq!(project.services[0].id, "api");
        assert_eq!(project.services[0].image, "ghcr.io/me/api");
    }

    #[tokio::test]
    async fn service_commands_preserve_file_order_project_dir_and_log_tail() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("compose.yaml");
        let production = dir.path().join("compose.production.yaml");
        fs::write(&base, "services: {}\n").await.unwrap();
        fs::write(&production, "services: {}\n").await.unwrap();
        let runner = Arc::new(RecordingRunner::default());
        let compose = Compose::new(
            ProjectConfig {
                compose_files: vec![base.clone(), production.clone()],
                health_timeout_seconds: 1,
                command_timeout_seconds: 1,
                id: "demo".into(),
                services: Vec::new(),
            },
            dir.path().join("state/compose.deploy.yaml"),
            runner.clone(),
        );
        compose.pull("api", Duration::from_secs(1)).await.unwrap();
        compose
            .logs("api", 200, Duration::from_secs(1))
            .await
            .unwrap();
        compose
            .recreate("api", Duration::from_secs(1))
            .await
            .unwrap();
        compose.stop("api", Duration::from_secs(1)).await.unwrap();
        compose.remove("api", Duration::from_secs(1)).await.unwrap();
        let calls = runner.0.lock().unwrap();
        let (args, cwd) = &calls[0];
        assert_eq!(cwd, dir.path());
        assert_eq!(args.last().unwrap(), "api");
        assert_eq!(args[1..3], ["--project-name", "demo"]);
        let base_at = args
            .iter()
            .position(|arg| arg == &base.display().to_string())
            .unwrap();
        let production_at = args
            .iter()
            .position(|arg| arg == &production.display().to_string())
            .unwrap();
        let override_at = args
            .iter()
            .position(|arg| arg.ends_with("compose.deploy.yaml"))
            .unwrap();
        assert!(base_at < production_at && production_at < override_at);
        assert_eq!(args[args.len() - 2], "pull");
        let (log_args, log_cwd) = &calls[1];
        assert_eq!(log_cwd, dir.path());
        assert_eq!(
            &log_args[log_args.len() - 5..],
            ["logs", "--no-color", "--tail", "200", "api"]
        );
        assert_eq!(
            &calls[2].0[calls[2].0.len() - 5..],
            ["up", "-d", "--force-recreate", "--no-deps", "api"]
        );
        assert!(!calls[2].0.iter().any(|arg| arg == "pull"));
        assert_eq!(&calls[3].0[calls[3].0.len() - 2..], ["stop", "api"]);
        assert_eq!(
            &calls[4].0[calls[4].0.len() - 4..],
            ["rm", "--stop", "--force", "api"]
        );
    }

    #[tokio::test]
    async fn real_compose_keeps_first_file_as_relative_path_base_when_available() {
        let available = std::process::Command::new("docker")
            .args(["compose", "version"])
            .output()
            .is_ok_and(|output| output.status.success());
        if !available {
            return;
        }

        let dir = tempdir().unwrap();
        let base = dir.path().join("compose.yaml");
        let overlay = dir.path().join("compose.production.yaml");
        fs::write(
            &base,
            "services:\n  api:\n    image: nginx:1.26\n    volumes:\n      - ./data:/data\n",
        )
        .await
        .unwrap();
        fs::write(&overlay, "services:\n  api:\n    image: nginx:1.27\n")
            .await
            .unwrap();
        let args = vec![
            "compose".into(),
            "--project-directory".into(),
            dir.path().display().to_string(),
            "-f".into(),
            base.display().to_string(),
            "-f".into(),
            overlay.display().to_string(),
            "config".into(),
            "--format".into(),
            "json".into(),
        ];
        let output = ProcessRunner
            .run("docker", &args, dir.path(), Duration::from_secs(10))
            .await
            .unwrap();
        assert!(output.success, "{}", output.log);
        let value: serde_json::Value = serde_json::from_str(&output.log).unwrap();
        assert_eq!(value["services"]["api"]["image"], "nginx:1.27");
        assert_eq!(
            value["services"]["api"]["volumes"][0]["source"],
            dir.path().join("data").display().to_string()
        );
    }

    #[tokio::test]
    async fn real_docker_deploys_pinned_digest_and_reads_runtime_when_enabled() {
        if std::env::var_os("WINDPLUME_DOCKER_INTEGRATION").is_none() {
            return;
        }
        let dir = tempdir().unwrap();
        let compose_file = dir.path().join("compose.yaml");
        fs::write(
            &compose_file,
            "services:\n  app:\n    image: alpine:3.20\n    command: [\"sh\", \"-c\", \"sleep 60\"]\n",
        )
        .await
        .unwrap();
        let service = service_from_image("app".into(), "alpine:3.20".into()).unwrap();
        let project = ProjectConfig {
            compose_files: vec![compose_file],
            health_timeout_seconds: 20,
            command_timeout_seconds: 120,
            id: format!("windplumeit{}", std::process::id()),
            services: vec![service.clone()],
        };
        let override_file = dir.path().join("compose.deploy.yaml");
        let runner = Arc::new(ProcessRunner);
        let compose = Compose::new(project.clone(), override_file.clone(), runner.clone());
        let result = async {
            write_image_override(
                &override_file,
                &project.services,
                &BTreeMap::from([("app".into(), "alpine:3.20".into())]),
            )
            .await?;
            compose.pull("app", Duration::from_secs(120)).await?;
            let artifact = compose
                .image_artifact("alpine:3.20", "alpine", Duration::from_secs(30))
                .await?;
            write_image_override(
                &override_file,
                &project.services,
                &BTreeMap::from([("app".into(), artifact.pinned_image.clone())]),
            )
            .await?;
            compose.up("app", Duration::from_secs(60)).await?;
            compose
                .wait_healthy_image(
                    "app",
                    Some(&artifact.image_id),
                    Duration::from_secs(20),
                    Duration::from_secs(20),
                )
                .await?;
            let states = compose.runtimes(Duration::from_secs(20)).await?;
            let state = states
                .get("app")
                .ok_or_else(|| anyhow::anyhow!("integration container missing"))?;
            anyhow::ensure!(state.actual_image_id.as_deref() == Some(artifact.image_id.as_str()));
            anyhow::ensure!(!state.mixed_images);
            anyhow::Ok(())
        }
        .await;
        let (mut args, cwd) = compose.base_args();
        args.extend(["down".into(), "--remove-orphans".into()]);
        let _ = runner
            .run("docker", &args, &cwd, Duration::from_secs(60))
            .await;
        result.unwrap();
    }
}
