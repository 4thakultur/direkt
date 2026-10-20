use direkt::auth::AuthService;
use direkt::config::{Config, LogFormat};
use direkt::engine::{Engine, EngineSettings};
use direkt::net::NetworkPolicy;
use direkt::providers::Providers;
use direkt::service::DownloadService;
use direkt::store::Store;
use direkt::web::{self, AppState};
use std::net::SocketAddr;
use std::path::Path;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: direkt [serve | reset-admin]

  serve        run the server (default)
  reset-admin  delete the administrator account, all sessions, and all API
               tokens; the next start offers first-run setup again

Configuration is read from $DIREKT_CONFIG (default ./direkt.toml).";

enum Command {
    Serve,
    ResetAdmin,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["serve"] => Command::Serve,
        ["reset-admin"] => Command::ResetAdmin,
        ["-h" | "--help" | "help"] => {
            println!("{USAGE}");
            return Ok(());
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };

    let (config, config_path) = match Config::load() {
        Ok(loaded) => loaded,
        Err(e) => {
            // Logging is not initialised yet; config errors go straight to stderr.
            eprintln!("direkt: {e}");
            std::process::exit(2);
        }
    };
    init_logging(&config);

    match command {
        Command::Serve => serve(config, &config_path).await,
        Command::ResetAdmin => reset_admin(&config).await,
    }
}

async fn open_store(config: &Config) -> anyhow::Result<Store> {
    ensure_dir(&config.storage.data_dir)?;
    Store::open(&config.storage.data_dir.join("direkt.db"))
        .await
        .map_err(|e| anyhow::anyhow!("opening database: {e}"))
}

async fn reset_admin(config: &Config) -> anyhow::Result<()> {
    let store = open_store(config).await?;
    store
        .reset_admin()
        .await
        .map_err(|e| anyhow::anyhow!("resetting administrator: {e}"))?;
    store.close().await;
    println!(
        "Administrator account, sessions, and API tokens removed. \
         Restart Direkt and open the web UI to run setup again."
    );
    Ok(())
}

async fn serve(config: Config, config_path: &Path) -> anyhow::Result<()> {
    tracing::info!(config = %config_path.display(), "starting direkt {}", env!("CARGO_PKG_VERSION"));

    for dir in [&config.storage.download_dir, &config.storage.incomplete_dir] {
        ensure_dir(dir)?;
    }
    let store = open_store(&config).await?;
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

    let auth = AuthService::new(store.clone(), &config.storage.data_dir)
        .await
        .map_err(|e| anyhow::anyhow!("initializing authentication: {e}"))?;
    if let Some(path) = auth.pending_setup_token_path() {
        tracing::warn!(
            setup_token_file = %path.display(),
            "no administrator account yet: open the web UI and enter the setup token from this file"
        );
    }

    let policy = NetworkPolicy::new(config.network.allowed_private_networks.clone());
    let client = direkt::http_client::build_client(policy.clone())?;
    let providers = Providers::standard(&config, client.clone());
    let engine = Engine::new(
        store.clone(),
        providers.clone(),
        client,
        policy.clone(),
        EngineSettings::from_config(&config),
    );
    let dispatcher = engine.start();
    let downloads = DownloadService::new(store.clone(), engine.clone(), providers, policy);

    let app = web::router(AppState {
        downloads,
        auth,
        store: store.clone(),
        secure_cookies: config.server.secure_cookies,
    });

    let listener = tokio::net::TcpListener::bind(config.server.listen).await?;
    tracing::info!(addr = %config.server.listen, "listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    engine.shutdown(dispatcher).await;
    store.close().await;
    tracing::info!("shutdown complete");
    Ok(())
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
