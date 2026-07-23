use crate::{compose::Compose, config::Config, github::GithubClient, storage::Storage};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::Semaphore;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub storage: Storage,
    pub github: GithubClient,
    pub compose: Compose,
    pub override_file: PathBuf,
    pub deploy_lock: Arc<Semaphore>,
}
