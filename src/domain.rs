//! Core download domain model and state machine.
//!
//! A download has two independent lifecycles:
//!
//! * [`TransferState`] — the byte transfer itself.
//! * [`PostState`] — media identification/organization, which only starts
//!   once the transfer is `Completed`. A post-processing failure never changes
//!   the transfer state.
//!
//! See `docs/state-machine.md` for the transition diagram.

use crate::error::ErrorKind;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    /// Waiting for a free worker slot.
    Queued,
    /// A worker owns the job and is transferring bytes.
    Downloading,
    /// A transient failure occurred; the job waits until `next_attempt_at`.
    Retrying,
    /// Paused by the user. Will not run until resumed.
    Paused,
    /// Requires user intervention (e.g. credentials) before it can continue.
    Blocked,
    /// All bytes were transferred and the file is complete.
    Completed,
    /// Permanent failure or retries exhausted.
    Failed,
    /// Cancelled by the user.
    Cancelled,
}

impl TransferState {
    pub const ALL: [Self; 8] = [
        Self::Queued,
        Self::Downloading,
        Self::Retrying,
        Self::Paused,
        Self::Blocked,
        Self::Completed,
        Self::Failed,
        Self::Cancelled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Downloading => "downloading",
            Self::Retrying => "retrying",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == s)
    }

    /// Whether `self -> next` is an allowed transition.
    pub fn can_transition_to(self, next: Self) -> bool {
        use TransferState::*;
        matches!(
            (self, next),
            (Queued, Downloading | Paused | Cancelled)
                // `Downloading -> Queued` happens when a running transfer is
                // interrupted by shutdown or a crash and must be re-run.
                | (Downloading, Completed | Retrying | Blocked | Failed | Paused | Cancelled | Queued)
                | (Retrying, Downloading | Paused | Cancelled)
                | (Paused, Queued | Cancelled)
                | (Blocked, Queued | Cancelled)
                | (Failed, Queued)
                | (Cancelled, Queued)
        )
    }

    /// States from which `next` may be entered.
    pub fn predecessors(next: Self) -> impl Iterator<Item = Self> {
        Self::ALL
            .into_iter()
            .filter(move |from| from.can_transition_to(next))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PostState {
    /// Transfer not yet completed; post-processing has not started.
    None,
    /// Transfer completed; waiting for post-processing.
    Pending,
    /// Identification/organization in progress.
    Processing,
    /// File organized into its final destination.
    Completed,
    /// Identification or organization failed; the downloaded file is preserved.
    Failed,
    /// Post-processing is disabled; the file stays in the download directory.
    Skipped,
}

impl PostState {
    pub const ALL: [Self; 6] = [
        Self::None,
        Self::Pending,
        Self::Processing,
        Self::Completed,
        Self::Failed,
        Self::Skipped,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Pending => "pending",
            Self::Processing => "processing",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == s)
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        use PostState::*;
        matches!(
            (self, next),
            (None, Pending)
                | (Pending, Processing | Skipped)
                // `Processing -> Pending` recovers an organization step that
                // was interrupted by a restart.
                | (Processing, Completed | Failed | Pending)
                | (Failed, Pending)
        )
    }
}

/// Persisted error details attached to a download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct DownloadError {
    pub kind: ErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Download {
    pub id: Uuid,
    /// The URL as submitted. May contain tokens in its query string; never
    /// log it or return it to clients that should not see it.
    #[serde(skip)]
    pub url: String,
    /// Redacted URL that is safe to display and log.
    pub display_url: String,
    pub provider: String,
    pub state: TransferState,
    pub post_state: PostState,
    pub filename: Option<String>,
    pub total_bytes: Option<i64>,
    pub downloaded_bytes: i64,
    pub retry_count: i64,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub error: Option<DownloadError>,
    pub post_error: Option<DownloadError>,
    /// Final location of the completed file, once known.
    pub file_path: Option<String>,
    /// Validators from the last response, used to resume safely.
    #[serde(skip)]
    pub resume_etag: Option<String>,
    #[serde(skip)]
    pub resume_last_modified: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct DownloadEvent {
    pub id: i64,
    pub download_id: Uuid,
    pub at: DateTime<Utc>,
    pub kind: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_state_string_round_trip() {
        for state in TransferState::ALL {
            assert_eq!(TransferState::parse(state.as_str()), Some(state));
        }
        assert_eq!(TransferState::parse("nope"), None);
    }

    #[test]
    fn post_state_string_round_trip() {
        for state in PostState::ALL {
            assert_eq!(PostState::parse(state.as_str()), Some(state));
        }
    }

    #[test]
    fn completed_is_final() {
        for next in TransferState::ALL {
            assert!(!TransferState::Completed.can_transition_to(next));
        }
    }

    #[test]
    fn only_a_worker_can_complete_or_fail() {
        for from in TransferState::ALL {
            let expected = from == TransferState::Downloading;
            assert_eq!(from.can_transition_to(TransferState::Completed), expected);
            assert_eq!(from.can_transition_to(TransferState::Failed), expected);
        }
    }

    #[test]
    fn no_self_transitions() {
        for state in TransferState::ALL {
            assert!(!state.can_transition_to(state));
        }
        for state in PostState::ALL {
            assert!(!state.can_transition_to(state));
        }
    }

    #[test]
    fn user_actions() {
        use TransferState::*;
        // Pause and cancel are possible from every non-final waiting/running state.
        for from in [Queued, Downloading, Retrying] {
            assert!(from.can_transition_to(Paused));
            assert!(from.can_transition_to(Cancelled));
        }
        // Resume/retry always goes back through the queue.
        for from in [Paused, Blocked, Failed, Cancelled] {
            assert!(from.can_transition_to(Queued));
            assert!(!from.can_transition_to(Downloading));
        }
    }

    #[test]
    fn predecessors_match_table() {
        let preds: Vec<_> = TransferState::predecessors(TransferState::Downloading).collect();
        assert_eq!(preds, vec![TransferState::Queued, TransferState::Retrying]);
    }

    #[test]
    fn post_processing_never_skips_pending() {
        assert!(!PostState::None.can_transition_to(PostState::Processing));
        assert!(!PostState::None.can_transition_to(PostState::Completed));
        assert!(!PostState::Completed.can_transition_to(PostState::Pending));
        assert!(PostState::Failed.can_transition_to(PostState::Pending));
    }
}
