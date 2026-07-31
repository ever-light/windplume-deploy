use crate::{
    compose::{Compose, RuntimeState},
    config::Config,
    registry::RegistryClient,
    storage::Storage,
    system::SystemManager,
    update::UpdateManager,
};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

#[derive(Clone, Default)]
pub struct RuntimeStateCache {
    inner: Arc<Mutex<RuntimeStateCacheInner>>,
}

#[derive(Default)]
struct RuntimeStateCacheInner {
    states: Option<BTreeMap<String, RuntimeState>>,
    last_attempt: Option<Instant>,
    refreshing: bool,
    error: Option<String>,
}

pub struct RuntimeStateSnapshot {
    pub states: Option<BTreeMap<String, RuntimeState>>,
    pub refreshing: bool,
    pub error: Option<String>,
    pub start_refresh: bool,
}

impl RuntimeStateCache {
    pub fn snapshot(&self, max_age: Duration) -> RuntimeStateSnapshot {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let expired = inner
            .last_attempt
            .is_none_or(|attempt| attempt.elapsed() >= max_age);
        let start_refresh = expired && !inner.refreshing;
        if start_refresh {
            inner.refreshing = true;
            inner.last_attempt = Some(Instant::now());
        }
        RuntimeStateSnapshot {
            states: inner.states.clone(),
            refreshing: inner.refreshing,
            error: inner.error.clone(),
            start_refresh,
        }
    }

    pub fn complete(&self, result: anyhow::Result<BTreeMap<String, RuntimeState>>) {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner.refreshing = false;
        match result {
            Ok(states) => {
                inner.states = Some(states);
                inner.error = None;
            }
            Err(error) => inner.error = Some(error.to_string()),
        }
    }

    pub fn invalidate(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner.last_attempt = None;
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner.states = None;
        inner.last_attempt = None;
        inner.error = None;
    }
}

#[derive(Clone)]
pub struct ProjectRuntime {
    pub compose: Compose,
    pub override_file: PathBuf,
    pub deploy_lock: Arc<Semaphore>,
    pub runtime_cache: RuntimeStateCache,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub storage: Storage,
    pub registry: RegistryClient,
    pub system: SystemManager,
    pub updates: UpdateManager,
    pub projects: Arc<HashMap<String, ProjectRuntime>>,
}

impl AppState {
    pub fn project_runtime(&self, id: &str) -> Option<&ProjectRuntime> {
        self.projects.get(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_cache_deduplicates_refreshes_and_keeps_snapshot_when_invalidated() {
        let cache = RuntimeStateCache::default();
        let first = cache.snapshot(Duration::from_secs(10));
        assert!(first.start_refresh);
        assert!(first.refreshing);
        assert!(first.states.is_none());

        let concurrent = cache.snapshot(Duration::from_secs(10));
        assert!(!concurrent.start_refresh);
        assert!(concurrent.refreshing);

        let states = BTreeMap::from([(
            "api".into(),
            RuntimeState {
                container_status: "healthy".into(),
                ..Default::default()
            },
        )]);
        cache.complete(Ok(states));
        let cached = cache.snapshot(Duration::from_secs(10));
        assert!(!cached.start_refresh);
        assert!(!cached.refreshing);
        assert_eq!(cached.states.unwrap()["api"].container_status, "healthy");

        cache.invalidate();
        let invalidated = cache.snapshot(Duration::from_secs(10));
        assert!(invalidated.start_refresh);
        assert_eq!(
            invalidated.states.unwrap()["api"].container_status,
            "healthy"
        );
    }
}
