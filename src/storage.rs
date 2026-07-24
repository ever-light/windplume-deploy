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
    pub updated_at: String,
    pub last_deployment_id: Option<String>,
}

#[derive(Clone, Debug, FromRow, Serialize)]
pub struct Deployment {
    pub id: String,
    pub project_id: String,
    pub service_id: String,
    pub previous_version: Option<String>,
    pub target_version: String,
    pub status: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub command_log: String,
    pub error_message: Option<String>,
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
        this.interrupt_unfinished().await?;
        Ok(this)
    }
    async fn interrupt_unfinished(&self) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE deployments SET status='interrupted', finished_at=? WHERE status IN ('queued','running')")
            .bind(Utc::now().to_rfc3339()).execute(&self.pool).await?;
        Ok(())
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
        sqlx::query("INSERT INTO deployments(id,project_id,service_id,previous_version,target_version,status,started_at,command_log) VALUES(?,?,?,?,?,?,?,?)")
            .bind(&d.id).bind(&d.project_id).bind(&d.service_id).bind(&d.previous_version).bind(&d.target_version)
            .bind(&d.status).bind(&d.started_at).bind(&d.command_log).execute(&self.pool).await?;
        self.prune().await
    }
    pub async fn mark_running(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE deployments SET status='running' WHERE id=? AND status='queued'")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    pub async fn finish_success(
        &self,
        id: &str,
        project_id: &str,
        service_id: &str,
        version: &str,
        image: &str,
        log: &str,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now().to_rfc3339();
        let log = truncate_utf8(log, self.max_log_bytes);
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO service_state(project_id,service_id,desired_version,image,updated_at,last_deployment_id) VALUES(?,?,?,?,?,?) ON CONFLICT(project_id,service_id) DO UPDATE SET desired_version=excluded.desired_version,image=excluded.image,updated_at=excluded.updated_at,last_deployment_id=excluded.last_deployment_id")
            .bind(project_id).bind(service_id).bind(version).bind(image).bind(&now).bind(id).execute(&mut *tx).await?;
        sqlx::query("UPDATE deployments SET status='succeeded',finished_at=?,command_log=?,rollback_status='not_needed' WHERE id=?")
            .bind(&now).bind(log).bind(id).execute(&mut *tx).await?;
        tx.commit().await
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
    pub async fn deployments(&self, limit: u32) -> Result<Vec<Deployment>, sqlx::Error> {
        sqlx::query_as("SELECT * FROM deployments ORDER BY started_at DESC LIMIT ?")
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
    async fn prune(&self) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM deployments WHERE id IN (SELECT id FROM deployments ORDER BY started_at DESC LIMIT -1 OFFSET ?)")
            .bind(self.history_limit).execute(&self.pool).await?;
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
    #[test]
    fn truncates_on_boundary() {
        assert_eq!(
            truncate_utf8("a中文", 4),
            "中文".chars().skip(1).collect::<String>()
        );
    }

    #[tokio::test]
    async fn startup_interrupts_unfinished_and_prunes_history() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path(), 2, 32).await.unwrap();
        for number in 0..3 {
            let deployment = Deployment {
                id: number.to_string(),
                project_id: "app".into(),
                service_id: "svc".into(),
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
                .all(|item| item.status == "interrupted")
        );
        reopened.health().await.unwrap();
    }

    #[tokio::test]
    async fn same_service_id_is_isolated_between_projects() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path(), 10, 1024).await.unwrap();
        storage
            .finish_success("one", "frontend", "web", "1.0.0", "image:1.0.0", "")
            .await
            .unwrap();
        storage
            .finish_success("two", "backend", "web", "2.0.0", "image:2.0.0", "")
            .await
            .unwrap();
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
}
