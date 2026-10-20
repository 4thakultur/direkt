//! Authentication: one administrator account, browser sessions, and
//! revocable API tokens.
//!
//! First-run setup: while no administrator exists, a random one-time setup
//! token is written to `<data_dir>/setup-token` (mode 0600). Creating the
//! account requires this token, so nobody who merely reaches an exposed
//! instance can claim it first. The token is never logged; only its file path
//! is.

use crate::error::{AppError, AppResult, ErrorKind};
use crate::store::{AdminAccount, ApiToken, Store};
use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 3600);
const API_TOKEN_PREFIX: &str = "dkt_";
const MIN_PASSWORD_CHARS: usize = 10;
/// Argon2 cost grows with input; cap it so login cannot be used for DoS.
const MAX_PASSWORD_BYTES: usize = 1024;
const MAX_USERNAME_CHARS: usize = 64;
const MAX_TOKEN_NAME_CHARS: usize = 64;

/// Unsuccessful attempts allowed per client per window.
const LOGIN_MAX_ATTEMPTS: u32 = 5;
const LOGIN_WINDOW: Duration = Duration::from_secs(15 * 60);
const LIMITER_MAX_ENTRIES: usize = 10_000;

#[derive(Clone)]
pub struct AuthService {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    setup_token_path: PathBuf,
    /// SHA-256 of the current setup token, while setup is pending.
    setup_token_hash: Mutex<Option<String>>,
    /// Hash verified when the username is wrong, so timing does not reveal it.
    dummy_hash: String,
    limiter: LoginLimiter,
}

/// A newly issued browser session.
#[derive(Debug, Clone)]
pub struct Session {
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

impl AuthService {
    /// Initialize authentication. If no administrator exists yet, generate a
    /// setup token and write it to `<data_dir>/setup-token`.
    pub async fn new(store: Store, data_dir: &Path) -> AppResult<Self> {
        let dummy_hash = hash_password(random_hex(16)).await?;
        let service = Self {
            inner: Arc::new(Inner {
                store,
                setup_token_path: data_dir.join("setup-token"),
                setup_token_hash: Mutex::new(None),
                dummy_hash,
                limiter: LoginLimiter::default(),
            }),
        };
        if service.needs_setup().await? {
            service.issue_setup_token()?;
        } else {
            service.remove_setup_token_file();
        }
        Ok(service)
    }

    /// Path of the setup-token file, if setup is pending.
    pub fn pending_setup_token_path(&self) -> Option<&Path> {
        self.setup_hash()
            .is_some()
            .then_some(self.inner.setup_token_path.as_path())
    }

    pub async fn needs_setup(&self) -> AppResult<bool> {
        Ok(self.inner.store.admin().await?.is_none())
    }

    /// Create the administrator account and log in.
    pub async fn setup(
        &self,
        client: IpAddr,
        setup_token: &str,
        username: &str,
        password: &str,
    ) -> AppResult<Session> {
        self.inner.limiter.attempt(client)?;
        let expected = self
            .setup_hash()
            .ok_or_else(|| AppError::conflict("setup has already been completed"))?;
        if sha256_hex(setup_token.trim()) != expected {
            return Err(AppError::unauthorized("invalid setup token"));
        }
        // The token proves control of the server; form mistakes don't count.
        self.inner.limiter.succeeded(client);
        let username = validate_username(username)?;
        validate_password(password)?;

        let password_hash = hash_password(password.to_owned()).await?;
        self.inner
            .store
            .create_admin(
                &AdminAccount {
                    username: username.to_owned(),
                    password_hash,
                },
                Utc::now(),
            )
            .await?;
        *self.setup_slot() = None;
        self.remove_setup_token_file();
        tracing::info!(username, "administrator account created");
        self.new_session().await
    }

    pub async fn login(
        &self,
        client: IpAddr,
        username: &str,
        password: &str,
    ) -> AppResult<Session> {
        // Counted before verifying, so parallel guesses cannot all slip in
        // before the first failure is recorded.
        self.inner.limiter.attempt(client)?;
        if password.len() > MAX_PASSWORD_BYTES {
            return Err(invalid_credentials());
        }
        let admin = self.inner.store.admin().await?;
        let (hash, username_ok) = match &admin {
            Some(a) => (a.password_hash.clone(), a.username == username),
            None => (self.inner.dummy_hash.clone(), false),
        };
        // Always verify a hash so the response time does not reveal whether
        // the username exists.
        let password_ok = verify_password(password.to_owned(), hash).await?;
        if !(username_ok && password_ok) {
            tracing::warn!(%client, "failed login attempt");
            return Err(invalid_credentials());
        }
        self.inner.limiter.succeeded(client);
        let _ = self.inner.store.prune_sessions(Utc::now()).await;
        tracing::info!(%client, "administrator logged in");
        self.new_session().await
    }

    pub async fn logout(&self, session_token: &str) -> AppResult<()> {
        self.inner
            .store
            .delete_session(&sha256_hex(session_token))
            .await
    }

    pub async fn session_valid(&self, session_token: &str) -> AppResult<bool> {
        self.inner
            .store
            .session_valid(&sha256_hex(session_token), Utc::now())
            .await
    }

    /// Change the password, end every session, and start a new one.
    pub async fn change_password(
        &self,
        client: IpAddr,
        current: &str,
        new: &str,
    ) -> AppResult<Session> {
        self.inner.limiter.attempt(client)?;
        let admin = self
            .inner
            .store
            .admin()
            .await?
            .ok_or_else(|| AppError::conflict("no administrator account exists"))?;
        if current.len() > MAX_PASSWORD_BYTES
            || !verify_password(current.to_owned(), admin.password_hash).await?
        {
            return Err(AppError::invalid("current password is incorrect"));
        }
        self.inner.limiter.succeeded(client);
        validate_password(new)?;
        let hash = hash_password(new.to_owned()).await?;
        self.inner
            .store
            .update_admin_password(&hash, Utc::now())
            .await?;
        self.inner.store.delete_all_sessions().await?;
        tracing::info!("administrator password changed; all sessions ended");
        self.new_session().await
    }

    /// Validate a bearer token and record its use.
    pub async fn api_token_valid(&self, token: &str) -> AppResult<bool> {
        if !token.starts_with(API_TOKEN_PREFIX) {
            return Ok(false);
        }
        Ok(self
            .inner
            .store
            .use_api_token(&sha256_hex(token), Utc::now())
            .await?
            .is_some())
    }

    /// Create an API token. The secret is returned once and never stored.
    pub async fn create_api_token(&self, name: &str) -> AppResult<(ApiToken, String)> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > MAX_TOKEN_NAME_CHARS {
            return Err(AppError::invalid(format!(
                "token name must be 1-{MAX_TOKEN_NAME_CHARS} characters"
            )));
        }
        if name.chars().any(char::is_control) {
            return Err(AppError::invalid(
                "token name must not contain control characters",
            ));
        }
        let secret = format!("{API_TOKEN_PREFIX}{}", random_hex(32));
        let token = self
            .inner
            .store
            .insert_api_token(name, &sha256_hex(&secret), Utc::now())
            .await?;
        tracing::info!(token_id = %token.id, name, "API token created");
        Ok((token, secret))
    }

    pub async fn list_api_tokens(&self) -> AppResult<Vec<ApiToken>> {
        self.inner.store.list_api_tokens().await
    }

    pub async fn revoke_api_token(&self, id: Uuid) -> AppResult<()> {
        if !self.inner.store.delete_api_token(id).await? {
            return Err(AppError::not_found("API token not found"));
        }
        tracing::info!(token_id = %id, "API token revoked");
        Ok(())
    }

    async fn new_session(&self) -> AppResult<Session> {
        let token = random_hex(32);
        let now = Utc::now();
        let expires_at = now + chrono::Duration::from_std(SESSION_TTL).expect("constant fits");
        self.inner
            .store
            .insert_session(&sha256_hex(&token), now, expires_at)
            .await?;
        Ok(Session { token, expires_at })
    }

    fn issue_setup_token(&self) -> AppResult<()> {
        let token = random_hex(16);
        write_secret_file(&self.inner.setup_token_path, &token).map_err(|e| {
            tracing::error!(error = %e, path = %self.inner.setup_token_path.display(), "cannot write setup token");
            AppError::new(ErrorKind::Filesystem, "cannot write setup token file")
        })?;
        *self.setup_slot() = Some(sha256_hex(&token));
        Ok(())
    }

    fn remove_setup_token_file(&self) {
        match std::fs::remove_file(&self.inner.setup_token_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(error = %e, "could not remove setup token file"),
        }
    }

    fn setup_slot(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.inner
            .setup_token_hash
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn setup_hash(&self) -> Option<String> {
        self.setup_slot().clone()
    }

    #[cfg(test)]
    pub(crate) fn read_setup_token(&self) -> String {
        std::fs::read_to_string(&self.inner.setup_token_path).unwrap()
    }
}

fn invalid_credentials() -> AppError {
    AppError::unauthorized("invalid username or password")
}

fn validate_username(username: &str) -> AppResult<&str> {
    let username = username.trim();
    if username.is_empty() || username.chars().count() > MAX_USERNAME_CHARS {
        return Err(AppError::invalid(format!(
            "username must be 1-{MAX_USERNAME_CHARS} characters"
        )));
    }
    if username
        .chars()
        .any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(AppError::invalid(
            "username must not contain spaces or control characters",
        ));
    }
    Ok(username)
}

fn validate_password(password: &str) -> AppResult<()> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(AppError::invalid(format!(
            "password must be at least {MIN_PASSWORD_CHARS} characters"
        )));
    }
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(AppError::invalid(format!(
            "password must be at most {MAX_PASSWORD_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Argon2 takes ~19 MiB and noticeable CPU per call; cap concurrent hashes so
/// a burst of login requests cannot exhaust memory or CPU.
static HASHING_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

async fn hash_password(password: String) -> AppResult<String> {
    let _slot = HASHING_SLOTS
        .acquire()
        .await
        .map_err(|_| AppError::internal("password hashing unavailable"))?;
    tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(password.as_bytes())
            .map(|h| h.to_string())
    })
    .await
    .map_err(|_| AppError::internal("password hashing task failed"))?
    .map_err(|e| {
        tracing::error!(error = %e, "password hashing failed");
        AppError::internal("password hashing failed")
    })
}

async fn verify_password(password: String, hash: String) -> AppResult<bool> {
    let _slot = HASHING_SLOTS
        .acquire()
        .await
        .map_err(|_| AppError::internal("password verification unavailable"))?;
    tokio::task::spawn_blocking(move || {
        let parsed = PasswordHash::new(&hash).map_err(|e| {
            tracing::error!(error = %e, "stored password hash is invalid");
            AppError::internal("stored password hash is invalid")
        })?;
        Ok(Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok())
    })
    .await
    .map_err(|_| AppError::internal("password verification task failed"))?
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("operating system RNG is unavailable");
    hex::encode(buf)
}

pub(crate) fn sha256_hex(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

/// Write a secret to a new file readable only by the owner.
fn write_secret_file(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        // `mode` only applies on creation; tighten an existing file too.
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()
}

/// Per-client attempt counter for login, setup and password changes.
///
/// Every attempt is counted *before* the credentials are checked, and only
/// a success clears the count, so concurrent requests cannot exceed the
/// limit. IPv6 clients are grouped by /64 prefix, because a single host
/// usually controls a whole /64. The table is bounded: if it is full of
/// recently active clients, new clients are refused (fail closed) instead
/// of growing it.
///
/// Behind a reverse proxy every client shares the proxy's address, so the
/// limit then applies to all clients together. That still bounds
/// brute-forcing, at the cost of locking out the real user during an attack.
#[derive(Default)]
struct LoginLimiter {
    entries: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

impl LoginLimiter {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<IpAddr, (u32, Instant)>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record an attempt, or refuse it if the client used up its budget.
    fn attempt(&self, client: IpAddr) -> AppResult<()> {
        let key = limiter_key(client);
        let mut entries = self.lock();
        if entries.len() >= LIMITER_MAX_ENTRIES && !entries.contains_key(&key) {
            entries.retain(|_, (_, since)| since.elapsed() < LOGIN_WINDOW);
            if entries.len() >= LIMITER_MAX_ENTRIES {
                return Err(AppError::new(
                    ErrorKind::RateLimited,
                    "too many sign-in attempts from many addresses; try again later",
                ));
            }
        }
        let entry = entries.entry(key).or_insert((0, Instant::now()));
        if entry.1.elapsed() >= LOGIN_WINDOW {
            *entry = (0, Instant::now());
        }
        if entry.0 >= LOGIN_MAX_ATTEMPTS {
            let wait = LOGIN_WINDOW.saturating_sub(entry.1.elapsed());
            return Err(AppError::new(
                ErrorKind::RateLimited,
                format!(
                    "too many failed attempts; try again in {} minutes",
                    wait.as_secs().div_ceil(60)
                ),
            ));
        }
        entry.0 += 1;
        Ok(())
    }

    fn succeeded(&self, client: IpAddr) {
        self.lock().remove(&limiter_key(client));
    }
}

/// The address a client is counted under: IPv4 as is (including IPv4-mapped
/// IPv6), IPv6 by its /64 network.
pub(crate) fn limiter_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let bits = u128::from(v6) & !((1u128 << 64) - 1);
                IpAddr::V6(std::net::Ipv6Addr::from(bits))
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 1));

    async fn service() -> (AuthService, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().await.unwrap();
        (AuthService::new(store, dir.path()).await.unwrap(), dir)
    }

    #[tokio::test]
    async fn setup_requires_the_token_from_disk() {
        let (auth, dir) = service().await;
        assert!(auth.needs_setup().await.unwrap());
        let path = dir.path().join("setup-token");
        assert_eq!(auth.pending_setup_token_path(), Some(path.as_path()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let err = auth
            .setup(IP, "wrong", "admin", "correct horse battery")
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unauthorized);

        let token = auth.read_setup_token();
        let err = auth.setup(IP, &token, "admin", "short").await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidRequest);

        let session = auth
            .setup(IP, &token, "admin", "correct horse battery")
            .await
            .unwrap();
        assert!(auth.session_valid(&session.token).await.unwrap());
        assert!(!path.exists(), "setup token file is removed");
        assert!(!auth.needs_setup().await.unwrap());

        // The token cannot be reused.
        let err = auth
            .setup(IP, &token, "mallory", "correct horse battery")
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
    }

    #[tokio::test]
    async fn login_logout_and_password_change() {
        let (auth, _dir) = service().await;
        let token = auth.read_setup_token();
        auth.setup(IP, &token, "admin", "correct horse battery")
            .await
            .unwrap();

        let err = auth
            .login(IP, "admin", "wrong password!")
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unauthorized);
        let err = auth
            .login(IP, "nobody", "correct horse battery")
            .await
            .unwrap_err();
        assert_eq!(err.message, "invalid username or password");

        let s1 = auth
            .login(IP, "admin", "correct horse battery")
            .await
            .unwrap();
        assert!(auth.session_valid(&s1.token).await.unwrap());
        auth.logout(&s1.token).await.unwrap();
        assert!(!auth.session_valid(&s1.token).await.unwrap());

        let s2 = auth
            .login(IP, "admin", "correct horse battery")
            .await
            .unwrap();
        let err = auth
            .change_password(IP, "wrong", "another long password")
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
        let s3 = auth
            .change_password(IP, "correct horse battery", "another long password")
            .await
            .unwrap();
        assert!(
            !auth.session_valid(&s2.token).await.unwrap(),
            "old sessions end"
        );
        assert!(auth.session_valid(&s3.token).await.unwrap());
        auth.login(IP, "admin", "another long password")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn failed_logins_are_rate_limited_per_client() {
        let (auth, _dir) = service().await;
        let token = auth.read_setup_token();
        auth.setup(IP, &token, "admin", "correct horse battery")
            .await
            .unwrap();
        for _ in 0..LOGIN_MAX_ATTEMPTS {
            auth.login(IP, "admin", "wrong password!")
                .await
                .unwrap_err();
        }
        // Even the correct password is refused while locked out.
        let err = auth
            .login(IP, "admin", "correct horse battery")
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::RateLimited);
        // Other clients are unaffected.
        let other: IpAddr = "198.51.100.7".parse().unwrap();
        auth.login(other, "admin", "correct horse battery")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn parallel_guesses_cannot_exceed_the_limit() {
        let (auth, _dir) = service().await;
        let token = auth.read_setup_token();
        auth.setup(IP, &token, "admin", "correct horse battery")
            .await
            .unwrap();
        let attempts: Vec<_> = (0..20)
            .map(|i| {
                let auth = auth.clone();
                tokio::spawn(async move {
                    auth.login(IP, "admin", &format!("wrong password {i}"))
                        .await
                })
            })
            .collect();
        let mut checked = 0;
        for attempt in attempts {
            match attempt.await.unwrap().unwrap_err().kind {
                ErrorKind::Unauthorized => checked += 1,
                ErrorKind::RateLimited => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(
            checked, LOGIN_MAX_ATTEMPTS,
            "only the budget reaches the password check"
        );
    }

    #[test]
    fn ipv6_clients_share_their_64_and_the_table_is_bounded() {
        let limiter = LoginLimiter::default();
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        let other: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        for i in 0..LOGIN_MAX_ATTEMPTS {
            limiter.attempt(if i % 2 == 0 { a } else { b }).unwrap();
        }
        assert!(limiter.attempt(a).is_err());
        assert!(limiter.attempt(b).is_err(), "same /64");
        limiter.attempt(other).unwrap();
        assert_eq!(
            limiter_key("::ffff:192.0.2.1".parse().unwrap()),
            "192.0.2.1".parse::<IpAddr>().unwrap()
        );

        let full = LoginLimiter::default();
        for i in 0..LIMITER_MAX_ENTRIES as u32 {
            full.attempt(IpAddr::V4(std::net::Ipv4Addr::from(i)))
                .unwrap();
        }
        let err = full.attempt("198.51.100.1".parse().unwrap()).unwrap_err();
        assert_eq!(err.kind, ErrorKind::RateLimited, "fails closed when full");
        // Known clients keep working.
        full.attempt(IpAddr::V4(std::net::Ipv4Addr::from(0)))
            .unwrap();
    }

    #[tokio::test]
    async fn api_tokens() {
        let (auth, _dir) = service().await;
        let (token, secret) = auth.create_api_token("  sonarr ").await.unwrap();
        assert_eq!(token.name, "sonarr");
        assert!(secret.starts_with(API_TOKEN_PREFIX));
        assert!(auth.api_token_valid(&secret).await.unwrap());
        assert!(!auth.api_token_valid("dkt_bogus").await.unwrap());
        assert!(!auth.api_token_valid("").await.unwrap());
        assert!(auth.create_api_token("").await.is_err());
        assert!(auth.create_api_token(&"x".repeat(65)).await.is_err());

        auth.revoke_api_token(token.id).await.unwrap();
        assert!(!auth.api_token_valid(&secret).await.unwrap());
        assert_eq!(
            auth.revoke_api_token(token.id).await.unwrap_err().kind,
            ErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn restart_with_existing_admin_needs_no_setup() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db.sqlite");
        let store = Store::open(&db).await.unwrap();
        let auth = AuthService::new(store.clone(), dir.path()).await.unwrap();
        let token = auth.read_setup_token();
        auth.setup(IP, &token, "admin", "correct horse battery")
            .await
            .unwrap();
        store.close().await;

        let store = Store::open(&db).await.unwrap();
        let auth = AuthService::new(store, dir.path()).await.unwrap();
        assert!(auth.pending_setup_token_path().is_none());
        assert!(!dir.path().join("setup-token").exists());
    }
}
