#![forbid(unsafe_code)]

use direkt::auth::AuthService;
use direkt::config::{Config, LogFormat};
use direkt::engine::{Engine, EngineSettings, Limits};
use direkt::media::{ArtworkService, Organizer};
use direkt::net::NetworkPolicy;
use direkt::providers::{OneFichierAccount, Providers};
use direkt::service::DownloadService;
use direkt::settings::{EnvKeys, SettingsService};
use direkt::store::Store;
use direkt::web::{self, AppState};
use std::path::Path;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: direkt [serve | reset-admin | reset-2fa]

  serve        run the server (default)
  reset-admin  delete the administrator account, all sessions, and all API
               tokens; the next start offers first-run setup again
  reset-2fa    turn off two-factor authentication, for a lost authenticator
               app; the account, sessions and API tokens stay

Configuration is read from $DIREKT_CONFIG (default ./direkt.toml).";

enum Command {
    Serve,
    ResetAdmin,
    ResetTwoFactor,
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
        ["reset-2fa"] => Command::ResetTwoFactor,
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
        Command::ResetTwoFactor => reset_two_factor(&config).await,
    }
}

async fn open_store(config: &Config) -> anyhow::Result<Store> {
    // Holds the database and setup token: owner-only.
    direkt::fsutil::create_private_dir(&config.storage.data_dir).map_err(|e| {
        anyhow::anyhow!(
            "creating data directory {}: {e}",
            config.storage.data_dir.display()
        )
    })?;
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

async fn reset_two_factor(config: &Config) -> anyhow::Result<()> {
    let store = open_store(config).await?;
    let was_on = store
        .disable_two_factor()
        .await
        .map_err(|e| anyhow::anyhow!("turning off two-factor authentication: {e}"))?;
    store.close().await;
    if was_on {
        tracing::warn!("two-factor authentication turned off from the command line");
        println!(
            "Two-factor authentication is off. Sign in with the password, then \
             set it up again under Settings."
        );
    } else {
        println!("Two-factor authentication was already off.");
    }
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

    let env_keys = EnvKeys {
        onefichier: direkt::config::onefichier_key_from_env(|name| std::env::var(name).ok())
            .map_err(|e| anyhow::anyhow!("{e}"))?,
        tmdb: direkt::config::tmdb_key_from_env(|name| std::env::var(name).ok())
            .map_err(|e| anyhow::anyhow!("{e}"))?,
        library: direkt::config::library_from_env(|name| std::env::var(name).ok())
            .map_err(|e| anyhow::anyhow!("{e}"))?,
    };
    let policy = NetworkPolicy::new(config.network.allowed_private_networks.clone());
    let onefichier_account = OneFichierAccount::open(store.clone(), policy.clone())
        .await
        .map_err(|e| anyhow::anyhow!("loading 1fichier account: {e}"))?;
    // Changed live from the settings page; starts from the configuration.
    let limits = Limits::new(config.downloads.max_concurrent);
    let settings = SettingsService::load(
        store.clone(),
        env_keys,
        onefichier_account.clone(),
        limits.clone(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("loading settings: {e}"))?;

    let client = direkt::http_client::build_client(policy.clone())?;
    let providers = Providers::standard(
        client.clone(),
        settings.onefichier_key(),
        onefichier_account,
    );
    let engine = Engine::with_limits(
        store.clone(),
        providers.clone(),
        client.clone(),
        policy.clone(),
        EngineSettings::from_config(&config),
        limits,
    );
    let dispatcher = engine.start();
    let artwork = ArtworkService::new(
        config.storage.data_dir.join("artwork"),
        client.clone(),
        settings.tmdb_key(),
    );
    let organizer = Organizer::new(
        store.clone(),
        settings.library_cell(),
        artwork.clone(),
        engine.completions(),
    );
    let organizing = organizer.start();
    let downloads = DownloadService::new(
        store.clone(),
        engine.clone(),
        providers,
        policy,
        artwork.clone(),
    );

    let app = web::router(AppState {
        downloads,
        auth,
        settings,
        artwork,
        store: store.clone(),
        secure_cookies: config.server.secure_cookies,
    });

    let listener = tokio::net::TcpListener::bind(config.server.listen).await?;
    tracing::info!(addr = %config.server.listen, "listening");
    web::server::serve(listener, app, shutdown_signal()).await?;

    engine.shutdown(dispatcher).await;
    organizer.shutdown(organizing).await;
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
