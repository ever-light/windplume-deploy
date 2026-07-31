use crate::{config::VersionSourceConfig, error::AppError};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use regex::Regex;
use reqwest::{Client, StatusCode, header};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;

#[derive(Clone, Debug, Serialize)]
pub struct PackageVersion {
    pub version: String,
    pub source_id: String,
    pub digest: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Clone)]
struct RawVersion {
    version: String,
    source_id: String,
    digest: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct OciTags {
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct OciToken {
    token: Option<String>,
    access_token: Option<String>,
}

#[derive(Default, Deserialize)]
struct DockerConfig {
    #[serde(default)]
    auths: HashMap<String, DockerAuth>,
}

#[derive(Default, Deserialize)]
struct DockerAuth {
    auth: Option<String>,
}

struct RegistryCredentials {
    username: String,
    password: String,
}

type Cache = Arc<RwLock<HashMap<String, (Instant, Vec<PackageVersion>)>>>;

#[derive(Clone)]
pub struct RegistryClient {
    client: Client,
    ttl: Duration,
    cache: Cache,
    docker_config: Option<PathBuf>,
}

impl RegistryClient {
    pub fn new(ttl: Duration) -> Result<Self, AppError> {
        let client = Client::builder()
            .user_agent("windplume-deploy/0.1")
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| AppError::Internal(e.to_string()))?;
        Ok(Self {
            client,
            ttl,
            cache: Default::default(),
            docker_config: docker_config_path(),
        })
    }

    pub async fn versions(
        &self,
        cache_key: &str,
        source: &VersionSourceConfig,
        pattern: &str,
        refresh: bool,
    ) -> Result<Vec<PackageVersion>, AppError> {
        if let Some(result) = self.cached_versions(cache_key, refresh).await {
            return Ok(result);
        }

        let raw = match source {
            VersionSourceConfig::DockerHub {
                namespace,
                repository,
            } => {
                self.oci_versions("registry-1.docker.io", &format!("{namespace}/{repository}"))
                    .await?
            }
            VersionSourceConfig::OciRegistry {
                registry,
                repository,
            } => self.oci_versions(registry, repository).await?,
        };
        let result = normalize(raw, pattern)?;
        self.store_versions(cache_key, result.clone()).await;
        Ok(result)
    }

    async fn cached_versions(&self, cache_key: &str, refresh: bool) -> Option<Vec<PackageVersion>> {
        if refresh {
            return None;
        }
        self.cache
            .read()
            .await
            .get(cache_key)
            .filter(|(at, _)| at.elapsed() < self.ttl)
            .map(|(_, result)| result.clone())
    }

    async fn store_versions(&self, cache_key: &str, result: Vec<PackageVersion>) {
        self.cache
            .write()
            .await
            .insert(cache_key.into(), (Instant::now(), result));
    }

    pub async fn invalidate_project(&self, project_id: &str) {
        let prefix = format!("{project_id}/");
        self.cache
            .write()
            .await
            .retain(|key, _| !key.starts_with(&prefix));
    }

    async fn oci_versions(
        &self,
        registry: &str,
        repository: &str,
    ) -> Result<Vec<RawVersion>, AppError> {
        let registry = registry.trim_end_matches('/');
        let credentials = self.registry_credentials(registry).await;
        let registry_url = if registry.starts_with("http://") || registry.starts_with("https://") {
            registry.to_owned()
        } else {
            format!("https://{registry}")
        };
        let url = format!("{registry_url}/v2/{repository}/tags/list");
        let mut response = self
            .client
            .get(&url)
            .query(&[("n", 1000)])
            .send()
            .await
            .map_err(|error| AppError::Package(network_message("OCI Registry", &error)))?;

        if response.status() == StatusCode::UNAUTHORIZED {
            let challenge = response
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| AppError::Package("OCI Registry 未提供 Bearer 认证信息".into()))?;
            let realm = challenge_parameter(challenge, "realm")
                .ok_or_else(|| AppError::Package("OCI Registry 认证地址无效".into()))?;
            let service = challenge_parameter(challenge, "service");
            let scope = challenge_parameter(challenge, "scope")
                .unwrap_or_else(|| format!("repository:{repository}:pull"));
            let mut request = self.client.get(realm).query(&[("scope", scope)]);
            if let Some(service) = service {
                request = request.query(&[("service", service)]);
            }
            if let Some(credentials) = &credentials {
                request = request.basic_auth(&credentials.username, Some(&credentials.password));
            }
            let token_response = request
                .send()
                .await
                .map_err(|error| AppError::Package(network_message("OCI Registry", &error)))?;
            if !token_response.status().is_success() {
                let hint = if credentials.is_some() {
                    "已使用 Docker 登录凭据；请确认 Token 具有 read:packages 权限且账号可读取该镜像"
                } else {
                    "未找到该 Registry 的 Docker 登录凭据；私有镜像请先以服务用户执行 docker login"
                };
                return Err(AppError::Package(format!(
                    "OCI Registry 无法签发拉取 Token ({})；{hint}",
                    token_response.status(),
                )));
            }
            let token: OciToken = token_response
                .json()
                .await
                .map_err(|_| AppError::Package("OCI Registry Token 响应无效".into()))?;
            let token = token
                .token
                .or(token.access_token)
                .ok_or_else(|| AppError::Package("OCI Registry Token 响应缺少 Token".into()))?;
            response = self
                .client
                .get(&url)
                .query(&[("n", 1000)])
                .bearer_auth(token)
                .send()
                .await
                .map_err(|error| AppError::Package(network_message("OCI Registry", &error)))?;
        }

        if !response.status().is_success() {
            return Err(AppError::Package(format!(
                "OCI Registry 标签请求失败 ({})",
                response.status()
            )));
        }
        let tags: OciTags = response
            .json()
            .await
            .map_err(|_| AppError::Package("OCI Registry 返回了无效响应".into()))?;
        Ok(tags
            .tags
            .unwrap_or_default()
            .into_iter()
            .map(|tag| RawVersion {
                source_id: tag.clone(),
                version: tag,
                digest: None,
                created_at: None,
                updated_at: None,
            })
            .collect())
    }

    async fn registry_credentials(&self, registry: &str) -> Option<RegistryCredentials> {
        let path = self.docker_config.as_ref()?;
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => {
                tracing::warn!(path=%path.display(), error=%error, "cannot read Docker config");
                return None;
            }
        };
        let config: DockerConfig = match serde_json::from_slice(&bytes) {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!(path=%path.display(), error=%error, "invalid Docker config");
                return None;
            }
        };
        let auth = config.auths.iter().find_map(|(server, value)| {
            same_registry(server, registry)
                .then_some(value.auth.as_deref())
                .flatten()
        })?;
        match decode_docker_auth(auth) {
            Some(credentials) => Some(credentials),
            None => {
                tracing::warn!(registry=%registry, "invalid inline Docker registry credentials");
                None
            }
        }
    }
}

fn docker_config_path() -> Option<PathBuf> {
    if let Some(directory) = std::env::var_os("DOCKER_CONFIG") {
        return Some(PathBuf::from(directory).join("config.json"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".docker/config.json"))
}

fn registry_name(value: &str) -> &str {
    value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .unwrap_or(value)
        .trim_end_matches('/')
}

fn same_registry(configured: &str, requested: &str) -> bool {
    let configured = registry_name(configured);
    let requested = registry_name(requested);
    configured == requested
        || matches!(
            (configured, requested),
            ("index.docker.io/v1", "registry-1.docker.io")
                | ("registry-1.docker.io", "index.docker.io/v1")
        )
}

fn decode_docker_auth(auth: &str) -> Option<RegistryCredentials> {
    let decoded = STANDARD.decode(auth).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (username, password) = decoded.split_once(':')?;
    (!username.is_empty() && !password.is_empty()).then(|| RegistryCredentials {
        username: username.to_owned(),
        password: password.to_owned(),
    })
}

fn network_message(source: &str, error: &reqwest::Error) -> String {
    if error.is_timeout() {
        format!("连接 {source} 超时")
    } else {
        format!("无法连接 {source}")
    }
}

fn challenge_parameter(challenge: &str, name: &str) -> Option<String> {
    let value = challenge.strip_prefix("Bearer ")?;
    value.split(',').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        (key == name).then(|| value.trim_matches('"').to_owned())
    })
}

fn normalize(raw: Vec<RawVersion>, pattern: &str) -> Result<Vec<PackageVersion>, AppError> {
    let regex = Regex::new(pattern).map_err(|error| AppError::Internal(error.to_string()))?;
    let mut seen = HashSet::new();
    let mut out = raw
        .into_iter()
        .filter(|item| regex.is_match(&item.version) && seen.insert(item.version.clone()))
        .map(|item| PackageVersion {
            version: item.version,
            source_id: item.source_id,
            digest: item.digest,
            created_at: item.created_at,
            updated_at: item.updated_at,
        })
        .collect::<Vec<_>>();
    out.sort_by(
        |a, b| match (Version::parse(&a.version), Version::parse(&b.version)) {
            (Ok(a), Ok(b)) => b.cmp(&a),
            (Ok(_), Err(_)) => std::cmp::Ordering::Less,
            (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
            (Err(_), Err(_)) => b
                .updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.version.cmp(&a.version)),
        },
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: i64, tag: &str, updated: &str) -> RawVersion {
        let updated = updated.parse().unwrap();
        RawVersion {
            version: tag.into(),
            source_id: id.to_string(),
            digest: Some(format!("sha256:{id}")),
            created_at: Some(updated),
            updated_at: Some(updated),
        }
    }

    #[test]
    fn filters_deduplicates_and_sorts_tags_from_any_source() {
        let result = normalize(
            vec![
                item(1, "1.2.0", "2026-01-01T00:00:00Z"),
                item(2, "latest", "2026-01-03T00:00:00Z"),
                item(3, "1.10.0", "2026-01-02T00:00:00Z"),
                item(4, "1.2.0", "2026-01-04T00:00:00Z"),
            ],
            r"^\d+\.\d+\.\d+$",
        )
        .unwrap();
        assert_eq!(
            result
                .iter()
                .map(|item| item.version.as_str())
                .collect::<Vec<_>>(),
            ["1.10.0", "1.2.0"]
        );
        assert_eq!(result[1].digest.as_deref(), Some("sha256:1"));
    }

    #[test]
    fn parses_standard_bearer_challenge() {
        let challenge = r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:owner/image:pull""#;
        assert_eq!(
            challenge_parameter(challenge, "realm").as_deref(),
            Some("https://ghcr.io/token")
        );
        assert_eq!(
            challenge_parameter(challenge, "scope").as_deref(),
            Some("repository:owner/image:pull")
        );
    }

    #[test]
    fn parses_oci_tag_response_used_by_ghcr_and_docker_hub() {
        let oci: OciTags = serde_json::from_value(serde_json::json!({
            "name": "owner/image",
            "tags": ["2.0.0", "1.0.0"]
        }))
        .unwrap();
        assert_eq!(oci.tags.unwrap(), ["2.0.0", "1.0.0"]);
    }

    #[test]
    fn reads_inline_docker_login_credentials_without_exposing_secret() {
        let auth = STANDARD.encode("octocat:github_pat_secret");
        let credentials = decode_docker_auth(&auth).unwrap();
        assert_eq!(credentials.username, "octocat");
        assert_eq!(credentials.password, "github_pat_secret");
        assert!(same_registry("https://ghcr.io", "ghcr.io"));
    }

    #[tokio::test]
    async fn invalidates_only_selected_project_cache() {
        let client = RegistryClient::new(Duration::from_secs(60)).unwrap();
        let cached = PackageVersion {
            version: "1.0.0".into(),
            source_id: "1.0.0".into(),
            digest: None,
            created_at: None,
            updated_at: None,
        };
        let now = Instant::now();
        client
            .cache
            .write()
            .await
            .insert("app/api".into(), (now, vec![cached.clone()]));
        client
            .cache
            .write()
            .await
            .insert("other/api".into(), (now, vec![cached]));

        client.invalidate_project("app").await;

        let cache = client.cache.read().await;
        assert!(!cache.contains_key("app/api"));
        assert!(cache.contains_key("other/api"));
    }

    #[tokio::test]
    async fn cache_can_be_bypassed_and_failed_refresh_preserves_previous_versions() {
        let client = RegistryClient::new(Duration::from_secs(604_800)).unwrap();
        let old = PackageVersion {
            version: "1.0.0".into(),
            source_id: "1.0.0".into(),
            digest: None,
            created_at: None,
            updated_at: None,
        };
        client.store_versions("app/api", vec![old]).await;

        let cached = client.cached_versions("app/api", false).await.unwrap();
        assert_eq!(cached[0].version, "1.0.0");
        assert!(client.cached_versions("app/api", true).await.is_none());

        let refreshed = PackageVersion {
            version: "2.0.0".into(),
            source_id: "2.0.0".into(),
            digest: None,
            created_at: None,
            updated_at: None,
        };
        client.store_versions("app/api", vec![refreshed]).await;

        // A failed forced refresh never calls store_versions, so the last good result remains.
        assert!(client.cached_versions("app/api", true).await.is_none());
        let preserved = client.cached_versions("app/api", false).await.unwrap();
        assert_eq!(preserved[0].version, "2.0.0");
    }
}
