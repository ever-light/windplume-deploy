use crate::{
    config::{GithubOwnerKind, VersionSourceConfig},
    error::AppError,
};
use chrono::{DateTime, Utc};
use regex::Regex;
use reqwest::{Client, RequestBuilder, StatusCode, header};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
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

#[derive(Deserialize)]
struct GithubVersion {
    id: i64,
    name: String,
    metadata: GithubMetadata,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}
#[derive(Deserialize)]
struct GithubMetadata {
    container: GithubContainer,
}
#[derive(Deserialize)]
struct GithubContainer {
    tags: Vec<String>,
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

type Cache = Arc<RwLock<HashMap<String, (Instant, Vec<PackageVersion>)>>>;

#[derive(Clone)]
pub struct RegistryClient {
    client: Client,
    github_api_base: String,
    github_token: Option<String>,
    ttl: Duration,
    cache: Cache,
}

impl RegistryClient {
    pub fn new(
        github_api_base: String,
        github_token: Option<String>,
        ttl: Duration,
    ) -> Result<Self, AppError> {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            "application/vnd.github+json".parse().unwrap(),
        );
        headers.insert("X-GitHub-Api-Version", "2022-11-28".parse().unwrap());
        let client = Client::builder()
            .user_agent("windplume-deploy/0.1")
            .default_headers(headers)
            .build()
            .map_err(|e| AppError::Internal(e.to_string()))?;
        Ok(Self {
            client,
            github_api_base: github_api_base.trim_end_matches('/').into(),
            github_token,
            ttl,
            cache: Default::default(),
        })
    }

    pub async fn versions(
        &self,
        cache_key: &str,
        source: &VersionSourceConfig,
        pattern: &str,
        refresh: bool,
    ) -> Result<Vec<PackageVersion>, AppError> {
        if !refresh
            && let Some((at, result)) = self.cache.read().await.get(cache_key)
            && at.elapsed() < self.ttl
        {
            return Ok(result.clone());
        }

        let raw = match source {
            VersionSourceConfig::GithubPackages {
                owner,
                package,
                owner_kind,
            } => self.github_versions(owner, package, *owner_kind).await?,
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
        self.cache
            .write()
            .await
            .insert(cache_key.into(), (Instant::now(), result.clone()));
        Ok(result)
    }

    async fn github_versions(
        &self,
        owner: &str,
        package: &str,
        owner_kind: GithubOwnerKind,
    ) -> Result<Vec<RawVersion>, AppError> {
        let owner_path = match owner_kind {
            GithubOwnerKind::User => "users",
            GithubOwnerKind::Organization => "orgs",
        };
        let mut page = 1;
        let mut out = Vec::new();
        loop {
            let url = format!(
                "{}/{}/{}/packages/container/{}/versions",
                self.github_api_base, owner_path, owner, package
            );
            let request = self
                .client
                .get(url)
                .query(&[("per_page", 100), ("page", page)]);
            let response = self
                .github_auth(request)
                .send()
                .await
                .map_err(|e| AppError::Package(network_message("GitHub", &e)))?;
            if !response.status().is_success() {
                let status = response.status();
                let msg = match status {
                    StatusCode::UNAUTHORIZED => "GitHub Token 无效",
                    StatusCode::FORBIDDEN => "GitHub 拒绝访问或已限流",
                    StatusCode::NOT_FOUND => "GitHub 包不存在、不是公开包或无权访问",
                    _ => "GitHub API 请求失败",
                };
                return Err(AppError::Package(format!("{msg} ({status})")));
            }
            let batch: Vec<GithubVersion> = response
                .json()
                .await
                .map_err(|_| AppError::Package("GitHub 返回了无效响应".into()))?;
            let count = batch.len();
            for item in batch {
                for tag in item.metadata.container.tags {
                    out.push(RawVersion {
                        version: tag,
                        source_id: item.id.to_string(),
                        digest: Some(item.name.clone()).filter(|value| !value.is_empty()),
                        created_at: Some(item.created_at),
                        updated_at: Some(item.updated_at),
                    });
                }
            }
            if count < 100 {
                break;
            }
            page += 1;
        }
        Ok(out)
    }

    async fn oci_versions(
        &self,
        registry: &str,
        repository: &str,
    ) -> Result<Vec<RawVersion>, AppError> {
        let registry = registry.trim_end_matches('/');
        let registry = if registry.starts_with("http://") || registry.starts_with("https://") {
            registry.to_owned()
        } else {
            format!("https://{registry}")
        };
        let url = format!("{registry}/v2/{repository}/tags/list");
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
            let token_response = request
                .send()
                .await
                .map_err(|error| AppError::Package(network_message("OCI Registry", &error)))?;
            if !token_response.status().is_success() {
                return Err(AppError::Package(format!(
                    "OCI Registry 无法签发公开拉取 Token ({})",
                    token_response.status()
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

    fn github_auth(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.github_token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }
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
}
