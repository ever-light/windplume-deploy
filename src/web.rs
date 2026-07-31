use crate::{deployment, error::AppError, state::AppState};
use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{Method, Request, StatusCode, header},
    middleware::{Next, from_fn_with_state},
    response::{Html, IntoResponse},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const CONTAINER_LOG_DEFAULT_TAIL: u32 = 50;
const CONTAINER_LOG_MAX_TAIL: u32 = 200;
const HISTORY_RETENTION_DAYS: i64 = 30;
// Keep this slightly below the browser's one-minute cadence so each scheduled
// request can refresh Docker state despite request and command timing jitter.
const RUNTIME_CACHE_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(55);

pub fn router(state: AppState) -> Router {
    let csrf = CsrfToken(Arc::from(uuid::Uuid::new_v4().simple().to_string()));
    Router::new()
        .route("/health", get(health))
        .route("/api/session", get(session))
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
            "/api/projects/{project_id}/services/{service_id}/rollback",
            post(rollback_service),
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
        .route("/api/system/overview", get(system_overview))
        .route("/api/system/images", get(system_images))
        .route("/api/system/images/cleanup", post(cleanup_system_images))
        .route("/api/system/build-cache/cleanup", post(cleanup_build_cache))
        .route("/api/deployments/cleanup", post(cleanup_deployments))
        .route("/api/deployments", get(deployments))
        .route("/api/deployments/{id}", get(deployment))
        .route("/", get(index))
        .route("/assets/app.js", get(js))
        .route("/assets/app.css", get(css))
        .layer(from_fn_with_state(csrf.clone(), csrf_guard))
        .layer(Extension(csrf))
        .with_state(state)
}

#[derive(Clone)]
struct CsrfToken(Arc<str>);

async fn session(Extension(csrf): Extension<CsrfToken>) -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({"csrf_token": csrf.0.as_ref()})),
    )
}

async fn csrf_guard(
    State(csrf): State<CsrfToken>,
    request: Request<Body>,
    next: Next,
) -> axum::response::Response {
    if matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        return next.run(request).await;
    }
    let token_matches = request
        .headers()
        .get("x-windplume-csrf")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == csrf.0.as_ref());
    let origin_matches = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(|origin| {
            let origin_authority = origin
                .parse::<axum::http::Uri>()
                .ok()
                .and_then(|uri| uri.authority().map(|value| value.as_str().to_owned()));
            let host = request
                .headers()
                .get(header::HOST)
                .and_then(|value| value.to_str().ok());
            origin_authority
                .as_deref()
                .zip(host)
                .is_some_and(|(origin, host)| origin.eq_ignore_ascii_case(host))
        })
        .unwrap_or(true);
    if !token_matches || !origin_matches {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "code": "csrf_rejected",
                "message": "请求来源校验失败，请刷新页面后重试"
            })),
        )
            .into_response();
    }
    next.run(request).await
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
    runtime_refreshing: bool,
    runtime_error: Option<String>,
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
    image_status: &'static str,
    drift: bool,
    replicas: usize,
    rollback_available: bool,
}

fn image_status(
    managed: bool,
    runtime_loaded: bool,
    expected_image: Option<&str>,
    expected_image_id: Option<&str>,
    actual: &crate::compose::RuntimeState,
) -> &'static str {
    if !managed {
        return "unmanaged";
    }
    if !runtime_loaded {
        return "unknown";
    }
    if actual.mixed_images {
        return "drift";
    }
    if let Some(expected_id) = expected_image_id {
        return if actual.actual_image_id.as_deref() == Some(expected_id) {
            "matched"
        } else {
            "drift"
        };
    }
    match (expected_image, actual.actual_image.as_deref()) {
        (Some(expected), Some(running)) if expected == running => "matched",
        _ => "drift",
    }
}

async fn projects(State(state): State<AppState>) -> Result<Json<Vec<ProjectView>>, AppError> {
    let mut out = Vec::new();
    for configured_project in &state.config.projects {
        let runtime = state
            .project_runtime(&configured_project.id)
            .ok_or_else(|| AppError::Internal("Compose 项目运行时不存在".into()))?;
        let project = runtime.compose.project();
        let busy = state.updates.is_active() || runtime.deploy_lock.available_permits() == 0;
        let runtime_timeout = std::time::Duration::from_secs(15).min(project.command_timeout());
        let snapshot = runtime.runtime_cache.snapshot(RUNTIME_CACHE_MAX_AGE);
        if snapshot.start_refresh {
            let project_id = project.id.clone();
            let compose = runtime.compose.clone();
            let cache = runtime.runtime_cache.clone();
            let no_services = project.services.is_empty();
            tokio::spawn(async move {
                let result = if no_services {
                    Ok(std::collections::BTreeMap::new())
                } else {
                    compose.runtimes(runtime_timeout).await
                };
                if let Err(error) = &result {
                    tracing::warn!(project_id=%project_id,error=%error,"cannot query project containers");
                }
                cache.complete(result);
            });
        }
        let runtime_loaded = snapshot.states.is_some();
        let mut runtime_states = snapshot.states;
        let mut services = Vec::new();
        for service in &project.services {
            let desired = state.storage.state(&project.id, &service.id).await?;
            let actual = runtime_states
                .as_mut()
                .map(|states| {
                    states
                        .remove(&service.id)
                        .unwrap_or(crate::compose::RuntimeState {
                            actual_image: None,
                            actual_image_id: None,
                            replicas: 0,
                            mixed_images: false,
                            container_status: "down".into(),
                        })
                })
                .unwrap_or(crate::compose::RuntimeState {
                    actual_image: None,
                    actual_image_id: None,
                    replicas: 0,
                    mixed_images: false,
                    container_status: if snapshot.refreshing {
                        "loading".into()
                    } else {
                        "unknown".into()
                    },
                });
            let desired_image = desired.as_ref().map(|item| item.image.clone());
            let expected_image = desired.as_ref().map(|item| item.pinned_image.as_str());
            let image_status = image_status(
                desired.is_some(),
                runtime_loaded,
                expected_image,
                desired.as_ref().map(|item| item.image_id.as_str()),
                &actual,
            );
            let rollback_available = if let Some(desired) = &desired {
                state
                    .storage
                    .previous_revision(&project.id, &service.id, &desired.last_deployment_id)
                    .await?
                    .is_some()
            } else {
                false
            };
            services.push(ServiceView {
                id: service.id.clone(),
                image: service.image.clone(),
                version_source: service.version_source.kind(),
                desired_version: desired.as_ref().map(|item| item.desired_version.clone()),
                desired_image,
                actual_image: actual.actual_image,
                container_status: actual.container_status,
                image_status,
                drift: image_status == "drift",
                replicas: actual.replicas,
                rollback_available,
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
            runtime_refreshing: snapshot.refreshing,
            runtime_error: snapshot.error,
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

async fn rollback_service(
    State(state): State<AppState>,
    Path((project_id, service_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let operation = deployment::enqueue_rollback(state, &project_id, &service_id).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "operation_id": operation.id,
            "status": operation.status
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

#[derive(Default, Deserialize)]
struct LogQuery {
    tail: Option<u32>,
}

async fn service_logs(
    State(state): State<AppState>,
    Path((project_id, service_id)): Path<(String, String)>,
    Query(query): Query<LogQuery>,
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
    let tail = query
        .tail
        .unwrap_or(CONTAINER_LOG_DEFAULT_TAIL)
        .clamp(1, CONTAINER_LOG_MAX_TAIL);
    let logs = runtime
        .compose
        .logs(
            &service_id,
            tail,
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
        "tail": tail,
        "max_tail": CONTAINER_LOG_MAX_TAIL,
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
    runtime.runtime_cache.clear();
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

async fn system_overview(
    State(state): State<AppState>,
) -> Result<Json<crate::system::SystemOverview>, AppError> {
    state
        .system
        .overview()
        .await
        .map(Json)
        .map_err(|error| AppError::System(error.to_string()))
}

async fn protected_images(
    state: &AppState,
) -> Result<std::collections::BTreeMap<String, std::collections::BTreeSet<String>>, AppError> {
    let mut protected =
        std::collections::BTreeMap::<String, std::collections::BTreeSet<String>>::new();
    for runtime in state.projects.values() {
        let project = runtime.compose.project();
        for service in &project.services {
            let Some(current) = state.storage.state(&project.id, &service.id).await? else {
                continue;
            };
            protected
                .entry(current.image_id.clone())
                .or_default()
                .insert("current".into());
            if let Some(previous) = state
                .storage
                .previous_revision(&project.id, &service.id, &current.last_deployment_id)
                .await?
            {
                protected
                    .entry(previous.image_id)
                    .or_default()
                    .insert("rollback".into());
            }
        }
    }
    Ok(protected)
}

fn managed_services(state: &AppState) -> Vec<(String, String, String)> {
    let mut services = state
        .projects
        .values()
        .flat_map(|runtime| {
            let project = runtime.compose.project();
            project
                .services
                .into_iter()
                .map(move |service| (project.id.clone(), service.id, service.image))
        })
        .collect::<Vec<_>>();
    services.sort();
    services
}

async fn managed_images(state: &AppState) -> Result<Vec<crate::system::ManagedImage>, AppError> {
    state
        .system
        .images(&managed_services(state), &protected_images(state).await?)
        .await
        .map_err(|error| AppError::System(error.to_string()))
}

async fn system_images(State(state): State<AppState>) -> Result<Json<serde_json::Value>, AppError> {
    let images = managed_images(&state).await?;
    let removable_count = images.iter().filter(|image| image.removable).count();
    let removable_size_bytes = images
        .iter()
        .filter(|image| image.removable)
        .map(|image| image.size_bytes)
        .sum::<u64>();
    Ok(Json(serde_json::json!({
        "images": images,
        "removable_count": removable_count,
        "removable_size_bytes": removable_size_bytes
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageCleanupRequest {
    image_ids: Vec<String>,
}

fn validate_image_ids(
    image_ids: Vec<String>,
) -> Result<std::collections::BTreeSet<String>, AppError> {
    let image_ids = image_ids
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    if image_ids.is_empty() || image_ids.len() > 100 {
        return Err(AppError::Invalid("请选择 1 到 100 个镜像".into()));
    }
    if image_ids.iter().any(|id| {
        id.len() != 71
            || !id.starts_with("sha256:")
            || !id[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err(AppError::Invalid("镜像 ID 格式无效".into()));
    }
    Ok(image_ids)
}

fn acquire_maintenance_permits(
    state: &AppState,
) -> Result<Vec<tokio::sync::OwnedSemaphorePermit>, AppError> {
    if state.updates.is_active() {
        return Err(AppError::Updating);
    }
    let mut runtimes = state.projects.values().collect::<Vec<_>>();
    runtimes.sort_by_key(|runtime| runtime.compose.project().id);
    let mut permits = Vec::new();
    for runtime in runtimes {
        permits.push(
            runtime
                .deploy_lock
                .clone()
                .try_acquire_owned()
                .map_err(|_| AppError::Busy)?,
        );
    }
    Ok(permits)
}

async fn cleanup_system_images(
    State(state): State<AppState>,
    payload: Result<Json<ImageCleanupRequest>, JsonRejection>,
) -> Result<Json<serde_json::Value>, AppError> {
    let Json(payload) =
        payload.map_err(|error| AppError::Invalid(format!("JSON 请求无效: {error}")))?;
    let requested = validate_image_ids(payload.image_ids)?;
    let _permits = acquire_maintenance_permits(&state)?;
    let candidates = managed_images(&state)
        .await?
        .into_iter()
        .filter(|image| image.removable)
        .map(|image| image.id)
        .collect::<std::collections::BTreeSet<_>>();
    if let Some(id) = requested.iter().find(|id| !candidates.contains(*id)) {
        return Err(AppError::Invalid(format!(
            "镜像已被引用、已不存在或不属于受管 Compose：{id}"
        )));
    }

    let mut deleted_ids = Vec::new();
    let mut failed = Vec::new();
    for id in requested {
        match state.system.remove_image(&id).await {
            Ok(log) => {
                tracing::info!(image_id=%id, output=%log.trim(), "removed managed Docker image");
                deleted_ids.push(id);
            }
            Err(error) => {
                tracing::warn!(image_id=%id, error=%error, "cannot remove managed Docker image");
                failed.push(serde_json::json!({"id": id, "message": error.to_string()}));
            }
        }
    }
    Ok(Json(serde_json::json!({
        "deleted_ids": deleted_ids,
        "failed": failed
    })))
}

async fn cleanup_build_cache(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, AppError> {
    let _permits = acquire_maintenance_permits(&state)?;
    let output = state
        .system
        .prune_build_cache()
        .await
        .map_err(|error| AppError::System(error.to_string()))?;
    tracing::info!(output=%output.trim(), "pruned Docker build cache older than seven days");
    Ok(Json(serde_json::json!({
        "retention_days": 7,
        "output": output.trim()
    })))
}

async fn start_update(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    if !state.updates.self_update_supported() {
        return Err(AppError::Update(
            "当前不支持自更新：需 Linux x86_64，并需运行 install.sh 安装签名公钥和更新助手".into(),
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
) -> Result<Json<Vec<crate::storage::DeploymentSummary>>, AppError> {
    Ok(Json(
        state
            .storage
            .deployments(query.limit.unwrap_or(50).clamp(1, 500))
            .await?,
    ))
}

async fn cleanup_deployments(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, AppError> {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(HISTORY_RETENTION_DAYS);
    let deleted = state
        .storage
        .delete_deployments_before(&cutoff.to_rfc3339())
        .await?;
    Ok(Json(serde_json::json!({
        "deleted": deleted,
        "retention_days": HISTORY_RETENTION_DAYS
    })))
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
        system::SystemManager,
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

    #[test]
    fn image_status_distinguishes_unmanaged_matched_and_drifted_services() {
        let runtime = |image: Option<&str>, image_id: Option<&str>, mixed_images| {
            crate::compose::RuntimeState {
                actual_image: image.map(str::to_owned),
                actual_image_id: image_id.map(str::to_owned),
                replicas: usize::from(image.is_some()),
                mixed_images,
                container_status: "running".into(),
            }
        };
        assert_eq!(
            image_status(false, false, None, None, &runtime(None, None, false)),
            "unmanaged"
        );
        assert_eq!(
            image_status(true, false, None, None, &runtime(None, None, false)),
            "unknown"
        );
        assert_eq!(
            image_status(
                true,
                true,
                Some("repo/app:1.0.0"),
                None,
                &runtime(Some("repo/app:1.0.0"), None, false)
            ),
            "matched"
        );
        assert_eq!(
            image_status(
                true,
                true,
                Some("repo/app:1.0.0"),
                Some("sha256:one"),
                &runtime(Some("repo/app@sha256:digest"), Some("sha256:one"), false)
            ),
            "matched"
        );
        assert_eq!(
            image_status(
                true,
                true,
                Some("repo/app:1.0.0"),
                Some("sha256:one"),
                &runtime(Some("repo/app@sha256:digest"), Some("sha256:two"), false)
            ),
            "drift"
        );
        assert_eq!(
            image_status(
                true,
                true,
                Some("repo/app:1.0.0"),
                None,
                &runtime(Some("repo/app:1.0.0"), None, true)
            ),
            "drift"
        );
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
            runtime_cache: Default::default(),
        };
        router(AppState {
            config,
            storage,
            registry,
            system: SystemManager::new(dir.path().into(), Arc::new(ProcessRunner)),
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
            runtime_cache: Default::default(),
        };
        (
            AppState {
                config,
                storage,
                registry: RegistryClient::new(Duration::from_secs(60)).unwrap(),
                system: SystemManager::new(dir.path().into(), runner.clone()),
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
        assert!(js.contains("$(\"#update-check\").onclick = () => loadSystemUpdate(true)"));
        assert!(js.contains("重建当前版本"));
        assert!(js.contains("<span>部署版本</span>"));
        assert!(js.contains("<span>运行镜像</span>"));
        assert!(js.contains("运行镜像偏离部署基线"));
        assert!(!js.contains("<span>期望版本</span>"));
        assert!(!js.contains("<span>一致性</span>"));
        assert!(js.contains("currentTail === 50 ? 100 : maxTail"));
        assert!(js.contains("/api/deployments/cleanup"));
        assert!(js.contains("/api/system/overview"));
        assert!(js.contains("/api/system/images/cleanup"));
        assert!(js.contains("/api/system/build-cache/cleanup"));
        assert!(js.contains("if (!sameDisk)"));
        assert!(js.contains("数据目录共用此文件系统"));
        assert!(js.contains("visibilitychange"));
        assert!(js.contains("project.runtime_refreshing"));
        assert!(js.contains("const PASSIVE_REFRESH_INTERVAL_MS = 60_000"));
        assert!(js.contains("}, PASSIVE_REFRESH_INTERVAL_MS);"));

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
        assert!(index.contains("部署管理"));
        assert!(index.contains("系统资源"));
        assert!(index.contains("主机与 Docker 资源"));
        let projects_at = index.find("Compose 项目").unwrap();
        let versions_at = index.find("versions-section").unwrap();
        let update_at = index.find("system-update-section").unwrap();
        let system_resources_at = index.find("panel-system").unwrap();
        assert!(projects_at < versions_at);
        assert!(versions_at < update_at);
        assert!(update_at < system_resources_at);
        assert!(index.contains("受管 Compose 镜像"));
        assert!(index.contains("清除 30 天前记录"));
        assert!(!index.contains("<th>更新时间</th>"));
        assert!(!index.contains("<th>Digest</th>"));

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
        assert!(css.contains(".log-dialog .log-content{height:55vh"));

        let rejected = app
            .clone()
            .oneshot(
                Request::post("/api/projects/app/services/missing/deploy")
                    .header("x-windplume-csrf", csrf_token(&app).await)
                    .header(header::CONTENT_TYPE, "text/plain")
                    .body(Body::from("no"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        assert_eq!(rejected.headers()[header::CONTENT_TYPE], "application/json");

        let missing_csrf = app
            .clone()
            .oneshot(
                Request::post("/api/deployments/cleanup")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_csrf.status(), StatusCode::FORBIDDEN);

        let foreign_origin = app
            .clone()
            .oneshot(
                Request::post("/api/deployments/cleanup")
                    .header("host", "deploy.internal")
                    .header("origin", "https://evil.example")
                    .header("x-windplume-csrf", csrf_token(&app).await)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(foreign_origin.status(), StatusCode::FORBIDDEN);

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
    async fn projects_returns_loading_state_then_reuses_async_runtime_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let (state, runner) = refresh_state(
            &dir,
            vec![CommandOutput {
                success: true,
                log: String::new(),
            }],
        )
        .await;

        let first = projects(State(state.clone())).await.unwrap().0;
        assert!(first[0].runtime_refreshing);
        assert_eq!(first[0].services[0].container_status, "loading");
        assert_eq!(first[0].services[0].image_status, "unmanaged");

        let mut refreshed = None;
        for _ in 0..10 {
            tokio::task::yield_now().await;
            let current = projects(State(state.clone())).await.unwrap().0;
            if !current[0].runtime_refreshing {
                refreshed = Some(current);
                break;
            }
        }
        let refreshed = refreshed.expect("runtime refresh should finish");
        assert_eq!(refreshed[0].services[0].container_status, "down");
        assert_eq!(runner.calls.lock().unwrap().len(), 1);
    }

    async fn csrf_token(app: &Router) -> String {
        let response = app
            .clone()
            .oneshot(Request::get("/api/session").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        body["csrf_token"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn container_logs_default_to_50_and_clamp_to_200_lines() {
        let dir = tempfile::tempdir().unwrap();
        let outputs = vec![
            CommandOutput {
                success: true,
                log: "latest 50".into(),
            },
            CommandOutput {
                success: true,
                log: "latest 200".into(),
            },
        ];
        let (state, runner) = refresh_state(&dir, outputs).await;
        let app = router(state);

        for (query, expected) in [("", 50_u64), ("?tail=500", 200_u64)] {
            let response = app
                .clone()
                .oneshot(
                    Request::get(format!("/api/projects/app/services/old/logs{query}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert_eq!(body["tail"].as_u64(), Some(expected));
            assert_eq!(body["max_tail"].as_u64(), Some(200));
        }

        let calls = runner.calls.lock().unwrap();
        assert!(calls[0].windows(2).any(|args| args == ["--tail", "50"]));
        assert!(calls[1].windows(2).any(|args| args == ["--tail", "200"]));
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

    #[tokio::test]
    async fn image_cleanup_revalidates_and_removes_only_an_unreferenced_managed_image() {
        let dir = tempfile::tempdir().unwrap();
        let image_id = format!("sha256:{}", "a".repeat(64));
        let image_row = serde_json::json!({
            "Containers": "N/A",
            "CreatedAt": "2026-07-30 00:00:00 +0000 UTC",
            "Digest": "sha256:old",
            "ID": image_id,
            "Repository": "nginx",
            "Size": "100MB",
            "Tag": "1.0.0"
        })
        .to_string();
        let outputs = vec![
            CommandOutput {
                success: true,
                log: image_row,
            },
            CommandOutput {
                success: true,
                log: String::new(),
            },
            CommandOutput {
                success: true,
                log: "Deleted".into(),
            },
        ];
        let (state, runner) = refresh_state(&dir, outputs).await;
        let app = router(state);
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/system/images/cleanup")
                    .header("x-windplume-csrf", csrf_token(&app).await)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({"image_ids": [image_id]}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls[0],
            [
                "image",
                "ls",
                "--digests",
                "--no-trunc",
                "--format",
                "{{json .}}"
            ]
        );
        assert_eq!(calls[1][..2], ["container", "ls"]);
        assert_eq!(calls[2][..2], ["image", "rm"]);
    }

    #[tokio::test]
    async fn image_cleanup_rejects_an_image_referenced_by_a_stopped_container() {
        let dir = tempfile::tempdir().unwrap();
        let image_id = format!("sha256:{}", "b".repeat(64));
        let image_row = serde_json::json!({
            "Containers": "N/A",
            "CreatedAt": "2026-07-30 00:00:00 +0000 UTC",
            "Digest": "sha256:used",
            "ID": image_id,
            "Repository": "nginx",
            "Size": "100MB",
            "Tag": "1.0.0"
        })
        .to_string();
        let (state, runner) = refresh_state(
            &dir,
            vec![
                CommandOutput {
                    success: true,
                    log: image_row,
                },
                CommandOutput {
                    success: true,
                    log: "container-id".into(),
                },
                CommandOutput {
                    success: true,
                    log: format!("{image_id}\n"),
                },
            ],
        )
        .await;
        let app = router(state);
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/system/images/cleanup")
                    .header("x-windplume-csrf", csrf_token(&app).await)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({"image_ids": [image_id]}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[1][..2], ["container", "ls"]);
        assert_eq!(calls[2][..2], ["container", "inspect"]);
    }
}
