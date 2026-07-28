use crate::{
    compose::write_override,
    config::{ProjectConfig, ServiceConfig},
    error::AppError,
    state::AppState,
    storage::Deployment,
};
use chrono::Utc;
use std::collections::{BTreeMap, HashSet};
use tokio::sync::OwnedSemaphorePermit;
use uuid::Uuid;

pub async fn enqueue(
    state: AppState,
    project_id: &str,
    service_id: &str,
    version: &str,
) -> Result<Deployment, AppError> {
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
        previous_version: old.map(|state| state.desired_version),
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

async fn versions_from_db(
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
        .map(|state| (state.service_id, state.desired_version))
        .collect())
}

async fn run(
    state: AppState,
    project: ProjectConfig,
    service: ServiceConfig,
    item: Deployment,
    _permit: OwnedSemaphorePermit,
) {
    if let Err(error) = state.storage.mark_running(&item.id).await {
        tracing::error!(deployment_id=%item.id,error=%error,"cannot mark deployment running");
        return;
    }
    let mut log = String::new();
    let result = deploy_candidate(&state, &project, &service, &item.target_version, &mut log).await;
    match result {
        Ok(()) => {
            let image = format!("{}:{}", service.image, item.target_version);
            if let Err(error) = state
                .storage
                .finish_success(
                    &item.id,
                    &project.id,
                    &service.id,
                    &item.target_version,
                    &image,
                    &log,
                )
                .await
            {
                tracing::error!(deployment_id=%item.id,error=%error,"cannot persist success");
                return;
            }
            if let Err(error) = rebuild_project_override(&state, &project).await {
                tracing::error!(project_id=%project.id,error=%error,"cannot finalize override");
            }
        }
        Err(error) => {
            log.push_str(&format!("\n部署失败: {error}\n开始回退\n"));
            let rollback = rollback(&state, &project, &service, &mut log).await;
            let rollback_status = if rollback.is_ok() {
                "succeeded"
            } else {
                "failed"
            };
            if let Err(rollback_error) = rollback {
                log.push_str(&format!("回退失败: {rollback_error}\n"));
            }
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
    version: &str,
    log: &mut String,
) -> anyhow::Result<()> {
    let runtime = state
        .project_runtime(&project.id)
        .ok_or_else(|| anyhow::anyhow!("Compose 项目运行时不存在"))?;
    let mut versions = versions_from_db(state, project).await?;
    versions.insert(service.id.clone(), version.into());
    write_override(&runtime.override_file, &project.services, &versions).await?;
    log.push_str(
        &runtime
            .compose
            .pull(&service.id, project.command_timeout())
            .await?,
    );
    log.push_str(
        &runtime
            .compose
            .up(&service.id, project.command_timeout())
            .await?,
    );
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

async fn rollback(
    state: &AppState,
    project: &ProjectConfig,
    service: &ServiceConfig,
    log: &mut String,
) -> anyhow::Result<()> {
    let runtime = state
        .project_runtime(&project.id)
        .ok_or_else(|| anyhow::anyhow!("Compose 项目运行时不存在"))?;
    let versions = versions_from_db(state, project).await?;
    write_override(&runtime.override_file, &project.services, &versions).await?;
    log.push_str(
        &runtime
            .compose
            .up(&service.id, project.command_timeout())
            .await?,
    );
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

pub async fn rebuild_project_override(
    state: &AppState,
    project: &ProjectConfig,
) -> anyhow::Result<()> {
    let runtime = state
        .project_runtime(&project.id)
        .ok_or_else(|| anyhow::anyhow!("Compose 项目运行时不存在"))?;
    let versions = versions_from_db(state, project).await?;
    write_override(&runtime.override_file, &project.services, &versions).await
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
        let runtime = ProjectRuntime {
            compose: Compose::new(
                project.clone(),
                override_file.clone(),
                Arc::new(FakeRunner(Mutex::new(outputs))),
            ),
            override_file,
            deploy_lock: Arc::new(Semaphore::new(1)),
        };
        (
            AppState {
                config,
                storage,
                registry,
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

    #[tokio::test]
    async fn persists_success_after_healthy_container() {
        let dir = tempfile::tempdir().unwrap();
        let inspect = r#"[{"Config":{"Image":"ghcr.io/owner/identity:1.2.3","Healthcheck":null},"State":{"Status":"running","Health":null}}]"#;
        let (state, project, service) = state(
            &dir,
            vec![
                output(true, "pull"),
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
    async fn failed_deploy_rolls_back_only_its_project() {
        let dir = tempfile::tempdir().unwrap();
        let inspect = r#"[{"Config":{"Image":"base:latest","Healthcheck":null},"State":{"Status":"running","Health":null}}]"#;
        let (state, project, service) = state(
            &dir,
            vec![
                output(false, "candidate failed"),
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
    async fn removed_service_state_is_preserved_but_excluded_from_override() {
        let dir = tempfile::tempdir().unwrap();
        let (state, mut project, service) = state(&dir, Vec::new()).await;
        let item = queued();
        state.storage.create_deployment(&item).await.unwrap();
        state
            .storage
            .finish_success(
                &item.id,
                "app",
                &service.id,
                "1.2.3",
                "ghcr.io/owner/identity:1.2.3",
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
