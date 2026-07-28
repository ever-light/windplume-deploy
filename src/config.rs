use anyhow::{Context, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

fn default_listen() -> SocketAddr {
    "127.0.0.1:8180".parse().unwrap()
}
fn default_cache() -> u64 {
    60
}
fn default_data_dir() -> PathBuf {
    "/var/lib/windplume-deploy".into()
}
fn default_history() -> u32 {
    500
}
fn default_log() -> usize {
    65536
}
fn default_health() -> u64 {
    120
}
fn default_command() -> u64 {
    600
}
fn default_tag_pattern() -> String {
    r"^.+$".into()
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub registries: RegistryConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    pub projects: Vec<ProjectConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryConfig {
    #[serde(default = "default_cache")]
    pub cache_seconds: u64,
}
impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            cache_seconds: default_cache(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "default_history")]
    pub history_limit: u32,
    #[serde(default = "default_log")]
    pub max_log_bytes: usize,
}
impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            history_limit: default_history(),
            max_log_bytes: default_log(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    pub compose_files: Vec<PathBuf>,
    #[serde(default = "default_health")]
    pub health_timeout_seconds: u64,
    #[serde(default = "default_command")]
    pub command_timeout_seconds: u64,
    #[serde(skip)]
    pub id: String,
    #[serde(skip)]
    pub services: Vec<ServiceConfig>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ServiceConfig {
    pub id: String,
    pub image: String,
    pub tag_pattern: String,
    pub version_source: VersionSourceConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VersionSourceConfig {
    DockerHub {
        namespace: String,
        repository: String,
    },
    OciRegistry {
        registry: String,
        repository: String,
    },
}

impl VersionSourceConfig {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::DockerHub { .. } => "docker_hub",
            Self::OciRegistry { .. } => "oci_registry",
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("无法读取配置 {}", path.display()))?;
        let cfg: Self = serde_yaml::from_str(&text).context("配置 YAML 无效")?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.registries.cache_seconds == 0
            || self.storage.history_limit == 0
            || self.storage.max_log_bytes == 0
        {
            bail!("所有缓存和存储限制必须大于零");
        }
        if self.projects.is_empty() {
            bail!("至少配置一个 Compose 项目");
        }

        for project in &self.projects {
            if project.compose_files.is_empty() {
                bail!("每个 Compose 项目至少需要一个 compose_files 路径");
            }
            for file in &project.compose_files {
                if !file.is_absolute() {
                    bail!("Compose 文件必须使用绝对路径: {}", file.display());
                }
                if !file.is_file() {
                    bail!("Compose 文件不存在: {}", file.display());
                }
            }
            if project.health_timeout_seconds == 0 || project.command_timeout_seconds == 0 {
                bail!("Compose 项目的超时必须大于零");
            }
        }

        fs::create_dir_all(&self.storage.data_dir).context("无法创建数据目录")?;
        let probe = self.storage.data_dir.join(".write-test");
        fs::write(&probe, b"").context("数据目录不可写")?;
        fs::remove_file(probe).context("数据目录写入检查失败")?;
        Ok(())
    }

    pub fn validate_resolved(&self) -> anyhow::Result<()> {
        let valid_project = Regex::new(r"^[a-z0-9][a-z0-9_-]*$").unwrap();
        let mut ids = HashSet::new();
        for project in &self.projects {
            if !valid_project.is_match(&project.id) {
                bail!("Compose 解析出的项目名无效: {}", project.id);
            }
            if !ids.insert(&project.id) {
                bail!("Compose 项目名重复: {}", project.id);
            }
            if project.services.is_empty() {
                bail!("Compose 项目 {} 没有可管理的 image 服务", project.id);
            }
        }
        Ok(())
    }
}

impl ProjectConfig {
    pub fn command_timeout(&self) -> Duration {
        Duration::from_secs(self.command_timeout_seconds)
    }

    pub fn health_timeout(&self) -> Duration {
        Duration::from_secs(self.health_timeout_seconds)
    }

    pub fn project_dir(&self) -> &Path {
        self.compose_files[0]
            .parent()
            .expect("绝对 Compose 文件应有父目录")
    }
}

pub fn service_from_image(id: String, image: String) -> anyhow::Result<ServiceConfig> {
    let repository = image
        .split_once('@')
        .map_or(image.as_str(), |(value, _)| value);
    let last_slash = repository.rfind('/');
    let last_colon = repository.rfind(':');
    let repository = match (last_slash, last_colon) {
        (_, Some(colon)) if last_slash.is_none_or(|slash| colon > slash) => &repository[..colon],
        _ => repository,
    };
    if repository.is_empty() {
        bail!("服务 {id} 的 image 无效: {image}");
    }

    let parts = repository.split('/').collect::<Vec<_>>();
    let first = parts[0];
    let has_registry = first == "localhost" || first.contains('.') || first.contains(':');
    let (base_image, version_source) = if has_registry {
        if parts.len() < 2 {
            bail!("服务 {id} 的 image 缺少 repository: {image}");
        }
        let path = parts[1..].join("/");
        if matches!(
            first,
            "docker.io" | "index.docker.io" | "registry-1.docker.io"
        ) {
            docker_hub_source(repository.to_owned(), &path)?
        } else {
            (
                repository.to_owned(),
                VersionSourceConfig::OciRegistry {
                    registry: first.to_owned(),
                    repository: path,
                },
            )
        }
    } else {
        docker_hub_source(repository.to_owned(), repository)?
    };

    Ok(ServiceConfig {
        id,
        image: base_image,
        tag_pattern: default_tag_pattern(),
        version_source,
    })
}

fn docker_hub_source(
    base_image: String,
    path: &str,
) -> anyhow::Result<(String, VersionSourceConfig)> {
    let (namespace, repository) = path
        .split_once('/')
        .map_or(("library", path), |(namespace, repository)| {
            (namespace, repository)
        });
    if namespace.is_empty() || repository.is_empty() {
        bail!("Docker Hub 镜像路径无效: {path}");
    }
    Ok((
        base_image,
        VersionSourceConfig::DockerHub {
            namespace: namespace.to_owned(),
            repository: repository.to_owned(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_yaml_uses_operational_defaults_and_rejects_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let compose = dir.path().join("compose.yaml");
        fs::write(&compose, "services: {}\n").unwrap();
        let config = dir.path().join("config.yaml");
        fs::write(
            &config,
            format!(
                "storage:\n  data_dir: {}\nprojects:\n  - compose_files:\n      - {}\n",
                dir.path().join("data").display(),
                compose.display()
            ),
        )
        .unwrap();

        let cfg = Config::load(&config).unwrap();
        assert_eq!(cfg.server.listen, default_listen());
        assert_eq!(cfg.storage.history_limit, 500);
        assert_eq!(cfg.projects[0].health_timeout_seconds, 120);

        fs::write(
            &config,
            format!(
                "projects:\n  - unexpected: value\n    compose_files:\n      - {}\n",
                compose.display()
            ),
        )
        .unwrap();
        assert!(Config::load(&config).is_err());
    }

    #[test]
    fn requires_absolute_existing_compose_files() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let cfg = Config {
            server: ServerConfig::default(),
            registries: RegistryConfig::default(),
            storage: StorageConfig {
                data_dir: data,
                ..StorageConfig::default()
            },
            projects: vec![ProjectConfig {
                compose_files: vec!["relative.yaml".into()],
                health_timeout_seconds: 10,
                command_timeout_seconds: 10,
                id: String::new(),
                services: Vec::new(),
            }],
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn infers_docker_hub_ghcr_and_registry_with_port() {
        let official = service_from_image("web".into(), "nginx:1.27".into()).unwrap();
        assert_eq!(official.image, "nginx");
        assert!(matches!(
            official.version_source,
            VersionSourceConfig::DockerHub {
                ref namespace,
                ref repository
            } if namespace == "library" && repository == "nginx"
        ));

        let ghcr = service_from_image("api".into(), "ghcr.io/owner/api:1.2.3".into()).unwrap();
        assert_eq!(ghcr.image, "ghcr.io/owner/api");
        assert!(matches!(
            ghcr.version_source,
            VersionSourceConfig::OciRegistry {
                ref registry,
                ref repository
            } if registry == "ghcr.io" && repository == "owner/api"
        ));

        let private = service_from_image(
            "job".into(),
            "registry.example.test:5000/team/job@sha256:abc".into(),
        )
        .unwrap();
        assert_eq!(private.image, "registry.example.test:5000/team/job");
    }
}
