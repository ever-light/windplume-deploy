mod compose;
mod config;
mod deployment;
mod error;
mod github;
mod state;
mod storage;
mod web;
use clap::Parser;
use compose::ProcessRunner;
use config::Config;
use state::AppState;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use tower_http::{sensitive_headers::SetSensitiveRequestHeadersLayer, trace::TraceLayer};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "/etc/wind-plume-deploy/config.yaml")]
    config: PathBuf,
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "wind_plume_deploy=info,tower_http=info".into()),
        )
        .init();
    let args = Args::parse();
    let (cfg, token) = Config::load(&args.config)?;
    let cfg = Arc::new(cfg);
    let storage = storage::Storage::open(
        &cfg.storage.data_dir,
        cfg.storage.history_limit,
        cfg.storage.max_log_bytes,
    )
    .await?;
    let github = github::GithubClient::new(
        cfg.github.api_base.clone(),
        token,
        Duration::from_secs(cfg.github.cache_seconds),
    )?;
    let override_file = cfg.storage.data_dir.join("compose.deploy.yaml");
    let compose = compose::Compose::new(
        cfg.compose.clone(),
        override_file.clone(),
        Arc::new(ProcessRunner),
    );
    let state = AppState {
        config: cfg.clone(),
        storage,
        github,
        compose,
        override_file,
        deploy_lock: Arc::new(Semaphore::new(1)),
    };
    deployment::rebuild_override(&state).await?;
    let app = web::router(state)
        .layer(SetSensitiveRequestHeadersLayer::new(std::iter::once(
            axum::http::header::AUTHORIZATION,
        )))
        .layer(TraceLayer::new_for_http());
    let listener = tokio::net::TcpListener::bind(cfg.server.listen).await?;
    tracing::info!(listen=%cfg.server.listen,"wind-plume-deploy started");
    axum::serve(listener, app).await?;
    Ok(())
}
