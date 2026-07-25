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
        .route("/api/projects", get(projects))
        .route(
            "/api/projects/{project_id}/services/{service_id}/versions",
            get(versions),
        )
        .route(
            "/api/projects/{project_id}/services/{service_id}/deploy",
            post(deploy),
        )
        .route("/api/deployments", get(deployments))
        .route("/api/deployments/{id}", get(deployment))
        .route("/", get(index))
        .route("/assets/app.js", get(js))
        .route("/assets/app.css", get(css))
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> Result<Json<serde_json::Value>, AppError> {
    state.storage.health().await?;
    Ok(Json(serde_json::json!({"status":"ok"})))
}

#[derive(Serialize)]
struct ProjectView {
    id: String,
    compose_files: Vec<String>,
    deployment_in_progress: bool,
    services: Vec<ServiceView>,
}

#[derive(Serialize)]
struct ServiceView {
    id: String,
    image: String,
    version_source: &'static str,
    desired_version: Option<String>,
    desired_image: Option<String>,
    actual_image: Option<String>,
    container_status: String,
    drift: bool,
}

async fn projects(State(state): State<AppState>) -> Result<Json<Vec<ProjectView>>, AppError> {
    let mut out = Vec::new();
    for project in &state.config.projects {
        let runtime = state
            .project_runtime(&project.id)
            .ok_or_else(|| AppError::Internal("Compose 项目运行时不存在".into()))?;
        let busy = runtime.deploy_lock.available_permits() == 0;
        let mut services = Vec::new();
        for service in &project.services {
            let desired = state.storage.state(&project.id, &service.id).await?;
            let actual = runtime
                .compose
                .runtime(
                    &service.id,
                    std::time::Duration::from_secs(15).min(project.command_timeout()),
                )
                .await;
            let desired_image = desired.as_ref().map(|item| item.image.clone());
            let drift = match (&desired_image, &actual.actual_image) {
                (Some(expected), Some(running)) => expected != running,
                (None, None) => false,
                _ => true,
            };
            services.push(ServiceView {
                id: service.id.clone(),
                image: service.image.clone(),
                version_source: service.version_source.kind(),
                desired_version: desired.map(|item| item.desired_version),
                desired_image,
                actual_image: actual.actual_image,
                container_status: actual.container_status,
                drift,
            });
        }
        out.push(ProjectView {
            id: project.id.clone(),
            compose_files: project
                .compose_files
                .iter()
                .map(|file| file.display().to_string())
                .collect(),
            deployment_in_progress: busy,
            services,
        });
    }
    Ok(Json(out))
}

#[derive(Default, Deserialize)]
struct VersionQuery {
    #[serde(default)]
    refresh: bool,
}

async fn versions(
    State(state): State<AppState>,
    Path((project_id, service_id)): Path<(String, String)>,
    Query(query): Query<VersionQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let service = state
        .config
        .service(&project_id, &service_id)
        .ok_or(AppError::NotFound)?;
    let cache_key = format!("{project_id}/{service_id}");
    let versions = state
        .registry
        .versions(
            &cache_key,
            &service.version_source,
            &service.tag_pattern,
            query.refresh,
        )
        .await?;
    Ok(Json(serde_json::json!({
        "project_id": project_id,
        "service_id": service_id,
        "source": service.version_source.kind(),
        "versions": versions
    })))
}

#[derive(Deserialize)]
struct DeployRequest {
    version: String,
}

async fn deploy(
    State(state): State<AppState>,
    Path((project_id, service_id)): Path<(String, String)>,
    body: Result<Json<DeployRequest>, JsonRejection>,
) -> Result<impl IntoResponse, AppError> {
    let Json(request) = body.map_err(|error| AppError::Invalid(error.body_text()))?;
    let deployment =
        deployment::enqueue(state, &project_id, &service_id, request.version.trim()).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "deployment_id": deployment.id,
            "status": deployment.status
        })),
    ))
}

#[derive(Default, Deserialize)]
struct HistoryQuery {
    limit: Option<u32>,
}

async fn deployments(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Vec<crate::storage::Deployment>>, AppError> {
    Ok(Json(
        state
            .storage
            .deployments(query.limit.unwrap_or(50).clamp(1, 500))
            .await?,
    ))
}

async fn deployment(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<crate::storage::Deployment>, AppError> {
    state
        .storage
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
        config::{Config, ProjectConfig, RegistryConfig, ServerConfig, StorageConfig},
        registry::RegistryClient,
        state::ProjectRuntime,
        storage::Storage,
    };
    use axum::{body::Body, http::Request};
    use std::{collections::HashMap, sync::Arc, time::Duration};
    use tokio::sync::Semaphore;
    use tower::ServiceExt;

    async fn app(dir: &tempfile::TempDir) -> Router {
        let compose_file = dir.path().join("compose.yaml");
        tokio::fs::write(&compose_file, "services: {}\n")
            .await
            .unwrap();
        let project = ProjectConfig {
            compose_files: vec![compose_file],
            health_timeout_seconds: 1,
            command_timeout_seconds: 1,
            id: "app".into(),
            services: Vec::new(),
        };
        let config = Arc::new(Config {
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
            },
            registries: RegistryConfig::default(),
            storage: StorageConfig {
                data_dir: dir.path().into(),
                history_limit: 10,
                max_log_bytes: 1024,
            },
            projects: vec![project.clone()],
        });
        let storage = Storage::open(dir.path(), 10, 1024).await.unwrap();
        let registry = RegistryClient::new(Duration::from_secs(60)).unwrap();
        let override_file = dir.path().join("override.yaml");
        let runtime = ProjectRuntime {
            compose: Compose::new(project, override_file.clone(), Arc::new(ProcessRunner)),
            override_file,
            deploy_lock: Arc::new(Semaphore::new(1)),
        };
        router(AppState {
            config,
            storage,
            registry,
            projects: Arc::new(HashMap::from([("app".into(), runtime)])),
        })
    }

    #[tokio::test]
    async fn health_projects_static_assets_and_json_rejection_work() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(&dir).await;
        let health = app
            .clone()
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
        assert_eq!(health.headers()[header::CONTENT_TYPE], "application/json");

        let projects = app
            .clone()
            .oneshot(Request::get("/api/projects").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(projects.status(), StatusCode::OK);

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
                Request::post("/api/projects/app/services/missing/deploy")
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
