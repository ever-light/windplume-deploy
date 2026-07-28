use crate::{deployment, error::AppError, state::AppState};
use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{StatusCode, header},
    response::{Html, IntoResponse},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

const CONTAINER_LOG_TAIL: u32 = 200;

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
        .route(
            "/api/projects/{project_id}/services/{service_id}/lifecycle",
            post(lifecycle),
        )
        .route(
            "/api/projects/{project_id}/services/{service_id}/logs",
            get(service_logs),
        )
        .route(
            "/api/projects/{project_id}/refresh-compose",
            post(refresh_compose),
        )
        .route("/api/system/update", get(system_update).post(start_update))
        .route("/api/system/update/status", get(system_update_status))
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
    for configured_project in &state.config.projects {
        let runtime = state
            .project_runtime(&configured_project.id)
            .ok_or_else(|| AppError::Internal("Compose 项目运行时不存在".into()))?;
        let project = runtime.compose.project();
        let busy = state.updates.is_active() || runtime.deploy_lock.available_permits() == 0;
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
    let runtime = state
        .project_runtime(&project_id)
        .ok_or(AppError::NotFound)?;
    let project = runtime.compose.project();
    let service = project
        .services
        .iter()
        .find(|service| service.id == service_id)
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
        .await
        .map_err(|error| {
            tracing::warn!(
                project_id = %project_id,
                service_id = %service_id,
                error = %error,
                "registry version query failed"
            );
            error
        })?;
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

#[derive(Deserialize)]
struct LifecycleRequest {
    action: deployment::LifecycleAction,
}

async fn lifecycle(
    State(state): State<AppState>,
    Path((project_id, service_id)): Path<(String, String)>,
    body: Result<Json<LifecycleRequest>, JsonRejection>,
) -> Result<impl IntoResponse, AppError> {
    let Json(request) = body.map_err(|error| AppError::Invalid(error.body_text()))?;
    let operation =
        deployment::enqueue_lifecycle(state, &project_id, &service_id, request.action).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "operation_id": operation.id,
            "status": operation.status
        })),
    ))
}

async fn service_logs(
    State(state): State<AppState>,
    Path((project_id, service_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    let runtime = state
        .project_runtime(&project_id)
        .ok_or(AppError::NotFound)?;
    let project = runtime.compose.project();
    if !project
        .services
        .iter()
        .any(|service| service.id == service_id)
    {
        return Err(AppError::NotFound);
    }
    let logs = runtime
        .compose
        .logs(
            &service_id,
            CONTAINER_LOG_TAIL,
            std::time::Duration::from_secs(30).min(project.command_timeout()),
        )
        .await
        .map_err(|error| {
            tracing::warn!(
                project_id = %project_id,
                service_id = %service_id,
                error = %error,
                "container log query failed"
            );
            AppError::Internal(error.to_string())
        })?;
    Ok(Json(serde_json::json!({
        "project_id": project_id,
        "service_id": service_id,
        "tail": CONTAINER_LOG_TAIL,
        "logs": logs
    })))
}

async fn refresh_compose(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    if state.updates.is_active() {
        return Err(AppError::Updating);
    }
    let runtime = state
        .project_runtime(&project_id)
        .cloned()
        .ok_or(AppError::NotFound)?;
    let _permit = runtime
        .deploy_lock
        .clone()
        .try_acquire_owned()
        .map_err(|_| AppError::Busy)?;
    let current = runtime.compose.project();
    let candidate = runtime
        .compose
        .resolve_candidate()
        .await
        .map_err(|error| AppError::Invalid(error.to_string()))?;
    if candidate.id != current.id {
        return Err(AppError::Invalid(format!(
            "Compose 项目名从 {} 变为 {}，请重启服务处理项目身份变化",
            current.id, candidate.id
        )));
    }
    if candidate.services.is_empty() {
        return Err(AppError::Invalid(
            "Compose 项目没有可管理的 image 服务".into(),
        ));
    }

    deployment::rebuild_project_override(&state, &candidate)
        .await
        .map_err(|error| AppError::Internal(error.to_string()))?;
    let service_count = candidate.services.len();
    runtime.compose.replace_project(candidate);
    state.registry.invalidate_project(&project_id).await;
    Ok(Json(serde_json::json!({
        "project_id": project_id,
        "service_count": service_count
    })))
}

#[derive(Default, Deserialize)]
struct UpdateQuery {
    #[serde(default)]
    refresh: bool,
}

async fn system_update(
    State(state): State<AppState>,
    Query(query): Query<UpdateQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let latest = state.updates.latest(query.refresh).await?;
    let update_available = match (
        semver::Version::parse(&latest.version),
        semver::Version::parse(crate::update::BUILD_VERSION),
    ) {
        (Ok(latest), Ok(current)) => latest > current,
        _ => false,
    };
    Ok(Json(serde_json::json!({
        "current_version": crate::update::BUILD_VERSION,
        "latest": latest,
        "update_available": update_available,
        "self_update_supported": state.updates.self_update_supported(),
        "status": state.updates.status().await
    })))
}

async fn system_update_status(State(state): State<AppState>) -> Json<crate::update::UpdateStatus> {
    Json(state.updates.status().await)
}

async fn start_update(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    if !state.updates.self_update_supported() {
        return Err(AppError::Update(
            "当前不支持自更新，请确认为 Linux x86_64 且已安装更新助手".into(),
        ));
    }
    let release = state.updates.latest(true).await?;
    let current = semver::Version::parse(crate::update::BUILD_VERSION)
        .map_err(|_| AppError::Update("当前程序版本无效".into()))?;
    let target = semver::Version::parse(&release.version)
        .map_err(|_| AppError::Update("Release 版本无效".into()))?;
    if target <= current {
        return Err(AppError::Invalid("当前已是最新稳定版".into()));
    }
    if !state.updates.begin() {
        return Err(AppError::Updating);
    }
    let mut permits = Vec::new();
    for runtime in state.projects.values() {
        match runtime.deploy_lock.clone().try_acquire_owned() {
            Ok(permit) => permits.push(permit),
            Err(_) => {
                state.updates.cancel();
                return Err(AppError::Busy);
            }
        }
    }
    let manager = state.updates.clone();
    let target_version = release.version.clone();
    tokio::spawn(async move {
        let _permits = permits;
        manager.prepare_and_trigger(release).await;
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "target_version": target_version,
            "status": "preparing"
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
        compose::{CommandOutput, CommandRunner, Compose, ProcessRunner},
        config::{
            Config, ProjectConfig, RegistryConfig, ServerConfig, StorageConfig, service_from_image,
        },
        registry::RegistryClient,
        state::ProjectRuntime,
        storage::Storage,
        update::UpdateManager,
    };
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use std::{
        collections::HashMap,
        path::Path,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio::sync::Semaphore;
    use tower::ServiceExt;

    struct FakeRunner {
        outputs: Mutex<Vec<CommandOutput>>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl CommandRunner for FakeRunner {
        async fn run(
            &self,
            _program: &str,
            args: &[String],
            _cwd: &Path,
            _timeout_for: Duration,
        ) -> anyhow::Result<CommandOutput> {
            self.calls.lock().unwrap().push(args.to_vec());
            Ok(self.outputs.lock().unwrap().remove(0))
        }
    }

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
            updates: UpdateManager::new(dir.path().into()).unwrap(),
            projects: Arc::new(HashMap::from([("app".into(), runtime)])),
        })
    }

    async fn refresh_state(
        dir: &tempfile::TempDir,
        outputs: Vec<CommandOutput>,
    ) -> (AppState, Arc<FakeRunner>) {
        let compose_file = dir.path().join("compose.yaml");
        tokio::fs::write(&compose_file, "services: {}\n")
            .await
            .unwrap();
        let project = ProjectConfig {
            compose_files: vec![compose_file],
            health_timeout_seconds: 1,
            command_timeout_seconds: 1,
            id: "app".into(),
            services: vec![service_from_image("old".into(), "nginx:1.0.0".into()).unwrap()],
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
        let runner = Arc::new(FakeRunner {
            outputs: Mutex::new(outputs),
            calls: Mutex::new(Vec::new()),
        });
        let override_file = dir.path().join("override.yaml");
        let runtime = ProjectRuntime {
            compose: Compose::new(project, override_file.clone(), runner.clone()),
            override_file,
            deploy_lock: Arc::new(Semaphore::new(1)),
        };
        (
            AppState {
                config,
                storage,
                registry: RegistryClient::new(Duration::from_secs(60)).unwrap(),
                updates: UpdateManager::new(dir.path().into()).unwrap(),
                projects: Arc::new(HashMap::from([("app".into(), runtime)])),
            },
            runner,
        )
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
        let js = String::from_utf8(to_bytes(js.into_body(), usize::MAX).await.unwrap().to_vec())
            .unwrap();
        assert!(js.contains("selectService(selected.project.id, selected.service.id, true)"));
        assert!(js.contains("/lifecycle"));
        assert!(js.contains("/api/system/update"));
        assert!(js.contains("重建当前版本"));

        let index = app
            .clone()
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let index = String::from_utf8(
            to_bytes(index.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(index.contains(">刷新版本</button>"));
        assert!(index.contains("系统更新"));

        let css = app
            .clone()
            .oneshot(Request::get("/assets/app.css").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let css = String::from_utf8(
            to_bytes(css.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(css.contains("width:min(61.8vw,1180px)"));
        assert!(css.contains("#container-logs-content{height:55vh"));

        let rejected = app
            .clone()
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

        let missing_logs = app
            .oneshot(
                Request::get("/api/projects/app/services/missing/logs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_logs.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn refresh_updates_snapshot_rejects_name_change_and_respects_lock() {
        let dir = tempfile::tempdir().unwrap();
        let outputs = vec![
            CommandOutput {
                success: true,
                log: r#"{"name":"app","services":{"api":{"image":"ghcr.io/me/api:2.0.0"}}}"#.into(),
            },
            CommandOutput {
                success: true,
                log: r#"{"name":"renamed","services":{"api":{"image":"ghcr.io/me/api:2.0.0"}}}"#
                    .into(),
            },
            CommandOutput {
                success: false,
                log: "invalid compose".into(),
            },
        ];
        let (state, runner) = refresh_state(&dir, outputs).await;

        let refreshed = refresh_compose(State(state.clone()), Path("app".into()))
            .await
            .unwrap();
        assert_eq!(refreshed.0["service_count"], 1);
        let project = state.project_runtime("app").unwrap().compose.project();
        assert_eq!(project.services[0].id, "api");

        let renamed = refresh_compose(State(state.clone()), Path("app".into())).await;
        assert!(matches!(renamed, Err(AppError::Invalid(_))));
        assert_eq!(
            state.project_runtime("app").unwrap().compose.project().id,
            "app"
        );

        let invalid = refresh_compose(State(state.clone()), Path("app".into())).await;
        assert!(matches!(invalid, Err(AppError::Invalid(_))));
        assert_eq!(
            state
                .project_runtime("app")
                .unwrap()
                .compose
                .project()
                .services[0]
                .id,
            "api"
        );

        let permit = state
            .project_runtime("app")
            .unwrap()
            .deploy_lock
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        let busy = refresh_compose(State(state), Path("app".into())).await;
        assert!(matches!(busy, Err(AppError::Busy)));
        drop(permit);

        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert!(
            calls
                .iter()
                .all(|args| args.iter().any(|arg| arg == "config"))
        );
        assert!(
            calls
                .iter()
                .all(|args| !args.iter().any(|arg| arg == "up" || arg == "pull"))
        );
    }
}
