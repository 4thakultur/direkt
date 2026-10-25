//! SQLite persistence for downloads and their event history.
//!
//! Every state change goes through a compare-and-swap UPDATE (`WHERE state =
//! expected`) inside a transaction that also appends an event. This keeps the
//! state machine enforced at the storage boundary and prevents two actors
//! (e.g. a worker and a user pausing) from silently overwriting each other.

use crate::domain::{Download, DownloadError, DownloadEvent, MediaType, PostState, TransferState};
use crate::error::{AppError, AppResult, ErrorKind};
use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{FromRow, SqlitePool};
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

mod auth;
mod settings;

pub use auth::{ApiToken, NewUser, UserChanges, UserRecord};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, Clone)]
pub struct Store {
    pool: SqlitePool,
}

/// Fields for a newly submitted download.
#[derive(Debug, Clone, Default)]
pub struct NewDownload {
    pub url: String,
    pub display_url: String,
    pub provider: String,
    pub password: Option<String>,
    /// File name from the confirmation step: shown until the transfer
    /// reports one, and the engine's last-resort name.
    pub filename: Option<String>,
    pub media_type: Option<MediaType>,
    pub title: Option<String>,
    pub year: Option<u16>,
    pub artwork_id: Option<String>,
    /// The account adding it.
    pub owner_id: Option<i64>,
}

/// Optional data recorded atomically with a transfer state change.
#[derive(Debug, Clone, Default)]
pub struct TransitionDetails {
    /// Error to record. `None` clears any previous error when entering
    /// `Queued`, `Downloading`, or `Completed`; otherwise it is left alone.
    pub error: Option<DownloadError>,
    /// When a `Retrying` job becomes eligible to run again.
    pub next_attempt_at: Option<DateTime<Utc>>,
    /// Human-readable event message; defaults to "state: from -> to".
    pub message: Option<String>,
}

/// Optional data recorded atomically with a post-processing state change.
#[derive(Debug, Clone, Default)]
pub struct PostDetails {
    /// Stored as `post_error`: why it failed, was skipped, or fell back.
    /// `None` clears it.
    pub error: Option<DownloadError>,
    /// The file's new location, after it was moved.
    pub file_path: Option<String>,
    /// Event message; defaults to "post-processing <state>".
    pub message: Option<String>,
}

/// Resource metadata learned from the server's response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransferInfo {
    pub total_bytes: Option<i64>,
    pub filename: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Bytes already present locally when the response started.
    pub downloaded_bytes: i64,
}

/// Summary of startup reconciliation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    pub requeued_transfers: u64,
    pub requeued_post_processing: u64,
}

impl Store {
    /// Open (creating if necessary) the database at `path` and run migrations.
    /// The database holds credentials, so it and SQLite's companion files are
    /// made readable by the owner only (SQLite creates `-wal`/`-shm` with the
    /// database file's permissions).
    pub async fn open(path: &Path) -> AppResult<Self> {
        crate::fsutil::create_private_file(path, &["-wal", "-shm"]).map_err(|e| {
            tracing::error!(error = %e, path = %path.display(), "cannot secure database file");
            AppError::new(
                ErrorKind::Filesystem,
                "cannot create or secure the database file",
            )
        })?;
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        Self::init(pool).await
    }

    /// In-memory database for tests. Uses a single connection because each
    /// SQLite in-memory connection is a separate database.
    #[cfg(test)]
    pub async fn open_in_memory() -> AppResult<Self> {
        let options = <SqliteConnectOptions as std::str::FromStr>::from_str("sqlite::memory:")?
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        Self::init(pool).await
    }

    async fn init(pool: SqlitePool) -> AppResult<Self> {
        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    #[cfg(test)]
    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn ping(&self) -> AppResult<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    pub async fn insert_download(
        &self,
        new: NewDownload,
        now: DateTime<Utc>,
    ) -> AppResult<Download> {
        let id = Uuid::new_v4();
        let ts = now.timestamp_millis();
        let mut tx = self.pool.begin().await?;
        let row: DownloadRow = sqlx::query_as(
            "INSERT INTO downloads
                (id, url, display_url, provider, download_password, filename, media_type,
                 title, year, artwork_id, owner_id, state, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'queued', ?, ?)
             RETURNING *",
        )
        .bind(id.to_string())
        .bind(&new.url)
        .bind(&new.display_url)
        .bind(&new.provider)
        .bind(&new.password)
        .bind(&new.filename)
        .bind(new.media_type.map(MediaType::as_str))
        .bind(&new.title)
        .bind(new.year)
        .bind(&new.artwork_id)
        .bind(new.owner_id)
        .bind(ts)
        .bind(ts)
        .fetch_one(&mut *tx)
        .await?;
        insert_event(&mut tx, id, ts, "submitted", "download submitted").await?;
        tx.commit().await?;
        row.try_into()
    }

    pub async fn get_download(&self, id: Uuid) -> AppResult<Option<Download>> {
        let row: Option<DownloadRow> = sqlx::query_as("SELECT * FROM downloads WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// Newest first. An empty `states` slice means all states; `owner`
    /// limits the list to one account's downloads.
    pub async fn list_downloads(
        &self,
        states: &[TransferState],
        owner: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> AppResult<Vec<Download>> {
        let filter = (!states.is_empty()).then(|| {
            let names: Vec<&str> = states.iter().map(|s| s.as_str()).collect();
            serde_json::to_string(&names).expect("string list serializes")
        });
        let rows: Vec<DownloadRow> = sqlx::query_as(
            "SELECT * FROM downloads
             WHERE (?1 IS NULL OR state IN (SELECT value FROM json_each(?1)))
               AND (?4 IS NULL OR owner_id = ?4)
             ORDER BY created_at DESC, rowid DESC
             LIMIT ?2 OFFSET ?3",
        )
        .bind(filter)
        .bind(limit)
        .bind(offset)
        .bind(owner)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub async fn list_events(&self, id: Uuid) -> AppResult<Vec<DownloadEvent>> {
        let rows: Vec<EventRow> =
            sqlx::query_as("SELECT * FROM download_events WHERE download_id = ? ORDER BY id")
                .bind(id.to_string())
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// Atomically move a download from `from` to `to`.
    ///
    /// Returns `Conflict` if the transition is not allowed or if the download
    /// is no longer in `from` (another actor changed it first), and `NotFound`
    /// if the download does not exist.
    pub async fn transition(
        &self,
        id: Uuid,
        from: TransferState,
        to: TransferState,
        details: TransitionDetails,
        now: DateTime<Utc>,
    ) -> AppResult<Download> {
        if !from.can_transition_to(to) {
            return Err(AppError::conflict(format!(
                "cannot change download from {} to {}",
                from.as_str(),
                to.as_str()
            )));
        }
        let ts = now.timestamp_millis();
        let clears_error = matches!(
            to,
            TransferState::Queued | TransferState::Downloading | TransferState::Completed
        );
        let (error_kind, error_message) = match &details.error {
            Some(e) => (Some(e.kind.as_str()), Some(e.message.as_str())),
            None => (None, None),
        };
        let entering_retry = to == TransferState::Retrying;
        let completed = to == TransferState::Completed;
        // A manual retry/resume from a terminal-ish state starts a fresh retry budget.
        let resets_retries = to == TransferState::Queued
            && matches!(
                from,
                TransferState::Failed | TransferState::Cancelled | TransferState::Blocked
            );

        let mut tx = self.pool.begin().await?;
        let row: Option<DownloadRow> = sqlx::query_as(
            "UPDATE downloads SET
                state = ?1,
                updated_at = ?2,
                error_kind = CASE WHEN ?3 IS NOT NULL THEN ?3 WHEN ?4 THEN NULL ELSE error_kind END,
                error_message = CASE WHEN ?3 IS NOT NULL THEN ?5 WHEN ?4 THEN NULL ELSE error_message END,
                next_attempt_at = ?6,
                retry_count = CASE WHEN ?7 THEN retry_count + 1 WHEN ?8 THEN 0 ELSE retry_count END,
                completed_at = CASE WHEN ?9 THEN ?2 ELSE completed_at END,
                post_state = CASE WHEN ?9 THEN 'pending' ELSE post_state END
             WHERE id = ?10 AND state = ?11
             RETURNING *",
        )
        .bind(to.as_str())
        .bind(ts)
        .bind(error_kind)
        .bind(clears_error)
        .bind(error_message)
        .bind(details.next_attempt_at.map(|t| t.timestamp_millis()))
        .bind(entering_retry)
        .bind(resets_retries)
        .bind(completed)
        .bind(id.to_string())
        .bind(from.as_str())
        .fetch_optional(&mut *tx)
        .await?;

        let Some(row) = row else {
            tx.rollback().await?;
            return Err(self.missing_or_conflict(id, from).await);
        };
        let message = details
            .message
            .unwrap_or_else(|| format!("{} -> {}", from.as_str(), to.as_str()));
        insert_event(&mut tx, id, ts, to.as_str(), &message).await?;
        tx.commit().await?;
        row.try_into()
    }

    /// Atomically move post-processing from `from` to `to`.
    pub async fn transition_post(
        &self,
        id: Uuid,
        from: PostState,
        to: PostState,
        error: Option<DownloadError>,
        now: DateTime<Utc>,
    ) -> AppResult<Download> {
        let details = PostDetails {
            error,
            ..Default::default()
        };
        self.transition_post_with(id, from, to, details, now).await
    }

    /// Like [`Store::transition_post`], with the organized file's new
    /// location and the event message. Leaving `Processing` for a final
    /// state forgets the recorded move target.
    pub async fn transition_post_with(
        &self,
        id: Uuid,
        from: PostState,
        to: PostState,
        details: PostDetails,
        now: DateTime<Utc>,
    ) -> AppResult<Download> {
        if !from.can_transition_to(to) {
            return Err(AppError::conflict(format!(
                "cannot change post-processing from {} to {}",
                from.as_str(),
                to.as_str()
            )));
        }
        let ts = now.timestamp_millis();
        let (kind, message) = match &details.error {
            Some(e) => (Some(e.kind.as_str()), Some(e.message.as_str())),
            None => (None, None),
        };
        let forget_target = matches!(
            to,
            PostState::Completed | PostState::Failed | PostState::Skipped
        );
        let mut tx = self.pool.begin().await?;
        let row: Option<DownloadRow> = sqlx::query_as(
            "UPDATE downloads SET
                post_state = ?1, updated_at = ?2, post_error_kind = ?3, post_error_message = ?4,
                file_path = COALESCE(?7, file_path),
                post_target = CASE WHEN ?8 THEN NULL ELSE post_target END
             WHERE id = ?5 AND post_state = ?6 AND state = 'completed'
             RETURNING *",
        )
        .bind(to.as_str())
        .bind(ts)
        .bind(kind)
        .bind(message)
        .bind(id.to_string())
        .bind(from.as_str())
        .bind(details.file_path.as_deref())
        .bind(forget_target)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Err(match self.get_download(id).await? {
                None => AppError::not_found("download not found"),
                Some(_) => {
                    AppError::conflict("download post-processing state changed concurrently")
                }
            });
        };
        let event_message = match (&details.message, &details.error) {
            (Some(m), _) => m.clone(),
            (None, Some(e)) => format!("post-processing {}: {}", to.as_str(), e.message),
            (None, None) => format!("post-processing {}", to.as_str()),
        };
        insert_event(
            &mut tx,
            id,
            ts,
            &format!("post_{}", to.as_str()),
            &event_message,
        )
        .await?;
        tx.commit().await?;
        row.try_into()
    }

    /// The completed download that has waited longest for post-processing.
    pub async fn next_post_pending(&self) -> AppResult<Option<Uuid>> {
        let id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM downloads WHERE state = 'completed' AND post_state = 'pending'
             ORDER BY completed_at, rowid LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        id.map(|id| Uuid::parse_str(&id).map_err(|_| corrupt("invalid download id")))
            .transpose()
    }

    /// The completed download of the file at `path` whose post-processing
    /// was skipped: the first volume of a split archive, waiting for the
    /// others.
    pub async fn skipped_with_file(&self, path: &str) -> AppResult<Option<Uuid>> {
        let id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM downloads WHERE state = 'completed' AND post_state = 'skipped'
             AND file_path = ?1 ORDER BY completed_at DESC, rowid DESC LIMIT 1",
        )
        .bind(path)
        .fetch_optional(&self.pool)
        .await?;
        id.map(|id| Uuid::parse_str(&id).map_err(|_| corrupt("invalid download id")))
            .transpose()
    }

    /// Record where post-processing is about to move the file. Only applies
    /// while it is `Processing`; returns whether a row was updated.
    pub async fn set_post_target(
        &self,
        id: Uuid,
        target: &str,
        now: DateTime<Utc>,
    ) -> AppResult<bool> {
        let result = sqlx::query(
            "UPDATE downloads SET post_target = ?1, updated_at = ?2
             WHERE id = ?3 AND post_state = 'processing'",
        )
        .bind(target)
        .bind(now.timestamp_millis())
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Record transfer progress without changing state. Only applies while the
    /// download is `Downloading`; returns whether a row was updated.
    pub async fn update_progress(
        &self,
        id: Uuid,
        downloaded_bytes: i64,
        total_bytes: Option<i64>,
        filename: Option<&str>,
        now: DateTime<Utc>,
    ) -> AppResult<bool> {
        let result = sqlx::query(
            "UPDATE downloads SET
                downloaded_bytes = ?1,
                total_bytes = COALESCE(?2, total_bytes),
                filename = COALESCE(?3, filename),
                updated_at = ?4
             WHERE id = ?5 AND state = 'downloading'",
        )
        .bind(downloaded_bytes)
        .bind(total_bytes)
        .bind(filename)
        .bind(now.timestamp_millis())
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Record what the server told us about the resource at the start of a
    /// transfer. Only applies while the download is `Downloading`.
    pub async fn update_transfer_info(
        &self,
        id: Uuid,
        info: &TransferInfo,
        now: DateTime<Utc>,
    ) -> AppResult<bool> {
        let result = sqlx::query(
            "UPDATE downloads SET
                total_bytes = ?1,
                filename = COALESCE(?2, filename),
                resume_etag = ?3,
                resume_last_modified = ?4,
                downloaded_bytes = ?5,
                updated_at = ?6
             WHERE id = ?7 AND state = 'downloading'",
        )
        .bind(info.total_bytes)
        .bind(info.filename.as_deref())
        .bind(info.etag.as_deref())
        .bind(info.last_modified.as_deref())
        .bind(info.downloaded_bytes)
        .bind(now.timestamp_millis())
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Replace the download password of a download that is not running or
    /// queued (it takes effect on the next attempt). Returns whether it applied.
    pub async fn set_download_password(
        &self,
        id: Uuid,
        password: &str,
        now: DateTime<Utc>,
    ) -> AppResult<bool> {
        let result = sqlx::query(
            "UPDATE downloads SET download_password = ?1, updated_at = ?2
             WHERE id = ?3 AND state IN ('paused', 'blocked', 'failed', 'cancelled')",
        )
        .bind(password)
        .bind(now.timestamp_millis())
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Delete a download and its event history if it is still in `state` and
    /// not being post-processed. Returns whether a row was deleted.
    pub async fn delete_download(&self, id: Uuid, state: TransferState) -> AppResult<bool> {
        let result = sqlx::query(
            "DELETE FROM downloads
             WHERE id = ?1 AND state = ?2 AND post_state != 'processing'",
        )
        .bind(id.to_string())
        .bind(state.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Delete every download in `state` that is not being post-processed, and
    /// their event history; only `owner`'s when given. Returns the deleted ids.
    pub async fn delete_all_in(
        &self,
        state: TransferState,
        owner: Option<i64>,
    ) -> AppResult<Vec<Uuid>> {
        let ids: Vec<String> = sqlx::query_scalar(
            "DELETE FROM downloads
             WHERE state = ?1 AND post_state != 'processing' AND (?2 IS NULL OR owner_id = ?2)
             RETURNING id",
        )
        .bind(state.as_str())
        .bind(owner)
        .fetch_all(&self.pool)
        .await?;
        ids.iter()
            .map(|id| Uuid::parse_str(id).map_err(|_| corrupt("invalid download id")))
            .collect()
    }

    /// Record the final destination before the completed file is moved there.
    pub async fn set_file_path(&self, id: Uuid, path: &str, now: DateTime<Utc>) -> AppResult<bool> {
        let result = sqlx::query(
            "UPDATE downloads SET file_path = ?1, updated_at = ?2
             WHERE id = ?3 AND state = 'downloading'",
        )
        .bind(path)
        .bind(now.timestamp_millis())
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Atomically claim the next runnable download for a worker: the oldest
    /// `Queued` job, or a `Retrying` job whose backoff has elapsed. The single
    /// UPDATE guarantees two workers can never claim the same job.
    pub async fn claim_next(&self, now: DateTime<Utc>) -> AppResult<Option<Download>> {
        let ts = now.timestamp_millis();
        let mut tx = self.pool.begin().await?;
        let row: Option<DownloadRow> = sqlx::query_as(
            "UPDATE downloads SET state = 'downloading', updated_at = ?1, next_attempt_at = NULL
             WHERE id = (
                SELECT id FROM downloads
                WHERE state = 'queued' OR (state = 'retrying' AND next_attempt_at <= ?1)
                ORDER BY created_at, rowid
                LIMIT 1
             )
             RETURNING *",
        )
        .bind(ts)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Ok(None);
        };
        let download: Download = row.try_into()?;
        insert_event(&mut tx, download.id, ts, "downloading", "transfer started").await?;
        tx.commit().await?;
        Ok(Some(download))
    }

    /// Earliest `next_attempt_at` among retrying jobs, so the scheduler knows
    /// when to wake up.
    pub async fn next_retry_at(&self) -> AppResult<Option<DateTime<Utc>>> {
        let ts: Option<i64> = sqlx::query_scalar(
            "SELECT MIN(next_attempt_at) FROM downloads WHERE state = 'retrying'",
        )
        .fetch_one(&self.pool)
        .await?;
        ts.map(from_millis).transpose()
    }

    /// Reconcile state after a restart. Any job that was mid-transfer or
    /// mid-organization when the process stopped is put back in line. Partial
    /// files are kept; the engine verifies them before resuming. Nothing is
    /// ever marked completed here.
    pub async fn recover_interrupted(&self, now: DateTime<Utc>) -> AppResult<RecoveryReport> {
        let ts = now.timestamp_millis();
        let mut tx = self.pool.begin().await?;
        let transfers: Vec<String> = sqlx::query_scalar(
            "UPDATE downloads SET state = 'queued', updated_at = ?1
             WHERE state = 'downloading' RETURNING id",
        )
        .bind(ts)
        .fetch_all(&mut *tx)
        .await?;
        for id in &transfers {
            sqlx::query(
                "INSERT INTO download_events (download_id, at, kind, message)
                 VALUES (?, ?, 'queued', 'transfer interrupted by restart; re-queued')",
            )
            .bind(id)
            .bind(ts)
            .execute(&mut *tx)
            .await?;
        }
        let post: Vec<String> = sqlx::query_scalar(
            "UPDATE downloads SET post_state = 'pending', updated_at = ?1
             WHERE post_state = 'processing' RETURNING id",
        )
        .bind(ts)
        .fetch_all(&mut *tx)
        .await?;
        for id in &post {
            sqlx::query(
                "INSERT INTO download_events (download_id, at, kind, message)
                 VALUES (?, ?, 'post_pending', 'post-processing interrupted by restart; re-queued')",
            )
            .bind(id)
            .bind(ts)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(RecoveryReport {
            requeued_transfers: transfers.len() as u64,
            requeued_post_processing: post.len() as u64,
        })
    }

    async fn missing_or_conflict(&self, id: Uuid, expected: TransferState) -> AppError {
        match self.get_download(id).await {
            Ok(None) => AppError::not_found("download not found"),
            Ok(Some(d)) => AppError::conflict(format!(
                "download is {} (expected {})",
                d.state.as_str(),
                expected.as_str()
            )),
            Err(e) => e,
        }
    }
}

async fn insert_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: Uuid,
    ts: i64,
    kind: &str,
    message: &str,
) -> AppResult<()> {
    sqlx::query("INSERT INTO download_events (download_id, at, kind, message) VALUES (?, ?, ?, ?)")
        .bind(id.to_string())
        .bind(ts)
        .bind(kind)
        .bind(message)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub(crate) fn from_millis(ms: i64) -> AppResult<DateTime<Utc>> {
    DateTime::from_timestamp_millis(ms).ok_or_else(|| corrupt("timestamp out of range"))
}

pub(crate) fn corrupt(what: &str) -> AppError {
    tracing::error!(what, "corrupt database row");
    AppError::new(ErrorKind::Persistence, "stored data is corrupt")
}

fn stored_error(kind: Option<String>, message: Option<String>) -> AppResult<Option<DownloadError>> {
    match (kind, message) {
        (Some(kind), message) => Ok(Some(DownloadError {
            kind: ErrorKind::parse(&kind).ok_or_else(|| corrupt("unknown error kind"))?,
            message: message.unwrap_or_default(),
        })),
        (None, _) => Ok(None),
    }
}

#[derive(FromRow)]
struct DownloadRow {
    id: String,
    url: String,
    display_url: String,
    provider: String,
    state: String,
    post_state: String,
    filename: Option<String>,
    total_bytes: Option<i64>,
    downloaded_bytes: i64,
    retry_count: i64,
    next_attempt_at: Option<i64>,
    error_kind: Option<String>,
    error_message: Option<String>,
    post_error_kind: Option<String>,
    post_error_message: Option<String>,
    resume_etag: Option<String>,
    resume_last_modified: Option<String>,
    file_path: Option<String>,
    download_password: Option<String>,
    media_type: Option<String>,
    title: Option<String>,
    artwork_id: Option<String>,
    year: Option<i64>,
    post_target: Option<String>,
    owner_id: Option<i64>,
    created_at: i64,
    updated_at: i64,
    completed_at: Option<i64>,
}

impl TryFrom<DownloadRow> for Download {
    type Error = AppError;

    fn try_from(r: DownloadRow) -> AppResult<Self> {
        Ok(Self {
            id: Uuid::parse_str(&r.id).map_err(|_| corrupt("invalid download id"))?,
            url: r.url,
            display_url: r.display_url,
            provider: r.provider,
            state: TransferState::parse(&r.state).ok_or_else(|| corrupt("unknown state"))?,
            post_state: PostState::parse(&r.post_state)
                .ok_or_else(|| corrupt("unknown post_state"))?,
            filename: r.filename,
            total_bytes: r.total_bytes,
            downloaded_bytes: r.downloaded_bytes,
            retry_count: r.retry_count,
            next_attempt_at: r.next_attempt_at.map(from_millis).transpose()?,
            error: stored_error(r.error_kind, r.error_message)?,
            post_error: stored_error(r.post_error_kind, r.post_error_message)?,
            file_path: r.file_path,
            resume_etag: r.resume_etag,
            resume_last_modified: r.resume_last_modified,
            download_password: r.download_password,
            media_type: r
                .media_type
                .map(|t| MediaType::parse(&t).ok_or_else(|| corrupt("unknown media_type")))
                .transpose()?,
            title: r.title,
            artwork_id: r.artwork_id,
            year: r.year.and_then(|y| u16::try_from(y).ok()),
            post_target: r.post_target,
            owner_id: r.owner_id,
            created_at: from_millis(r.created_at)?,
            updated_at: from_millis(r.updated_at)?,
            completed_at: r.completed_at.map(from_millis).transpose()?,
        })
    }
}

#[derive(FromRow)]
struct EventRow {
    id: i64,
    download_id: String,
    at: i64,
    kind: String,
    message: String,
}

impl TryFrom<EventRow> for DownloadEvent {
    type Error = AppError;

    fn try_from(r: EventRow) -> AppResult<Self> {
        Ok(Self {
            id: r.id,
            download_id: Uuid::parse_str(&r.download_id)
                .map_err(|_| corrupt("invalid download id"))?,
            at: from_millis(r.at)?,
            kind: r.kind,
            message: r.message,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TransferState::*;

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    fn new_dl(name: &str) -> NewDownload {
        NewDownload {
            url: format!("https://example.com/{name}?token=secret"),
            display_url: format!("https://example.com/{name}?…"),
            provider: "http".into(),
            ..Default::default()
        }
    }

    async fn store() -> Store {
        Store::open_in_memory().await.unwrap()
    }

    #[tokio::test]
    async fn insert_and_get() {
        let s = store().await;
        let d = s.insert_download(new_dl("a"), t(0)).await.unwrap();
        assert_eq!(d.state, Queued);
        assert_eq!(d.post_state, PostState::None);
        let got = s.get_download(d.id).await.unwrap().unwrap();
        assert_eq!(got, d);
        assert!(s.get_download(Uuid::new_v4()).await.unwrap().is_none());

        let events = s.list_events(d.id).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "submitted");
    }

    #[tokio::test]
    async fn claims_in_fifo_order_and_never_twice() {
        let s = store().await;
        let a = s.insert_download(new_dl("a"), t(0)).await.unwrap();
        let b = s.insert_download(new_dl("b"), t(1)).await.unwrap();

        let first = s.claim_next(t(2)).await.unwrap().unwrap();
        assert_eq!(first.id, a.id);
        assert_eq!(first.state, Downloading);
        let second = s.claim_next(t(2)).await.unwrap().unwrap();
        assert_eq!(second.id, b.id);
        assert!(s.claim_next(t(2)).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn retrying_jobs_wait_for_backoff() {
        let s = store().await;
        let d = s.insert_download(new_dl("a"), t(0)).await.unwrap();
        s.claim_next(t(1)).await.unwrap().unwrap();
        let retry = s
            .transition(
                d.id,
                Downloading,
                Retrying,
                TransitionDetails {
                    error: Some(DownloadError {
                        kind: ErrorKind::Network,
                        message: "connection reset".into(),
                    }),
                    next_attempt_at: Some(t(60)),
                    message: None,
                },
                t(2),
            )
            .await
            .unwrap();
        assert_eq!(retry.retry_count, 1);
        assert_eq!(retry.error.as_ref().unwrap().kind, ErrorKind::Network);
        assert_eq!(s.next_retry_at().await.unwrap(), Some(t(60)));

        assert!(s.claim_next(t(59)).await.unwrap().is_none());
        let claimed = s.claim_next(t(60)).await.unwrap().unwrap();
        assert_eq!(claimed.id, d.id);
        assert_eq!(claimed.next_attempt_at, None);
        // The last error stays visible while a retry runs... until it succeeds.
        let done = s
            .transition(
                d.id,
                Downloading,
                Completed,
                TransitionDetails::default(),
                t(61),
            )
            .await
            .unwrap();
        assert_eq!(done.error, None);
        assert_eq!(done.post_state, PostState::Pending);
        assert_eq!(done.completed_at, Some(t(61)));
    }

    #[tokio::test]
    async fn transition_is_compare_and_swap() {
        let s = store().await;
        let d = s.insert_download(new_dl("a"), t(0)).await.unwrap();
        s.transition(d.id, Queued, Paused, TransitionDetails::default(), t(1))
            .await
            .unwrap();
        // A stale actor that still believes the job is queued is rejected.
        let err = s
            .transition(d.id, Queued, Cancelled, TransitionDetails::default(), t(2))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
        assert_eq!(s.get_download(d.id).await.unwrap().unwrap().state, Paused);

        let err = s
            .transition(
                Uuid::new_v4(),
                Queued,
                Paused,
                TransitionDetails::default(),
                t(2),
            )
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn invalid_transitions_rejected() {
        let s = store().await;
        let d = s.insert_download(new_dl("a"), t(0)).await.unwrap();
        let err = s
            .transition(d.id, Queued, Completed, TransitionDetails::default(), t(1))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
        assert_eq!(s.get_download(d.id).await.unwrap().unwrap().state, Queued);
    }

    #[tokio::test]
    async fn manual_retry_resets_budget_and_clears_error() {
        let s = store().await;
        let d = s.insert_download(new_dl("a"), t(0)).await.unwrap();
        s.claim_next(t(1)).await.unwrap();
        s.transition(
            d.id,
            Downloading,
            Retrying,
            TransitionDetails {
                next_attempt_at: Some(t(2)),
                ..Default::default()
            },
            t(1),
        )
        .await
        .unwrap();
        s.claim_next(t(2)).await.unwrap();
        let failed = s
            .transition(
                d.id,
                Downloading,
                Failed,
                TransitionDetails {
                    error: Some(DownloadError {
                        kind: ErrorKind::RemoteUnavailable,
                        message: "HTTP 404".into(),
                    }),
                    ..Default::default()
                },
                t(3),
            )
            .await
            .unwrap();
        assert_eq!(failed.retry_count, 1);
        assert!(failed.error.is_some());

        let requeued = s
            .transition(d.id, Failed, Queued, TransitionDetails::default(), t(4))
            .await
            .unwrap();
        assert_eq!(requeued.retry_count, 0);
        assert_eq!(requeued.error, None);
    }

    #[tokio::test]
    async fn progress_only_while_downloading() {
        let s = store().await;
        let d = s.insert_download(new_dl("a"), t(0)).await.unwrap();
        assert!(
            !s.update_progress(d.id, 10, Some(100), None, t(1))
                .await
                .unwrap()
        );
        s.claim_next(t(1)).await.unwrap();
        assert!(
            s.update_progress(d.id, 10, Some(100), Some("a.mkv"), t(2))
                .await
                .unwrap()
        );
        assert!(s.update_progress(d.id, 50, None, None, t(3)).await.unwrap());
        let got = s.get_download(d.id).await.unwrap().unwrap();
        assert_eq!(got.downloaded_bytes, 50);
        assert_eq!(got.total_bytes, Some(100));
        assert_eq!(got.filename.as_deref(), Some("a.mkv"));
    }

    #[tokio::test]
    async fn transfer_info_and_file_path() {
        let s = store().await;
        let d = s.insert_download(new_dl("a"), t(0)).await.unwrap();
        let info = TransferInfo {
            total_bytes: Some(1000),
            filename: Some("a.mkv".into()),
            etag: Some("\"abc\"".into()),
            last_modified: None,
            downloaded_bytes: 200,
        };
        assert!(!s.update_transfer_info(d.id, &info, t(1)).await.unwrap());
        assert!(!s.set_file_path(d.id, "/x/a.mkv", t(1)).await.unwrap());
        s.claim_next(t(1)).await.unwrap();
        assert!(s.update_transfer_info(d.id, &info, t(2)).await.unwrap());
        assert!(s.set_file_path(d.id, "/x/a.mkv", t(2)).await.unwrap());
        let got = s.get_download(d.id).await.unwrap().unwrap();
        assert_eq!(got.total_bytes, Some(1000));
        assert_eq!(got.downloaded_bytes, 200);
        assert_eq!(got.resume_etag.as_deref(), Some("\"abc\""));
        assert_eq!(got.file_path.as_deref(), Some("/x/a.mkv"));
    }

    #[tokio::test]
    async fn recovery_requeues_interrupted_work_without_completing_it() {
        let s = store().await;
        let running = s.insert_download(new_dl("running"), t(0)).await.unwrap();
        let organizing = s.insert_download(new_dl("organizing"), t(1)).await.unwrap();
        let paused = s.insert_download(new_dl("paused"), t(2)).await.unwrap();

        // `organizing` completes its transfer and starts post-processing.
        s.claim_next(t(3)).await.unwrap();
        s.update_progress(running.id, 500, Some(1000), None, t(3))
            .await
            .unwrap();
        s.claim_next(t(3)).await.unwrap();
        s.transition(
            organizing.id,
            Downloading,
            Completed,
            TransitionDetails::default(),
            t(4),
        )
        .await
        .unwrap();
        s.transition_post(
            organizing.id,
            PostState::Pending,
            PostState::Processing,
            None,
            t(5),
        )
        .await
        .unwrap();
        s.transition(
            paused.id,
            Queued,
            Paused,
            TransitionDetails::default(),
            t(5),
        )
        .await
        .unwrap();

        let report = s.recover_interrupted(t(10)).await.unwrap();
        assert_eq!(
            report,
            RecoveryReport {
                requeued_transfers: 1,
                requeued_post_processing: 1
            }
        );

        let r = s.get_download(running.id).await.unwrap().unwrap();
        assert_eq!(r.state, Queued);
        assert_eq!(r.downloaded_bytes, 500, "partial progress is preserved");
        assert_eq!(r.completed_at, None);

        let o = s.get_download(organizing.id).await.unwrap().unwrap();
        assert_eq!(o.state, Completed);
        assert_eq!(o.post_state, PostState::Pending);

        assert_eq!(
            s.get_download(paused.id).await.unwrap().unwrap().state,
            Paused
        );

        let kinds: Vec<_> = s
            .list_events(running.id)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(kinds, ["submitted", "downloading", "queued"]);

        // Recovery is idempotent.
        assert_eq!(
            s.recover_interrupted(t(11)).await.unwrap(),
            RecoveryReport::default()
        );
    }

    #[tokio::test]
    async fn post_failure_keeps_transfer_completed() {
        let s = store().await;
        let d = s.insert_download(new_dl("a"), t(0)).await.unwrap();
        // Post-processing can't start before the transfer completes.
        let err = s
            .transition_post(d.id, PostState::None, PostState::Pending, None, t(0))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);

        s.claim_next(t(1)).await.unwrap();
        s.transition(
            d.id,
            Downloading,
            Completed,
            TransitionDetails::default(),
            t(2),
        )
        .await
        .unwrap();
        s.transition_post(d.id, PostState::Pending, PostState::Processing, None, t(3))
            .await
            .unwrap();
        let failed = s
            .transition_post(
                d.id,
                PostState::Processing,
                PostState::Failed,
                Some(DownloadError {
                    kind: ErrorKind::MediaIdentification,
                    message: "ambiguous title".into(),
                }),
                t(4),
            )
            .await
            .unwrap();
        assert_eq!(failed.state, Completed);
        assert_eq!(failed.post_state, PostState::Failed);
        assert_eq!(failed.error, None);
        assert_eq!(failed.post_error.unwrap().message, "ambiguous title");
    }

    #[tokio::test]
    async fn list_filters_and_orders_newest_first() {
        let s = store().await;
        let a = s.insert_download(new_dl("a"), t(0)).await.unwrap();
        let b = s.insert_download(new_dl("b"), t(1)).await.unwrap();
        s.transition(a.id, Queued, Paused, TransitionDetails::default(), t(2))
            .await
            .unwrap();

        let all = s.list_downloads(&[], None, 50, 0).await.unwrap();
        assert_eq!(all.iter().map(|d| d.id).collect::<Vec<_>>(), [b.id, a.id]);
        let paused = s.list_downloads(&[Paused], None, 50, 0).await.unwrap();
        assert_eq!(paused.len(), 1);
        assert_eq!(paused[0].id, a.id);
        assert_eq!(s.list_downloads(&[], None, 1, 1).await.unwrap()[0].id, a.id);
        let both = s
            .list_downloads(&[Paused, Queued], None, 50, 0)
            .await
            .unwrap();
        assert_eq!(both.len(), 2);
        assert!(
            s.list_downloads(&[Failed], None, 50, 0)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn deletes_only_downloads_in_the_given_state() {
        let s = store().await;
        let a = s.insert_download(new_dl("a"), t(0)).await.unwrap();
        let b = s.insert_download(new_dl("b"), t(0)).await.unwrap();
        let c = s.insert_download(new_dl("c"), t(0)).await.unwrap();
        for id in [a.id, b.id] {
            s.transition(id, Queued, Cancelled, TransitionDetails::default(), t(1))
                .await
                .unwrap();
        }

        assert!(
            !s.delete_download(c.id, Cancelled).await.unwrap(),
            "queued is kept"
        );
        assert!(s.delete_download(a.id, Cancelled).await.unwrap());
        assert!(!s.delete_download(a.id, Cancelled).await.unwrap());
        assert!(s.get_download(a.id).await.unwrap().is_none());
        assert!(s.list_events(a.id).await.unwrap().is_empty());

        assert_eq!(s.delete_all_in(Cancelled, None).await.unwrap(), [b.id]);
        assert!(s.delete_all_in(Cancelled, None).await.unwrap().is_empty());
        let left = s.list_downloads(&[], None, 50, 0).await.unwrap();
        assert_eq!(left.iter().map(|d| d.id).collect::<Vec<_>>(), [c.id]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn database_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("direkt.db");
        let s = Store::open(&path).await.unwrap();
        s.insert_download(new_dl("a"), t(0)).await.unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let file = dir.path().join(format!("direkt.db{suffix}"));
            if file.exists() {
                let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{}", file.display());
            }
        }
    }

    #[tokio::test]
    async fn file_database_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("direkt.db");
        let id = {
            let s = Store::open(&path).await.unwrap();
            let d = s.insert_download(new_dl("a"), t(0)).await.unwrap();
            s.claim_next(t(1)).await.unwrap();
            s.close().await;
            d.id
        };
        let s = Store::open(&path).await.unwrap();
        assert_eq!(
            s.recover_interrupted(t(2))
                .await
                .unwrap()
                .requeued_transfers,
            1
        );
        let d = s.get_download(id).await.unwrap().unwrap();
        assert_eq!(d.state, Queued);
        assert_eq!(d.url, "https://example.com/a?token=secret");
    }
}
