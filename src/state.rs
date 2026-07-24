use crate::{compose::Compose, config::Config, registry::RegistryClient, storage::Storage};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::Semaphore;

#[derive(Clone)]
pub struct ProjectRuntime {
    pub compose: Compose,
    pub override_file: PathBuf,
    pub deploy_lock: Arc<Semaphore>,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub storage: Storage,
    pub registry: RegistryClient,
    pub projects: Arc<HashMap<String, ProjectRuntime>>,
}

impl AppState {
    pub fn project_runtime(&self, id: &str) -> Option<&ProjectRuntime> {
        self.projects.get(id)
    }
}
