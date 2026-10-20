//! Application configuration.
//!
//! Configuration lives in a single TOML file whose path is given by the
//! `DIREKT_CONFIG` environment variable (default `./direkt.toml`). A missing
//! file means "all defaults". Unknown keys are rejected so typos are caught at
//! startup instead of silently ignored.

use ipnet::IpNet;
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

const MAX_RETRY_DELAY_SECS: u64 = 7 * 24 * 3600;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub downloads: DownloadsConfig,
    pub network: NetworkConfig,
    pub log: LogConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Address and port the HTTP server listens on.
    pub listen: SocketAddr,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 8080)),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// Directory for the database and other application state.
    pub data_dir: PathBuf,
    /// Directory where completed transfers are placed.
    pub download_dir: PathBuf,
    /// Directory for partial (in-progress) downloads.
    pub incomplete_dir: PathBuf,
    /// Free space (MiB) that downloads must leave on the incomplete-download
    /// filesystem. A download that would go below it is blocked.
    pub min_free_space_mb: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("data"),
            download_dir: PathBuf::from("downloads"),
            incomplete_dir: PathBuf::from("downloads/.incomplete"),
            min_free_space_mb: 1024,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DownloadsConfig {
    /// Maximum number of simultaneous transfers.
    pub max_concurrent: usize,
    /// Maximum automatic retries for transient failures before failing.
    pub max_retries: u32,
    /// Initial retry delay; doubles on each retry.
    pub retry_base_delay_secs: u64,
    /// Upper bound for the retry delay.
    pub retry_max_delay_secs: u64,
}

impl Default for DownloadsConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 3,
            max_retries: 5,
            retry_base_delay_secs: 30,
            retry_max_delay_secs: 3600,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    /// Private/local networks that downloads may target, in CIDR notation.
    /// Empty by default: only public addresses are reachable.
    pub allowed_private_networks: Vec<IpNet>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Pretty,
    Json,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    /// Log filter, e.g. `info` or `direkt=debug,info`. `RUST_LOG` overrides it.
    pub level: String,
    pub format: LogFormat,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: LogFormat::Pretty,
        }
    }
}

impl Config {
    /// Load configuration from the path in `DIREKT_CONFIG` (or `./direkt.toml`).
    pub fn load() -> Result<(Self, PathBuf), ConfigError> {
        let path = std::env::var_os("DIREKT_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("direkt.toml"));
        let config = Self::load_from(&path)?;
        Ok((config, path))
    }

    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        let config = match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|e| match e {
                ConfigError::Parse { source, .. } => ConfigError::Parse {
                    path: path.to_owned(),
                    source,
                },
                other => other,
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        config.validate()?;
        Ok(config)
    }

    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: PathBuf::new(),
            source,
        })
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let d = &self.downloads;
        if !(1..=32).contains(&d.max_concurrent) {
            return Err(ConfigError::Invalid(
                "downloads.max_concurrent must be between 1 and 32".into(),
            ));
        }
        if d.max_retries > 100 {
            return Err(ConfigError::Invalid(
                "downloads.max_retries must be at most 100".into(),
            ));
        }
        if d.retry_base_delay_secs == 0 {
            return Err(ConfigError::Invalid(
                "downloads.retry_base_delay_secs must be at least 1".into(),
            ));
        }
        if d.retry_max_delay_secs > MAX_RETRY_DELAY_SECS {
            return Err(ConfigError::Invalid(format!(
                "downloads.retry_max_delay_secs must be at most {MAX_RETRY_DELAY_SECS} (7 days)"
            )));
        }
        if d.retry_max_delay_secs < d.retry_base_delay_secs {
            return Err(ConfigError::Invalid(
                "downloads.retry_max_delay_secs must be >= retry_base_delay_secs".into(),
            ));
        }

        let s = &self.storage;
        for (name, path) in [
            ("storage.data_dir", &s.data_dir),
            ("storage.download_dir", &s.download_dir),
            ("storage.incomplete_dir", &s.incomplete_dir),
        ] {
            if path.as_os_str().is_empty() {
                return Err(ConfigError::Invalid(format!("{name} must not be empty")));
            }
        }
        if s.incomplete_dir == s.download_dir {
            return Err(ConfigError::Invalid(
                "storage.incomplete_dir must differ from storage.download_dir".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn empty_file_gives_defaults() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.downloads.max_concurrent, 3);
        assert_eq!(config.server.listen.port(), 8080);
    }

    #[test]
    fn parses_full_example() {
        let config = Config::parse(include_str!("../direkt.example.toml")).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn rejects_unknown_keys() {
        let err = Config::parse("[downloads]\nmax_concurent = 2\n").unwrap_err();
        assert!(err.to_string().contains("max_concurent"), "{err}");
    }

    #[test]
    fn parses_cidrs() {
        let config = Config::parse(
            "[network]\nallowed_private_networks = [\"192.168.1.0/24\", \"fd00::/8\"]\n",
        )
        .unwrap();
        assert_eq!(config.network.allowed_private_networks.len(), 2);
        assert!(Config::parse("[network]\nallowed_private_networks = [\"nope\"]\n").is_err());
    }

    #[test]
    fn rejects_out_of_range_values() {
        for text in [
            "[downloads]\nmax_concurrent = 0\n",
            "[downloads]\nmax_concurrent = 500\n",
            "[downloads]\nmax_retries = 1000\n",
            "[downloads]\nretry_base_delay_secs = 0\n",
            "[downloads]\nretry_base_delay_secs = 60\nretry_max_delay_secs = 10\n",
            "[downloads]\nretry_max_delay_secs = 99999999999\n",
            "[storage]\ndownload_dir = \"/x\"\nincomplete_dir = \"/x\"\n",
        ] {
            let config = Config::parse(text).unwrap();
            assert!(config.validate().is_err(), "{text}");
        }
    }

    #[test]
    fn missing_file_gives_defaults() {
        let config = Config::load_from(Path::new("/nonexistent/direkt.toml")).unwrap();
        assert_eq!(config.downloads.max_retries, 5);
    }
}
