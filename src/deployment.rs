use crate::{
    compose::{ImageArtifact, write_image_override},
    config::{ProjectConfig, ServiceConfig},
    error::AppError,
    state::AppState,
    storage::{Deployment, ServiceRevision},
};
use chrono::Utc;
use std::{
    collections::{BTreeMap, HashSet},
    fmt::Write as _,
    time::Duration,
};
use tokio::sync::OwnedSemaphorePermit;
use uuid::Uuid;

struct RuntimeCacheInvalidator(crate::state::RuntimeStateCache);

impl Drop for RuntimeCacheInvalidator {
    fn drop(&mut self) {
        self.0.invalidate();
    }
}

fn invalidate_runtime_on_exit(
    state: &AppState,
    project_id: &str,
) -> Option<RuntimeCacheInvalidator> {
    state
        .project_runtime(project_id)
        .map(|runtime| RuntimeCacheInvalidator(runtime.runtime_cache.clone()))
}

#[derive(Clone, Copy, Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleAction {
    Recreate,
    Stop,
    Down,
}

impl LifecycleAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Recreate => "recreate",
            Self::Stop => "stop",
            Self::Down => "down",
        }
    }
}

pub async fn enqueue(
    state: AppState,
    project_id: &str,
    service_id: &str,
    version: &str,
) -> Result<Deployment, AppError> {
    if state.updates.is_active() {
        return Err(AppError::Updating);
    }
    let runtime = state
        .project_runtime(project_id)
        .cloned()
        .ok_or(AppError::NotFound)?;
    let permit = runtime
        .deploy_lock
        .clone()
        .try_acquire_owned()
        .map_err(|_| AppError::Busy)?;
    let project = runtime.compose.project();
    let service = project
        .services
        .iter()
        .find(|service| service.id == service_id)
        .cloned()
        .ok_or(AppError::NotFound)?;
    let regex = regex::Regex::new(&service.tag_pattern)
        .map_err(|error| AppError::Internal(error.to_string()))?;
    if !regex.is_match(version) {
        return Err(AppError::Invalid("版本格式不符合服务规则".into()));
    }
    let cache_key = format!("{project_id}/{service_id}");
    let exists = state
        .registry
        .versions(
            &cache_key,
            &service.version_source,
            &service.tag_pattern,
            true,
        )
        .await?
        .iter()
        .any(|item| item.version == version);
    if !exists {
        return Err(AppError::VersionNotFound(version.into()));
    }

    let old = state.storage.state(project_id, service_id).await?;
    let item = Deployment {
        id: Uuid::new_v4().to_string(),
        project_id: project_id.into(),
        service_id: service_id.into(),
        operation: "deploy".into(),
        previous_version: old.as_ref().map(|state| state.desired_version.clone()),
        target_version: version.into(),
        status: "queued".into(),
        started_at: Utc::now().to_rfc3339(),
        finished_at: None,
        command_log: String::new(),
        error_message: None,
        rollback_status: None,
    };
    state.storage.create_deployment(&item).await?;
    let task_item = item.clone();
    tokio::spawn(async move {
        run(state, project, service, task_item, permit).await;
    });
    Ok(item)
}

pub async fn enqueue_rollback(
    state: AppState,
    project_id: &str,
    service_id: &str,
) -> Result<Deployment, AppError> {
    if state.updates.is_active() {
        return Err(AppError::Updating);
    }
    let runtime = state
        .project_runtime(project_id)
        .cloned()
        .ok_or(AppError::NotFound)?;
    let permit = runtime
        .deploy_lock
        .clone()
        .try_acquire_owned()
        .map_err(|_| AppError::Busy)?;
    let project = runtime.compose.project();
    let service = project
        .services
        .iter()
        .find(|service| service.id == service_id)
        .cloned()
        .ok_or(AppError::NotFound)?;
    let current = state
        .storage
        .state(project_id, service_id)
        .await?
        .ok_or_else(|| AppError::Invalid("当前服务还没有可回滚的部署基线".into()))?;
    let revision = state
        .storage
        .previous_revision(project_id, service_id, &current.last_deployment_id)
        .await?
        .ok_or_else(|| AppError::Invalid("没有找到上一个成功部署版本".into()))?;
    let item = Deployment {
        id: Uuid::new_v4().to_string(),
        project_id: project_id.into(),
        service_id: service_id.into(),
        operation: "rollback".into(),
        previous_version: Some(current.desired_version.clone()),
        target_version: revision.version.clone(),
        status: "queued".into(),
        started_at: Utc::now().to_rfc3339(),
        finished_at: None,
        command_log: String::new(),
        error_message: None,
        rollback_status: None,
    };
    state.storage.create_deployment(&item).await?;
    let task_item = item.clone();
    tokio::spawn(async move {
        run_revision(state, project, service, revision, task_item, permit).await;
    });
    Ok(item)
}

pub async fn enqueue_lifecycle(
    state: AppState,
    project_id: &str,
    service_id: &str,
    action: LifecycleAction,
) -> Result<Deployment, AppError> {
    if state.updates.is_active() {
        return Err(AppError::Updating);
    }
    let runtime = state
        .project_runtime(project_id)
        .cloned()
        .ok_or(AppError::NotFound)?;
    let permit = runtime
        .deploy_lock
        .clone()
        .try_acquire_owned()
        .map_err(|_| AppError::Busy)?;
    let project = runtime.compose.project();
    let service = project
        .services
        .iter()
        .find(|service| service.id == service_id)
        .cloned()
        .ok_or(AppError::NotFound)?;
    let current = state.storage.state(project_id, service_id).await?;
    let version = current.as_ref().map(|item| item.desired_version.clone());
    let item = Deployment {
        id: Uuid::new_v4().to_string(),
        project_id: project_id.into(),
        service_id: service_id.into(),
        operation: action.as_str().into(),
        previous_version: version.clone(),
        target_version: version.unwrap_or_default(),
        status: "queued".into(),
        started_at: Utc::now().to_rfc3339(),
        finished_at: None,
        command_log: String::new(),
        error_message: None,
        rollback_status: None,
    };
    state.storage.create_deployment(&item).await?;
    let task_item = item.clone();
    tokio::spawn(async move {
        run_lifecycle(state, project, service, action, task_item, permit).await;
    });
    Ok(item)
}

async fn run_lifecycle(
    state: AppState,
    project: ProjectConfig,
    service: ServiceConfig,
    action: LifecycleAction,
    item: Deployment,
    _permit: OwnedSemaphorePermit,
) {
    let _runtime_cache = invalidate_runtime_on_exit(&state, &project.id);
    if let Err(error) = state.storage.mark_running(&item.id).await {
        tracing::error!(operation_id=%item.id,error=%error,"cannot mark lifecycle operation running");
        return;
    }
    let mut log = String::new();
    let result = execute_lifecycle(&state, &project, &service, action, &mut log).await;
    match result {
        Ok(()) => {
            if let Err(error) = state.storage.finish_operation_success(&item.id, &log).await {
                tracing::error!(operation_id=%item.id,error=%error,"cannot persist lifecycle success");
            }
        }
        Err(error) => {
            log.push_str(&format!("\n操作失败: {error}\n"));
            append_container_logs(&state, &project, &service, &mut log).await;
            if let Err(storage_error) = state
                .storage
                .finish_failure(&item.id, &error.to_string(), "unavailable", &log)
                .await
            {
                tracing::error!(operation_id=%item.id,error=%storage_error,"cannot persist lifecycle failure");
            }
        }
    }
}

async fn execute_lifecycle(
    state: &AppState,
    project: &ProjectConfig,
    service: &ServiceConfig,
    action: LifecycleAction,
    log: &mut String,
) -> anyhow::Result<()> {
    let runtime = state
        .project_runtime(&project.id)
        .ok_or_else(|| anyhow::anyhow!("Compose 项目运行时不存在"))?;
    match action {
        LifecycleAction::Recreate => {
            rebuild_project_override(state, project).await?;
            begin_log_step(log, "重建容器");
            log.push_str(
                &runtime
                    .compose
                    .recreate(&service.id, project.command_timeout())
                    .await?,
            );
            begin_log_step(log, "检查容器状态");
            log.push_str(
                &runtime
                    .compose
                    .wait_healthy(
                        &service.id,
                        project.health_timeout(),
                        project.command_timeout(),
                    )
                    .await?,
            );
        }
        LifecycleAction::Stop => {
            begin_log_step(log, "停止容器");
            log.push_str(
                &runtime
                    .compose
                    .stop(&service.id, project.command_timeout())
                    .await?,
            );
        }
        LifecycleAction::Down => {
            begin_log_step(log, "下线容器");
            log.push_str(
                &runtime
                    .compose
                    .remove(&service.id, project.command_timeout())
                    .await?,
            );
        }
    }
    Ok(())
}

async fn images_from_db(
    state: &AppState,
    project: &ProjectConfig,
) -> anyhow::Result<BTreeMap<String, String>> {
    let managed = project
        .services
        .iter()
        .map(|service| service.id.as_str())
        .collect::<HashSet<_>>();
    Ok(state
        .storage
        .states(&project.id)
        .await?
        .into_iter()
        .filter(|state| managed.contains(state.service_id.as_str()))
        .map(|state| (state.service_id, state.pinned_image))
        .collect())
}

async fn run(
    state: AppState,
    project: ProjectConfig,
    service: ServiceConfig,
    item: Deployment,
    _permit: OwnedSemaphorePermit,
) {
    let _runtime_cache = invalidate_runtime_on_exit(&state, &project.id);
    if let Err(error) = state.storage.mark_running(&item.id).await {
        tracing::error!(deployment_id=%item.id,error=%error,"cannot mark deployment running");
        return;
    }
    let mut log = String::new();
    let result = deploy_candidate(
        &state,
        &project,
        &service,
        &item,
        &item.target_version,
        &mut log,
    )
    .await;
    match result {
        Ok(artifact) => {
            if let Err(error) = state.storage.set_phase(&item.id, "committing").await {
                log.push_str(&format!("\n无法记录提交阶段: {error}\n"));
                let rollback_result = rollback(&state, &project, &service, &mut log).await;
                let rollback_status = rollback_outcome(rollback_result, &mut log);
                let _ = state
                    .storage
                    .finish_failure(&item.id, &error.to_string(), rollback_status, &log)
                    .await;
                return;
            }
            if let Err(error) = state
                .storage
                .finish_artifact_success(
                    &item.id,
                    &project.id,
                    &service.id,
                    &item.target_version,
                    &artifact.image,
                    &artifact.pinned_image,
                    &artifact.digest,
                    &artifact.image_id,
                    &log,
                )
                .await
            {
                tracing::error!(deployment_id=%item.id,error=%error,"cannot persist success");
                let rollback_result = rollback(&state, &project, &service, &mut log).await;
                let rollback_status = rollback_outcome(rollback_result, &mut log);
                let _ = state
                    .storage
                    .finish_failure(
                        &item.id,
                        &format!("容器已启动但无法提交部署状态: {error}"),
                        rollback_status,
                        &log,
                    )
                    .await;
                return;
            }
            if let Err(error) = rebuild_project_override(&state, &project).await {
                tracing::error!(project_id=%project.id,error=%error,"cannot finalize override");
            }
        }
        Err(error) => {
            log.push_str(&format!("\n部署失败: {error}\n"));
            append_container_logs(&state, &project, &service, &mut log).await;
            begin_log_step(&mut log, "开始回退");
            let rollback = rollback(&state, &project, &service, &mut log).await;
            let rollback_status = rollback_outcome(rollback, &mut log);
            if let Err(storage_error) = state
                .storage
                .finish_failure(&item.id, &error.to_string(), rollback_status, &log)
                .await
            {
                tracing::error!(deployment_id=%item.id,error=%storage_error,"cannot persist failure");
            }
        }
    }
}

async fn deploy_candidate(
    state: &AppState,
    project: &ProjectConfig,
    service: &ServiceConfig,
    item: &Deployment,
    version: &str,
    log: &mut String,
) -> anyhow::Result<ImageArtifact> {
    let runtime = state
        .project_runtime(&project.id)
        .ok_or_else(|| anyhow::anyhow!("Compose 项目运行时不存在"))?;
    let image = format!("{}:{version}", service.image);
    let mut images = images_from_db(state, project).await?;
    images.insert(service.id.clone(), image.clone());
    write_image_override(&runtime.override_file, &project.services, &images).await?;
    state.storage.set_phase(&item.id, "pulling").await?;
    begin_log_step(log, "拉取镜像");
    log.push_str(
        &runtime
            .compose
            .pull(&service.id, project.command_timeout())
            .await?,
    );
    let artifact = runtime
        .compose
        .image_artifact(&image, &service.image, project.command_timeout())
        .await?;
    state
        .storage
        .set_target_artifact(
            &item.id,
            &artifact.image,
            &artifact.pinned_image,
            &artifact.digest,
            &artifact.image_id,
        )
        .await?;
    images.insert(service.id.clone(), artifact.pinned_image.clone());
    write_image_override(&runtime.override_file, &project.services, &images).await?;
    state.storage.set_phase(&item.id, "starting").await?;
    begin_log_step(log, "启动容器");
    log.push_str(
        &runtime
            .compose
            .up(&service.id, project.command_timeout())
            .await?,
    );
    state.storage.set_phase(&item.id, "checking").await?;
    begin_log_step(log, "检查容器状态");
    log.push_str(
        &runtime
            .compose
            .wait_healthy_image(
                &service.id,
                Some(&artifact.image_id),
                project.health_timeout(),
                project.command_timeout(),
            )
            .await?,
    );
    Ok(artifact)
}

async fn rollback(
    state: &AppState,
    project: &ProjectConfig,
    service: &ServiceConfig,
    log: &mut String,
) -> anyhow::Result<()> {
    let runtime = state
        .project_runtime(&project.id)
        .ok_or_else(|| anyhow::anyhow!("Compose 项目运行时不存在"))?;
    let images = images_from_db(state, project).await?;
    write_image_override(&runtime.override_file, &project.services, &images).await?;
    begin_log_step(log, "恢复上一版本容器");
    log.push_str(
        &runtime
            .compose
            .up(&service.id, project.command_timeout())
            .await?,
    );
    begin_log_step(log, "检查回退后的容器状态");
    log.push_str(
        &runtime
            .compose
            .wait_healthy(
                &service.id,
                project.health_timeout(),
                project.command_timeout(),
            )
            .await?,
    );
    Ok(())
}

async fn run_revision(
    state: AppState,
    project: ProjectConfig,
    service: ServiceConfig,
    revision: ServiceRevision,
    item: Deployment,
    _permit: OwnedSemaphorePermit,
) {
    let _runtime_cache = invalidate_runtime_on_exit(&state, &project.id);
    if let Err(error) = state.storage.mark_running(&item.id).await {
        tracing::error!(operation_id=%item.id,error=%error,"cannot mark rollback running");
        return;
    }
    let mut log = String::new();
    let result = async {
        let runtime = state
            .project_runtime(&project.id)
            .ok_or_else(|| anyhow::anyhow!("Compose 项目运行时不存在"))?;
        let target = revision.pinned_image.clone();
        state.storage.set_phase(&item.id, "starting").await?;
        state
            .storage
            .set_target_artifact(
                &item.id,
                &revision.image,
                &target,
                &revision.image_digest,
                &revision.image_id,
            )
            .await?;
        let mut images = images_from_db(&state, &project).await?;
        images.insert(service.id.clone(), target);
        write_image_override(&runtime.override_file, &project.services, &images).await?;
        begin_log_step(&mut log, "恢复上一个成功版本");
        log.push_str(
            &runtime
                .compose
                .up(&service.id, project.command_timeout())
                .await?,
        );
        state.storage.set_phase(&item.id, "checking").await?;
        begin_log_step(&mut log, "检查回滚后的容器状态");
        log.push_str(
            &runtime
                .compose
                .wait_healthy_image(
                    &service.id,
                    Some(&revision.image_id),
                    project.health_timeout(),
                    project.command_timeout(),
                )
                .await?,
        );
        anyhow::Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            let _ = state.storage.set_phase(&item.id, "committing").await;
            if let Err(error) = state
                .storage
                .finish_artifact_success(
                    &item.id,
                    &project.id,
                    &service.id,
                    &revision.version,
                    &revision.image,
                    &revision.pinned_image,
                    &revision.image_digest,
                    &revision.image_id,
                    &log,
                )
                .await
            {
                tracing::error!(operation_id=%item.id,error=%error,"cannot persist rollback");
                let rollback_result = rollback(&state, &project, &service, &mut log).await;
                let rollback_status = rollback_outcome(rollback_result, &mut log);
                let _ = state
                    .storage
                    .finish_failure(
                        &item.id,
                        &format!("容器已回滚但无法提交部署状态: {error}"),
                        rollback_status,
                        &log,
                    )
                    .await;
            }
        }
        Err(error) => {
            let rollback_result = rollback(&state, &project, &service, &mut log).await;
            let rollback_status = rollback_outcome(rollback_result, &mut log);
            let _ = state
                .storage
                .finish_failure(&item.id, &error.to_string(), rollback_status, &log)
                .await;
        }
    }
}

fn rollback_outcome(result: anyhow::Result<()>, log: &mut String) -> &'static str {
    match result {
        Ok(()) => "succeeded",
        Err(error) => {
            let _ = writeln!(log, "回退失败: {error}");
            "failed"
        }
    }
}

fn begin_log_step(log: &mut String, title: &str) {
    if !log.is_empty() && !log.ends_with('\n') {
        log.push('\n');
    }
    if !log.is_empty() {
        log.push('\n');
    }
    let _ = writeln!(log, "== {title} ==");
}

async fn append_container_logs(
    state: &AppState,
    project: &ProjectConfig,
    service: &ServiceConfig,
    log: &mut String,
) {
    begin_log_step(log, "容器最近 50 行日志");
    let Some(runtime) = state.project_runtime(&project.id) else {
        log.push_str("无法读取容器日志：Compose 项目运行时不存在\n");
        return;
    };
    match runtime
        .compose
        .logs(
            &service.id,
            50,
            Duration::from_secs(30).min(project.command_timeout()),
        )
        .await
    {
        Ok(output) if output.trim().is_empty() => log.push_str("（容器暂无日志）\n"),
        Ok(output) => {
            log.push_str(&output);
            if !log.ends_with('\n') {
                log.push('\n');
            }
        }
        Err(error) => {
            let _ = writeln!(log, "无法读取容器日志：{error}");
        }
    }
}

pub async fn rebuild_project_override(
    state: &AppState,
    project: &ProjectConfig,
) -> anyhow::Result<()> {
    let runtime = state
        .project_runtime(&project.id)
        .ok_or_else(|| anyhow::anyhow!("Compose 项目运行时不存在"))?;
    let images = images_from_db(state, project).await?;
    write_image_override(&runtime.override_file, &project.services, &images).await
}

pub async fn rebuild_override(state: &AppState) -> anyhow::Result<()> {
    for project in &state.config.projects {
        let runtime = state
            .project_runtime(&project.id)
            .ok_or_else(|| anyhow::anyhow!("Compose 项目运行时不存在"))?;
        rebuild_project_override(state, &runtime.compose.project()).await?;
    }
    Ok(())
}

pub async fn recover_interrupted(state: &AppState) -> anyhow::Result<()> {
    for (item, artifact) in state.storage.active_deployments().await? {
        let Some(runtime) = state.project_runtime(&item.project_id).cloned() else {
            state
                .storage
                .finish_interrupted(
                    &item.id,
                    "启动恢复时 Compose 项目已不存在",
                    "unavailable",
                    "无法恢复：Compose 项目已不存在\n",
                )
                .await?;
            continue;
        };
        let _permit = runtime.deploy_lock.clone().acquire_owned().await?;
        let project = runtime.compose.project();
        let Some(service) = project
            .services
            .iter()
            .find(|service| service.id == item.service_id)
            .cloned()
        else {
            state
                .storage
                .finish_interrupted(
                    &item.id,
                    "启动恢复时 Compose 服务已不存在",
                    "unavailable",
                    "无法恢复：Compose 服务已不存在\n",
                )
                .await?;
            continue;
        };
        if item.status == "queued" {
            state
                .storage
                .finish_interrupted(
                    &item.id,
                    "管理程序重启，尚未开始的操作已取消",
                    "not_needed",
                    "操作在开始前因管理程序重启而取消\n",
                )
                .await?;
            continue;
        }
        if !matches!(item.operation.as_str(), "deploy" | "rollback") {
            state
                .storage
                .finish_interrupted(
                    &item.id,
                    "管理程序在生命周期操作期间重启，请检查容器当前状态",
                    "unavailable",
                    "生命周期操作被管理程序重启中断\n",
                )
                .await?;
            continue;
        }

        let actual = runtime
            .compose
            .runtime(&service.id, project.command_timeout())
            .await;
        let target_id = artifact
            .target_image_id
            .as_deref()
            .filter(|value| !value.is_empty());
        let target_is_healthy = target_id.is_some()
            && actual.actual_image_id.as_deref() == target_id
            && !actual.mixed_images
            && matches!(actual.container_status.as_str(), "healthy" | "running");
        if target_is_healthy
            && matches!(artifact.phase.as_str(), "checking" | "committing")
            && let (Some(image), Some(pinned), Some(digest), Some(image_id)) = (
                artifact.target_image.as_deref(),
                artifact.target_pinned_image.as_deref(),
                artifact.target_digest.as_deref(),
                artifact.target_image_id.as_deref(),
            )
        {
            let log = "管理程序重启后确认候选镜像仍在健康运行，已补记部署成功\n";
            state
                .storage
                .finish_artifact_success(
                    &item.id,
                    &project.id,
                    &service.id,
                    &item.target_version,
                    image,
                    pinned,
                    digest,
                    image_id,
                    log,
                )
                .await?;
            rebuild_project_override(state, &project).await?;
            continue;
        }

        let mut log = String::from("管理程序重启后无法确认候选部署已安全完成\n");
        begin_log_step(&mut log, "恢复已提交版本");
        let rollback_result = rollback(state, &project, &service, &mut log).await;
        let (rollback_status, message) = match rollback_result {
            Ok(()) => ("succeeded", "中断部署已恢复到上一个已提交版本"),
            Err(error) => {
                let _ = writeln!(log, "启动恢复失败: {error}");
                ("failed", "中断部署恢复失败，需要人工检查")
            }
        };
        state
            .storage
            .finish_interrupted(&item.id, message, rollback_status, &log)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        compose::{CommandOutput, CommandRunner, Compose},
        config::{
            Config, ProjectConfig, RegistryConfig, ServerConfig, ServiceConfig, StorageConfig,
            VersionSourceConfig,
        },
        registry::RegistryClient,
        state::ProjectRuntime,
        storage::Storage,
        system::SystemManager,
        update::UpdateManager,
    };
    use std::{
        collections::HashMap,
        path::Path,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio::sync::Semaphore;

    struct FakeRunner(Mutex<Vec<CommandOutput>>);
    #[async_trait::async_trait]
    impl CommandRunner for FakeRunner {
        async fn run(
            &self,
            _program: &str,
            _args: &[String],
            _cwd: &Path,
            _timeout_for: Duration,
        ) -> anyhow::Result<CommandOutput> {
            Ok(self.0.lock().unwrap().remove(0))
        }
    }

    async fn state(
        dir: &tempfile::TempDir,
        outputs: Vec<CommandOutput>,
    ) -> (AppState, ProjectConfig, ServiceConfig) {
        let compose_file = dir.path().join("compose.yaml");
        tokio::fs::write(&compose_file, "services: {}\n")
            .await
            .unwrap();
        let service = ServiceConfig {
            id: "identity".into(),
            image: "ghcr.io/owner/identity".into(),
            tag_pattern: r"^\d+\.\d+\.\d+$".into(),
            version_source: VersionSourceConfig::OciRegistry {
                registry: "ghcr.io".into(),
                repository: "owner/identity".into(),
            },
        };
        let project = ProjectConfig {
            compose_files: vec![compose_file],
            health_timeout_seconds: 1,
            command_timeout_seconds: 1,
            id: "app".into(),
            services: vec![service.clone()],
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
        let runner = Arc::new(FakeRunner(Mutex::new(outputs)));
        let runtime = ProjectRuntime {
            compose: Compose::new(project.clone(), override_file.clone(), runner.clone()),
            override_file,
            deploy_lock: Arc::new(Semaphore::new(1)),
            runtime_cache: Default::default(),
        };
        (
            AppState {
                config,
                storage,
                registry,
                system: SystemManager::new(dir.path().into(), runner),
                updates: UpdateManager::new(dir.path().into()).unwrap(),
                projects: Arc::new(HashMap::from([("app".into(), runtime)])),
            },
            project,
            service,
        )
    }

    fn output(success: bool, log: &str) -> CommandOutput {
        CommandOutput {
            success,
            log: log.into(),
        }
    }

    fn queued() -> Deployment {
        Deployment {
            id: uuid::Uuid::new_v4().to_string(),
            project_id: "app".into(),
            service_id: "identity".into(),
            operation: "deploy".into(),
            previous_version: None,
            target_version: "1.2.3".into(),
            status: "queued".into(),
            started_at: Utc::now().to_rfc3339(),
            finished_at: None,
            command_log: String::new(),
            error_message: None,
            rollback_status: None,
        }
    }

    async fn persist_success(
        state: &AppState,
        id: &str,
        version: &str,
        digest: &str,
        image_id: &str,
    ) {
        let mut item = queued();
        item.id = id.into();
        item.target_version = version.into();
        state.storage.create_deployment(&item).await.unwrap();
        state
            .storage
            .finish_artifact_success(
                id,
                "app",
                "identity",
                version,
                &format!("ghcr.io/owner/identity:{version}"),
                &format!("ghcr.io/owner/identity@{digest}"),
                digest,
                image_id,
                "",
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn persists_success_after_healthy_container() {
        let dir = tempfile::tempdir().unwrap();
        let inspect = r#"[{"Image":"sha256:image","Config":{"Image":"ghcr.io/owner/identity@sha256:digest","Healthcheck":null},"State":{"Status":"running","Health":null}}]"#;
        let (state, project, service) = state(
            &dir,
            vec![
                output(true, "pull"),
                output(
                    true,
                    r#"[{"Id":"sha256:image","RepoDigests":["ghcr.io/owner/identity@sha256:digest"]}]"#,
                ),
                output(true, "up"),
                output(true, "container\n"),
                output(true, inspect),
            ],
        )
        .await;
        let item = queued();
        state.storage.create_deployment(&item).await.unwrap();
        let permit = state
            .project_runtime("app")
            .unwrap()
            .deploy_lock
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        run(state.clone(), project, service, item.clone(), permit).await;
        let stored = state.storage.deployment(&item.id).await.unwrap().unwrap();
        assert_eq!(stored.status, "succeeded");
        assert_eq!(stored.rollback_status.as_deref(), Some("not_needed"));
        assert!(stored.command_log.contains("== 拉取镜像 =="));
        assert!(stored.command_log.contains("容器状态：running"));
        assert!(!stored.command_log.contains(inspect));
        assert_eq!(
            state
                .storage
                .state("app", "identity")
                .await
                .unwrap()
                .unwrap()
                .desired_version,
            "1.2.3"
        );
    }

    #[tokio::test]
    async fn startup_recovery_commits_a_healthy_verified_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let inspect = r#"[{"Image":"sha256:image","Config":{"Image":"ghcr.io/owner/identity@sha256:digest","Healthcheck":null},"State":{"Status":"running","Health":null}}]"#;
        let (state, _, _) = state(
            &dir,
            vec![output(true, "container\n"), output(true, inspect)],
        )
        .await;
        let item = queued();
        state.storage.create_deployment(&item).await.unwrap();
        state.storage.mark_running(&item.id).await.unwrap();
        state.storage.set_phase(&item.id, "checking").await.unwrap();
        state
            .storage
            .set_target_artifact(
                &item.id,
                "ghcr.io/owner/identity:1.2.3",
                "ghcr.io/owner/identity@sha256:digest",
                "sha256:digest",
                "sha256:image",
            )
            .await
            .unwrap();

        recover_interrupted(&state).await.unwrap();

        let deployment = state.storage.deployment(&item.id).await.unwrap().unwrap();
        assert_eq!(deployment.status, "succeeded");
        let service = state
            .storage
            .state("app", "identity")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(service.image_digest, "sha256:digest");
        assert_eq!(service.image_id, "sha256:image");
    }

    #[tokio::test]
    async fn rollback_restores_the_previous_successful_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let inspect = r#"[{"Image":"sha256:image-one","Config":{"Image":"ghcr.io/owner/identity@sha256:one","Healthcheck":null},"State":{"Status":"running","Health":null}}]"#;
        let (state, _, _) = state(
            &dir,
            vec![
                output(true, "up"),
                output(true, "container\n"),
                output(true, inspect),
            ],
        )
        .await;
        persist_success(&state, "first", "1.0.0", "sha256:one", "sha256:image-one").await;
        let mut second = queued();
        second.id = "second".into();
        second.previous_version = Some("1.0.0".into());
        second.target_version = "2.0.0".into();
        state.storage.create_deployment(&second).await.unwrap();
        state
            .storage
            .finish_artifact_success(
                &second.id,
                "app",
                "identity",
                "2.0.0",
                "ghcr.io/owner/identity:2.0.0",
                "ghcr.io/owner/identity@sha256:two",
                "sha256:two",
                "sha256:image-two",
                "",
            )
            .await
            .unwrap();

        let operation = enqueue_rollback(state.clone(), "app", "identity")
            .await
            .unwrap();
        for _ in 0..100 {
            let current = state
                .storage
                .deployment(&operation.id)
                .await
                .unwrap()
                .unwrap();
            if current.status != "queued" && current.status != "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let operation = state
            .storage
            .deployment(&operation.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(operation.status, "succeeded");
        assert_eq!(operation.operation, "rollback");
        let current = state
            .storage
            .state("app", "identity")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.desired_version, "1.0.0");
        assert_eq!(current.image_digest, "sha256:one");
    }

    #[tokio::test]
    async fn failed_rollback_records_failed_when_current_version_cannot_be_restored() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _, _) = state(
            &dir,
            vec![
                output(false, "rollback target failed"),
                output(false, "current version restore failed"),
            ],
        )
        .await;
        persist_success(&state, "first", "1.0.0", "sha256:one", "sha256:image-one").await;
        persist_success(&state, "second", "2.0.0", "sha256:two", "sha256:image-two").await;

        let operation = enqueue_rollback(state.clone(), "app", "identity")
            .await
            .unwrap();
        for _ in 0..100 {
            let current = state
                .storage
                .deployment(&operation.id)
                .await
                .unwrap()
                .unwrap();
            if current.status != "queued" && current.status != "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let operation = state
            .storage
            .deployment(&operation.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(operation.status, "failed");
        assert_eq!(operation.rollback_status.as_deref(), Some("failed"));
        assert!(
            operation
                .command_log
                .contains("current version restore failed")
        );
        assert!(operation.command_log.contains("回退失败"));
    }

    #[tokio::test]
    async fn failed_deploy_rolls_back_only_its_project() {
        let dir = tempfile::tempdir().unwrap();
        let inspect = r#"[{"Config":{"Image":"base:latest","Healthcheck":null},"State":{"Status":"running","Health":null}}]"#;
        let (state, project, service) = state(
            &dir,
            vec![
                output(false, "candidate failed"),
                output(true, "application panic\n"),
                output(true, "rollback up"),
                output(true, "container\n"),
                output(true, inspect),
            ],
        )
        .await;
        let item = queued();
        state.storage.create_deployment(&item).await.unwrap();
        let permit = state
            .project_runtime("app")
            .unwrap()
            .deploy_lock
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        run(state.clone(), project, service, item.clone(), permit).await;
        let stored = state.storage.deployment(&item.id).await.unwrap().unwrap();
        assert_eq!(stored.status, "failed");
        assert_eq!(stored.rollback_status.as_deref(), Some("succeeded"));
        assert!(stored.command_log.contains("== 容器最近 50 行日志 =="));
        assert!(stored.command_log.contains("application panic"));
        assert!(!stored.command_log.contains(inspect));
        assert!(
            state
                .storage
                .state("app", "identity")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn container_log_failure_does_not_hide_original_deploy_error() {
        let dir = tempfile::tempdir().unwrap();
        let inspect = r#"[{"Config":{"Image":"base:latest","Healthcheck":null},"State":{"Status":"running","Health":null}}]"#;
        let (state, project, service) = state(
            &dir,
            vec![
                output(false, "candidate failed"),
                output(false, "logs unavailable"),
                output(true, "rollback up"),
                output(true, "container\n"),
                output(true, inspect),
            ],
        )
        .await;
        let item = queued();
        state.storage.create_deployment(&item).await.unwrap();
        let permit = state
            .project_runtime("app")
            .unwrap()
            .deploy_lock
            .clone()
            .acquire_owned()
            .await
            .unwrap();

        run(state.clone(), project, service, item.clone(), permit).await;

        let stored = state.storage.deployment(&item.id).await.unwrap().unwrap();
        assert_eq!(stored.status, "failed");
        assert!(
            stored
                .error_message
                .as_deref()
                .unwrap()
                .contains("candidate failed")
        );
        assert!(stored.command_log.contains("无法读取容器日志"));
        assert!(stored.command_log.contains("logs unavailable"));
        assert!(!stored.command_log.contains(inspect));
    }

    #[tokio::test]
    async fn lifecycle_operations_preserve_version_and_write_history() {
        let dir = tempfile::tempdir().unwrap();
        let inspect = r#"[{"Config":{"Image":"ghcr.io/owner/identity:1.2.3","Healthcheck":null},"State":{"Status":"running","Health":null}}]"#;
        let (state, project, service) = state(
            &dir,
            vec![
                output(true, "recreate"),
                output(true, "container\n"),
                output(true, inspect),
                output(true, "stop"),
                output(true, "remove"),
            ],
        )
        .await;
        persist_success(
            &state,
            "baseline",
            "1.2.3",
            "sha256:baseline",
            "sha256:image-baseline",
        )
        .await;

        for action in [
            LifecycleAction::Recreate,
            LifecycleAction::Stop,
            LifecycleAction::Down,
        ] {
            let mut item = queued();
            item.operation = action.as_str().into();
            item.previous_version = Some("1.2.3".into());
            item.target_version = "1.2.3".into();
            state.storage.create_deployment(&item).await.unwrap();
            let permit = state
                .project_runtime("app")
                .unwrap()
                .deploy_lock
                .clone()
                .acquire_owned()
                .await
                .unwrap();
            run_lifecycle(
                state.clone(),
                project.clone(),
                service.clone(),
                action,
                item.clone(),
                permit,
            )
            .await;
            let stored = state.storage.deployment(&item.id).await.unwrap().unwrap();
            assert_eq!(stored.status, "succeeded");
            assert_eq!(stored.operation, action.as_str());
            assert_eq!(stored.rollback_status.as_deref(), Some("not_needed"));
            assert!(!stored.command_log.contains(inspect));
        }

        assert_eq!(
            state
                .storage
                .state("app", "identity")
                .await
                .unwrap()
                .unwrap()
                .desired_version,
            "1.2.3"
        );
    }

    #[tokio::test]
    async fn system_update_blocks_new_service_operations() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _, _) = state(&dir, Vec::new()).await;
        assert!(state.updates.begin());
        assert!(matches!(
            enqueue_lifecycle(state.clone(), "app", "identity", LifecycleAction::Stop).await,
            Err(AppError::Updating)
        ));
        assert!(matches!(
            enqueue(state.clone(), "app", "identity", "1.2.3").await,
            Err(AppError::Updating)
        ));
        state.updates.cancel();
    }

    #[tokio::test]
    async fn removed_service_state_is_preserved_but_excluded_from_override() {
        let dir = tempfile::tempdir().unwrap();
        let (state, mut project, service) = state(&dir, Vec::new()).await;
        let item = queued();
        state.storage.create_deployment(&item).await.unwrap();
        state
            .storage
            .finish_artifact_success(
                &item.id,
                "app",
                &service.id,
                "1.2.3",
                "ghcr.io/owner/identity:1.2.3",
                "ghcr.io/owner/identity@sha256:digest",
                "sha256:digest",
                "sha256:image",
                "ok",
            )
            .await
            .unwrap();
        project.services = vec![ServiceConfig {
            id: "replacement".into(),
            image: "nginx".into(),
            tag_pattern: "^.+$".into(),
            version_source: VersionSourceConfig::DockerHub {
                namespace: "library".into(),
                repository: "nginx".into(),
            },
        }];

        rebuild_project_override(&state, &project).await.unwrap();

        assert!(
            state
                .storage
                .state("app", "identity")
                .await
                .unwrap()
                .is_some()
        );
        let override_file = &state.project_runtime("app").unwrap().override_file;
        let yaml: serde_yaml::Value =
            serde_yaml::from_str(&tokio::fs::read_to_string(override_file).await.unwrap()).unwrap();
        assert!(yaml["services"].as_mapping().unwrap().is_empty());
    }
}
