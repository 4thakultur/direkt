//! Runtime settings edited through the web UI and API.
//!
//! Currently this is the 1fichier API key. Its source is either the
//! `DIREKT_ONEFICHIER_API_KEY` environment variable, which wins and makes the
//! setting read-only, or the value saved through the settings page. Changes
//! apply immediately: the provider reads the key from a shared [`SecretCell`]
//! on every attempt. The key is never returned; callers only ever see a mask.

use crate::config::{Secret, validate_onefichier_key};
use crate::error::{AppError, AppResult};
use crate::providers::{AccountView, OneFichierAccount, SignInOutcome};
use crate::store::Store;
use chrono::Utc;
use serde::Serialize;
use std::sync::{Arc, RwLock};

const ONEFICHIER_KEY: &str = "onefichier.api_key";

/// What is shown instead of a configured secret.
pub const SECRET_MASK: &str = "************************";

/// A secret shared between the settings service and the component using it.
#[derive(Clone, Default)]
pub struct SecretCell(Arc<RwLock<Option<Secret>>>);

impl SecretCell {
    pub fn new(value: Option<Secret>) -> Self {
        Self(Arc::new(RwLock::new(value)))
    }

    pub fn get(&self) -> Option<Secret> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set(&self, value: Option<Secret>) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = value;
    }
}

impl std::fmt::Debug for SecretCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretCell([redacted])")
    }
}

/// Where the active 1fichier API key comes from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum KeySource {
    /// Not configured.
    #[default]
    None,
    /// Saved through the settings page or API.
    Settings,
    /// The `DIREKT_ONEFICHIER_API_KEY` environment variable (read-only here).
    Environment,
}

/// The 1fichier API key as shown to users: never the key itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct OneFichierSettings {
    /// The signed-in account, used in preference to the API key.
    pub account: AccountView,
    /// Whether an API key is configured.
    pub configured: bool,
    /// `SECRET_MASK` when configured, otherwise absent.
    pub api_key: Option<String>,
    pub source: KeySource,
    /// False when the key comes from the environment.
    pub editable: bool,
}

#[derive(Clone)]
pub struct SettingsService {
    store: Store,
    onefichier_key: SecretCell,
    env_locked: bool,
    onefichier_account: Arc<OneFichierAccount>,
}

impl SettingsService {
    /// Load settings. `env_key` is the key from the environment, if any; it
    /// takes precedence over the stored one.
    pub async fn load(
        store: Store,
        env_key: Option<Secret>,
        onefichier_account: Arc<OneFichierAccount>,
    ) -> AppResult<Self> {
        let env_locked = env_key.is_some();
        let key = match env_key {
            Some(key) => Some(key),
            None => store.get_setting(ONEFICHIER_KEY).await?.map(Secret::new),
        };
        Ok(Self {
            store,
            onefichier_key: SecretCell::new(key),
            env_locked,
            onefichier_account,
        })
    }

    /// The cell the 1fichier provider reads its key from.
    pub fn onefichier_key(&self) -> SecretCell {
        self.onefichier_key.clone()
    }

    pub fn onefichier(&self) -> OneFichierSettings {
        let configured = self.onefichier_key.get().is_some();
        OneFichierSettings {
            account: self.onefichier_account.view(),
            configured,
            api_key: configured.then(|| SECRET_MASK.to_owned()),
            source: match (configured, self.env_locked) {
                (_, true) => KeySource::Environment,
                (true, false) => KeySource::Settings,
                (false, false) => KeySource::None,
            },
            editable: !self.env_locked,
        }
    }

    pub async fn set_onefichier_key(&self, key: &str) -> AppResult<OneFichierSettings> {
        self.ensure_editable()?;
        let key = key.trim();
        validate_onefichier_key(key).map_err(AppError::invalid)?;
        self.store
            .set_setting(ONEFICHIER_KEY, key, Utc::now())
            .await?;
        self.onefichier_key.set(Some(Secret::new(key)));
        tracing::info!("1fichier API key updated");
        Ok(self.onefichier())
    }

    pub async fn clear_onefichier_key(&self) -> AppResult<OneFichierSettings> {
        self.ensure_editable()?;
        self.store.delete_setting(ONEFICHIER_KEY).await?;
        self.onefichier_key.set(None);
        tracing::info!("1fichier API key removed");
        Ok(self.onefichier())
    }

    /// Sign in to 1fichier. Returns whether a two-factor code is needed to
    /// complete the sign-in.
    pub async fn sign_in_onefichier(
        &self,
        email: &str,
        password: &str,
        code: Option<&str>,
    ) -> AppResult<bool> {
        let outcome = self
            .onefichier_account
            .sign_in(email, password, code)
            .await?;
        Ok(matches!(outcome, SignInOutcome::TwoFactorRequired))
    }

    pub async fn sign_out_onefichier(&self) -> AppResult<OneFichierSettings> {
        self.onefichier_account.sign_out().await?;
        Ok(self.onefichier())
    }

    fn ensure_editable(&self) -> AppResult<()> {
        if self.env_locked {
            return Err(AppError::conflict(format!(
                "the 1fichier API key is set by the {} environment variable; change it there",
                crate::config::ONEFICHIER_API_KEY_ENV
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    const KEY: &str = "AbCdEfGhIjKlMnOpQrStUvWxYz012345";

    async fn account(store: &Store) -> Arc<OneFichierAccount> {
        Arc::new(
            OneFichierAccount::load(
                store.clone(),
                crate::providers::Website::new(
                    crate::http_client::build_no_redirect_client(
                        Default::default(),
                        crate::providers::BROWSER_USER_AGENT,
                    )
                    .unwrap(),
                    crate::providers::Throttle::new(std::time::Duration::ZERO),
                ),
            )
            .await
            .unwrap(),
        )
    }

    async fn load(store: &Store, env: Option<Secret>) -> SettingsService {
        SettingsService::load(store.clone(), env, account(store).await)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn key_is_stored_masked_and_applied_live() {
        let store = Store::open_in_memory().await.unwrap();
        let settings = load(&store, None).await;
        let cell = settings.onefichier_key();
        assert_eq!(settings.onefichier().source, KeySource::None);
        assert!(cell.get().is_none());

        let shown = settings
            .set_onefichier_key(&format!("  {KEY}\n"))
            .await
            .unwrap();
        assert_eq!(shown.api_key.as_deref(), Some(SECRET_MASK));
        assert_eq!(shown.source, KeySource::Settings);
        assert_eq!(
            cell.get().unwrap().expose(),
            KEY,
            "provider sees it immediately"
        );
        assert!(!format!("{shown:?}{cell:?}").contains(KEY));

        // Survives a restart.
        let reloaded = load(&store, None).await;
        assert_eq!(reloaded.onefichier_key().get().unwrap().expose(), KEY);

        let err = settings.set_onefichier_key("not a key").await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
        assert_eq!(
            cell.get().unwrap().expose(),
            KEY,
            "invalid input changes nothing"
        );

        settings.clear_onefichier_key().await.unwrap();
        assert!(cell.get().is_none());
        assert!(load(&store, None).await.onefichier_key().get().is_none());
    }

    #[tokio::test]
    async fn environment_key_wins_and_locks_the_setting() {
        let store = Store::open_in_memory().await.unwrap();
        store
            .set_setting(ONEFICHIER_KEY, "StoredKey_0123456789abcdef", Utc::now())
            .await
            .unwrap();
        let settings = load(&store, Some(Secret::new(KEY))).await;
        assert_eq!(settings.onefichier_key().get().unwrap().expose(), KEY);
        let shown = settings.onefichier();
        assert_eq!(shown.source, KeySource::Environment);
        assert!(!shown.editable);
        let err = settings.set_onefichier_key(KEY).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
        assert!(settings.clear_onefichier_key().await.is_err());
    }
}
