//! Runtime settings edited through the web UI and API.
//!
//! API keys: the 1fichier API key and the TMDB API key (artwork). Each comes either from an environment variable (`DIREKT_ONEFICHIER_API_KEY`,
//! `DIREKT_TMDB_API_KEY`), which wins and makes the setting read-only, or from
//! the value saved through the settings page. Changes apply immediately: the
//! component using a key reads it from a shared [`SecretCell`] on every
//! attempt. Keys are never returned; callers only ever see a mask.
//!
//! Media library folders ([`LibrarySettings`]): where movies, TV series and
//! unidentified videos are organized into. They come only from environment
//! variables set at launch (`DIREKT_MOVIES_DIR`, `DIREKT_TV_DIR`,
//! `DIREKT_UNSORTED_DIR`) and are read-only here: the web UI and API can show
//! them but never change where Direkt writes. The organizer reads them from
//! a shared [`LibraryCell`].
//!
//! Miscellaneous preferences ([`MiscSettings`]): the unit speeds are shown
//! in, an overall speed limit, how many downloads run at once (the
//! configuration file's value until changed here), whether the web UI asks
//! for confirmation before a download starts, and whether archives are
//! deleted once extracted (read by the organizer from a [`FlagCell`]). They
//! apply immediately.

use crate::config::{LibraryEnv, MAX_CONCURRENT_RANGE, Secret, validate_onefichier_key};
use crate::engine::Limits;
use crate::error::{AppError, AppResult};
use crate::media::library::LibraryPaths;
use crate::media::tmdb;
use crate::providers::{AccountView, CdnSource, CdnView, OneFichierAccount, SignInOutcome};
use crate::store::Store;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

const ONEFICHIER_KEY: &str = "onefichier.api_key";
const TMDB_KEY: &str = "tmdb.api_key";
const SPEED_UNIT: &str = "ui.speed_unit";
const CONFIRM_DOWNLOADS: &str = "ui.confirm_downloads";
const SPEED_LIMIT: &str = "downloads.speed_limit";
const MAX_CONCURRENT: &str = "downloads.max_concurrent";
const DELETE_ARCHIVES: &str = "media.delete_extracted_archives";

/// Lowest speed limit, in bytes per second (10 kB/s).
pub const MIN_SPEED_LIMIT: u64 = 10_000;
/// Highest speed limit, in bytes per second (1 TB/s): anything above is a typo.
const MAX_SPEED_LIMIT: u64 = 1_000_000_000_000;

/// What is shown instead of a configured secret.
pub const SECRET_MASK: &str = "************************";

/// A secret shared between the settings service and the component using it.
#[derive(Clone, Default)]
pub struct SecretCell(Arc<RwLock<Versioned>>);

#[derive(Default)]
struct Versioned {
    value: Option<Secret>,
    /// Bumped on every change, so users of the secret can tell it changed.
    version: u64,
}

impl SecretCell {
    pub fn new(value: Option<Secret>) -> Self {
        Self(Arc::new(RwLock::new(Versioned { value, version: 0 })))
    }

    pub fn get(&self) -> Option<Secret> {
        self.0
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .value
            .clone()
    }

    pub fn version(&self) -> u64 {
        self.0.read().unwrap_or_else(|e| e.into_inner()).version
    }

    pub(crate) fn set(&self, value: Option<Secret>) {
        let mut inner = self.0.write().unwrap_or_else(|e| e.into_inner());
        inner.value = value;
        inner.version += 1;
    }
}

impl std::fmt::Debug for SecretCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretCell([redacted])")
    }
}

/// Where an API key comes from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum KeySource {
    /// Not configured.
    #[default]
    None,
    /// Saved through the settings page or API.
    Settings,
    /// An environment variable (read-only here).
    Environment,
}

impl KeySource {
    fn of(configured: bool, env_locked: bool) -> Self {
        match (configured, env_locked) {
            (_, true) => Self::Environment,
            (true, false) => Self::Settings,
            (false, false) => Self::None,
        }
    }
}

/// The TMDB key used for artwork, as shown to users: never the key itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct TmdbSettings {
    /// Whether a key is configured.
    pub configured: bool,
    /// `SECRET_MASK` when configured, otherwise absent.
    pub api_key: Option<String>,
    pub source: KeySource,
    /// False when the key comes from the environment.
    pub editable: bool,
}

/// The 1fichier API key as shown to users: never the key itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct OneFichierSettings {
    /// The signed-in account, used in preference to the API key.
    pub account: AccountView,
    /// CDN credits as last read from 1fichier.
    pub cdn: Option<CdnView>,
    /// Whether an API key is configured.
    pub configured: bool,
    /// `SECRET_MASK` when configured, otherwise absent.
    pub api_key: Option<String>,
    pub source: KeySource,
    /// False when the key comes from the environment.
    pub editable: bool,
}

/// How download speeds are written in the web UI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SpeedUnit {
    /// Bytes in powers of 1024: KiB/s, MiB/s.
    #[default]
    Binary,
    /// Bytes in powers of 1000: kB/s, MB/s.
    Decimal,
    /// Bits in powers of 1000, as internet plans are sold: kbit/s, Mbit/s.
    Bits,
}

/// A unit the speed limit can be entered in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitUnit {
    /// Form value and label without "/s", e.g. `MiB`.
    pub id: &'static str,
    /// Bytes per second for one of this unit per second.
    pub bytes: u64,
}

const LIMIT_UNITS: [LimitUnit; 6] = [
    LimitUnit {
        id: "KiB",
        bytes: 1 << 10,
    },
    LimitUnit {
        id: "MiB",
        bytes: 1 << 20,
    },
    LimitUnit {
        id: "kB",
        bytes: 1_000,
    },
    LimitUnit {
        id: "MB",
        bytes: 1_000_000,
    },
    LimitUnit {
        id: "kbit",
        bytes: 125,
    },
    LimitUnit {
        id: "Mbit",
        bytes: 125_000,
    },
];

impl SpeedUnit {
    pub const ALL: [SpeedUnit; 3] = [SpeedUnit::Binary, SpeedUnit::Decimal, SpeedUnit::Bits];

    pub fn as_str(self) -> &'static str {
        match self {
            SpeedUnit::Binary => "binary",
            SpeedUnit::Decimal => "decimal",
            SpeedUnit::Bits => "bits",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|u| u.as_str() == value)
    }

    pub fn label(self) -> &'static str {
        match self {
            SpeedUnit::Binary => "MiB/s",
            SpeedUnit::Decimal => "MB/s",
            SpeedUnit::Bits => "Mbit/s",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            SpeedUnit::Binary => "Bytes, 1 MiB = 1024 KiB. Like Linux tools.",
            SpeedUnit::Decimal => "Bytes, 1 MB = 1000 kB. Like browsers and macOS.",
            SpeedUnit::Bits => "Bits, 1 Mbit = 1000 kbit. Like internet plans.",
        }
    }

    /// The small and large unit the speed limit is entered in.
    pub fn limit_units(self) -> [LimitUnit; 2] {
        let i = match self {
            SpeedUnit::Binary => 0,
            SpeedUnit::Decimal => 2,
            SpeedUnit::Bits => 4,
        };
        [LIMIT_UNITS[i], LIMIT_UNITS[i + 1]]
    }

    /// A speed such as `12.3 MiB/s`, `12.9 MB/s` or `103 Mbit/s`.
    pub fn format(self, bytes_per_sec: u64) -> String {
        let (value, step, units): (f64, f64, [&str; 5]) = match self {
            SpeedUnit::Binary => (
                bytes_per_sec as f64,
                1024.0,
                ["B/s", "KiB/s", "MiB/s", "GiB/s", "TiB/s"],
            ),
            SpeedUnit::Decimal => (
                bytes_per_sec as f64,
                1000.0,
                ["B/s", "kB/s", "MB/s", "GB/s", "TB/s"],
            ),
            SpeedUnit::Bits => (
                bytes_per_sec as f64 * 8.0,
                1000.0,
                ["bit/s", "kbit/s", "Mbit/s", "Gbit/s", "Tbit/s"],
            ),
        };
        let mut value = value;
        let mut unit = 0;
        while value >= step && unit < units.len() - 1 {
            value /= step;
            unit += 1;
        }
        match unit {
            0 => format!("{value:.0} {}", units[0]),
            // Three significant digits at most: 1.23, 12.3, 123.
            _ if value >= 100.0 => format!("{value:.0} {}", units[unit]),
            _ => format!("{value:.1} {}", units[unit]),
        }
    }

    /// The smallest speed at or above `bytes_per_sec` that reads round in
    /// this unit: a power of two for binary, 1, 2 or 5 followed by zeros
    /// otherwise.
    pub fn round_up(self, bytes_per_sec: u64) -> u64 {
        fn one_two_five(n: u64) -> u64 {
            let mut magnitude = 1u64;
            loop {
                for step in [1, 2, 5] {
                    let candidate = magnitude.saturating_mul(step);
                    if candidate >= n || candidate == u64::MAX {
                        return candidate;
                    }
                }
                magnitude = magnitude.saturating_mul(10);
            }
        }
        match self {
            SpeedUnit::Binary => bytes_per_sec.next_power_of_two(),
            SpeedUnit::Decimal => one_two_five(bytes_per_sec),
            SpeedUnit::Bits => one_two_five(bytes_per_sec.saturating_mul(8)).div_ceil(8),
        }
    }

    /// A speed limit as the number and unit to pre-fill the form with.
    pub fn limit_parts(self, bytes_per_sec: u64) -> (String, LimitUnit) {
        let [small, large] = self.limit_units();
        let unit = if bytes_per_sec >= large.bytes {
            large
        } else {
            small
        };
        let value = format!("{:.2}", bytes_per_sec as f64 / unit.bytes as f64);
        let value = value.trim_end_matches('0').trim_end_matches('.').to_owned();
        (value, unit)
    }
}

/// Parse a speed limit typed into the form: a number (empty or 0 for no
/// limit) and a unit id such as `MiB`. Returns bytes per second, 0 for none.
pub fn parse_speed_limit(value: &str, unit: &str) -> Result<u64, String> {
    let value = value.trim().replace(',', ".");
    if value.is_empty() {
        return Ok(0);
    }
    let number: f64 = value
        .parse()
        .ok()
        .filter(|n: &f64| n.is_finite() && *n >= 0.0)
        .ok_or("enter the speed limit as a number, or leave it empty for no limit")?;
    let unit = LIMIT_UNITS
        .iter()
        .find(|u| u.id == unit)
        .ok_or("unknown speed limit unit")?;
    let bytes = (number * unit.bytes as f64).round();
    if bytes > MAX_SPEED_LIMIT as f64 {
        return Err("the speed limit is too high".into());
    }
    Ok(bytes as u64)
}

/// Miscellaneous preferences, as shown and changed on the settings page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct MiscSettings {
    /// The unit the web UI shows download speeds in.
    pub speed_unit: SpeedUnit,
    /// Overall download speed limit in bytes per second, shared by all
    /// downloads; absent when unlimited.
    pub speed_limit: Option<u64>,
    /// Downloads that run at once.
    pub max_concurrent: usize,
    /// `downloads.max_concurrent` from the configuration file, used until
    /// changed here.
    pub default_max_concurrent: usize,
    /// The web UI shows a confirmation menu (type, title, artwork) before a
    /// download starts. When off, Download starts it with the detected type.
    pub confirm_downloads: bool,
    /// Delete an archive (all its volumes) once every video in it has been
    /// extracted into the library. Off by default: the archive stays in the
    /// download folder.
    pub delete_extracted_archives: bool,
}

/// A change to [`MiscSettings`]. Fields left out keep their value.
#[derive(Debug, Clone, Default, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MiscUpdate {
    pub speed_unit: Option<SpeedUnit>,
    /// Bytes per second, at least 10000; 0 removes the limit.
    pub speed_limit: Option<u64>,
    /// 1 to 32. Lowering it lets running downloads finish.
    #[schema(example = 3)]
    pub max_concurrent: Option<usize>,
    pub confirm_downloads: Option<bool>,
    pub delete_extracted_archives: Option<bool>,
}

/// An on/off setting shared between the settings service and the component
/// using it.
#[derive(Debug, Clone, Default)]
pub struct FlagCell(Arc<AtomicBool>);

impl FlagCell {
    pub fn new(value: bool) -> Self {
        Self(Arc::new(AtomicBool::new(value)))
    }

    pub fn get(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub fn set(&self, value: bool) {
        self.0.store(value, Ordering::Relaxed);
    }
}

/// The library folders, shared between the settings service and the
/// organizer.
#[derive(Debug, Clone, Default)]
pub struct LibraryCell(Arc<RwLock<LibraryPaths>>);

impl LibraryCell {
    pub fn new(paths: LibraryPaths) -> Self {
        Self(Arc::new(RwLock::new(paths)))
    }

    pub fn get(&self) -> LibraryPaths {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    #[cfg(test)]
    pub(crate) fn set(&self, paths: LibraryPaths) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = paths;
    }
}

/// One library folder as shown on the settings page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct FolderSetting {
    /// Absolute path; absent when the variable is not set.
    pub path: Option<String>,
    /// The environment variable that sets it.
    pub variable: String,
    /// What was wrong with the folder when Direkt started, e.g. it does not
    /// exist or cannot be written to.
    pub problem: Option<String>,
}

/// The media library folders, set with environment variables at launch.
/// Completed movies and episodes are organized into them; without a movie or
/// TV folder, files stay where they are.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct LibrarySettings {
    pub movies: FolderSetting,
    pub tv: FolderSetting,
    /// Where videos that cannot be identified go. When absent, they stay in
    /// the download folder and are reported.
    pub unsorted: FolderSetting,
}

/// The preferences not held by [`Limits`].
#[derive(Debug, Clone, Copy)]
struct Preferences {
    speed_unit: SpeedUnit,
    confirm_downloads: bool,
}

#[derive(Clone)]
pub struct SettingsService {
    store: Store,
    onefichier_key: SecretCell,
    env_locked: bool,
    onefichier_account: Arc<OneFichierAccount>,
    tmdb_key: SecretCell,
    tmdb_env_locked: bool,
    limits: Limits,
    preferences: Arc<RwLock<Preferences>>,
    library: LibraryCell,
    /// The library folders as checked at startup.
    library_view: LibrarySettings,
    delete_archives: FlagCell,
}

/// Settings from the environment, which take precedence over stored ones.
#[derive(Debug, Default)]
pub struct EnvKeys {
    pub onefichier: Option<Secret>,
    pub tmdb: Option<Secret>,
    pub library: LibraryEnv,
}

impl SettingsService {
    /// Load the stored settings and apply them, including to `limits`.
    pub async fn load(
        store: Store,
        env: EnvKeys,
        onefichier_account: Arc<OneFichierAccount>,
        limits: Limits,
    ) -> AppResult<Self> {
        let env_locked = env.onefichier.is_some();
        let tmdb_env_locked = env.tmdb.is_some();
        let key = match env.onefichier {
            Some(key) => Some(key),
            None => store.get_setting(ONEFICHIER_KEY).await?.map(Secret::new),
        };
        let tmdb_key = match env.tmdb {
            Some(key) => Some(key),
            None => store.get_setting(TMDB_KEY).await?.map(Secret::new),
        };
        let preferences = Preferences {
            speed_unit: stored(&store, SPEED_UNIT, SpeedUnit::default(), SpeedUnit::parse).await?,
            confirm_downloads: stored(&store, CONFIRM_DOWNLOADS, true, |v| v.parse().ok()).await?,
        };
        let delete_archives = stored(&store, DELETE_ARCHIVES, false, |v| v.parse().ok()).await?;
        let speed_limit = stored(&store, SPEED_LIMIT, None, |v| {
            v.parse::<u64>()
                .ok()
                .filter(|n| (MIN_SPEED_LIMIT..=MAX_SPEED_LIMIT).contains(n))
                .map(Some)
        })
        .await?;
        limits.set_speed_limit(speed_limit);
        let max_concurrent = stored(&store, MAX_CONCURRENT, None, |v| {
            v.parse::<usize>()
                .ok()
                .filter(|n| MAX_CONCURRENT_RANGE.contains(n))
                .map(Some)
        })
        .await?;
        if let Some(n) = max_concurrent {
            limits.set_max_concurrent(n);
        }
        let library = LibraryPaths {
            movies: env.library.movies,
            tv: env.library.tv,
            unsorted: env.library.unsorted,
        };
        let checked = library.clone();
        let library_view = tokio::task::spawn_blocking(move || describe_library(&checked))
            .await
            .map_err(|_| AppError::internal("internal task failed"))?;
        Ok(Self {
            store,
            onefichier_key: SecretCell::new(key),
            env_locked,
            onefichier_account,
            tmdb_key: SecretCell::new(tmdb_key),
            tmdb_env_locked,
            limits,
            preferences: Arc::new(RwLock::new(preferences)),
            library: LibraryCell::new(library),
            library_view,
            delete_archives: FlagCell::new(delete_archives),
        })
    }

    /// The cell the organizer reads the library folders from.
    pub fn library_cell(&self) -> LibraryCell {
        self.library.clone()
    }

    /// The flag the organizer reads before deleting an extracted archive.
    pub fn delete_archives_cell(&self) -> FlagCell {
        self.delete_archives.clone()
    }

    pub fn library(&self) -> LibrarySettings {
        self.library_view.clone()
    }

    pub fn misc(&self) -> MiscSettings {
        let preferences = *self.preferences.read().unwrap_or_else(|e| e.into_inner());
        MiscSettings {
            speed_unit: preferences.speed_unit,
            speed_limit: self.limits.speed_limit(),
            max_concurrent: self.limits.max_concurrent(),
            default_max_concurrent: self.limits.default_max_concurrent(),
            confirm_downloads: preferences.confirm_downloads,
            delete_extracted_archives: self.delete_archives.get(),
        }
    }

    /// Validate and save a change, then apply it. Nothing changes if any
    /// field is invalid.
    pub async fn update_misc(&self, update: MiscUpdate) -> AppResult<MiscSettings> {
        let mut changes: Vec<(&str, Option<String>)> = Vec::new();
        if let Some(unit) = update.speed_unit {
            changes.push((SPEED_UNIT, Some(unit.as_str().to_owned())));
        }
        if let Some(confirm) = update.confirm_downloads {
            changes.push((CONFIRM_DOWNLOADS, Some(confirm.to_string())));
        }
        if let Some(delete) = update.delete_extracted_archives {
            changes.push((DELETE_ARCHIVES, Some(delete.to_string())));
        }
        if let Some(limit) = update.speed_limit {
            if limit != 0 && !(MIN_SPEED_LIMIT..=MAX_SPEED_LIMIT).contains(&limit) {
                return Err(AppError::invalid(
                    "the speed limit must be at least 10 kB/s (80 kbit/s), or 0 for no limit",
                ));
            }
            changes.push((SPEED_LIMIT, (limit != 0).then(|| limit.to_string())));
        }
        if let Some(n) = update.max_concurrent {
            if !MAX_CONCURRENT_RANGE.contains(&n) {
                return Err(AppError::invalid(format!(
                    "simultaneous downloads must be between {} and {}",
                    MAX_CONCURRENT_RANGE.start(),
                    MAX_CONCURRENT_RANGE.end()
                )));
            }
            // At the default nothing is stored, so a later change to the
            // configuration file applies.
            let stored = (n != self.limits.default_max_concurrent()).then(|| n.to_string());
            changes.push((MAX_CONCURRENT, stored));
        }
        self.store.apply_settings(&changes, Utc::now()).await?;

        {
            let mut preferences = self.preferences.write().unwrap_or_else(|e| e.into_inner());
            if let Some(unit) = update.speed_unit {
                preferences.speed_unit = unit;
            }
            if let Some(confirm) = update.confirm_downloads {
                preferences.confirm_downloads = confirm;
            }
        }
        if let Some(limit) = update.speed_limit {
            self.limits.set_speed_limit(Some(limit).filter(|&l| l > 0));
        }
        if let Some(n) = update.max_concurrent {
            self.limits.set_max_concurrent(n);
        }
        if let Some(delete) = update.delete_extracted_archives {
            self.delete_archives.set(delete);
        }
        let misc = self.misc();
        tracing::info!(
            speed_limit = misc.speed_limit,
            max_concurrent = misc.max_concurrent,
            speed_unit = misc.speed_unit.as_str(),
            confirm_downloads = misc.confirm_downloads,
            delete_extracted_archives = misc.delete_extracted_archives,
            "settings updated"
        );
        Ok(misc)
    }

    /// The cell artwork lookups read the TMDB key from.
    pub fn tmdb_key(&self) -> SecretCell {
        self.tmdb_key.clone()
    }

    pub fn tmdb(&self) -> TmdbSettings {
        let configured = self.tmdb_key.get().is_some();
        TmdbSettings {
            configured,
            api_key: configured.then(|| SECRET_MASK.to_owned()),
            source: KeySource::of(configured, self.tmdb_env_locked),
            editable: !self.tmdb_env_locked,
        }
    }

    pub async fn set_tmdb_key(&self, key: &str) -> AppResult<TmdbSettings> {
        self.ensure_tmdb_editable()?;
        let key = key.trim();
        tmdb::validate_key(key).map_err(AppError::invalid)?;
        self.store.set_setting(TMDB_KEY, key, Utc::now()).await?;
        self.tmdb_key.set(Some(Secret::new(key)));
        tracing::info!("TMDB API key updated");
        Ok(self.tmdb())
    }

    pub async fn clear_tmdb_key(&self) -> AppResult<TmdbSettings> {
        self.ensure_tmdb_editable()?;
        self.store.delete_setting(TMDB_KEY).await?;
        self.tmdb_key.set(None);
        tracing::info!("TMDB API key removed");
        Ok(self.tmdb())
    }

    fn ensure_tmdb_editable(&self) -> AppResult<()> {
        if self.tmdb_env_locked {
            return Err(AppError::conflict(format!(
                "the TMDB API key is set by the {} environment variable; change it there",
                crate::config::TMDB_API_KEY_ENV
            )));
        }
        Ok(())
    }

    /// The cell the 1fichier provider reads its key from.
    pub fn onefichier_key(&self) -> SecretCell {
        self.onefichier_key.clone()
    }

    pub fn onefichier(&self) -> OneFichierSettings {
        let configured = self.onefichier_key.get().is_some();
        OneFichierSettings {
            account: self.onefichier_account.view(),
            cdn: self.onefichier_account.cdn_view(),
            configured,
            api_key: configured.then(|| SECRET_MASK.to_owned()),
            source: KeySource::of(configured, self.env_locked),
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
        if !self.onefichier_account.is_signed_in() {
            // Show the key's CDN credits right away; informative only.
            if let Err(e) = self
                .onefichier_account
                .refresh_cdn(Some(Secret::new(key)))
                .await
            {
                tracing::warn!(error = %e, "could not read 1fichier CDN credits");
            }
        }
        Ok(self.onefichier())
    }

    pub async fn clear_onefichier_key(&self) -> AppResult<OneFichierSettings> {
        self.ensure_editable()?;
        self.store.delete_setting(ONEFICHIER_KEY).await?;
        self.onefichier_key.set(None);
        self.onefichier_account.clear_cdn(CdnSource::ApiKey).await?;
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

    /// Read the 1fichier CDN credits again (at most every 5 minutes).
    pub async fn refresh_onefichier_cdn(&self) -> AppResult<OneFichierSettings> {
        self.onefichier_account
            .refresh_cdn(self.onefichier_key.get())
            .await?;
        Ok(self.onefichier())
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

/// A stored preference, or `default` if it is unset or unreadable.
async fn stored<T>(
    store: &Store,
    key: &str,
    default: T,
    parse: impl FnOnce(&str) -> Option<T>,
) -> AppResult<T> {
    let Some(raw) = store.get_setting(key).await? else {
        return Ok(default);
    };
    Ok(parse(&raw).unwrap_or_else(|| {
        tracing::warn!(key, "ignoring an invalid stored setting");
        default
    }))
}

/// Check each library folder and log what is wrong with it, so a missing
/// mount or a permission problem shows up at launch rather than on the first
/// download.
fn describe_library(paths: &LibraryPaths) -> LibrarySettings {
    use crate::config::{MOVIES_DIR_ENV, TV_DIR_ENV, UNSORTED_DIR_ENV};
    let folder = |path: &Option<PathBuf>, variable: &str| {
        let problem = path.as_deref().and_then(|p| check_library_dir(p).err());
        if let (Some(path), Some(problem)) = (path, &problem) {
            tracing::warn!(variable, path = %path.display(), %problem, "media library folder is not usable");
        }
        FolderSetting {
            path: path.as_ref().map(|p| p.to_string_lossy().into_owned()),
            variable: variable.to_owned(),
            problem,
        }
    };
    LibrarySettings {
        movies: folder(&paths.movies, MOVIES_DIR_ENV),
        tv: folder(&paths.tv, TV_DIR_ENV),
        unsorted: folder(&paths.unsorted, UNSORTED_DIR_ENV),
    }
}

/// Whether `path` is an existing directory Direkt can write to.
pub fn check_library_dir(path: &Path) -> Result<(), String> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err("not a folder".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(match differently_cased_sibling(path) {
                Some(name) => format!(
                    "does not exist, but \"{name}\" does next to it; folder names are \
                     case-sensitive"
                ),
                None => "does not exist; check the container's volume mounts".into(),
            });
        }
        Err(e) => return Err(format!("cannot be read: {e}")),
    }
    // Media servers often run as another user: catch permission problems now
    // rather than on the first download.
    let probe = path.join(".direkt-write-check");
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => Err(format!(
            "Direkt cannot write to it ({e}); check the folder's owner and permissions"
        )),
    }
}

/// A folder beside `path` whose name matches it except for letter case.
fn differently_cased_sibling(path: &Path) -> Option<String> {
    let wanted = path.file_name()?.to_str()?;
    std::fs::read_dir(path.parent()?)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .find(|name| name != wanted && name.eq_ignore_ascii_case(wanted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    const KEY: &str = "AbCdEfGhIjKlMnOpQrStUvWxYz012345";

    async fn load(store: &Store, env: Option<Secret>) -> SettingsService {
        SettingsService::load(
            store.clone(),
            EnvKeys {
                onefichier: env,
                tmdb: None,
                ..Default::default()
            },
            crate::providers::unconfigured_onefichier_account().await,
            Limits::new(3),
        )
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

    #[tokio::test]
    async fn tmdb_key_is_stored_masked_and_applied_live() {
        const TMDB: &str = "0123456789abcdef0123456789abcdef";
        let store = Store::open_in_memory().await.unwrap();
        let settings = load(&store, None).await;
        let cell = settings.tmdb_key();
        assert!(!settings.tmdb().configured);
        let version = cell.version();

        let shown = settings.set_tmdb_key(&format!(" {TMDB} ")).await.unwrap();
        assert_eq!(shown.api_key.as_deref(), Some(SECRET_MASK));
        assert_eq!(shown.source, KeySource::Settings);
        assert_eq!(cell.get().unwrap().expose(), TMDB);
        assert!(cell.version() > version, "users can tell the key changed");
        assert!(!format!("{shown:?}").contains(TMDB));
        assert_eq!(
            load(&store, None).await.tmdb_key().get().unwrap().expose(),
            TMDB
        );

        let err = settings.set_tmdb_key("nope").await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
        settings.clear_tmdb_key().await.unwrap();
        assert!(cell.get().is_none());

        let locked = SettingsService::load(
            store.clone(),
            EnvKeys {
                onefichier: None,
                tmdb: Some(Secret::new(TMDB)),
                ..Default::default()
            },
            crate::providers::unconfigured_onefichier_account().await,
            Limits::new(3),
        )
        .await
        .unwrap();
        assert_eq!(locked.tmdb().source, KeySource::Environment);
        assert!(!locked.tmdb().editable);
        assert_eq!(
            locked.set_tmdb_key(TMDB).await.unwrap_err().kind,
            ErrorKind::Conflict
        );
    }

    #[test]
    fn speeds_in_each_unit() {
        let mb = 1_000_000;
        assert_eq!(SpeedUnit::Binary.format(0), "0 B/s");
        assert_eq!(SpeedUnit::Binary.format(1536), "1.5 KiB/s");
        assert_eq!(SpeedUnit::Binary.format(12 * mb), "11.4 MiB/s");
        assert_eq!(SpeedUnit::Decimal.format(12 * mb), "12.0 MB/s");
        assert_eq!(SpeedUnit::Decimal.format(999), "999 B/s");
        assert_eq!(SpeedUnit::Bits.format(12 * mb), "96.0 Mbit/s");
        assert_eq!(SpeedUnit::Bits.format(125 * mb), "1.0 Gbit/s");
        assert_eq!(SpeedUnit::Bits.format(15 * mb), "120 Mbit/s");
        for unit in SpeedUnit::ALL {
            assert_eq!(SpeedUnit::parse(unit.as_str()), Some(unit));
        }

        // Graph scales read round in every unit.
        assert_eq!(SpeedUnit::Binary.round_up(65_536), 65_536);
        assert_eq!(SpeedUnit::Binary.round_up(70_000), 131_072);
        assert_eq!(SpeedUnit::Decimal.round_up(65_536), 100_000);
        assert_eq!(SpeedUnit::Decimal.round_up(3_100_000), 5_000_000);
        assert_eq!(SpeedUnit::Decimal.round_up(5_000_000), 5_000_000);
        assert_eq!(
            SpeedUnit::Bits.format(SpeedUnit::Bits.round_up(65_536)),
            "1.0 Mbit/s"
        );
        assert_eq!(
            SpeedUnit::Bits.format(SpeedUnit::Bits.round_up(15_000_000)),
            "200 Mbit/s"
        );
    }

    #[test]
    fn speed_limits_from_the_form() {
        assert_eq!(parse_speed_limit("", "MB"), Ok(0));
        assert_eq!(parse_speed_limit(" 0 ", "MB"), Ok(0));
        assert_eq!(parse_speed_limit("5", "MB"), Ok(5_000_000));
        assert_eq!(parse_speed_limit("1,5", "MiB"), Ok(1_572_864));
        assert_eq!(parse_speed_limit("100", "Mbit"), Ok(12_500_000));
        assert_eq!(parse_speed_limit("512", "KiB"), Ok(524_288));
        assert!(parse_speed_limit("-1", "MB").is_err());
        assert!(parse_speed_limit("fast", "MB").is_err());
        assert!(parse_speed_limit("inf", "MB").is_err());
        assert!(parse_speed_limit("5", "GB").is_err());
        assert!(parse_speed_limit("1e20", "MB").is_err());

        assert_eq!(
            SpeedUnit::Decimal.limit_parts(5_000_000),
            ("5".into(), LIMIT_UNITS[3])
        );
        assert_eq!(
            SpeedUnit::Binary.limit_parts(1_572_864),
            ("1.5".into(), LIMIT_UNITS[1])
        );
        assert_eq!(
            SpeedUnit::Bits.limit_parts(100_000),
            ("800".into(), LIMIT_UNITS[4])
        );
        assert_eq!(
            SpeedUnit::Binary.limit_parts(5_000_000),
            ("4.77".into(), LIMIT_UNITS[1])
        );
    }

    #[tokio::test]
    async fn misc_settings_apply_live_and_persist() {
        let store = Store::open_in_memory().await.unwrap();
        let limits = Limits::new(3);
        let account = crate::providers::unconfigured_onefichier_account().await;
        let load = |limits: Limits| {
            SettingsService::load(store.clone(), EnvKeys::default(), account.clone(), limits)
        };
        let settings = load(limits.clone()).await.unwrap();
        let defaults = settings.misc();
        assert_eq!(defaults.speed_unit, SpeedUnit::Binary);
        assert_eq!(defaults.speed_limit, None);
        assert_eq!(defaults.max_concurrent, 3);
        assert!(defaults.confirm_downloads);
        assert!(
            !defaults.delete_extracted_archives,
            "archives are kept by default"
        );
        let organizer_flag = settings.delete_archives_cell();

        let changed = settings
            .update_misc(MiscUpdate {
                speed_unit: Some(SpeedUnit::Bits),
                speed_limit: Some(2_000_000),
                max_concurrent: Some(5),
                confirm_downloads: Some(false),
                delete_extracted_archives: Some(true),
            })
            .await
            .unwrap();
        assert_eq!(changed.speed_limit, Some(2_000_000));
        assert!(changed.delete_extracted_archives);
        assert!(organizer_flag.get(), "the organizer sees it now");
        assert_eq!(limits.speed_limit(), Some(2_000_000), "engine sees it now");
        assert_eq!(limits.max_concurrent(), 5);

        // After a restart, with the configuration file's default.
        let reloaded_limits = Limits::new(3);
        let reloaded = load(reloaded_limits.clone()).await.unwrap();
        assert_eq!(reloaded.misc(), changed);
        assert!(reloaded.delete_archives_cell().get());
        assert_eq!(reloaded_limits.max_concurrent(), 5);
        assert_eq!(reloaded_limits.speed_limit(), Some(2_000_000));

        // Invalid input changes nothing, even the valid fields with it.
        for bad in [
            MiscUpdate {
                speed_limit: Some(MIN_SPEED_LIMIT - 1),
                confirm_downloads: Some(true),
                ..Default::default()
            },
            MiscUpdate {
                max_concurrent: Some(0),
                confirm_downloads: Some(true),
                ..Default::default()
            },
            MiscUpdate {
                max_concurrent: Some(33),
                ..Default::default()
            },
        ] {
            let err = settings.update_misc(bad).await.unwrap_err();
            assert_eq!(err.kind, ErrorKind::InvalidRequest);
        }
        assert_eq!(settings.misc(), changed);

        // 0 removes the limit; the default concurrency is not stored, so a
        // new value in the configuration file applies after a restart.
        settings
            .update_misc(MiscUpdate {
                speed_limit: Some(0),
                max_concurrent: Some(3),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(limits.speed_limit(), None);
        assert_eq!(store.get_setting(MAX_CONCURRENT).await.unwrap(), None);
        let new_default = Limits::new(8);
        load(new_default.clone()).await.unwrap();
        assert_eq!(new_default.max_concurrent(), 8);
    }

    #[tokio::test]
    async fn library_folders_come_only_from_the_environment() {
        let store = Store::open_in_memory().await.unwrap();
        let settings = load(&store, None).await;
        assert_eq!(settings.library_cell().get(), LibraryPaths::default());
        assert_eq!(settings.library().movies.path, None);
        assert_eq!(settings.library().movies.variable, "DIREKT_MOVIES_DIR");

        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("movies")).unwrap();
        std::fs::write(root.path().join("file"), b"x").unwrap();
        let settings = SettingsService::load(
            store.clone(),
            EnvKeys {
                library: LibraryEnv {
                    movies: Some(root.path().join("movies")),
                    tv: Some(root.path().join("missing")),
                    unsorted: Some(root.path().join("file")),
                },
                ..Default::default()
            },
            crate::providers::unconfigured_onefichier_account().await,
            Limits::new(3),
        )
        .await
        .unwrap();
        assert_eq!(
            settings.library_cell().get().movies,
            Some(root.path().join("movies")),
            "the organizer sees them"
        );
        let shown = settings.library();
        assert_eq!(shown.movies.problem, None);
        assert!(!root.path().join("movies/.direkt-write-check").exists());
        assert!(shown.tv.problem.unwrap().contains("does not exist"));
        assert!(shown.unsorted.problem.unwrap().contains("not a folder"));
    }

    #[tokio::test]
    async fn unreadable_stored_values_fall_back_to_defaults() {
        let store = Store::open_in_memory().await.unwrap();
        let now = Utc::now();
        for (key, value) in [
            (SPEED_UNIT, "furlongs"),
            (CONFIRM_DOWNLOADS, "maybe"),
            (SPEED_LIMIT, "1"),
            (MAX_CONCURRENT, "999"),
        ] {
            store.set_setting(key, value, now).await.unwrap();
        }
        let limits = Limits::new(3);
        let settings = SettingsService::load(
            store,
            EnvKeys::default(),
            crate::providers::unconfigured_onefichier_account().await,
            limits,
        )
        .await
        .unwrap();
        let misc = settings.misc();
        assert_eq!(misc.speed_unit, SpeedUnit::Binary);
        assert!(misc.confirm_downloads);
        assert_eq!(misc.speed_limit, None);
        assert_eq!(misc.max_concurrent, 3);
    }

    #[test]
    fn library_folder_problems_are_explained() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("Movies")).unwrap();
        let err = check_library_dir(&root.path().join("movies")).unwrap_err();
        assert!(err.contains("\"Movies\" does next to it"), "{err}");
        let err = check_library_dir(&root.path().join("tv")).unwrap_err();
        assert!(err.contains("volume mounts"), "{err}");
        assert_eq!(check_library_dir(&root.path().join("Movies")), Ok(()));
    }
}
