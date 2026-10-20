//! The signed-in 1fichier account: stored credentials and session, sign-in
//! (including two-factor authentication), and automatic re-sign-in.
//!
//! The email and password are stored in the settings table, as the user
//! chose, so Direkt can sign in again on its own when 1fichier ends the
//! session. The session cookie is stored as well, so that a restart doesn't
//! need a new sign-in. 1fichier limits sign-in attempts, so:
//! * only one sign-in runs at a time, and a worker that finds the session
//!   already renewed by another reuses it;
//! * once 1fichier rejects the credentials, no further automatic attempt is
//!   made until the user signs in again from the settings;
//! * accounts with two-factor authentication cannot be signed in again
//!   automatically. Their downloads wait for the user instead.

use super::api::{API_BASE, ApiClient};
use super::website::{
    BROWSER_USER_AGENT, CdnCredits, FileInfo, LinkOutcome, LoginOutcome, PendingTwoFactor, Website,
};
use super::{DEFAULT_MIN_INTERVAL, Throttle};
use crate::config::Secret;
use crate::error::{AppError, AppResult, ErrorKind, TransferError};
use crate::settings::SECRET_MASK;
use crate::store::Store;
use chrono::Utc;
use serde::Serialize;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use url::Url;

const KEY_EMAIL: &str = "onefichier.email";
const KEY_PASSWORD: &str = "onefichier.password";
const KEY_SESSION: &str = "onefichier.session";
const KEY_TWO_FACTOR: &str = "onefichier.two_factor";
const KEY_PREMIUM_UNTIL: &str = "onefichier.premium_until";
const KEY_CDN_BALANCE: &str = "onefichier.cdn.balance";
const KEY_CDN_IN_USE: &str = "onefichier.cdn.in_use";
const KEY_CDN_SOURCE: &str = "onefichier.cdn.source";
const KEY_CDN_CHECKED_AT: &str = "onefichier.cdn.checked_at";

/// Minimum spacing of manual CDN refreshes: `user/info.cgi` allows one call
/// per 5 minutes per account.
const CDN_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How long a started two-factor sign-in waits for its code.
const TWO_FACTOR_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// The 1fichier login form's limits.
const MAX_EMAIL_CHARS: usize = 255;
const MAX_PASSWORD_CHARS: usize = 128;

pub struct OneFichierAccount {
    store: Store,
    website: Website,
    api: ApiClient,
    cdn: RwLock<Option<Cdn>>,
    state: RwLock<Option<Account>>,
    /// Serializes sign-ins.
    sign_in_lock: tokio::sync::Mutex<()>,
    pending: Mutex<Option<Pending>>,
}

#[derive(Clone)]
struct Account {
    email: String,
    password: Secret,
    session: Option<Secret>,
    two_factor: bool,
    premium_until: Option<String>,
    /// Why downloads can't sign in by themselves, if they can't.
    attention: Option<String>,
    /// Incremented whenever the session changes.
    generation: u64,
}

struct Pending {
    email: String,
    password: Secret,
    step: PendingTwoFactor,
    started: Instant,
}

/// Where CDN credit figures were read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CdnSource {
    /// The signed-in account's settings page.
    Account,
    /// `user/info.cgi` with the API key.
    ApiKey,
}

impl CdnSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::ApiKey => "api_key",
        }
    }
}

#[derive(Clone)]
struct Cdn {
    credits: CdnCredits,
    source: CdnSource,
    checked_at: chrono::DateTime<Utc>,
}

/// CDN credits as shown to users.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct CdnView {
    /// Balance as reported by 1fichier (e.g. "12.35 GB"); absent when the
    /// account has no CDN credits.
    pub balance: Option<String>,
    /// Whether the account is set to spend CDN credits on its downloads.
    pub in_use: Option<bool>,
    pub source: CdnSource,
    pub checked_at: chrono::DateTime<Utc>,
}

/// The account as shown to users. The password is never included.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct AccountView {
    pub signed_in: bool,
    pub email: Option<String>,
    /// `SECRET_MASK` when a password is stored.
    pub password: Option<String>,
    /// End of the premium subscription (`YYYY-MM-DD`) as of the last sign-in.
    /// Absent if the account has no subscription.
    pub premium_until: Option<String>,
    pub two_factor: bool,
    /// A sign-in was started and is waiting for the two-factor code.
    pub two_factor_pending: bool,
    /// Set when downloads need the user to sign in again.
    pub attention: Option<String>,
}

#[derive(Debug)]
pub enum SignInOutcome {
    SignedIn,
    TwoFactorRequired,
}

impl OneFichierAccount {
    /// Load the account from the settings, using the network policy for all
    /// requests to 1fichier.
    pub async fn open(
        store: Store,
        policy: crate::net::NetworkPolicy,
    ) -> anyhow::Result<Arc<Self>> {
        let client = crate::http_client::build_no_redirect_client(policy, BROWSER_USER_AGENT)?;
        let website = Website::new(client, Throttle::new(DEFAULT_MIN_INTERVAL));
        let api_base = Url::parse(API_BASE).expect("constant URL");
        Ok(Arc::new(Self::load(store, website, api_base).await?))
    }

    /// `api_base` is used for reading CDN credits with an API key.
    /// Name and size of a file from its public page (no sign-in needed).
    pub(crate) async fn file_info(&self, file_url: &Url) -> Result<FileInfo, TransferError> {
        self.website.file_info(file_url).await
    }

    pub(crate) async fn load(store: Store, website: Website, api_base: Url) -> AppResult<Self> {
        let cdn = load_cdn(&store).await?;
        let api = ApiClient {
            client: website.client(),
            base: api_base,
            throttle: website.throttle(),
        };
        let email = store.get_setting(KEY_EMAIL).await?;
        let password = store.get_setting(KEY_PASSWORD).await?;
        let state = match (email, password) {
            (Some(email), Some(password)) => Some(Account {
                email,
                password: Secret::new(password),
                session: store.get_setting(KEY_SESSION).await?.map(Secret::new),
                two_factor: store.get_setting(KEY_TWO_FACTOR).await?.as_deref() == Some("1"),
                premium_until: store.get_setting(KEY_PREMIUM_UNTIL).await?,
                attention: None,
                generation: 0,
            }),
            _ => None,
        };
        Ok(Self {
            store,
            website,
            api,
            cdn: RwLock::new(cdn),
            state: RwLock::new(state),
            sign_in_lock: tokio::sync::Mutex::new(()),
            pending: Mutex::new(None),
        })
    }

    /// The request spacing shared by every 1fichier call (site and API).
    pub(crate) fn throttle(&self) -> Arc<Throttle> {
        self.website.throttle()
    }

    pub fn is_signed_in(&self) -> bool {
        self.current().is_some()
    }

    pub fn view(&self) -> AccountView {
        let pending = self.pending_email();
        match self.current() {
            Some(a) => AccountView {
                signed_in: true,
                email: Some(a.email),
                password: Some(SECRET_MASK.to_owned()),
                premium_until: a.premium_until,
                two_factor: a.two_factor,
                two_factor_pending: pending.is_some(),
                attention: a.attention,
            },
            None => AccountView {
                email: pending.clone(),
                two_factor_pending: pending.is_some(),
                ..Default::default()
            },
        }
    }

    /// Sign in from the settings. `password` may be [`SECRET_MASK`] to reuse
    /// the stored password for the same email. `code` completes a two-factor
    /// sign-in.
    pub async fn sign_in(
        &self,
        email: &str,
        password: &str,
        code: Option<&str>,
    ) -> AppResult<SignInOutcome> {
        let email = email.trim();
        if email.is_empty() || !email.contains('@') || email.chars().count() > MAX_EMAIL_CHARS {
            return Err(AppError::invalid(
                "enter the email address of your 1fichier account",
            ));
        }
        let code = code.map(str::trim).filter(|c| !c.is_empty());
        if let Some(code) = code
            && !(code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(AppError::invalid("the two-factor code has 6 digits"));
        }
        let _guard = self.sign_in_lock.lock().await;

        // Complete a pending two-factor sign-in for the same email.
        if let Some(code) = code
            && let Some(pending) = self.take_pending(email)
        {
            let session = self
                .website
                .submit_two_factor(&pending.step, code)
                .await
                .map_err(AppError::from)?;
            self.finish_sign_in(email, pending.password, session, true)
                .await?;
            return Ok(SignInOutcome::SignedIn);
        }

        let password = self.password_for(email, password)?;
        match self
            .website
            .login(email, password.expose())
            .await
            .map_err(AppError::from)?
        {
            LoginOutcome::Session(session) => {
                self.finish_sign_in(email, password, session, false).await?;
                Ok(SignInOutcome::SignedIn)
            }
            LoginOutcome::TwoFactor(step) => {
                if let Some(code) = code {
                    let session = self
                        .website
                        .submit_two_factor(&step, code)
                        .await
                        .map_err(AppError::from)?;
                    self.finish_sign_in(email, password, session, true).await?;
                    return Ok(SignInOutcome::SignedIn);
                }
                *self.pending_slot() = Some(Pending {
                    email: email.to_owned(),
                    password,
                    step,
                    started: Instant::now(),
                });
                tracing::info!("1fichier sign-in waiting for the two-factor code");
                Ok(SignInOutcome::TwoFactorRequired)
            }
        }
    }

    /// Forget the account (credentials and session).
    pub async fn sign_out(&self) -> AppResult<()> {
        let _guard = self.sign_in_lock.lock().await;
        for key in [
            KEY_EMAIL,
            KEY_PASSWORD,
            KEY_SESSION,
            KEY_TWO_FACTOR,
            KEY_PREMIUM_UNTIL,
        ] {
            self.store.delete_setting(key).await?;
        }
        *self.state_slot() = None;
        *self.pending_slot() = None;
        self.clear_cdn(CdnSource::Account).await?;
        tracing::info!("signed out of 1fichier");
        Ok(())
    }

    /// CDN credits as last read, if any.
    pub fn cdn_view(&self) -> Option<CdnView> {
        self.cdn
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|c| CdnView {
                balance: c.credits.balance.clone(),
                in_use: c.credits.in_use,
                source: c.source,
                checked_at: c.checked_at,
            })
    }

    /// Read CDN credits again: from the signed-in account, or else with
    /// `api_key`. Refused within 5 minutes of the last check, which is
    /// 1fichier's limit for `user/info.cgi`.
    pub async fn refresh_cdn(&self, api_key: Option<Secret>) -> AppResult<()> {
        if let Some(previous) = self.cdn_view() {
            let elapsed = (Utc::now() - previous.checked_at)
                .to_std()
                .unwrap_or_default();
            if elapsed < CDN_REFRESH_INTERVAL {
                let wait = CDN_REFRESH_INTERVAL - elapsed;
                return Err(AppError::new(
                    ErrorKind::RateLimited,
                    format!(
                        "1fichier allows checking credits every 5 minutes; try again in {} min",
                        wait.as_secs().div_ceil(60)
                    ),
                ));
            }
        }
        if self.is_signed_in() {
            return self
                .refresh_cdn_from_account()
                .await
                .map_err(AppError::from);
        }
        let api_key = api_key.ok_or_else(|| {
            AppError::conflict("sign in to 1fichier or add an API key to see CDN credits")
        })?;
        self.refresh_cdn_with_key(&api_key).await
    }

    /// Read CDN credits with an API key (after the key is saved).
    pub async fn refresh_cdn_with_key(&self, api_key: &Secret) -> AppResult<()> {
        let credits = self
            .api
            .cdn_credits(api_key)
            .await
            .map_err(AppError::from)?;
        self.store_cdn(credits, CdnSource::ApiKey).await
    }

    async fn refresh_cdn_from_account(&self) -> Result<(), TransferError> {
        let account = self.current().ok_or_else(not_signed_in)?;
        let session = match account.session {
            Some(session) => session,
            None => self.renew_session(account.generation).await?.0,
        };
        let credits = self.website.cdn_credits(&session).await?;
        self.store_cdn(credits, CdnSource::Account).await?;
        Ok(())
    }

    async fn store_cdn(&self, credits: CdnCredits, source: CdnSource) -> AppResult<()> {
        // Millisecond precision, as stored, so the time is the same after a restart.
        let now = chrono::DateTime::from_timestamp_millis(Utc::now().timestamp_millis())
            .expect("current time is in range");
        match &credits.balance {
            Some(balance) => {
                self.store
                    .set_setting(KEY_CDN_BALANCE, balance, now)
                    .await?
            }
            None => self.store.delete_setting(KEY_CDN_BALANCE).await?,
        }
        match credits.in_use {
            Some(in_use) => {
                self.store
                    .set_setting(KEY_CDN_IN_USE, if in_use { "1" } else { "0" }, now)
                    .await?
            }
            None => self.store.delete_setting(KEY_CDN_IN_USE).await?,
        }
        self.store
            .set_setting(KEY_CDN_SOURCE, source.as_str(), now)
            .await?;
        self.store
            .set_setting(KEY_CDN_CHECKED_AT, &now.timestamp_millis().to_string(), now)
            .await?;
        *self.cdn.write().unwrap_or_else(|e| e.into_inner()) = Some(Cdn {
            credits,
            source,
            checked_at: now,
        });
        Ok(())
    }

    /// Forget CDN figures that came from `source`.
    pub async fn clear_cdn(&self, source: CdnSource) -> AppResult<()> {
        if self.cdn_view().is_none_or(|c| c.source != source) {
            return Ok(());
        }
        for key in [
            KEY_CDN_BALANCE,
            KEY_CDN_IN_USE,
            KEY_CDN_SOURCE,
            KEY_CDN_CHECKED_AT,
        ] {
            self.store.delete_setting(key).await?;
        }
        *self.cdn.write().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(())
    }

    /// Get a direct download link for `file_url`, signing in again once if
    /// the session has ended.
    pub(crate) async fn direct_link(
        &self,
        file_url: &Url,
        password: Option<&str>,
    ) -> Result<Url, TransferError> {
        let account = self.current().ok_or_else(not_signed_in)?;
        let (session, generation) = match account.session {
            Some(session) => (session, account.generation),
            None => self.renew_session(account.generation).await?,
        };
        match self
            .website
            .direct_link(&session, file_url, password)
            .await?
        {
            LinkOutcome::Link(link) => Ok(link),
            LinkOutcome::SessionExpired => {
                tracing::info!("1fichier session ended; signing in again");
                let (session, _) = self.renew_session(generation).await?;
                match self
                    .website
                    .direct_link(&session, file_url, password)
                    .await?
                {
                    LinkOutcome::Link(link) => Ok(link),
                    LinkOutcome::SessionExpired => Err(TransferError::block(
                        ErrorKind::AuthenticationFailed,
                        "1fichier ended the session right after signing in; sign in again under Settings",
                    )),
                }
            }
        }
    }

    /// Sign in again with the stored credentials, unless another worker
    /// already did since `seen_generation`.
    async fn renew_session(&self, seen_generation: u64) -> Result<(Secret, u64), TransferError> {
        let _guard = self.sign_in_lock.lock().await;
        let account = self.current().ok_or_else(not_signed_in)?;
        if account.generation != seen_generation
            && let Some(session) = account.session
        {
            return Ok((session, account.generation));
        }
        if let Some(reason) = account.attention {
            return Err(TransferError::block(
                ErrorKind::AuthenticationFailed,
                reason,
            ));
        }
        if account.two_factor {
            return Err(self.needs_attention(
                "1fichier session ended; this account uses two-factor authentication, \
                 so sign in again under Settings, then retry",
            ));
        }
        match self
            .website
            .login(&account.email, account.password.expose())
            .await
        {
            Ok(LoginOutcome::Session(session)) => {
                let generation = self.store_session(&session).await?;
                tracing::info!("signed in to 1fichier again");
                Ok((session, generation))
            }
            Ok(LoginOutcome::TwoFactor(_)) => {
                let _ = self.store.set_setting(KEY_TWO_FACTOR, "1", Utc::now()).await;
                Err(self.needs_attention(
                    "1fichier now asks for a two-factor code; sign in again under Settings, then retry",
                ))
            }
            Err(e) if e.kind == ErrorKind::AuthenticationFailed => Err(self.needs_attention(
                "1fichier rejected the stored email or password; sign in again under Settings, then retry",
            )),
            Err(e) => Err(e),
        }
    }

    /// Record that automatic sign-in is impossible until the user acts.
    fn needs_attention(&self, reason: &str) -> TransferError {
        if let Some(account) = self.state_slot().as_mut() {
            account.attention = Some(reason.to_owned());
            account.session = None;
        }
        tracing::warn!(reason, "1fichier account needs attention");
        TransferError::block(ErrorKind::AuthenticationFailed, reason)
    }

    async fn store_session(&self, session: &Secret) -> Result<u64, TransferError> {
        self.store
            .set_setting(KEY_SESSION, session.expose(), Utc::now())
            .await?;
        let mut slot = self.state_slot();
        let account = slot.as_mut().ok_or_else(not_signed_in)?;
        account.session = Some(session.clone());
        account.generation += 1;
        Ok(account.generation)
    }

    async fn finish_sign_in(
        &self,
        email: &str,
        password: Secret,
        session: Secret,
        two_factor: bool,
    ) -> AppResult<()> {
        // The subscription end is informative; a failure to read it is not
        // a sign-in failure.
        let premium_until = self
            .website
            .premium_until(&session)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "could not read 1fichier subscription status");
                None
            });
        let now = Utc::now();
        self.store.set_setting(KEY_EMAIL, email, now).await?;
        self.store
            .set_setting(KEY_PASSWORD, password.expose(), now)
            .await?;
        self.store
            .set_setting(KEY_SESSION, session.expose(), now)
            .await?;
        self.store
            .set_setting(KEY_TWO_FACTOR, if two_factor { "1" } else { "0" }, now)
            .await?;
        match &premium_until {
            Some(date) => self.store.set_setting(KEY_PREMIUM_UNTIL, date, now).await?,
            None => self.store.delete_setting(KEY_PREMIUM_UNTIL).await?,
        }
        let session_for_cdn = session.clone();
        {
            let mut slot = self.state_slot();
            let generation = slot.as_ref().map_or(0, |a| a.generation + 1);
            *slot = Some(Account {
                email: email.to_owned(),
                password,
                session: Some(session),
                two_factor,
                premium_until,
                attention: None,
                generation,
            });
        }
        *self.pending_slot() = None;
        tracing::info!(two_factor, "signed in to 1fichier");
        // Informative, like the subscription end: don't fail the sign-in.
        match self.website.cdn_credits(&session_for_cdn).await {
            Ok(credits) => self.store_cdn(credits, CdnSource::Account).await?,
            Err(e) => tracing::warn!(error = %e, "could not read 1fichier CDN credits"),
        }
        Ok(())
    }

    /// The password to use: the typed one, or the stored one if the mask was
    /// submitted unchanged for the same account.
    fn password_for(&self, email: &str, typed: &str) -> AppResult<Secret> {
        if typed == SECRET_MASK {
            return self
                .current()
                .filter(|a| a.email.eq_ignore_ascii_case(email))
                .map(|a| a.password)
                .ok_or_else(|| AppError::invalid("enter your 1fichier password"));
        }
        if typed.is_empty() {
            return Err(AppError::invalid("enter your 1fichier password"));
        }
        if typed.chars().count() > MAX_PASSWORD_CHARS {
            return Err(AppError::invalid(format!(
                "1fichier passwords are at most {MAX_PASSWORD_CHARS} characters"
            )));
        }
        Ok(Secret::new(typed))
    }

    fn take_pending(&self, email: &str) -> Option<Pending> {
        let mut slot = self.pending_slot();
        match slot.take() {
            Some(p)
                if p.email.eq_ignore_ascii_case(email)
                    && p.started.elapsed() < TWO_FACTOR_TIMEOUT =>
            {
                Some(p)
            }
            _ => None,
        }
    }

    fn pending_email(&self) -> Option<String> {
        self.pending_slot()
            .as_ref()
            .filter(|p| p.started.elapsed() < TWO_FACTOR_TIMEOUT)
            .map(|p| p.email.clone())
    }

    fn current(&self) -> Option<Account> {
        self.state.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn state_slot(&self) -> std::sync::RwLockWriteGuard<'_, Option<Account>> {
        self.state.write().unwrap_or_else(|e| e.into_inner())
    }

    fn pending_slot(&self) -> std::sync::MutexGuard<'_, Option<Pending>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }
}

async fn load_cdn(store: &Store) -> AppResult<Option<Cdn>> {
    let source = match store.get_setting(KEY_CDN_SOURCE).await?.as_deref() {
        Some("account") => CdnSource::Account,
        Some("api_key") => CdnSource::ApiKey,
        _ => return Ok(None),
    };
    let checked_at = store
        .get_setting(KEY_CDN_CHECKED_AT)
        .await?
        .and_then(|ms| ms.parse::<i64>().ok())
        .and_then(chrono::DateTime::from_timestamp_millis);
    let Some(checked_at) = checked_at else {
        return Ok(None);
    };
    Ok(Some(Cdn {
        credits: CdnCredits {
            balance: store.get_setting(KEY_CDN_BALANCE).await?,
            in_use: store.get_setting(KEY_CDN_IN_USE).await?.map(|v| v == "1"),
        },
        source,
        checked_at,
    }))
}

fn not_signed_in() -> TransferError {
    TransferError::block(
        ErrorKind::AuthenticationFailed,
        "not signed in to 1fichier; sign in under Settings, then retry",
    )
}
