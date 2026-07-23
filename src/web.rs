use crate::{deployment, error::AppError, state::AppState};
use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{StatusCode, header},
    response::{Html, IntoResponse},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/services", get(services))
        .route("/api/services/{id}/packages", get(packages))
        .route("/api/services/{id}/deploy", post(deploy))
        .route("/api/deployments", get(deployments))
        .route("/api/deployments/{id}", get(deployment))
        .route("/", get(index))
        .route("/assets/app.js", get(js))
        .route("/assets/app.css", get(css))
        .with_state(state)
}
async fn health(State(s): State<AppState>) -> Result<Json<serde_json::Value>, AppError> {
    s.storage.health().await?;
    Ok(Json(serde_json::json!({"status":"ok"})))
}

#[derive(Serialize)]
struct ServiceView {
    id: String,
    name: String,
    compose_service: String,
    desired_version: Option<String>,
    desired_image: Option<String>,
    actual_image: Option<String>,
    container_status: String,
    drift: bool,
    deployment_in_progress: bool,
}
async fn services(State(s): State<AppState>) -> Result<Json<Vec<ServiceView>>, AppError> {
    let mut out = Vec::new();
    let busy = s.deploy_lock.available_permits() == 0;
    for svc in &s.config.services {
        let desired = s.storage.state(&svc.id).await?;
        let runtime = s
            .compose
            .runtime(
                &svc.compose_service,
                std::time::Duration::from_secs(15).min(s.config.command_timeout()),
            )
            .await;
        let desired_image = desired.as_ref().map(|x| x.image.clone());
        let drift = match (&desired_image, &runtime.actual_image) {
            (Some(a), Some(b)) => a != b,
            (None, None) => false,
            _ => true,
        };
        out.push(ServiceView {
            id: svc.id.clone(),
            name: svc.name.clone(),
            compose_service: svc.compose_service.clone(),
            desired_version: desired.map(|x| x.desired_version),
            desired_image,
            actual_image: runtime.actual_image,
            container_status: runtime.container_status,
            drift,
            deployment_in_progress: busy,
        });
    }
    Ok(Json(out))
}
#[derive(Default, Deserialize)]
struct PackageQuery {
    #[serde(default)]
    refresh: bool,
}
async fn packages(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<PackageQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let svc = s
        .config
        .services
        .iter()
        .find(|x| x.id == id)
        .ok_or(AppError::NotFound)?;
    let versions = s.github.versions(svc, q.refresh).await?;
    Ok(Json(
        serde_json::json!({"service_id":id,"versions":versions}),
    ))
}
#[derive(Deserialize)]
struct DeployRequest {
    version: String,
}
async fn deploy(
    State(s): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<DeployRequest>, JsonRejection>,
) -> Result<impl IntoResponse, AppError> {
    let Json(req) = body.map_err(|e| AppError::Invalid(e.body_text()))?;
    let d = deployment::enqueue(s, &id, req.version.trim()).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"deployment_id":d.id,"status":d.status})),
    ))
}
#[derive(Default, Deserialize)]
struct HistoryQuery {
    limit: Option<u32>,
}
async fn deployments(
    State(s): State<AppState>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<Vec<crate::storage::Deployment>>, AppError> {
    Ok(Json(
        s.storage
            .deployments(q.limit.unwrap_or(50).clamp(1, 500))
            .await?,
    ))
}
async fn deployment(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<crate::storage::Deployment>, AppError> {
    s.storage
        .deployment(&id)
        .await?
        .map(Json)
        .ok_or(AppError::NotFound)
}
async fn index() -> Html<&'static str> {
    Html(include_str!("assets/index.html"))
}
async fn js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("assets/app.js"),
    )
}
async fn css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("assets/app.css"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        compose::{Compose, ProcessRunner},
        config::{ComposeConfig, Config, GithubConfig, ServerConfig, StorageConfig},
        github::GithubClient,
        storage::Storage,
    };
    use axum::{body::Body, http::Request};
    use std::{path::PathBuf, sync::Arc, time::Duration};
    use tokio::sync::Semaphore;
    use tower::ServiceExt;

    async fn app(dir: &tempfile::TempDir) -> Router {
        let compose_file = dir.path().join("compose.yaml");
        tokio::fs::write(&compose_file, "services: {}\n")
            .await
            .unwrap();
        let config = Arc::new(Config {
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
            },
            github: GithubConfig {
                token_file: PathBuf::from("unused"),
                api_base: "http://127.0.0.1:1".into(),
                cache_seconds: 60,
            },
            storage: StorageConfig {
                data_dir: dir.path().into(),
                history_limit: 10,
                max_log_bytes: 1024,
            },
            compose: ComposeConfig {
                project_name: "test".into(),
                file: compose_file,
                health_timeout_seconds: 1,
                command_timeout_seconds: 1,
            },
            services: Vec::new(),
        });
        let storage = Storage::open(dir.path(), 10, 1024).await.unwrap();
        let github = GithubClient::new(
            config.github.api_base.clone(),
            "secret".into(),
            Duration::from_secs(60),
        )
        .unwrap();
        let override_file = dir.path().join("override.yaml");
        let compose = Compose::new(
            config.compose.clone(),
            override_file.clone(),
            Arc::new(ProcessRunner),
        );
        router(AppState {
            config,
            storage,
            github,
            compose,
            override_file,
            deploy_lock: Arc::new(Semaphore::new(1)),
        })
    }

    #[tokio::test]
    async fn health_static_assets_and_json_rejection_have_expected_content_type() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(&dir).await;
        let health = app
            .clone()
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
        assert_eq!(health.headers()[header::CONTENT_TYPE], "application/json");

        let js = app
            .clone()
            .oneshot(Request::get("/assets/app.js").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            js.headers()[header::CONTENT_TYPE],
            "text/javascript; charset=utf-8"
        );

        let rejected = app
            .oneshot(
                Request::post("/api/services/missing/deploy")
                    .header(header::CONTENT_TYPE, "text/plain")
                    .body(Body::from("no"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        assert_eq!(rejected.headers()[header::CONTENT_TYPE], "application/json");
    }
}
