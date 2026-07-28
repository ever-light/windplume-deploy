use crate::config::{ProjectConfig, ServiceConfig, service_from_image};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
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
pub async fn write_override(
    path: &Path,
    services: &[ServiceConfig],
    versions: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    let by_id: BTreeMap<_, _> = services.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut map = BTreeMap::new();
    for (id, version) in versions {
        let svc = by_id
            .get(id.as_str())
            .ok_or_else(|| anyhow::anyhow!("未知服务状态 {id}"))?;
        map.insert(
            svc.id.clone(),
            OverrideService {
                image: format!("{}:{}", svc.image, version),
            },
        );
    }
    let bytes = serde_yaml::to_string(&Override { services: map })?.into_bytes();
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
    pub container_status: String,
}
#[derive(Deserialize)]
struct Inspect {
    #[serde(rename = "Config")]
    config: InspectConfig,
    #[serde(rename = "State")]
    state: InspectState,
}
#[derive(Deserialize)]
struct InspectConfig {
    #[serde(rename = "Image")]
    image: String,
    #[serde(rename = "Healthcheck")]
    healthcheck: Option<serde_json::Value>,
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
        args.extend(["up".into(), "-d".into(), service.into()]);
        let out = self.runner.run("docker", &args, &cwd, limit).await?;
        if !out.success {
            anyhow::bail!("docker compose up 执行失败\n{}", out.log);
        }
        Ok(out.log)
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
    ) -> anyhow::Result<(RuntimeState, String, bool)> {
        if ids.is_empty() {
            anyhow::bail!("Compose 未返回容器");
        }
        let mut args = vec!["inspect".into()];
        args.extend_from_slice(ids);
        let cwd = self.project().project_dir().to_path_buf();
        let out = self.runner.run("docker", &args, &cwd, limit).await?;
        if !out.success {
            anyhow::bail!("docker inspect 执行失败\n{}", out.log);
        }
        let values: Vec<Inspect> = serde_json::from_str(&out.log)?;
        if values.len() != ids.len() {
            anyhow::bail!("部分容器无法检查");
        }
        let mut ready = true;
        let mut worst = "healthy".to_owned();
        let mut image = None;
        for v in values {
            image.get_or_insert(v.config.image);
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
        Ok((
            RuntimeState {
                actual_image: image,
                container_status: worst,
            },
            out.log,
            ready,
        ))
    }
    pub async fn wait_healthy(
        &self,
        service: &str,
        health_limit: Duration,
        command_limit: Duration,
    ) -> anyhow::Result<String> {
        let deadline = Instant::now() + health_limit;
        let mut log = String::new();
        loop {
            let (ids, pslog) = self.ids(service, command_limit).await?;
            log.push_str(&pslog);
            match self.inspect(&ids, command_limit).await {
                Ok((_, inspect_log, true)) => {
                    log.push_str(&inspect_log);
                    return Ok(log);
                }
                Ok((state, inspect_log, false)) => {
                    log.push_str(&inspect_log);
                    if matches!(
                        state.container_status.as_str(),
                        "unhealthy" | "exited" | "dead"
                    ) {
                        anyhow::bail!("容器状态异常: {}\n{}", state.container_status, log);
                    }
                }
                Err(e) => {
                    if Instant::now() >= deadline {
                        return Err(e);
                    }
                }
            }
            if Instant::now() >= deadline {
                anyhow::bail!("等待容器健康状态超时\n{log}");
            }
            sleep(Duration::from_secs(2)).await;
        }
    }
    pub async fn runtime(&self, service: &str, limit: Duration) -> RuntimeState {
        let Ok((ids, _)) = self.ids(service, limit).await else {
            return RuntimeState {
                actual_image: None,
                container_status: "unknown".into(),
            };
        };
        if ids.is_empty() {
            return RuntimeState {
                actual_image: None,
                container_status: "down".into(),
            };
        }
        self.inspect(&ids, limit)
            .await
            .map(|x| x.0)
            .unwrap_or(RuntimeState {
                actual_image: None,
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
        let mut v = BTreeMap::new();
        v.insert("a".into(), "1.2.3".into());
        write_override(&p, &[s], &v).await.unwrap();
        let text = fs::read_to_string(p).await.unwrap();
        assert!(text.contains("ghcr.io/o/i:1.2.3"));
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
}
