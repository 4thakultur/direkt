use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use direkt::config::{Config, LogFormat};
use direkt::engine::{Engine, EngineSettings};
use direkt::net::NetworkPolicy;
use direkt::providers::Providers;
use direkt::store::Store;
use serde_json::json;
use std::path::Path;
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
struct AppState {
    store: Store,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (config, config_path) = match Config::load() {
        Ok(loaded) => loaded,
        Err(e) => {
            // Logging is not initialised yet; config errors go straight to stderr.
            eprintln!("direkt: {e}");
            std::process::exit(2);
        }
    };
    init_logging(&config);
    tracing::info!(config = %config_path.display(), "starting direkt {}", env!("CARGO_PKG_VERSION"));

    for dir in [
        &config.storage.data_dir,
        &config.storage.download_dir,
        &config.storage.incomplete_dir,
    ] {
        ensure_dir(dir)?;
    }

    let store = Store::open(&config.storage.data_dir.join("direkt.db"))
        .await
        .map_err(|e| anyhow::anyhow!("opening database: {e}"))?;
    let report = store
        .recover_interrupted(chrono::Utc::now())
        .await
        .map_err(|e| anyhow::anyhow!("recovering interrupted work: {e}"))?;
    if report != Default::default() {
        tracing::info!(
            transfers = report.requeued_transfers,
            post_processing = report.requeued_post_processing,
            "re-queued work interrupted by previous shutdown"
        );
    }

    let policy = NetworkPolicy::new(config.network.allowed_private_networks.clone());
    let client = direkt::http_client::build_client(policy.clone())?;
    let engine = Engine::new(
        store.clone(),
        Providers::default(),
        client,
        policy,
        EngineSettings::from_config(&config),
    );
    let dispatcher = engine.start();

    let app = Router::new()
        .route("/healthz", get(health))
        .with_state(AppState {
            store: store.clone(),
        });

    let listener = tokio::net::TcpListener::bind(config.server.listen).await?;
    tracing::info!(addr = %config.server.listen, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    engine.shutdown(dispatcher).await;
    store.close().await;
    tracing::info!("shutdown complete");
    Ok(())
}

async fn health(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    match state.store.ping().await {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "ok" }))),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "error", "database": "unavailable" })),
        ),
    }
}

fn init_logging(config: &Config) {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&config.log.level))
        .unwrap_or_else(|e| {
            eprintln!("direkt: invalid log.level ({e}); falling back to 'info'");
            EnvFilter::new("info")
        });
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match config.log.format {
        LogFormat::Pretty => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}

fn ensure_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)
        .map_err(|e| anyhow::anyhow!("creating directory {}: {e}", dir.display()))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
