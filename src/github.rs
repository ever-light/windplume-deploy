use crate::{config::ServiceConfig, error::AppError};
use chrono::{DateTime, Utc};
use regex::Regex;
use reqwest::{Client, StatusCode, header};
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
    pub package_version_id: i64,
    pub digest: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
#[derive(Deserialize)]
struct ApiVersion {
    id: i64,
    name: String,
    metadata: Metadata,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}
#[derive(Deserialize)]
struct Metadata {
    container: Container,
}
#[derive(Deserialize)]
struct Container {
    tags: Vec<String>,
}

type Cache = Arc<RwLock<HashMap<String, (Instant, Vec<PackageVersion>)>>>;
#[derive(Clone)]
pub struct GithubClient {
    client: Client,
    api_base: String,
    token: String,
    ttl: Duration,
    cache: Cache,
}

impl GithubClient {
    pub fn new(api_base: String, token: String, ttl: Duration) -> Result<Self, AppError> {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            "application/vnd.github+json".parse().unwrap(),
        );
        headers.insert("X-GitHub-Api-Version", "2022-11-28".parse().unwrap());
        let client = Client::builder()
            .user_agent("wind-plume-deploy/0.1")
            .default_headers(headers)
            .build()
            .map_err(|e| AppError::Internal(e.to_string()))?;
        Ok(Self {
            client,
            api_base: api_base.trim_end_matches('/').into(),
            token,
            ttl,
            cache: Default::default(),
        })
    }
    pub async fn versions(
        &self,
        svc: &ServiceConfig,
        refresh: bool,
    ) -> Result<Vec<PackageVersion>, AppError> {
        if !refresh
            && let Some((at, result)) = self.cache.read().await.get(&svc.id)
            && at.elapsed() < self.ttl
        {
            return Ok(result.clone());
        }
        let mut page = 1;
        let mut raw = Vec::new();
        loop {
            let url = format!(
                "{}/users/{}/packages/container/{}/versions",
                self.api_base, svc.github_owner, svc.github_package
            );
            let response = self
                .client
                .get(url)
                .bearer_auth(&self.token)
                .query(&[("per_page", 100), ("page", page)])
                .send()
                .await
                .map_err(|e| AppError::Package(network_message(&e)))?;
            if !response.status().is_success() {
                let status = response.status();
                let msg = match status {
                    StatusCode::UNAUTHORIZED => "GitHub Token 无效",
                    StatusCode::FORBIDDEN => "GitHub 拒绝访问或已限流",
                    StatusCode::NOT_FOUND => "GitHub 包不存在或无权访问",
                    _ => "GitHub API 请求失败",
                };
                return Err(AppError::Package(format!("{msg} ({status})")));
            }
            let batch: Vec<ApiVersion> = response
                .json()
                .await
                .map_err(|_| AppError::Package("GitHub 返回了无效响应".into()))?;
            let count = batch.len();
            raw.extend(batch);
            if count < 100 {
                break;
            }
            page += 1;
        }
        let result = normalize(raw, &svc.tag_pattern)?;
        self.cache
            .write()
            .await
            .insert(svc.id.clone(), (Instant::now(), result.clone()));
        Ok(result)
    }
}
fn network_message(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "连接 GitHub 超时".into()
    } else {
        "无法连接 GitHub".into()
    }
}
fn normalize(raw: Vec<ApiVersion>, pattern: &str) -> Result<Vec<PackageVersion>, AppError> {
    let regex = Regex::new(pattern).map_err(|e| AppError::Internal(e.to_string()))?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in raw {
        for tag in item.metadata.container.tags {
            if regex.is_match(&tag) && seen.insert(tag.clone()) {
                out.push(PackageVersion {
                    version: tag,
                    package_version_id: item.id,
                    digest: Some(item.name.clone()).filter(|x| !x.is_empty()),
                    created_at: item.created_at,
                    updated_at: item.updated_at,
                });
            }
        }
    }
    out.sort_by(
        |a, b| match (Version::parse(&a.version), Version::parse(&b.version)) {
            (Ok(a), Ok(b)) => b.cmp(&a),
            (Ok(_), Err(_)) => std::cmp::Ordering::Less,
            (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
            (Err(_), Err(_)) => b.updated_at.cmp(&a.updated_at),
        },
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: i64, name: &str, tags: &[&str], updated: &str) -> ApiVersion {
        ApiVersion {
            id,
            name: name.into(),
            metadata: Metadata {
                container: Container {
                    tags: tags.iter().map(|tag| (*tag).into()).collect(),
                },
            },
            created_at: updated.parse().unwrap(),
            updated_at: updated.parse().unwrap(),
        }
    }

    #[test]
    fn expands_filters_deduplicates_and_sorts_tags() {
        let result = normalize(
            vec![
                item(1, "sha256:a", &["1.2.0", "latest"], "2026-01-01T00:00:00Z"),
                item(2, "sha256:b", &["1.10.0", "1.2.0"], "2026-01-02T00:00:00Z"),
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
        assert_eq!(result[1].digest.as_deref(), Some("sha256:a"));
    }
}
