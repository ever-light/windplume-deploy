use crate::{compose::write_override, error::AppError, state::AppState, storage::Deployment};
use chrono::Utc;
use std::collections::BTreeMap;
use tokio::sync::OwnedSemaphorePermit;
use uuid::Uuid;

pub async fn enqueue(
    state: AppState,
    service_id: &str,
    version: &str,
) -> Result<Deployment, AppError> {
    let svc = state
        .config
        .services
        .iter()
        .find(|s| s.id == service_id)
        .cloned()
        .ok_or(AppError::NotFound)?;
    let regex =
        regex::Regex::new(&svc.tag_pattern).map_err(|e| AppError::Internal(e.to_string()))?;
    if !regex.is_match(version) {
        return Err(AppError::Invalid("版本格式不符合服务规则".into()));
    }
    let exists = state
        .github
        .versions(&svc, true)
        .await?
        .iter()
        .any(|v| v.version == version);
    if !exists {
        return Err(AppError::VersionNotFound(version.into()));
    }
    let permit = state
        .deploy_lock
        .clone()
        .try_acquire_owned()
        .map_err(|_| AppError::Busy)?;
    let old = state.storage.state(service_id).await?;
    let item = Deployment {
        id: Uuid::new_v4().to_string(),
        service_id: service_id.into(),
        previous_version: old.map(|s| s.desired_version),
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
        run(state, svc, task_item, permit).await;
    });
    Ok(item)
}

async fn versions_from_db(state: &AppState) -> anyhow::Result<BTreeMap<String, String>> {
    Ok(state
        .storage
        .states()
        .await?
        .into_iter()
        .map(|s| (s.service_id, s.desired_version))
        .collect())
}

async fn run(
    state: AppState,
    svc: crate::config::ServiceConfig,
    item: Deployment,
    _permit: OwnedSemaphorePermit,
) {
    if let Err(e) = state.storage.mark_running(&item.id).await {
        tracing::error!(deployment_id=%item.id,error=%e,"cannot mark deployment running");
        return;
    }
    let mut log = String::new();
    let result = deploy_candidate(&state, &svc, &item.target_version, &mut log).await;
    match result {
        Ok(()) => {
            let image = format!("{}:{}", svc.image, item.target_version);
            if let Err(e) = state
                .storage
                .finish_success(&item.id, &svc.id, &item.target_version, &image, &log)
                .await
            {
                tracing::error!(deployment_id=%item.id,error=%e,"cannot persist success");
                return;
            }
            if let Ok(v) = versions_from_db(&state).await
                && let Err(e) =
                    write_override(&state.override_file, &state.config.services, &v).await
            {
                tracing::error!(error=%e,"cannot finalize override");
            }
        }
        Err(error) => {
            log.push_str(&format!("\n部署失败: {error}\n开始回退\n"));
            let rollback = rollback(&state, &svc, &mut log).await;
            let rollback_status = if rollback.is_ok() {
                "succeeded"
            } else {
                "failed"
            };
            if let Err(e) = rollback {
                log.push_str(&format!("回退失败: {e}\n"));
            }
            if let Err(e) = state
                .storage
                .finish_failure(&item.id, &error.to_string(), rollback_status, &log)
                .await
            {
                tracing::error!(deployment_id=%item.id,error=%e,"cannot persist failure");
            }
        }
    }
}

async fn deploy_candidate(
    state: &AppState,
    svc: &crate::config::ServiceConfig,
    version: &str,
    log: &mut String,
) -> anyhow::Result<()> {
    let mut versions = versions_from_db(state).await?;
    versions.insert(svc.id.clone(), version.into());
    write_override(&state.override_file, &state.config.services, &versions).await?;
    log.push_str(
        &state
            .compose
            .up(&svc.compose_service, state.config.command_timeout())
            .await?,
    );
    log.push_str(
        &state
            .compose
            .wait_healthy(
                &svc.compose_service,
                state.config.health_timeout(),
                state.config.command_timeout(),
            )
            .await?,
    );
    Ok(())
}
async fn rollback(
    state: &AppState,
    svc: &crate::config::ServiceConfig,
    log: &mut String,
) -> anyhow::Result<()> {
    let versions = versions_from_db(state).await?;
    write_override(&state.override_file, &state.config.services, &versions).await?;
    log.push_str(
        &state
            .compose
            .up(&svc.compose_service, state.config.command_timeout())
            .await?,
    );
    log.push_str(
        &state
            .compose
            .wait_healthy(
                &svc.compose_service,
                state.config.health_timeout(),
                state.config.command_timeout(),
            )
            .await?,
    );
    Ok(())
}

pub async fn rebuild_override(state: &AppState) -> anyhow::Result<()> {
    let versions = versions_from_db(state).await?;
    write_override(&state.override_file, &state.config.services, &versions).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        compose::{CommandOutput, CommandRunner, Compose},
        config::{ComposeConfig, Config, GithubConfig, ServerConfig, ServiceConfig, StorageConfig},
        github::GithubClient,
        storage::Storage,
    };
    use std::{
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
    ) -> (AppState, ServiceConfig) {
        let compose_file = dir.path().join("compose.yaml");
        tokio::fs::write(&compose_file, "services: {}\n")
            .await
            .unwrap();
        let service = ServiceConfig {
            id: "identity".into(),
            name: "Identity".into(),
            github_owner: "owner".into(),
            github_package: "package".into(),
            image: "ghcr.io/owner/identity".into(),
            compose_service: "identity-service".into(),
            tag_pattern: r"^\d+\.\d+\.\d+$".into(),
        };
        let compose_config = ComposeConfig {
            project_name: "test".into(),
            file: compose_file,
            health_timeout_seconds: 1,
            command_timeout_seconds: 1,
        };
        let config = Arc::new(Config {
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
            },
            github: GithubConfig {
                token_file: dir.path().join("unused"),
                api_base: "http://127.0.0.1:1".into(),
                cache_seconds: 60,
            },
            storage: StorageConfig {
                data_dir: dir.path().into(),
                history_limit: 10,
                max_log_bytes: 1024,
            },
            compose: compose_config.clone(),
            services: vec![service.clone()],
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
            compose_config,
            override_file.clone(),
            Arc::new(FakeRunner(Mutex::new(outputs))),
        );
        (
            AppState {
                config,
                storage,
                github,
                compose,
                override_file,
                deploy_lock: Arc::new(Semaphore::new(1)),
            },
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
        let (state, service) = state(
            &dir,
            vec![
                output(true, "up"),
                output(true, "container\n"),
                output(true, inspect),
            ],
        )
        .await;
        let item = queued();
        state.storage.create_deployment(&item).await.unwrap();
        let permit = state.deploy_lock.clone().acquire_owned().await.unwrap();
        run(state.clone(), service, item.clone(), permit).await;
        let stored = state.storage.deployment(&item.id).await.unwrap().unwrap();
        assert_eq!(stored.status, "succeeded");
        assert_eq!(stored.rollback_status.as_deref(), Some("not_needed"));
        assert_eq!(
            state
                .storage
                .state("identity")
                .await
                .unwrap()
                .unwrap()
                .desired_version,
            "1.2.3"
        );
    }

    #[tokio::test]
    async fn failed_deploy_records_successful_rollback_without_changing_state() {
        let dir = tempfile::tempdir().unwrap();
        let inspect = r#"[{"Config":{"Image":"base:latest","Healthcheck":null},"State":{"Status":"running","Health":null}}]"#;
        let (state, service) = state(
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
        let permit = state.deploy_lock.clone().acquire_owned().await.unwrap();
        run(state.clone(), service, item.clone(), permit).await;
        let stored = state.storage.deployment(&item.id).await.unwrap().unwrap();
        assert_eq!(stored.status, "failed");
        assert_eq!(stored.rollback_status.as_deref(), Some("succeeded"));
        assert!(state.storage.state("identity").await.unwrap().is_none());
    }
}
