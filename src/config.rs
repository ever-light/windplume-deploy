use anyhow::{Context, bail};
use regex::Regex;
use serde::Deserialize;
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
fn default_api() -> String {
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
    pub github: GithubConfig,
    pub storage: StorageConfig,
    pub compose: ComposeConfig,
    pub services: Vec<ServiceConfig>,
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
pub struct GithubConfig {
    pub token_file: PathBuf,
    #[serde(default = "default_api")]
    pub api_base: String,
    #[serde(default = "default_cache")]
    pub cache_seconds: u64,
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
pub struct ComposeConfig {
    pub project_name: String,
    pub file: PathBuf,
    #[serde(default = "default_health")]
    pub health_timeout_seconds: u64,
    #[serde(default = "default_command")]
    pub command_timeout_seconds: u64,
}
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
pub struct ServiceConfig {
    pub id: String,
    pub name: String,
    pub github_owner: String,
    pub github_package: String,
    pub image: String,
    pub compose_service: String,
    pub tag_pattern: String,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<(Self, String)> {
        let text =
            fs::read_to_string(path).with_context(|| format!("无法读取配置 {}", path.display()))?;
        let cfg: Self = serde_yaml::from_str(&text).context("配置 YAML 无效")?;
        let token = cfg.validate()?;
        Ok((cfg, token))
    }

    pub fn validate(&self) -> anyhow::Result<String> {
        if self.github.cache_seconds == 0
            || self.storage.history_limit == 0
            || self.storage.max_log_bytes == 0
            || self.compose.health_timeout_seconds == 0
            || self.compose.command_timeout_seconds == 0
        {
            bail!("所有缓存、超时和存储限制必须大于零");
        }
        if self.github.api_base.trim().is_empty() || self.compose.project_name.trim().is_empty() {
            bail!("GitHub API 地址和 Compose 项目名不能为空");
        }
        if !self.compose.file.is_file() {
            bail!("Compose 文件不存在: {}", self.compose.file.display());
        }
        let valid = Regex::new(r"^[A-Za-z0-9_-]+$").unwrap();
        let mut ids = HashSet::new();
        let mut compose = HashSet::new();
        if self.services.is_empty() {
            bail!("至少配置一个服务");
        }
        for svc in &self.services {
            if !valid.is_match(&svc.id) || !valid.is_match(&svc.compose_service) {
                bail!("服务 ID 和 Compose 服务名只允许字母、数字、短横线和下划线");
            }
            if !ids.insert(&svc.id) || !compose.insert(&svc.compose_service) {
                bail!("服务 ID 和 Compose 服务名必须唯一");
            }
            if svc.image.trim().is_empty()
                || svc.github_owner.trim().is_empty()
                || svc.github_package.trim().is_empty()
            {
                bail!("镜像、GitHub owner/package 不能为空");
            }
            Regex::new(&svc.tag_pattern)
                .with_context(|| format!("服务 {} 的 tag_pattern 无效", svc.id))?;
        }
        let token =
            fs::read_to_string(&self.github.token_file).context("无法读取 GitHub Token 文件")?;
        let token = token.trim().to_owned();
        if token.is_empty() {
            bail!("GitHub Token 文件为空");
        }
        fs::create_dir_all(&self.storage.data_dir).context("无法创建数据目录")?;
        let probe = self.storage.data_dir.join(".write-test");
        fs::write(&probe, b"").context("数据目录不可写")?;
        fs::remove_file(probe).context("数据目录写入检查失败")?;
        Ok(token)
    }
    pub fn command_timeout(&self) -> Duration {
        Duration::from_secs(self.compose.command_timeout_seconds)
    }
    pub fn health_timeout(&self) -> Duration {
        Duration::from_secs(self.compose.health_timeout_seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid(dir: &tempfile::TempDir) -> Config {
        let token = dir.path().join("token");
        let compose = dir.path().join("compose.yaml");
        fs::write(&token, "secret\n").unwrap();
        fs::write(&compose, "services: {}\n").unwrap();
        Config {
            server: ServerConfig::default(),
            github: GithubConfig {
                token_file: token,
                api_base: default_api(),
                cache_seconds: 60,
            },
            storage: StorageConfig {
                data_dir: dir.path().join("data"),
                history_limit: 10,
                max_log_bytes: 1024,
            },
            compose: ComposeConfig {
                project_name: "test".into(),
                file: compose,
                health_timeout_seconds: 10,
                command_timeout_seconds: 10,
            },
            services: vec![ServiceConfig {
                id: "identity".into(),
                name: "Identity".into(),
                github_owner: "owner".into(),
                github_package: "package".into(),
                image: "ghcr.io/owner/image".into(),
                compose_service: "identity-service".into(),
                tag_pattern: r"^\d+\.\d+\.\d+$".into(),
            }],
        }
    }

    #[test]
    fn accepts_complete_configuration() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(valid(&dir).validate().unwrap(), "secret");
    }

    #[test]
    fn rejects_duplicates_regex_missing_token_and_zero_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let mut duplicate = valid(&dir);
        duplicate.services.push(duplicate.services[0].clone());
        assert!(duplicate.validate().is_err());

        let dir = tempfile::tempdir().unwrap();
        let mut regex = valid(&dir);
        regex.services[0].tag_pattern = "[".into();
        assert!(regex.validate().is_err());

        let dir = tempfile::tempdir().unwrap();
        let mut token = valid(&dir);
        token.github.token_file = dir.path().join("missing");
        assert!(token.validate().is_err());

        let dir = tempfile::tempdir().unwrap();
        let mut timeout = valid(&dir);
        timeout.compose.command_timeout_seconds = 0;
        assert!(timeout.validate().is_err());
    }
}
