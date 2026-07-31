use chrono::Utc;
use serde::Serialize;
use sqlx::{
    FromRow, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use std::{path::Path, str::FromStr};

#[derive(Clone)]
pub struct Storage {
    pub pool: SqlitePool,
    history_limit: u32,
    max_log_bytes: usize,
}

#[derive(Clone, Debug, FromRow, Serialize)]
pub struct ServiceState {
    pub project_id: String,
    pub service_id: String,
    pub desired_version: String,
    pub image: String,
    pub pinned_image: String,
    pub image_digest: String,
    pub image_id: String,
    pub updated_at: String,
    pub last_deployment_id: String,
}

#[derive(Clone, Debug, FromRow)]
pub struct DeploymentArtifact {
    pub phase: String,
    pub target_image: Option<String>,
    pub target_pinned_image: Option<String>,
    pub target_digest: Option<String>,
    pub target_image_id: Option<String>,
}

#[derive(Clone, Debug, FromRow)]
pub struct ServiceRevision {
    pub version: String,
    pub image: String,
    pub pinned_image: String,
    pub image_digest: String,
    pub image_id: String,
}

#[derive(Clone, Debug, FromRow, Serialize)]
pub struct Deployment {
    pub id: String,
    pub project_id: String,
    pub service_id: String,
    pub operation: String,
    pub previous_version: Option<String>,
    pub target_version: String,
    pub status: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub command_log: String,
    pub error_message: Option<String>,
    pub rollback_status: Option<String>,
}

#[derive(Clone, Debug, FromRow, Serialize)]
pub struct DeploymentSummary {
    pub id: String,
    pub project_id: String,
    pub service_id: String,
    pub operation: String,
    pub previous_version: Option<String>,
    pub target_version: String,
    pub status: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub rollback_status: Option<String>,
}

impl Storage {
    pub async fn open(
        data_dir: &Path,
        history_limit: u32,
        max_log_bytes: usize,
    ) -> anyhow::Result<Self> {
        let url = format!("sqlite://{}", data_dir.join("deploy.db").display());
        let options = SqliteConnectOptions::from_str(&url)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;
        sqlx::migrate!().run(&pool).await?;
        let this = Self {
            pool,
            history_limit,
            max_log_bytes,
        };
        Ok(this)
    }
    pub async fn health(&self) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE service_state SET updated_at=updated_at WHERE 0")
            .execute(&mut *tx)
            .await?;
        tx.rollback().await?;
        Ok(())
    }
    pub async fn states(&self, project_id: &str) -> Result<Vec<ServiceState>, sqlx::Error> {
        sqlx::query_as("SELECT * FROM service_state WHERE project_id=? ORDER BY service_id")
            .bind(project_id)
            .fetch_all(&self.pool)
            .await
    }
    pub async fn state(
        &self,
        project_id: &str,
        service_id: &str,
    ) -> Result<Option<ServiceState>, sqlx::Error> {
        sqlx::query_as("SELECT * FROM service_state WHERE project_id=? AND service_id=?")
            .bind(project_id)
            .bind(service_id)
            .fetch_optional(&self.pool)
            .await
    }
    pub async fn create_deployment(&self, d: &Deployment) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO deployments(id,project_id,service_id,operation,previous_version,target_version,status,started_at,command_log) VALUES(?,?,?,?,?,?,?,?,?)")
            .bind(&d.id).bind(&d.project_id).bind(&d.service_id).bind(&d.operation).bind(&d.previous_version).bind(&d.target_version)
            .bind(&d.status).bind(&d.started_at).bind(&d.command_log).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO deployment_artifacts(deployment_id) VALUES(?)")
            .bind(&d.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.prune().await
    }
    pub async fn set_phase(&self, id: &str, phase: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE deployment_artifacts SET phase=? WHERE deployment_id=?")
            .bind(phase)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    pub async fn set_target_artifact(
        &self,
        id: &str,
        image: &str,
        pinned_image: &str,
        digest: &str,
        image_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE deployment_artifacts SET target_image=?,target_pinned_image=?,target_digest=?,target_image_id=? WHERE deployment_id=?")
            .bind(image).bind(pinned_image).bind(digest).bind(image_id).bind(id)
            .execute(&self.pool).await?;
        Ok(())
    }
    pub async fn artifact(&self, id: &str) -> Result<Option<DeploymentArtifact>, sqlx::Error> {
        sqlx::query_as("SELECT phase,target_image,target_pinned_image,target_digest,target_image_id FROM deployment_artifacts WHERE deployment_id=?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }
    pub async fn active_deployments(
        &self,
    ) -> Result<Vec<(Deployment, DeploymentArtifact)>, sqlx::Error> {
        let deployments: Vec<Deployment> = sqlx::query_as(
            "SELECT * FROM deployments WHERE status IN ('queued','running') ORDER BY started_at",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::new();
        for deployment in deployments {
            if let Some(artifact) = self.artifact(&deployment.id).await? {
                out.push((deployment, artifact));
            }
        }
        Ok(out)
    }
    pub async fn mark_running(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE deployments SET status='running' WHERE id=? AND status='queued'")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn finish_artifact_success(
        &self,
        id: &str,
        project_id: &str,
        service_id: &str,
        version: &str,
        image: &str,
        pinned_image: &str,
        image_digest: &str,
        image_id: &str,
        log: &str,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now().to_rfc3339();
        let log = truncate_utf8(log, self.max_log_bytes);
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO service_state(project_id,service_id,desired_version,image,updated_at,last_deployment_id,pinned_image,image_digest,image_id) VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(project_id,service_id) DO UPDATE SET desired_version=excluded.desired_version,image=excluded.image,updated_at=excluded.updated_at,last_deployment_id=excluded.last_deployment_id,pinned_image=excluded.pinned_image,image_digest=excluded.image_digest,image_id=excluded.image_id")
            .bind(project_id).bind(service_id).bind(version).bind(image).bind(&now).bind(id)
            .bind(pinned_image).bind(image_digest).bind(image_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE deployments SET status='succeeded',finished_at=?,command_log=?,rollback_status='not_needed' WHERE id=?")
            .bind(&now).bind(log).bind(id).execute(&mut *tx).await?;
        sqlx::query("UPDATE deployment_artifacts SET phase='committed' WHERE deployment_id=?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO service_revisions(deployment_id,project_id,service_id,version,image,pinned_image,image_digest,image_id,created_at) VALUES(?,?,?,?,?,?,?,?,?)")
            .bind(id).bind(project_id).bind(service_id).bind(version).bind(image)
            .bind(pinned_image).bind(image_digest).bind(image_id).bind(&now)
            .execute(&mut *tx).await?;
        tx.commit().await
    }
    pub async fn previous_revision(
        &self,
        project_id: &str,
        service_id: &str,
        current_deployment_id: &str,
    ) -> Result<Option<ServiceRevision>, sqlx::Error> {
        sqlx::query_as(
            "SELECT version,image,pinned_image,image_digest,image_id
             FROM service_revisions
             WHERE project_id=? AND service_id=? AND deployment_id<>?
             ORDER BY id DESC LIMIT 1",
        )
        .bind(project_id)
        .bind(service_id)
        .bind(current_deployment_id)
        .fetch_optional(&self.pool)
        .await
    }
    pub async fn finish_failure(
        &self,
        id: &str,
        error: &str,
        rollback: &str,
        log: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE deployments SET status='failed',finished_at=?,command_log=?,error_message=?,rollback_status=? WHERE id=?")
            .bind(Utc::now().to_rfc3339()).bind(truncate_utf8(log, self.max_log_bytes)).bind(error).bind(rollback).bind(id).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn finish_interrupted(
        &self,
        id: &str,
        error: &str,
        rollback: &str,
        log: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE deployments SET status='interrupted',finished_at=?,command_log=?,error_message=?,rollback_status=? WHERE id=?")
            .bind(Utc::now().to_rfc3339()).bind(truncate_utf8(log, self.max_log_bytes))
            .bind(error).bind(rollback).bind(id).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn finish_operation_success(&self, id: &str, log: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE deployments SET status='succeeded',finished_at=?,command_log=?,rollback_status='not_needed' WHERE id=?")
            .bind(Utc::now().to_rfc3339()).bind(truncate_utf8(log, self.max_log_bytes)).bind(id).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn deployments(&self, limit: u32) -> Result<Vec<DeploymentSummary>, sqlx::Error> {
        sqlx::query_as("SELECT id,project_id,service_id,operation,previous_version,target_version,status,started_at,finished_at,rollback_status FROM deployments ORDER BY started_at DESC LIMIT ?")
            .bind(limit.min(self.history_limit))
            .fetch_all(&self.pool)
            .await
    }
    pub async fn deployment(&self, id: &str) -> Result<Option<Deployment>, sqlx::Error> {
        sqlx::query_as("SELECT * FROM deployments WHERE id=?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }
    pub async fn delete_deployments_before(&self, cutoff: &str) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM deployments WHERE status NOT IN ('queued','running') AND datetime(started_at) < datetime(?)",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
    async fn prune(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            "DELETE FROM deployments WHERE id IN (
            SELECT id FROM deployments
            WHERE status NOT IN ('queued','running')
            ORDER BY started_at DESC LIMIT -1 OFFSET ?
        )",
        )
        .bind(self.history_limit)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

pub fn truncate_utf8(value: &str, max: usize) -> &str {
    if value.len() <= max {
        return value;
    }
    let mut start = value.len() - max;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    &value[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn record_success(
        storage: &Storage,
        id: &str,
        project: &str,
        service: &str,
        version: &str,
    ) {
        let image = format!("repo/{service}:{version}");
        let digest = format!("sha256:{id}");
        let pinned = format!("repo/{service}@{digest}");
        let image_id = format!("sha256:image-{id}");
        let deployment = Deployment {
            id: id.into(),
            project_id: project.into(),
            service_id: service.into(),
            operation: "deploy".into(),
            previous_version: None,
            target_version: version.into(),
            status: "queued".into(),
            started_at: Utc::now().to_rfc3339(),
            finished_at: None,
            command_log: String::new(),
            error_message: None,
            rollback_status: None,
        };
        storage.create_deployment(&deployment).await.unwrap();
        storage
            .finish_artifact_success(
                id, project, service, version, &image, &pinned, &digest, &image_id, "",
            )
            .await
            .unwrap();
    }

    #[test]
    fn truncates_on_boundary() {
        assert_eq!(
            truncate_utf8("a中文", 4),
            "中文".chars().skip(1).collect::<String>()
        );
    }

    #[tokio::test]
    async fn startup_preserves_unfinished_for_recovery_and_prunes_history() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path(), 2, 32).await.unwrap();
        for number in 0..3 {
            let deployment = Deployment {
                id: number.to_string(),
                project_id: "app".into(),
                service_id: "svc".into(),
                operation: "deploy".into(),
                previous_version: None,
                target_version: format!("1.0.{number}"),
                status: "queued".into(),
                started_at: format!("2026-01-01T00:00:0{number}Z"),
                finished_at: None,
                command_log: String::new(),
                error_message: None,
                rollback_status: None,
            };
            storage.create_deployment(&deployment).await.unwrap();
        }
        assert_eq!(storage.deployments(10).await.unwrap().len(), 2);
        drop(storage);
        let reopened = Storage::open(dir.path(), 2, 32).await.unwrap();
        assert!(
            reopened
                .deployments(10)
                .await
                .unwrap()
                .iter()
                .all(|item| item.status == "queued")
        );
        reopened.health().await.unwrap();
        assert!(
            reopened
                .deployments(10)
                .await
                .unwrap()
                .iter()
                .all(|item| item.operation == "deploy")
        );
    }

    #[tokio::test]
    async fn rejects_a_database_with_a_different_migration_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path(), 10, 1024).await.unwrap();
        sqlx::query("UPDATE _sqlx_migrations SET checksum=x'00' WHERE version=1")
            .execute(&storage.pool)
            .await
            .unwrap();
        storage.pool.close().await;

        let error = match Storage::open(dir.path(), 10, 1024).await {
            Ok(_) => panic!("database with an incompatible checksum was accepted"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("was previously applied but has been modified")
        );
    }

    #[tokio::test]
    async fn same_service_id_is_isolated_between_projects() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path(), 10, 1024).await.unwrap();
        record_success(&storage, "one", "frontend", "web", "1.0.0").await;
        record_success(&storage, "two", "backend", "web", "2.0.0").await;
        assert_eq!(
            storage
                .state("frontend", "web")
                .await
                .unwrap()
                .unwrap()
                .desired_version,
            "1.0.0"
        );
        assert_eq!(
            storage
                .state("backend", "web")
                .await
                .unwrap()
                .unwrap()
                .desired_version,
            "2.0.0"
        );
    }

    #[tokio::test]
    async fn preserves_previous_success_as_a_rollback_revision() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path(), 10, 1024).await.unwrap();
        record_success(&storage, "baseline", "app", "api", "1.0.0").await;
        let deployment = Deployment {
            id: "next".into(),
            project_id: "app".into(),
            service_id: "api".into(),
            operation: "deploy".into(),
            previous_version: Some("1.0.0".into()),
            target_version: "2.0.0".into(),
            status: "queued".into(),
            started_at: Utc::now().to_rfc3339(),
            finished_at: None,
            command_log: String::new(),
            error_message: None,
            rollback_status: None,
        };
        storage.create_deployment(&deployment).await.unwrap();
        storage
            .finish_artifact_success(
                "next",
                "app",
                "api",
                "2.0.0",
                "repo/api:2.0.0",
                "repo/api@sha256:two",
                "sha256:two",
                "sha256:image-two",
                "",
            )
            .await
            .unwrap();

        let previous = storage
            .previous_revision("app", "api", "next")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(previous.version, "1.0.0");
        assert_eq!(previous.image, "repo/api:1.0.0");
    }

    #[tokio::test]
    async fn deletes_only_finished_history_older_than_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path(), 10, 1024).await.unwrap();
        for (id, status, started_at) in [
            ("old", "succeeded", "2026-01-01T00:00:00Z"),
            ("active", "running", "2026-01-01T00:00:00Z"),
            ("recent", "failed", "2026-03-01T00:00:00Z"),
        ] {
            sqlx::query("INSERT INTO deployments(id,project_id,service_id,operation,target_version,status,started_at,command_log) VALUES(?, 'app', 'api', 'deploy', '1.0.0', ?, ?, '')")
                .bind(id)
                .bind(status)
                .bind(started_at)
                .execute(&storage.pool)
                .await
                .unwrap();
        }

        assert_eq!(
            storage
                .delete_deployments_before("2026-02-01T00:00:00Z")
                .await
                .unwrap(),
            1
        );
        assert!(storage.deployment("old").await.unwrap().is_none());
        assert!(storage.deployment("active").await.unwrap().is_some());
        assert!(storage.deployment("recent").await.unwrap().is_some());
    }
}
