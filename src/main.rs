#![forbid(unsafe_code)]

use direkt::auth::AuthService;
use direkt::config::{Config, LogFormat};
use direkt::engine::{Engine, EngineSettings, Limits};
use direkt::media::archive::SevenZip;
use direkt::media::{ArtworkService, Organizer};
use direkt::net::NetworkPolicy;
use direkt::providers::{OneFichierAccount, Providers};
use direkt::service::DownloadService;
use direkt::settings::{EnvKeys, SettingsService};
use direkt::store::Store;
use direkt::web::{self, AppState};
use std::path::Path;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: direkt [serve | reset-password <username> | reset-2fa <username>
              | reset-admin | healthcheck]

  serve                      run the server (default)
  reset-password <username>  give the account a new temporary password,
                             printed here, and sign it out; the next sign-in
                             must replace it
  reset-2fa <username>       turn off the account's two-factor
                             authentication, for a lost authenticator app;
                             its password, sessions and API tokens stay
  reset-admin                delete every administrator account with its
                             sessions and API tokens; the next start offers
                             first-run setup again. Other accounts and all
                             downloads stay
  healthcheck                exit 0 if the server running with this
                             configuration answers its health endpoint, 1
                             otherwise (for container health checks)

Configuration is read from $DIREKT_CONFIG (default ./direkt.toml).";

enum Command {
    Serve,
    ResetPassword(String),
    ResetTwoFactor(String),
    ResetAdmin,
    HealthCheck,
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
        ["reset-password", username] => Command::ResetPassword((*username).to_owned()),
        ["reset-2fa", username] => Command::ResetTwoFactor((*username).to_owned()),
        ["reset-password" | "reset-2fa"] => {
            eprintln!("direkt: name the account, e.g. `direkt {} alice`", args[0]);
            std::process::exit(2);
        }
        ["healthcheck"] => Command::HealthCheck,
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
        Command::ResetPassword(username) => reset_password(&config, &username).await,
        Command::ResetTwoFactor(username) => reset_two_factor(&config, &username).await,
        Command::HealthCheck => web::server::probe_health(config.server.listen)
            .await
            .map_err(|e| anyhow::anyhow!("health check failed: {e}")),
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
    let deleted = store
        .delete_admins()
        .await
        .map_err(|e| anyhow::anyhow!("deleting administrator accounts: {e}"))?;
    store.close().await;
    tracing::warn!(
        deleted,
        "administrator accounts deleted from the command line"
    );
    println!(
        "{deleted} administrator account(s) deleted, with their sessions and API tokens. \
         Restart Direkt and open the web UI to create a new administrator. Other accounts \
         and all downloads were kept."
    );
    Ok(())
}

/// Find an account by name, for the recovery commands.
async fn account(store: &Store, username: &str) -> anyhow::Result<direkt::users::User> {
    store
        .user_by_name(username)
        .await
        .map_err(|e| anyhow::anyhow!("looking up the account: {e}"))?
        .map(|r| r.user)
        .ok_or_else(|| anyhow::anyhow!("no account is named {username:?}"))
}

async fn reset_password(config: &Config, username: &str) -> anyhow::Result<()> {
    let store = open_store(config).await?;
    let user = account(&store, username).await?;
    let issued = direkt::auth::accounts::issue_temporary_password(&store, user)
        .await
        .map_err(|e| anyhow::anyhow!("setting a temporary password: {e}"))?;
    store.close().await;
    tracing::warn!(user = %issued.user.username, "temporary password issued from the command line");
    println!(
        "Temporary password for {}: {}\n\
         It works until {} and must be replaced at the next sign-in. The account \
         was signed out.{}",
        issued.user.username,
        issued.password,
        issued.expires_at.format("%Y-%m-%d %H:%M UTC"),
        if issued.user.disabled {
            " It is disabled: an administrator must enable it before it can sign in."
        } else {
            ""
        }
    );
    Ok(())
}

async fn reset_two_factor(config: &Config, username: &str) -> anyhow::Result<()> {
    let store = open_store(config).await?;
    let user = account(&store, username).await?;
    let was_on = store
        .disable_two_factor(user.id)
        .await
        .map_err(|e| anyhow::anyhow!("turning off two-factor authentication: {e}"))?;
    store.close().await;
    if was_on {
        tracing::warn!(user = %user.username, "two-factor authentication turned off from the command line");
        println!(
            "Two-factor authentication is off for {}. Sign in with the password{}.",
            user.username,
            if user.require_two_factor {
                "; it is required for this account, so the sign-in sets it up again"
            } else {
                ", then set it up again under Settings"
            }
        );
    } else {
        println!(
            "Two-factor authentication was already off for {}.",
            user.username
        );
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
            "no administrator account: open the web UI and enter the setup token from this file"
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
    let sevenzip = SevenZip::find();
    match &sevenzip {
        Some(z) => {
            tracing::info!(program = %z.program().display(), "archives are extracted with 7-Zip")
        }
        None => tracing::warn!("7-Zip (7z) not found; archives are not extracted"),
    }
    let organizer = Organizer::new(
        store.clone(),
        settings.library_cell(),
        artwork.clone(),
        sevenzip,
        settings.delete_archives_cell(),
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
