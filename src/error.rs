//! Structured application error model.
//!
//! [`AppError`] carries a machine-readable [`ErrorKind`] plus a message that is
//! safe to show to users. Internal diagnostic detail (database errors, OS
//! errors, full URLs) must go to logs, never into `message`.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The request or submitted URL is malformed or not permitted.
    InvalidRequest,
    /// No provider can handle the submitted URL.
    UnsupportedProvider,
    /// Credentials were missing or rejected by a provider.
    AuthenticationFailed,
    /// A provider temporarily refuses service (rate limit, wait time, quota).
    ProviderRestricted,
    /// Connection, DNS, TLS, or timeout failure.
    Network,
    /// The remote resource is gone or does not exist.
    RemoteUnavailable,
    /// Not enough free space to continue writing.
    InsufficientDiskSpace,
    /// Local filesystem operation failed.
    Filesystem,
    /// The database failed.
    Persistence,
    /// Media could not be identified or organized.
    MediaIdentification,
    /// A notification could not be delivered.
    Notification,
    /// The caller is not logged in or presented invalid credentials.
    Unauthorized,
    /// The caller is authenticated but the request is not allowed (for
    /// example a cross-site form submission).
    Forbidden,
    /// Too many attempts; the caller must wait before trying again.
    RateLimited,
    /// The requested entity does not exist.
    NotFound,
    /// The operation conflicts with the current state of the entity.
    Conflict,
    /// Unexpected internal failure.
    Internal,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::UnsupportedProvider => "unsupported_provider",
            Self::AuthenticationFailed => "authentication_failed",
            Self::ProviderRestricted => "provider_restricted",
            Self::Network => "network",
            Self::RemoteUnavailable => "remote_unavailable",
            Self::InsufficientDiskSpace => "insufficient_disk_space",
            Self::Filesystem => "filesystem",
            Self::Persistence => "persistence",
            Self::MediaIdentification => "media_identification",
            Self::Notification => "notification",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::RateLimited => "rate_limited",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::Internal => "internal",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "invalid_request" => Self::InvalidRequest,
            "unsupported_provider" => Self::UnsupportedProvider,
            "authentication_failed" => Self::AuthenticationFailed,
            "provider_restricted" => Self::ProviderRestricted,
            "network" => Self::Network,
            "remote_unavailable" => Self::RemoteUnavailable,
            "insufficient_disk_space" => Self::InsufficientDiskSpace,
            "filesystem" => Self::Filesystem,
            "persistence" => Self::Persistence,
            "media_identification" => Self::MediaIdentification,
            "notification" => Self::Notification,
            "unauthorized" => Self::Unauthorized,
            "forbidden" => Self::Forbidden,
            "rate_limited" => Self::RateLimited,
            "not_found" => Self::NotFound,
            "conflict" => Self::Conflict,
            "internal" => Self::Internal,
            _ => return None,
        })
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind}: {message}")]
pub struct AppError {
    pub kind: ErrorKind,
    /// User-safe description. Must never contain secrets or full URLs.
    pub message: String,
}

impl AppError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidRequest, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unauthorized, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }
}

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        tracing::error!(error = %err, "database error");
        Self::new(ErrorKind::Persistence, "database operation failed")
    }
}

impl From<sqlx::migrate::MigrateError> for AppError {
    fn from(err: sqlx::migrate::MigrateError) -> Self {
        tracing::error!(error = %err, "database migration failed");
        Self::new(ErrorKind::Persistence, "database migration failed")
    }
}

pub type AppResult<T> = Result<T, AppError>;

/// What the engine should do after a failed transfer attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Transient: try again later, optionally after a server-specified delay.
    Retry { after: Option<std::time::Duration> },
    /// Needs user intervention (free disk space, fix permissions, credentials).
    Block,
    /// Permanent: retrying without a change will not help.
    Fail,
}

/// A failed transfer attempt, classified for the retry logic.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind}: {message}")]
pub struct TransferError {
    pub kind: ErrorKind,
    /// User-safe description. Must never contain secrets or full URLs.
    pub message: String,
    pub disposition: Disposition,
}

impl TransferError {
    pub fn retry(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self::new(kind, message, Disposition::Retry { after: None })
    }

    pub fn retry_after(
        kind: ErrorKind,
        message: impl Into<String>,
        after: Option<std::time::Duration>,
    ) -> Self {
        Self::new(kind, message, Disposition::Retry { after })
    }

    pub fn block(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self::new(kind, message, Disposition::Block)
    }

    pub fn fail(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self::new(kind, message, Disposition::Fail)
    }

    fn new(kind: ErrorKind, message: impl Into<String>, disposition: Disposition) -> Self {
        Self {
            kind,
            message: message.into(),
            disposition,
        }
    }

    /// Classify a local I/O failure. Both cases need the user to act (free
    /// space, fix permissions or mounts), so the job is blocked rather than
    /// failed and its partial data is kept.
    pub fn from_io(err: &std::io::Error, context: &str) -> Self {
        tracing::warn!(error = %err, context, "local filesystem error");
        if err.kind() == std::io::ErrorKind::StorageFull {
            Self::block(
                ErrorKind::InsufficientDiskSpace,
                format!("disk full while {context}"),
            )
        } else {
            Self::block(
                ErrorKind::Filesystem,
                format!("filesystem error while {context}: {}", err.kind()),
            )
        }
    }
}

impl From<TransferError> for AppError {
    fn from(err: TransferError) -> Self {
        Self::new(err.kind, err.message)
    }
}

impl From<AppError> for TransferError {
    fn from(err: AppError) -> Self {
        // Persistence problems are usually transient (locked or busy database).
        let disposition = match err.kind {
            ErrorKind::Persistence | ErrorKind::Internal => Disposition::Retry { after: None },
            _ => Disposition::Fail,
        };
        Self::new(err.kind, err.message, disposition)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_string_round_trip() {
        let kinds = [
            ErrorKind::InvalidRequest,
            ErrorKind::UnsupportedProvider,
            ErrorKind::AuthenticationFailed,
            ErrorKind::ProviderRestricted,
            ErrorKind::Network,
            ErrorKind::RemoteUnavailable,
            ErrorKind::InsufficientDiskSpace,
            ErrorKind::Filesystem,
            ErrorKind::Persistence,
            ErrorKind::MediaIdentification,
            ErrorKind::Notification,
            ErrorKind::Unauthorized,
            ErrorKind::Forbidden,
            ErrorKind::RateLimited,
            ErrorKind::NotFound,
            ErrorKind::Conflict,
            ErrorKind::Internal,
        ];
        for kind in kinds {
            assert_eq!(ErrorKind::parse(kind.as_str()), Some(kind));
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
        }
        assert_eq!(ErrorKind::parse("bogus"), None);
    }
}
