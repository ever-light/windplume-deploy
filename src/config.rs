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
fn default_github_api() -> String {
    "https://api.github.com".into()
}
fn default_cache() -> u64 {
    60
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

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub registries: RegistryConfig,
    pub storage: StorageConfig,
    pub projects: Vec<ProjectConfig>,
}

#[derive(Clone, Debug, Deserialize)]
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
pub struct RegistryConfig {
    #[serde(default = "default_cache")]
    pub cache_seconds: u64,
    #[serde(default)]
    pub github: GithubRegistryConfig,
}
impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            cache_seconds: default_cache(),
            github: GithubRegistryConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct GithubRegistryConfig {
    pub token_file: Option<PathBuf>,
    #[serde(default = "default_github_api")]
    pub api_base: String,
}
impl Default for GithubRegistryConfig {
    fn default() -> Self {
        Self {
            token_file: None,
            api_base: default_github_api(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct StorageConfig {
    pub data_dir: PathBuf,
    #[serde(default = "default_history")]
    pub history_limit: u32,
    #[serde(default = "default_log")]
    pub max_log_bytes: usize,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProjectConfig {
    pub id: String,
    pub name: String,
    pub compose: ComposeConfig,
    pub services: Vec<ServiceConfig>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ComposeConfig {
    pub project_name: String,
    pub files: Vec<PathBuf>,
    #[serde(default = "default_health")]
    pub health_timeout_seconds: u64,
    #[serde(default = "default_command")]
    pub command_timeout_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServiceConfig {
    pub id: String,
    pub name: String,
    pub image: String,
    pub compose_service: String,
    pub tag_pattern: String,
    pub version_source: VersionSourceConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VersionSourceConfig {
    GithubPackages {
        owner: String,
        package: String,
        #[serde(default)]
        owner_kind: GithubOwnerKind,
    },
    DockerHub {
        namespace: String,
        repository: String,
    },
    OciRegistry {
        registry: String,
        repository: String,
    },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GithubOwnerKind {
    #[default]
    User,
    Organization,
}

impl VersionSourceConfig {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::GithubPackages { .. } => "github_packages",
            Self::DockerHub { .. } => "docker_hub",
            Self::OciRegistry { .. } => "oci_registry",
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        let values: &[&str] = match self {
            Self::GithubPackages { owner, package, .. } => &[owner, package],
            Self::DockerHub {
                namespace,
                repository,
            } => &[namespace, repository],
            Self::OciRegistry {
                registry,
                repository,
            } => &[registry, repository],
        };
        if values.iter().any(|value| value.trim().is_empty()) {
            bail!("版本来源的 owner、namespace、package 或 repository 不能为空");
        }
        Ok(())
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<(Self, Option<String>)> {
        let text =
            fs::read_to_string(path).with_context(|| format!("无法读取配置 {}", path.display()))?;
        let cfg: Self = serde_yaml::from_str(&text).context("配置 YAML 无效")?;
        let token = cfg.validate()?;
        Ok((cfg, token))
    }

    pub fn validate(&self) -> anyhow::Result<Option<String>> {
        if self.registries.cache_seconds == 0
            || self.storage.history_limit == 0
            || self.storage.max_log_bytes == 0
        {
            bail!("所有缓存和存储限制必须大于零");
        }
        if self.registries.github.api_base.trim().is_empty() {
            bail!("Registry API 地址不能为空");
        }
        if self.projects.is_empty() {
            bail!("至少配置一个 Compose 项目");
        }

        let valid = Regex::new(r"^[A-Za-z0-9_-]+$").unwrap();
        let mut project_ids = HashSet::new();
        for project in &self.projects {
            if !valid.is_match(&project.id) || !project_ids.insert(&project.id) {
                bail!("Compose 项目 ID 必须唯一，且只允许字母、数字、短横线和下划线");
            }
            if project.name.trim().is_empty() || project.compose.project_name.trim().is_empty() {
                bail!("Compose 项目名称不能为空");
            }
            if project.compose.files.is_empty() {
                bail!("Compose 项目 {} 至少需要一个文件", project.id);
            }
            for file in &project.compose.files {
                if !file.is_file() {
                    bail!("Compose 文件不存在: {}", file.display());
                }
            }
            if project.compose.health_timeout_seconds == 0
                || project.compose.command_timeout_seconds == 0
            {
                bail!("Compose 项目 {} 的超时必须大于零", project.id);
            }
            if project.services.is_empty() {
                bail!("Compose 项目 {} 至少需要一个服务", project.id);
            }
            let mut service_ids = HashSet::new();
            let mut compose_services = HashSet::new();
            for service in &project.services {
                if !valid.is_match(&service.id)
                    || !valid.is_match(&service.compose_service)
                    || !service_ids.insert(&service.id)
                    || !compose_services.insert(&service.compose_service)
                {
                    bail!(
                        "项目 {} 的服务 ID 和 Compose 服务名必须唯一，且只允许字母、数字、短横线和下划线",
                        project.id
                    );
                }
                if service.name.trim().is_empty() || service.image.trim().is_empty() {
                    bail!("项目 {} 的服务名称和镜像不能为空", project.id);
                }
                Regex::new(&service.tag_pattern).with_context(|| {
                    format!(
                        "项目 {} 服务 {} 的 tag_pattern 无效",
                        project.id, service.id
                    )
                })?;
                service.version_source.validate()?;
            }
        }

        let token = match &self.registries.github.token_file {
            Some(path) => {
                let token = fs::read_to_string(path).context("无法读取 GitHub Token 文件")?;
                let token = token.trim().to_owned();
                if token.is_empty() {
                    bail!("GitHub Token 文件为空");
                }
                Some(token)
            }
            None => None,
        };

        fs::create_dir_all(&self.storage.data_dir).context("无法创建数据目录")?;
        let probe = self.storage.data_dir.join(".write-test");
        fs::write(&probe, b"").context("数据目录不可写")?;
        fs::remove_file(probe).context("数据目录写入检查失败")?;
        Ok(token)
    }

    pub fn project(&self, id: &str) -> Option<&ProjectConfig> {
        self.projects.iter().find(|project| project.id == id)
    }

    pub fn service(&self, project_id: &str, service_id: &str) -> Option<&ServiceConfig> {
        self.project(project_id)?
            .services
            .iter()
            .find(|service| service.id == service_id)
    }
}

impl ComposeConfig {
    pub fn command_timeout(&self) -> Duration {
        Duration::from_secs(self.command_timeout_seconds)
    }
    pub fn health_timeout(&self) -> Duration {
        Duration::from_secs(self.health_timeout_seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid(dir: &tempfile::TempDir) -> Config {
        let compose = dir.path().join("compose.yaml");
        fs::write(&compose, "services: {}\n").unwrap();
        Config {
            server: ServerConfig::default(),
            registries: RegistryConfig::default(),
            storage: StorageConfig {
                data_dir: dir.path().join("data"),
                history_limit: 10,
                max_log_bytes: 1024,
            },
            projects: vec![ProjectConfig {
                id: "app".into(),
                name: "App".into(),
                compose: ComposeConfig {
                    project_name: "app".into(),
                    files: vec![compose],
                    health_timeout_seconds: 10,
                    command_timeout_seconds: 10,
                },
                services: vec![ServiceConfig {
                    id: "api".into(),
                    name: "API".into(),
                    image: "ghcr.io/owner/api".into(),
                    compose_service: "api".into(),
                    tag_pattern: r"^\d+\.\d+\.\d+$".into(),
                    version_source: VersionSourceConfig::GithubPackages {
                        owner: "owner".into(),
                        package: "api".into(),
                        owner_kind: GithubOwnerKind::User,
                    },
                }],
            }],
        }
    }

    #[test]
    fn accepts_multiple_projects_and_optional_github_token() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = valid(&dir);
        let second_compose = dir.path().join("second.yaml");
        fs::write(&second_compose, "services: {}\n").unwrap();
        cfg.projects.push(ProjectConfig {
            id: "jobs".into(),
            name: "Jobs".into(),
            compose: ComposeConfig {
                project_name: "jobs".into(),
                files: vec![second_compose],
                health_timeout_seconds: 10,
                command_timeout_seconds: 10,
            },
            services: vec![ServiceConfig {
                id: "api".into(),
                name: "Worker".into(),
                image: "example/worker".into(),
                compose_service: "api".into(),
                tag_pattern: ".*".into(),
                version_source: VersionSourceConfig::DockerHub {
                    namespace: "example".into(),
                    repository: "worker".into(),
                },
            }],
        });
        assert!(cfg.validate().unwrap().is_none());
    }

    #[test]
    fn rejects_duplicate_projects_services_invalid_regex_and_missing_compose() {
        let dir = tempfile::tempdir().unwrap();
        let mut duplicate_project = valid(&dir);
        duplicate_project
            .projects
            .push(duplicate_project.projects[0].clone());
        assert!(duplicate_project.validate().is_err());

        let dir = tempfile::tempdir().unwrap();
        let mut duplicate_service = valid(&dir);
        let service = duplicate_service.projects[0].services[0].clone();
        duplicate_service.projects[0].services.push(service);
        assert!(duplicate_service.validate().is_err());

        let dir = tempfile::tempdir().unwrap();
        let mut regex = valid(&dir);
        regex.projects[0].services[0].tag_pattern = "[".into();
        assert!(regex.validate().is_err());

        let dir = tempfile::tempdir().unwrap();
        let mut missing = valid(&dir);
        missing.projects[0].compose.files = vec![dir.path().join("missing.yaml")];
        assert!(missing.validate().is_err());
    }
}
