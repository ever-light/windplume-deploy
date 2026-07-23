use crate::config::{ComposeConfig, ServiceConfig};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
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
            svc.compose_service.clone(),
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
    cfg: ComposeConfig,
    override_file: PathBuf,
    runner: std::sync::Arc<dyn CommandRunner>,
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
    pub fn new(
        cfg: ComposeConfig,
        override_file: PathBuf,
        runner: std::sync::Arc<dyn CommandRunner>,
    ) -> Self {
        Self {
            cfg,
            override_file,
            runner,
        }
    }
    fn cwd(&self) -> &Path {
        self.cfg.file.parent().unwrap_or_else(|| Path::new("."))
    }
    fn base_args(&self) -> Vec<String> {
        vec![
            "compose".into(),
            "--project-name".into(),
            self.cfg.project_name.clone(),
            "-f".into(),
            self.cfg.file.display().to_string(),
            "-f".into(),
            self.override_file.display().to_string(),
        ]
    }
    pub async fn up(&self, service: &str, limit: Duration) -> anyhow::Result<String> {
        let mut args = self.base_args();
        args.extend(["up".into(), "-d".into(), service.into()]);
        let out = self.runner.run("docker", &args, self.cwd(), limit).await?;
        if !out.success {
            anyhow::bail!("docker compose up 执行失败\n{}", out.log);
        }
        Ok(out.log)
    }
    async fn ids(&self, service: &str, limit: Duration) -> anyhow::Result<(Vec<String>, String)> {
        let mut args = self.base_args();
        args.extend(["ps".into(), "-q".into(), service.into()]);
        let out = self.runner.run("docker", &args, self.cwd(), limit).await?;
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
        let out = self.runner.run("docker", &args, self.cwd(), limit).await?;
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
        self.inspect(&ids, limit)
            .await
            .map(|x| x.0)
            .unwrap_or(RuntimeState {
                actual_image: None,
                container_status: "unknown".into(),
            })
    }
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
    #[tokio::test]
    async fn stable_override() {
        let d = tempdir().unwrap();
        let p = d.path().join("o.yaml");
        let s = ServiceConfig {
            id: "a".into(),
            name: "A".into(),
            github_owner: "o".into(),
            github_package: "p".into(),
            image: "ghcr.io/o/i".into(),
            compose_service: "svc".into(),
            tag_pattern: ".*".into(),
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
        ])));
        let compose = Compose::new(
            ComposeConfig {
                project_name: "test".into(),
                file: compose_file,
                health_timeout_seconds: 1,
                command_timeout_seconds: 1,
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
    }
}
