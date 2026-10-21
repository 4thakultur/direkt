//! Authentication: accounts, browser sessions, optional two-factor sign-in
//! with an authenticator app, and revocable API tokens. Account management by
//! administrators is in [`accounts`].
//!
//! Two-factor sign-in: with it on, a correct password does not start a
//! session. It starts a short-lived pending sign-in, which a code from the
//! authenticator app (or a single-use recovery code) completes. API tokens
//! are not affected: they are created from a signed-in session.
//!
//! First-run setup: while no administrator exists, a random one-time setup
//! token is written to `<data_dir>/setup-token` (mode 0600). Creating the
//! first administrator requires this token, so nobody who merely reaches an
//! exposed instance can claim it first. The token is never logged; only its
//! file path is. Every other account is created by an administrator, with a
//! temporary password that must be replaced at the first sign-in.

pub mod accounts;

use crate::error::{AppError, AppResult, ErrorKind};
use crate::store::{ApiToken, NewUser, Store};
use crate::totp;
use crate::users::{Permission, Role, User};
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
pub const MIN_PASSWORD_CHARS: usize = 12;
/// From this length a password counts as a passphrase and needs no mix of
/// character kinds.
pub const PASSPHRASE_CHARS: usize = 20;
/// Different characters a password must use, so `aaaaaaaaaaaa…` is refused.
const MIN_DISTINCT_CHARS: usize = 6;
/// Argon2 cost grows with input; cap it so login cannot be used for DoS.
const MAX_PASSWORD_BYTES: usize = 1024;
const MAX_USERNAME_CHARS: usize = 64;
const MAX_TOKEN_NAME_CHARS: usize = 64;

/// Unsuccessful attempts allowed per client per window.
const LOGIN_MAX_ATTEMPTS: u32 = 5;
const LOGIN_WINDOW: Duration = Duration::from_secs(15 * 60);
const LIMITER_MAX_ENTRIES: usize = 10_000;

/// How long a password-checked sign-in waits for its two-factor code.
pub const PENDING_LOGIN_TTL: Duration = Duration::from_secs(5 * 60);
/// Codes tried against one pending sign-in before it is dropped.
const PENDING_LOGIN_MAX_ATTEMPTS: u32 = 5;
const PENDING_LOGINS_MAX: usize = 64;
/// Wrong two-factor codes allowed from all clients together per window, so
/// someone who has the password cannot spread guesses over many addresses.
const CODE_GLOBAL_MAX_ATTEMPTS: u32 = 20;
/// How long a started authenticator setup stays valid.
const ENROLLMENT_TTL: Duration = Duration::from_secs(15 * 60);
const ENROLLMENTS_MAX: usize = 64;
const RECOVERY_CODES: usize = 10;
/// Base32 characters per recovery code (48 random bits).
const RECOVERY_CODE_CHARS: usize = 10;
const TOTP_ISSUER: &str = "Direkt";

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
    /// Wrong two-factor codes from every client together.
    code_limiter: LoginLimiter,
    /// Sign-ins waiting for a two-factor code, by token hash.
    pending_logins: Mutex<HashMap<String, PendingLogin>>,
    /// Authenticator setups waiting for their first code, by account.
    enrollments: Mutex<HashMap<i64, (String, Instant)>>,
}

struct PendingLogin {
    user_id: i64,
    started: Instant,
    attempts: u32,
}

/// The result of a correct username and password.
#[derive(Debug)]
pub enum LoginOutcome {
    Session(Session),
    /// Two-factor authentication is on: complete the sign-in with
    /// [`AuthService::complete_login`] and this token.
    CodeRequired(String),
}

impl LoginOutcome {
    #[cfg(test)]
    pub(crate) fn expect_session(self) -> Session {
        match self {
            Self::Session(session) => session,
            Self::CodeRequired(_) => panic!("a two-factor code is required"),
        }
    }
}

/// Two-factor authentication as the settings page shows it.
#[derive(Debug, Clone, Default)]
pub struct TwoFactorStatus {
    pub enabled: bool,
    /// An administrator requires it: it cannot be turned off.
    pub required: bool,
    pub recovery_codes_left: u32,
    /// A started setup, waiting for the first code.
    pub enrollment: Option<Enrollment>,
}

/// What the authenticator app needs: the QR code's link, and the secret for
/// typing in by hand.
#[derive(Debug, Clone)]
pub struct Enrollment {
    pub secret: String,
    pub uri: String,
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
                code_limiter: LoginLimiter::new(CODE_GLOBAL_MAX_ATTEMPTS),
                pending_logins: Mutex::new(HashMap::new()),
                enrollments: Mutex::new(HashMap::new()),
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

    /// No enabled administrator exists: first-run setup creates one.
    pub async fn needs_setup(&self) -> AppResult<bool> {
        Ok(!self.inner.store.has_admin().await?)
    }

    /// Create the first administrator account and log in.
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
        check_password_strength(password, username)?;

        let password_hash = hash_password(password.to_owned()).await?;
        let user = self
            .inner
            .store
            .create_first_admin(
                &NewUser {
                    username: username.to_owned(),
                    password_hash,
                    role: Role::Admin,
                    permissions: Vec::new(),
                    require_two_factor: false,
                    temporary_until: None,
                },
                Utc::now(),
            )
            .await?;
        *self.setup_slot() = None;
        self.remove_setup_token_file();
        tracing::info!(username, "administrator account created");
        self.inner.store.record_login(user.id, Utc::now()).await?;
        self.new_session(user.id).await
    }

    pub async fn login(
        &self,
        client: IpAddr,
        username: &str,
        password: &str,
    ) -> AppResult<LoginOutcome> {
        // Counted before verifying, so parallel guesses cannot all slip in
        // before the first failure is recorded.
        self.inner.limiter.attempt(client)?;
        if password.len() > MAX_PASSWORD_BYTES || username.len() > MAX_USERNAME_CHARS * 4 {
            return Err(invalid_credentials());
        }
        let record = self.inner.store.user_by_name(username.trim()).await?;
        let hash = match &record {
            Some(r) => r.password_hash.clone(),
            None => self.inner.dummy_hash.clone(),
        };
        // Always verify a hash so the response time does not reveal whether
        // the username exists.
        let password_ok = verify_password(password.to_owned(), hash).await?;
        let user = match record {
            Some(r) if password_ok => r.user,
            _ => {
                tracing::warn!(%client, "failed login attempt");
                return Err(invalid_credentials());
            }
        };
        sign_in_allowed(&user)?;
        if user.two_factor_enabled {
            // The attempt budget stays spent until the code is right too, so
            // a known password does not buy fresh code guesses.
            tracing::info!(%client, user = %user.username, "password accepted; waiting for two-factor code");
            return Ok(LoginOutcome::CodeRequired(
                self.start_pending_login(user.id)?,
            ));
        }
        self.inner.limiter.succeeded(client);
        let _ = self.inner.store.prune_sessions(Utc::now()).await;
        self.inner.store.record_login(user.id, Utc::now()).await?;
        tracing::info!(%client, user = %user.username, "logged in");
        Ok(LoginOutcome::Session(self.new_session(user.id).await?))
    }

    /// Whether a pending sign-in can still be completed.
    pub fn login_pending(&self, pending_token: &str) -> bool {
        self.pending()
            .get(&sha256_hex(pending_token))
            .is_some_and(|p| p.started.elapsed() < PENDING_LOGIN_TTL)
    }

    /// Complete a sign-in with an authenticator or recovery code.
    pub async fn complete_login(
        &self,
        client: IpAddr,
        pending_token: &str,
        code: &str,
    ) -> AppResult<Session> {
        self.inner.limiter.attempt(client)?;
        let key = sha256_hex(pending_token);
        let user_id = {
            let mut pending = self.pending();
            let Some(login) = pending.get_mut(&key) else {
                return Err(login_expired());
            };
            if login.started.elapsed() >= PENDING_LOGIN_TTL
                || login.attempts >= PENDING_LOGIN_MAX_ATTEMPTS
            {
                pending.remove(&key);
                return Err(login_expired());
            }
            login.attempts += 1;
            login.user_id
        };
        self.inner.code_limiter.attempt(GLOBAL)?;
        let user = match self.inner.store.user(user_id).await? {
            Some(r) => r.user,
            None => {
                self.pending().remove(&key);
                return Err(login_expired());
            }
        };
        if let Err(e) = sign_in_allowed(&user) {
            self.pending().remove(&key);
            return Err(e);
        }
        let Some(secret) = self.inner.store.totp_secret(user_id).await? else {
            // Turned off meanwhile; start over rather than skip the check.
            self.pending().remove(&key);
            return Err(login_expired());
        };
        if !self.check_code(user_id, &secret, code).await? {
            tracing::warn!(%client, user = %user.username, "wrong two-factor code");
            let mut pending = self.pending();
            if pending
                .get(&key)
                .is_some_and(|l| l.attempts >= PENDING_LOGIN_MAX_ATTEMPTS)
            {
                pending.remove(&key);
                return Err(AppError::unauthorized("too many wrong codes; log in again"));
            }
            return Err(AppError::unauthorized("invalid code"));
        }
        self.pending().remove(&key);
        self.inner.limiter.succeeded(client);
        self.inner.code_limiter.succeeded(GLOBAL);
        let _ = self.inner.store.prune_sessions(Utc::now()).await;
        self.inner.store.record_login(user_id, Utc::now()).await?;
        tracing::info!(%client, user = %user.username, "logged in with two-factor code");
        self.new_session(user_id).await
    }

    /// The account a browser session belongs to, unless it ended or the
    /// account is disabled.
    pub async fn session_user(&self, session_token: &str) -> AppResult<Option<User>> {
        let Some(id) = self
            .inner
            .store
            .session_user(&sha256_hex(session_token), Utc::now())
            .await?
        else {
            return Ok(None);
        };
        self.enabled_user(id).await
    }

    pub async fn session_valid(&self, session_token: &str) -> AppResult<bool> {
        Ok(self.session_user(session_token).await?.is_some())
    }

    /// The account an API token belongs to, recording the token's use.
    pub async fn token_user(&self, token: &str) -> AppResult<Option<User>> {
        if !token.starts_with(API_TOKEN_PREFIX) {
            return Ok(None);
        }
        let Some(id) = self
            .inner
            .store
            .use_api_token(&sha256_hex(token), Utc::now())
            .await?
        else {
            return Ok(None);
        };
        self.enabled_user(id).await
    }

    async fn enabled_user(&self, id: i64) -> AppResult<Option<User>> {
        Ok(self
            .inner
            .store
            .user(id)
            .await?
            .map(|r| r.user)
            .filter(|u| !u.disabled))
    }

    pub async fn two_factor_status(&self, user: &User) -> AppResult<TwoFactorStatus> {
        let enabled = self.inner.store.totp_secret(user.id).await?.is_some();
        Ok(TwoFactorStatus {
            enabled,
            required: user.require_two_factor,
            recovery_codes_left: if enabled {
                self.inner.store.recovery_codes_left(user.id).await?
            } else {
                0
            },
            enrollment: if enabled { None } else { self.enrollment(user) },
        })
    }

    /// Start setting up an authenticator app: check the password and make a
    /// new secret. Nothing changes until [`Self::finish_two_factor_setup`].
    pub async fn start_two_factor_setup(
        &self,
        client: IpAddr,
        user: &User,
        password: &str,
    ) -> AppResult<Enrollment> {
        self.confirm_password(client, user.id, password).await?;
        if self.inner.store.totp_secret(user.id).await?.is_some() {
            return Err(AppError::conflict(
                "two-factor authentication is already on",
            ));
        }
        self.new_enrollment(user)
    }

    /// The setup shown when an administrator requires two-factor
    /// authentication and the account has none yet. The account just signed
    /// in, so no password is asked; reloading keeps the same secret, so a
    /// code already scanned keeps working.
    pub async fn required_two_factor_setup(&self, user: &User) -> AppResult<Enrollment> {
        if self.inner.store.totp_secret(user.id).await?.is_some() {
            return Err(AppError::conflict(
                "two-factor authentication is already on",
            ));
        }
        match self.enrollment(user) {
            Some(enrollment) => Ok(enrollment),
            None => self.new_enrollment(user),
        }
    }

    fn new_enrollment(&self, user: &User) -> AppResult<Enrollment> {
        {
            let mut enrollments = self.enrollments();
            enrollments.retain(|_, (_, started)| started.elapsed() < ENROLLMENT_TTL);
            if enrollments.len() >= ENROLLMENTS_MAX && !enrollments.contains_key(&user.id) {
                return Err(AppError::new(
                    ErrorKind::RateLimited,
                    "too many two-factor setups in progress; try again later",
                ));
            }
            enrollments.insert(user.id, (totp::generate_secret(), Instant::now()));
        }
        self.enrollment(user)
            .ok_or_else(|| AppError::internal("two-factor setup was lost"))
    }

    pub fn cancel_two_factor_setup(&self, user_id: i64) {
        self.enrollments().remove(&user_id);
    }

    /// Turn on two-factor authentication once the app shows a matching code.
    /// Every other session of the account ends, so anyone else signed in as
    /// it must sign in again with a code. Returns the recovery codes, shown
    /// once.
    pub async fn finish_two_factor_setup(
        &self,
        client: IpAddr,
        user: &User,
        code: &str,
        current_session: &str,
    ) -> AppResult<Vec<String>> {
        self.inner.limiter.attempt(client)?;
        let secret = match self.enrollments().get(&user.id) {
            Some((secret, started)) if started.elapsed() < ENROLLMENT_TTL => secret.clone(),
            _ => {
                return Err(AppError::conflict(
                    "the setup expired; start again with your password",
                ));
            }
        };
        let key = totp::decode_base32(&secret).expect("generated secret is base32");
        let Some(step) = totp::verify(&key, code, Utc::now().timestamp()) else {
            return Err(AppError::invalid(
                "the code doesn't match; check that the phone's clock is set automatically",
            ));
        };
        self.inner.limiter.succeeded(client);
        let (codes, hashes) = new_recovery_codes();
        self.inner
            .store
            .enable_two_factor(user.id, &secret, step, &hashes, Utc::now())
            .await?;
        self.cancel_two_factor_setup(user.id);
        self.forget_pending_logins(user.id);
        self.inner
            .store
            .delete_user_sessions(user.id, Some(&sha256_hex(current_session)))
            .await?;
        tracing::info!(user = %user.username, "two-factor authentication turned on; other sessions ended");
        Ok(codes)
    }

    /// Turn off two-factor authentication. Needs the password and a code, and
    /// is refused while an administrator requires it.
    pub async fn disable_two_factor(
        &self,
        client: IpAddr,
        user: &User,
        password: &str,
        code: &str,
    ) -> AppResult<()> {
        if user.require_two_factor {
            return Err(AppError::new(
                ErrorKind::Forbidden,
                "an administrator requires two-factor authentication for your account",
            ));
        }
        self.confirm_password(client, user.id, password).await?;
        let Some(secret) = self.inner.store.totp_secret(user.id).await? else {
            return Err(AppError::conflict("two-factor authentication is off"));
        };
        self.inner.limiter.attempt(client)?;
        if !self.check_code(user.id, &secret, code).await? {
            return Err(AppError::invalid("invalid code"));
        }
        self.inner.limiter.succeeded(client);
        self.inner.store.disable_two_factor(user.id).await?;
        self.forget_pending_logins(user.id);
        tracing::info!(user = %user.username, "two-factor authentication turned off");
        Ok(())
    }

    /// Replace the recovery codes. Needs the password.
    pub async fn new_recovery_codes(
        &self,
        client: IpAddr,
        user: &User,
        password: &str,
    ) -> AppResult<Vec<String>> {
        self.confirm_password(client, user.id, password).await?;
        if self.inner.store.totp_secret(user.id).await?.is_none() {
            return Err(AppError::conflict("two-factor authentication is off"));
        }
        let (codes, hashes) = new_recovery_codes();
        self.inner
            .store
            .set_recovery_codes(user.id, &hashes, Utc::now())
            .await?;
        tracing::info!(user = %user.username, "recovery codes replaced");
        Ok(codes)
    }

    /// Check an authenticator code (each usable once) or a recovery code
    /// (used up).
    async fn check_code(&self, user_id: i64, secret: &str, code: &str) -> AppResult<bool> {
        if totp::looks_like_code(code) {
            let key = totp::decode_base32(secret)
                .ok_or_else(|| AppError::internal("stored two-factor secret is invalid"))?;
            return match totp::verify(&key, code, Utc::now().timestamp()) {
                Some(step) if self.inner.store.use_totp_step(user_id, step).await? => Ok(true),
                Some(_) => Err(AppError::unauthorized(
                    "this code was already used; wait for the next one",
                )),
                None => Ok(false),
            };
        }
        let normalized = normalize_recovery_code(code);
        if normalized.len() != RECOVERY_CODE_CHARS {
            return Ok(false);
        }
        let used = self
            .inner
            .store
            .use_recovery_code(user_id, &sha256_hex(&normalized))
            .await?;
        if used {
            tracing::warn!(user_id, "recovery code used");
        }
        Ok(used)
    }

    /// Check an account's password, counting the attempt.
    async fn confirm_password(
        &self,
        client: IpAddr,
        user_id: i64,
        password: &str,
    ) -> AppResult<()> {
        self.inner.limiter.attempt(client)?;
        let record = self
            .inner
            .store
            .user(user_id)
            .await?
            .ok_or_else(|| AppError::not_found("account not found"))?;
        if password.len() > MAX_PASSWORD_BYTES
            || !verify_password(password.to_owned(), record.password_hash).await?
        {
            return Err(AppError::invalid("password is incorrect"));
        }
        self.inner.limiter.succeeded(client);
        Ok(())
    }

    fn enrollment(&self, user: &User) -> Option<Enrollment> {
        let secret = match self.enrollments().get(&user.id) {
            Some((secret, started)) if started.elapsed() < ENROLLMENT_TTL => secret.clone(),
            _ => return None,
        };
        Some(Enrollment {
            uri: totp::otpauth_uri(&secret, TOTP_ISSUER, &user.username),
            secret,
        })
    }

    fn start_pending_login(&self, user_id: i64) -> AppResult<String> {
        let mut pending = self.pending();
        pending.retain(|_, p| p.started.elapsed() < PENDING_LOGIN_TTL);
        if pending.len() >= PENDING_LOGINS_MAX {
            return Err(AppError::new(
                ErrorKind::RateLimited,
                "too many sign-ins in progress; try again later",
            ));
        }
        let token = random_hex(32);
        pending.insert(
            sha256_hex(&token),
            PendingLogin {
                user_id,
                started: Instant::now(),
                attempts: 0,
            },
        );
        Ok(token)
    }

    /// Drop an account's sign-ins that wait for a code, e.g. after its
    /// password or two-factor setting changed.
    fn forget_pending_logins(&self, user_id: i64) {
        self.pending().retain(|_, p| p.user_id != user_id);
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, PendingLogin>> {
        self.inner
            .pending_logins
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn enrollments(&self) -> std::sync::MutexGuard<'_, HashMap<i64, (String, Instant)>> {
        self.inner
            .enrollments
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    pub async fn logout(&self, session_token: &str) -> AppResult<()> {
        self.inner
            .store
            .delete_session(&sha256_hex(session_token))
            .await
    }

    /// Change the password, end every session of the account, and start a
    /// new one.
    pub async fn change_password(
        &self,
        client: IpAddr,
        user: &User,
        current: &str,
        new: &str,
    ) -> AppResult<Session> {
        self.confirm_password(client, user.id, current)
            .await
            .map_err(|e| match e.kind {
                ErrorKind::InvalidRequest => AppError::invalid("current password is incorrect"),
                _ => e,
            })?;
        if new == current {
            return Err(AppError::invalid(
                "the new password must be different from the current one",
            ));
        }
        self.replace_password(user, new).await
    }

    /// Replace a temporary password at the first sign-in. The account just
    /// signed in with it, so it is not asked again.
    pub async fn choose_password(&self, user: &User, new: &str) -> AppResult<Session> {
        if !user.must_change_password {
            return Err(AppError::conflict(
                "your password is not temporary; change it under Settings",
            ));
        }
        let record = self
            .inner
            .store
            .user(user.id)
            .await?
            .ok_or_else(|| AppError::not_found("account not found"))?;
        if new.len() <= MAX_PASSWORD_BYTES
            && verify_password(new.to_owned(), record.password_hash).await?
        {
            return Err(AppError::invalid(
                "choose a password of your own, not the temporary one",
            ));
        }
        self.replace_password(user, new).await
    }

    async fn replace_password(&self, user: &User, new: &str) -> AppResult<Session> {
        check_password_strength(new, &user.username)?;
        let hash = hash_password(new.to_owned()).await?;
        self.inner
            .store
            .set_password(user.id, &hash, None, None, Utc::now())
            .await?;
        self.forget_pending_logins(user.id);
        tracing::info!(user = %user.username, "password changed; the account's sessions ended");
        self.new_session(user.id).await
    }

    /// Create an API token for an account. The secret is returned once and
    /// never stored.
    pub async fn create_api_token(&self, user: &User, name: &str) -> AppResult<(ApiToken, String)> {
        user.require(Permission::ApiTokens)?;
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
            .insert_api_token(user.id, name, &sha256_hex(&secret), Utc::now())
            .await?;
        tracing::info!(token_id = %token.id, user = %user.username, name, "API token created");
        Ok((token, secret))
    }

    /// The account's own API tokens.
    pub async fn list_api_tokens(&self, user: &User) -> AppResult<Vec<ApiToken>> {
        self.inner.store.list_api_tokens(user.id).await
    }

    /// Revoke one of the account's own API tokens.
    pub async fn revoke_api_token(&self, user: &User, id: Uuid) -> AppResult<()> {
        if !self.inner.store.delete_api_token(user.id, id).await? {
            return Err(AppError::not_found("API token not found"));
        }
        tracing::info!(token_id = %id, user = %user.username, "API token revoked");
        Ok(())
    }

    async fn new_session(&self, user_id: i64) -> AppResult<Session> {
        let token = random_hex(32);
        let now = Utc::now();
        let expires_at = now + chrono::Duration::from_std(SESSION_TTL).expect("constant fits");
        self.inner
            .store
            .insert_session(&sha256_hex(&token), user_id, now, expires_at)
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

/// Refuse a disabled account, or a temporary password past its expiry.
/// Only checked once the password was right, so it reveals nothing to
/// someone guessing.
fn sign_in_allowed(user: &User) -> AppResult<()> {
    if user.disabled {
        return Err(AppError::unauthorized(
            "this account is disabled; ask an administrator",
        ));
    }
    if user.must_change_password
        && user
            .password_expires_at
            .is_some_and(|until| until <= Utc::now())
    {
        return Err(AppError::unauthorized(
            "this temporary password has expired; ask an administrator for a new one",
        ));
    }
    Ok(())
}

fn invalid_credentials() -> AppError {
    AppError::unauthorized("invalid username or password")
}

fn login_expired() -> AppError {
    AppError::unauthorized("the sign-in expired; log in again")
}

/// The key the global code limiter counts every client under.
const GLOBAL: IpAddr = IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);

/// New recovery codes, formatted for display, with their hashes for storage.
fn new_recovery_codes() -> (Vec<String>, Vec<String>) {
    (0..RECOVERY_CODES)
        .map(|_| {
            let mut bytes = [0u8; RECOVERY_CODE_CHARS * 5 / 8];
            getrandom::fill(&mut bytes).expect("operating system RNG is unavailable");
            let code = totp::encode_base32(&bytes).to_ascii_lowercase();
            let hash = sha256_hex(&code);
            let (a, b) = code.split_at(RECOVERY_CODE_CHARS / 2);
            (format!("{a}-{b}"), hash)
        })
        .unzip()
}

/// Lowercase, without the hyphen or spaces people may type.
fn normalize_recovery_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

pub(crate) fn validate_username(username: &str) -> AppResult<&str> {
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

/// The password rules, for forms to show.
pub const PASSWORD_RULES: &str = "At least 12 characters, mixing three of: lowercase letters, \
capital letters, digits and symbols. A passphrase of 20 characters or more needs no mix. It must \
not contain the username.";

/// Refuse weak passwords: too short, too uniform, containing the username,
/// or (unless long enough to be a passphrase) using fewer than three kinds
/// of characters.
pub fn check_password_strength(password: &str, username: &str) -> AppResult<()> {
    let chars = password.chars().count();
    if chars < MIN_PASSWORD_CHARS {
        return Err(AppError::invalid(format!(
            "password must be at least {MIN_PASSWORD_CHARS} characters"
        )));
    }
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(AppError::invalid(format!(
            "password must be at most {MAX_PASSWORD_BYTES} bytes"
        )));
    }
    let mut distinct: Vec<char> = password.chars().collect();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() < MIN_DISTINCT_CHARS {
        return Err(AppError::invalid(
            "password repeats too few different characters",
        ));
    }
    let username = username.trim().to_lowercase();
    if username.chars().count() >= 3 && password.to_lowercase().contains(&username) {
        return Err(AppError::invalid("password must not contain the username"));
    }
    let kinds = [
        password.chars().any(char::is_lowercase),
        password.chars().any(char::is_uppercase),
        password.chars().any(|c| c.is_ascii_digit()),
        password.chars().any(|c| !c.is_alphanumeric()),
    ]
    .into_iter()
    .filter(|&k| k)
    .count();
    if chars < PASSPHRASE_CHARS && kinds < 3 {
        return Err(AppError::invalid(format!(
            "password must mix three of: lowercase letters, capital letters, digits and \
             symbols, or be a passphrase of at least {PASSPHRASE_CHARS} characters"
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
struct LoginLimiter {
    max_attempts: u32,
    entries: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

impl Default for LoginLimiter {
    fn default() -> Self {
        Self::new(LOGIN_MAX_ATTEMPTS)
    }
}

impl LoginLimiter {
    fn new(max_attempts: u32) -> Self {
        Self {
            max_attempts,
            entries: Mutex::default(),
        }
    }

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
        if entry.0 >= self.max_attempts {
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
pub(crate) mod tests {
    use super::*;

    pub(crate) const IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 1));
    pub(crate) const PASSWORD: &str = "correct horse battery";

    pub(crate) async fn service() -> (AuthService, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().await.unwrap();
        (AuthService::new(store, dir.path()).await.unwrap(), dir)
    }

    /// Complete first-run setup; returns the administrator and a session.
    pub(crate) async fn with_admin(auth: &AuthService) -> (User, Session) {
        let token = auth.read_setup_token();
        let session = auth.setup(IP, &token, "admin", PASSWORD).await.unwrap();
        let admin = auth.session_user(&session.token).await.unwrap().unwrap();
        (admin, session)
    }

    async fn user(auth: &AuthService, session: &Session) -> User {
        auth.session_user(&session.token).await.unwrap().unwrap()
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
            .setup(IP, "wrong", "admin", PASSWORD)
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unauthorized);

        let token = auth.read_setup_token();
        let err = auth.setup(IP, &token, "admin", "short").await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidRequest);

        let session = auth.setup(IP, &token, "admin", PASSWORD).await.unwrap();
        let admin = user(&auth, &session).await;
        assert!(admin.is_admin() && !admin.needs_onboarding());
        assert!(admin.last_login_at.is_some());
        assert!(!path.exists(), "setup token file is removed");
        assert!(!auth.needs_setup().await.unwrap());

        // The token cannot be reused.
        let err = auth
            .setup(IP, &token, "mallory", PASSWORD)
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
    }

    #[test]
    fn password_strength() {
        let weak = |p: &str| check_password_strength(p, "alice").unwrap_err().message;
        assert!(weak("Sh0rt!").contains("at least 12"));
        assert!(weak("aaaaaaaaaaaaaaaaaaaaaaaa").contains("too few different"));
        assert!(weak("lowercaseonly").contains("mix three"));
        assert!(weak("lowercase12345").contains("mix three"));
        assert!(weak("Alice-Was-Here-2024").contains("username"));
        check_password_strength("Lowercase12345", "alice").unwrap();
        check_password_strength("ne-pas-oublier-42", "alice").unwrap();
        check_password_strength(PASSWORD, "alice").unwrap();
        check_password_strength("tres longue phrase secrete", "alice").unwrap();
        // Short usernames are not matched, or every password would contain one.
        check_password_strength("Bobsleigh-2024", "bo").unwrap();
    }

    #[tokio::test]
    async fn login_logout_and_password_change() {
        let (auth, _dir) = service().await;
        let (admin, _) = with_admin(&auth).await;

        let err = auth
            .login(IP, "admin", "wrong password!")
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unauthorized);
        let err = auth.login(IP, "nobody", PASSWORD).await.unwrap_err();
        assert_eq!(err.message, "invalid username or password");

        let s1 = auth
            .login(IP, "ADMIN", PASSWORD)
            .await
            .unwrap()
            .expect_session();
        assert!(
            auth.session_valid(&s1.token).await.unwrap(),
            "names ignore case"
        );
        auth.logout(&s1.token).await.unwrap();
        assert!(!auth.session_valid(&s1.token).await.unwrap());

        let s2 = auth
            .login(IP, "admin", PASSWORD)
            .await
            .unwrap()
            .expect_session();
        let err = auth
            .change_password(IP, &admin, "wrong", "another long password")
            .await
            .unwrap_err();
        assert_eq!(err.message, "current password is incorrect");
        let err = auth
            .change_password(IP, &admin, PASSWORD, "weakpassword")
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
        let err = auth
            .change_password(IP, &admin, PASSWORD, PASSWORD)
            .await
            .unwrap_err();
        assert!(err.message.contains("different"), "{}", err.message);
        let s3 = auth
            .change_password(IP, &admin, PASSWORD, "another long password")
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
        with_admin(&auth).await;
        for _ in 0..LOGIN_MAX_ATTEMPTS {
            auth.login(IP, "admin", "wrong password!")
                .await
                .unwrap_err();
        }
        // Even the correct password is refused while locked out.
        let err = auth.login(IP, "admin", PASSWORD).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::RateLimited);
        // Other clients are unaffected.
        let other: IpAddr = "198.51.100.7".parse().unwrap();
        auth.login(other, "admin", PASSWORD).await.unwrap();
    }

    #[tokio::test]
    async fn parallel_guesses_cannot_exceed_the_limit() {
        let (auth, _dir) = service().await;
        with_admin(&auth).await;
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

    /// The authenticator app's code, `offset` steps from now.
    pub(crate) fn app_code(secret: &str, offset: i64) -> String {
        let key = totp::decode_base32(secret).unwrap();
        totp::code_at(&key, totp::step_at(Utc::now().timestamp()) + offset)
    }

    /// An administrator with two-factor authentication on. Returns the
    /// secret and the recovery codes.
    async fn with_two_factor(auth: &AuthService) -> (String, Vec<String>) {
        let (admin, session) = with_admin(auth).await;
        let secret = auth
            .start_two_factor_setup(IP, &admin, PASSWORD)
            .await
            .unwrap()
            .secret;
        let codes = auth
            .finish_two_factor_setup(IP, &admin, &app_code(&secret, 0), &session.token)
            .await
            .unwrap();
        (secret, codes)
    }

    fn pending(outcome: LoginOutcome) -> String {
        match outcome {
            LoginOutcome::CodeRequired(token) => token,
            LoginOutcome::Session(_) => panic!("signed in without a code"),
        }
    }

    #[tokio::test]
    async fn two_factor_setup_needs_the_password_and_a_matching_code() {
        let (auth, _dir) = service().await;
        let (admin, current) = with_admin(&auth).await;
        let other = auth
            .login(IP, "admin", PASSWORD)
            .await
            .unwrap()
            .expect_session();

        let err = auth
            .start_two_factor_setup(IP, &admin, "wrong password!")
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
        let err = auth
            .finish_two_factor_setup(IP, &admin, "123456", &current.token)
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict, "not started");

        let enrollment = auth
            .start_two_factor_setup(IP, &admin, PASSWORD)
            .await
            .unwrap();
        assert!(
            enrollment
                .uri
                .starts_with("otpauth://totp/Direkt:admin?secret=")
        );
        let status = auth.two_factor_status(&admin).await.unwrap();
        assert!(!status.enabled && status.enrollment.is_some());

        let wrong = app_code(&enrollment.secret, 5);
        let err = auth
            .finish_two_factor_setup(IP, &admin, &wrong, &current.token)
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
        assert!(!auth.two_factor_status(&admin).await.unwrap().enabled);

        let codes = auth
            .finish_two_factor_setup(IP, &admin, &app_code(&enrollment.secret, 0), &current.token)
            .await
            .unwrap();
        assert_eq!(codes.len(), RECOVERY_CODES);
        assert!(codes.iter().all(|c| c.len() == RECOVERY_CODE_CHARS + 1));
        let status = auth.two_factor_status(&admin).await.unwrap();
        assert!(status.enabled && status.enrollment.is_none());
        assert_eq!(status.recovery_codes_left, RECOVERY_CODES as u32);
        assert!(auth.session_valid(&current.token).await.unwrap());
        assert!(
            !auth.session_valid(&other.token).await.unwrap(),
            "other sessions end"
        );
        let err = auth
            .start_two_factor_setup(IP, &admin, PASSWORD)
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
    }

    #[tokio::test]
    async fn sign_in_with_two_factor() {
        let (auth, _dir) = service().await;
        let (secret, recovery) = with_two_factor(&auth).await;

        let token = pending(auth.login(IP, "admin", PASSWORD).await.unwrap());
        assert!(auth.login_pending(&token));
        let err = auth
            .complete_login(IP, &token, &app_code(&secret, 7))
            .await
            .unwrap_err();
        assert_eq!(err.message, "invalid code");
        let err = auth
            .complete_login(IP, "forged", &app_code(&secret, 1))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unauthorized);

        // The setup code's step is used; the next step's code works once.
        let code = app_code(&secret, 1);
        let session = auth.complete_login(IP, &token, &code).await.unwrap();
        assert!(auth.session_valid(&session.token).await.unwrap());
        assert!(
            !auth.login_pending(&token),
            "a pending sign-in is single use"
        );

        let token = pending(auth.login(IP, "admin", PASSWORD).await.unwrap());
        let err = auth.complete_login(IP, &token, &code).await.unwrap_err();
        assert!(err.message.contains("already used"), "{}", err.message);

        // A recovery code works once, typed loosely.
        let loose = recovery[0].to_uppercase().replace('-', " ");
        auth.complete_login(IP, &token, &loose).await.unwrap();
        let token = pending(auth.login(IP, "admin", PASSWORD).await.unwrap());
        let err = auth
            .complete_login(IP, &token, &recovery[0])
            .await
            .unwrap_err();
        assert_eq!(err.message, "invalid code");
        let admin = user(&auth, &session).await;
        assert_eq!(
            auth.two_factor_status(&admin)
                .await
                .unwrap()
                .recovery_codes_left,
            RECOVERY_CODES as u32 - 1
        );
    }

    #[tokio::test]
    async fn wrong_codes_end_the_pending_sign_in() {
        let (auth, _dir) = service().await;
        let (secret, _) = with_two_factor(&auth).await;
        let token = pending(auth.login(IP, "admin", PASSWORD).await.unwrap());
        // Each attempt from its own address, so only the per-sign-in limit
        // applies.
        for i in 0..PENDING_LOGIN_MAX_ATTEMPTS {
            let client = IpAddr::V4(std::net::Ipv4Addr::new(198, 51, 100, i as u8));
            auth.complete_login(client, &token, "000000x")
                .await
                .unwrap_err();
        }
        assert!(!auth.login_pending(&token));
        let other = IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 1));
        let err = auth
            .complete_login(other, &token, &app_code(&secret, 1))
            .await
            .unwrap_err();
        assert_eq!(err.message, "the sign-in expired; log in again");
    }

    #[tokio::test]
    async fn password_and_codes_share_the_attempt_budget() {
        let (auth, _dir) = service().await;
        with_two_factor(&auth).await;
        // A correct password does not refill the budget for code guesses.
        let mut guesses = 0;
        loop {
            let token = match auth.login(IP, "admin", PASSWORD).await {
                Ok(outcome) => pending(outcome),
                Err(e) => {
                    assert_eq!(e.kind, ErrorKind::RateLimited);
                    break;
                }
            };
            match auth.complete_login(IP, &token, "999999x").await {
                Err(e) if e.kind == ErrorKind::RateLimited => break,
                Err(_) => guesses += 1,
                Ok(_) => panic!("wrong code accepted"),
            }
        }
        assert!(guesses < LOGIN_MAX_ATTEMPTS, "{guesses} guesses");
    }

    #[tokio::test]
    async fn turning_two_factor_off_needs_password_and_code() {
        let (auth, _dir) = service().await;
        let (secret, _) = with_two_factor(&auth).await;
        let admin = auth.inner.store.user(1).await.unwrap().unwrap().user;
        let err = auth
            .disable_two_factor(IP, &admin, PASSWORD, "123")
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
        let err = auth
            .disable_two_factor(IP, &admin, "wrong password!", &app_code(&secret, 1))
            .await
            .unwrap_err();
        assert_eq!(err.message, "password is incorrect");
        auth.disable_two_factor(IP, &admin, PASSWORD, &app_code(&secret, 1))
            .await
            .unwrap();
        assert!(!auth.two_factor_status(&admin).await.unwrap().enabled);
        auth.login(IP, "admin", PASSWORD)
            .await
            .unwrap()
            .expect_session();
    }

    #[tokio::test]
    async fn recovery_codes_can_be_replaced() {
        let (auth, _dir) = service().await;
        let (_, old) = with_two_factor(&auth).await;
        let admin = auth.inner.store.user(1).await.unwrap().unwrap().user;
        let new = auth.new_recovery_codes(IP, &admin, PASSWORD).await.unwrap();
        assert_eq!(new.len(), RECOVERY_CODES);
        let token = pending(auth.login(IP, "admin", PASSWORD).await.unwrap());
        assert!(auth.complete_login(IP, &token, &old[0]).await.is_err());
        auth.complete_login(IP, &token, &new[0]).await.unwrap();
    }

    #[tokio::test]
    async fn api_tokens() {
        let (auth, _dir) = service().await;
        let (admin, _) = with_admin(&auth).await;
        let (token, secret) = auth.create_api_token(&admin, "  sonarr ").await.unwrap();
        assert_eq!(token.name, "sonarr");
        assert!(secret.starts_with(API_TOKEN_PREFIX));
        let found = auth.token_user(&secret).await.unwrap().unwrap();
        assert_eq!(found.id, admin.id);
        assert!(auth.token_user("dkt_bogus").await.unwrap().is_none());
        assert!(auth.token_user("").await.unwrap().is_none());
        assert!(auth.create_api_token(&admin, "").await.is_err());
        assert!(
            auth.create_api_token(&admin, &"x".repeat(65))
                .await
                .is_err()
        );

        auth.revoke_api_token(&admin, token.id).await.unwrap();
        assert!(auth.token_user(&secret).await.unwrap().is_none());
        assert_eq!(
            auth.revoke_api_token(&admin, token.id)
                .await
                .unwrap_err()
                .kind,
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
        auth.setup(IP, &token, "admin", PASSWORD).await.unwrap();
        store.close().await;

        let store = Store::open(&db).await.unwrap();
        let auth = AuthService::new(store, dir.path()).await.unwrap();
        assert!(auth.pending_setup_token_path().is_none());
        assert!(!dir.path().join("setup-token").exists());
    }
}
