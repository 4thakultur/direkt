//! Application service for downloads.
//!
//! This is the single entry point for download operations, so the API, the
//! web UI and future integrations share the same validation and state rules.
//!
//! Submitting can take two steps: [`DownloadService::preview`] looks the link
//! up (name, size, suggested type, title and artwork) and keeps the result,
//! with the password, under a preview id for an hour;
//! [`DownloadService::confirm`] then queues it with the type the user chose,
//! and the password, when it is given there instead. The password never
//! leaves the server.

use crate::domain::{Download, DownloadEvent, MediaType, PostState, TransferState};
use crate::engine::{Engine, LiveSnapshot, StopReason, ThroughputSnapshot, filename_from_url};
use crate::error::{AppError, AppResult, Disposition, ErrorKind};
use crate::media::parse::{detect, guess_as};
use crate::media::{Artwork, ArtworkService, Lookup};
use crate::net::{NetworkPolicy, redact_url, validate_submission_url};
use crate::providers::{Provider, Providers, ResolveRequest, ResourceInfo};
use crate::store::{NewDownload, Store, TransitionDetails};
use chrono::Utc;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use url::Url;
use uuid::Uuid;

/// Bound on optimistic-concurrency retries when a download changes state
/// while a user action is being applied.
const MAX_STATE_RACES: usize = 40;
/// How long a preview waits for the provider to describe the link.
const INSPECT_TIMEOUT: Duration = Duration::from_secs(25);
/// How long a preview waits for the artwork lookup.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a preview can be confirmed.
const PREVIEW_TTL: Duration = Duration::from_secs(3600);
/// Previews kept at once; the oldest are dropped beyond this.
const MAX_PREVIEWS: usize = 128;
/// Previews being looked up at once.
const MAX_CONCURRENT_PREVIEWS: usize = 4;

#[derive(Clone)]
pub struct DownloadService {
    store: Store,
    engine: Engine,
    providers: Providers,
    policy: NetworkPolicy,
    artwork: ArtworkService,
    previews: Arc<Mutex<HashMap<Uuid, Pending>>>,
    preview_permits: Arc<Semaphore>,
}

/// What the confirmation step shows for a link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct Preview {
    /// Pass to `confirm` (`preview_id` when creating a download).
    pub id: Uuid,
    pub provider: String,
    /// Submitted URL with the query string redacted.
    pub display_url: String,
    pub filename: Option<String>,
    pub size_bytes: Option<u64>,
    /// The size is rounded, as the provider displays it.
    pub size_approximate: bool,
    /// The type the download will get: detected, or chosen with `retype`.
    pub media_type: MediaType,
    /// The type Direkt detected from the name.
    pub detected_type: MediaType,
    /// Title found by the metadata lookup.
    pub title: Option<String>,
    pub year: Option<u16>,
    /// Cached artwork, served at `/artwork/{id}`.
    pub artwork_id: Option<String>,
    /// Why some details are missing, e.g. no TMDB key is set.
    pub notes: Vec<String>,
    /// Whether the provider takes a download password, which can still be
    /// given when confirming.
    pub accepts_password: bool,
}

/// A preview waiting for confirmation. Holds the password: never serialized.
#[derive(Clone)]
struct Pending {
    url: Url,
    provider: &'static str,
    password: Option<String>,
    preview: Preview,
    /// Notes from inspecting the link, kept when the type changes.
    inspect_notes: Vec<String>,
    created: Instant,
}

impl DownloadService {
    pub fn new(
        store: Store,
        engine: Engine,
        providers: Providers,
        policy: NetworkPolicy,
        artwork: ArtworkService,
    ) -> Self {
        Self {
            store,
            engine,
            providers,
            policy,
            artwork,
            previews: Arc::default(),
            preview_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_PREVIEWS)),
        }
    }

    /// Validate a URL, pick its provider, and queue it. `password` is for
    /// password-protected files on providers that support it.
    pub async fn submit(
        &self,
        raw_url: &str,
        password: Option<&str>,
        media_type: Option<MediaType>,
    ) -> AppResult<Download> {
        let (url, provider, password) = self.validate(raw_url, password)?;
        self.queue(NewDownload {
            display_url: redact_url(&url),
            url: url.into(),
            provider: provider.id().to_owned(),
            password,
            media_type,
            ..Default::default()
        })
        .await
    }

    /// Look a link up for the confirmation step: its name and size from the
    /// provider, a suggested type, and a title and artwork when found. A link
    /// that is gone or forbidden is an error; anything else that cannot be
    /// read is reported in `notes`, and the download can still be confirmed.
    pub async fn preview(&self, raw_url: &str, password: Option<&str>) -> AppResult<Preview> {
        let (url, provider, password) = self.validate(raw_url, password)?;
        let _permit = self
            .preview_permits
            .acquire()
            .await
            .map_err(|_| AppError::internal("shutting down"))?;
        let mut notes = Vec::new();
        let request = ResolveRequest {
            url: &url,
            password: password.as_deref(),
        };
        let info = match tokio::time::timeout(INSPECT_TIMEOUT, provider.inspect(request)).await {
            Ok(Ok(info)) => info,
            Ok(Err(e))
                if e.disposition == Disposition::Fail
                    && matches!(
                        e.kind,
                        ErrorKind::RemoteUnavailable | ErrorKind::InvalidRequest
                    ) =>
            {
                return Err(AppError::new(e.kind, e.message));
            }
            Ok(Err(e)) => {
                notes.push(format!("Could not read the file details: {}", e.message));
                ResourceInfo::default()
            }
            Err(_) => {
                notes.push("The file details took too long to load.".into());
                ResourceInfo::default()
            }
        };
        let filename = info
            .filename
            .as_deref()
            .and_then(crate::fsutil::sanitize_filename)
            .or_else(|| filename_from_url(&url));
        let detected = filename
            .as_deref()
            .map_or(MediaType::Unsorted, |f| detect(f).media_type);
        let mut preview = Preview {
            id: Uuid::new_v4(),
            provider: provider.id().to_owned(),
            display_url: redact_url(&url),
            filename,
            size_bytes: info.size,
            size_approximate: info.size_approximate,
            media_type: detected,
            detected_type: detected,
            title: None,
            year: None,
            artwork_id: None,
            notes: Vec::new(),
            accepts_password: provider.supports_password(),
        };
        self.describe(&mut preview, &notes).await;
        self.keep(Pending {
            url,
            provider: provider.id(),
            password,
            preview: preview.clone(),
            inspect_notes: notes,
            created: Instant::now(),
        });
        Ok(preview)
    }

    /// Change a preview's type, looking the title and artwork up again.
    pub async fn retype(&self, id: Uuid, media_type: MediaType) -> AppResult<Preview> {
        let mut pending = self.pending(id)?;
        if pending.preview.media_type != media_type {
            pending.preview.media_type = media_type;
            self.describe(&mut pending.preview, &pending.inspect_notes)
                .await;
            self.keep(pending.clone());
        }
        Ok(pending.preview)
    }

    /// Queue a previewed link. `media_type` overrides the preview's type,
    /// and a non-empty `password` the one given with the preview.
    pub async fn confirm(
        &self,
        id: Uuid,
        media_type: Option<MediaType>,
        password: Option<&str>,
    ) -> AppResult<Download> {
        let mut pending = self.pending(id)?;
        if let Some(password) = normalize_password(password)? {
            if !pending.preview.accepts_password {
                return Err(AppError::invalid(
                    "this kind of link does not use a download password",
                ));
            }
            pending.password = Some(password.to_owned());
        }
        if let Some(t) = media_type
            && t != pending.preview.media_type
        {
            pending.preview.media_type = t;
            self.describe(&mut pending.preview, &[]).await;
        }
        // Taken once: a double click must not queue the link twice.
        if self.take(id).is_none() {
            return Err(expired());
        }
        let preview = pending.preview;
        self.queue(NewDownload {
            display_url: preview.display_url,
            url: pending.url.into(),
            provider: pending.provider.to_owned(),
            password: pending.password,
            filename: preview.filename,
            media_type: Some(preview.media_type),
            title: preview.title,
            artwork_id: preview.artwork_id,
        })
        .await
    }

    /// Look a link up and queue it with the detected type, title and
    /// artwork: the confirmation step, confirmed without asking.
    pub async fn submit_detected(
        &self,
        raw_url: &str,
        password: Option<&str>,
    ) -> AppResult<Download> {
        let preview = self.preview(raw_url, password).await?;
        self.confirm(preview.id, None, None).await
    }

    fn validate(
        &self,
        raw_url: &str,
        password: Option<&str>,
    ) -> AppResult<(Url, &Arc<dyn Provider>, Option<String>)> {
        let url = validate_submission_url(raw_url, &self.policy)?;
        let provider = self.providers.for_url(&url).ok_or_else(|| {
            AppError::new(
                ErrorKind::UnsupportedProvider,
                "no download provider supports this URL",
            )
        })?;
        let url = provider.canonicalize(&url)?;
        let password = normalize_password(password)?;
        if password.is_some() && !provider.supports_password() {
            return Err(AppError::invalid(
                "this kind of link does not use a download password",
            ));
        }
        Ok((url, provider, password.map(str::to_owned)))
    }

    async fn queue(&self, new: NewDownload) -> AppResult<Download> {
        let download = self.store.insert_download(new, Utc::now()).await?;
        tracing::info!(
            id = %download.id,
            url = %download.display_url,
            provider = %download.provider,
            "download submitted"
        );
        self.engine.wake();
        Ok(download)
    }

    /// Fill in title and artwork for the preview's type.
    async fn describe(&self, preview: &mut Preview, inspect_notes: &[String]) {
        preview.notes = inspect_notes.to_vec();
        preview.title = None;
        preview.year = None;
        preview.artwork_id = None;
        let Some(guess) = preview
            .filename
            .as_deref()
            .and_then(|f| guess_as(f, preview.media_type))
        else {
            return;
        };
        let lookup = tokio::time::timeout(LOOKUP_TIMEOUT, self.artwork.find(&guess))
            .await
            .unwrap_or_else(|_| Lookup::Unavailable("the artwork lookup took too long".into()));
        match lookup {
            Lookup::Found(Artwork { id, title, year }) => {
                preview.artwork_id = Some(id);
                preview.title = Some(title);
                preview.year = year;
            }
            Lookup::NotFound => preview.notes.push(format!(
                "No artwork found for \u{201c}{}\u{201d}.",
                guess.title
            )),
            Lookup::NeedsKey => preview.notes.push(
                "Add a TMDB API key under Settings \u{2192} Artwork to see movie and series \
                 artwork."
                    .into(),
            ),
            Lookup::Unavailable(message) => {
                preview.notes.push(format!("No artwork: {message}."));
            }
        }
    }

    fn keep(&self, pending: Pending) {
        let mut previews = self.previews.lock().unwrap_or_else(|e| e.into_inner());
        previews.retain(|_, p| p.created.elapsed() < PREVIEW_TTL);
        if previews.len() >= MAX_PREVIEWS
            && let Some(oldest) = previews
                .iter()
                .min_by_key(|(_, p)| p.created)
                .map(|(id, _)| *id)
        {
            previews.remove(&oldest);
        }
        previews.insert(pending.preview.id, pending);
    }

    fn pending(&self, id: Uuid) -> AppResult<Pending> {
        let previews = self.previews.lock().unwrap_or_else(|e| e.into_inner());
        previews
            .get(&id)
            .filter(|p| p.created.elapsed() < PREVIEW_TTL)
            .cloned()
            .ok_or_else(expired)
    }

    fn take(&self, id: Uuid) -> Option<Pending> {
        self.previews
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
    }

    pub async fn get(&self, id: Uuid) -> AppResult<Download> {
        self.store
            .get_download(id)
            .await?
            .ok_or_else(|| AppError::not_found("download not found"))
    }

    pub async fn list(
        &self,
        states: &[TransferState],
        limit: i64,
        offset: i64,
    ) -> AppResult<Vec<Download>> {
        self.store.list_downloads(states, limit, offset).await
    }

    pub async fn events(&self, id: Uuid) -> AppResult<Vec<DownloadEvent>> {
        self.get(id).await?;
        self.store.list_events(id).await
    }

    /// Aggregate transfer throughput since the service started.
    pub fn throughput(&self) -> ThroughputSnapshot {
        self.engine.throughput()
    }

    /// Live progress of a running transfer, if it is running.
    pub fn live(&self, id: Uuid) -> Option<LiveSnapshot> {
        self.engine.live(id)
    }

    /// Pause a queued, retrying or running download. Partial data is kept.
    pub async fn pause(&self, id: Uuid) -> AppResult<Download> {
        self.stop(id, TransferState::Paused, StopReason::Pause, "pause")
            .await
    }

    /// Cancel a download that has not completed, and delete its partial data.
    pub async fn cancel(&self, id: Uuid) -> AppResult<Download> {
        self.stop(id, TransferState::Cancelled, StopReason::Cancel, "cancel")
            .await
    }

    /// Delete a cancelled or completed download and its history. With
    /// `delete_file`, a completed download's file is deleted from disk first;
    /// otherwise it stays. Other states are refused, so a running download
    /// can't be dropped by accident.
    pub async fn delete(&self, id: Uuid, delete_file: bool) -> AppResult<()> {
        let download = self.get(id).await?;
        match download.state {
            TransferState::Cancelled => {}
            TransferState::Completed if download.post_state == PostState::Processing => {
                return Err(AppError::conflict(
                    "this download is being organized; try again when it is done",
                ));
            }
            TransferState::Completed => {
                if delete_file && let Some(path) = &download.file_path {
                    self.remove_file(id, path).await?;
                }
            }
            state => {
                return Err(AppError::conflict(format!(
                    "only cancelled or completed downloads can be deleted; this one is {}",
                    state.as_str()
                )));
            }
        }
        if !self.store.delete_download(id, download.state).await? {
            return Err(AppError::conflict(
                "the download changed state; refresh and try again",
            ));
        }
        if download.state == TransferState::Cancelled {
            self.forget_partial(id).await;
        }
        tracing::info!(%id, delete_file, "download deleted by user");
        Ok(())
    }

    /// Delete every download in `state` (cancelled or completed) from the
    /// list. Files stay on disk. Returns how many were deleted.
    pub async fn delete_all(&self, state: TransferState) -> AppResult<usize> {
        if !matches!(state, TransferState::Cancelled | TransferState::Completed) {
            return Err(AppError::invalid(
                "only cancelled or completed downloads can be deleted",
            ));
        }
        let ids = self.store.delete_all_in(state).await?;
        if state == TransferState::Cancelled {
            for &id in &ids {
                self.forget_partial(id).await;
            }
        }
        tracing::info!(
            count = ids.len(),
            state = state.as_str(),
            "downloads deleted by user"
        );
        Ok(ids.len())
    }

    async fn remove_file(&self, id: Uuid, path: &str) -> AppResult<()> {
        match self.engine.remove_completed_file(path.into()).await {
            Ok(removed) => {
                tracing::info!(%id, removed, "downloaded file deleted by user");
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                tracing::warn!(%id, error = %e, "refused to delete downloaded file");
                Err(AppError::new(
                    ErrorKind::Filesystem,
                    "the file is not in the download directory, so Direkt won't delete it; \
                     remove it from the list instead",
                ))
            }
            Err(e) => {
                tracing::warn!(%id, error = %e, "could not delete downloaded file");
                Err(AppError::new(
                    ErrorKind::Filesystem,
                    format!("could not delete the file: {e}"),
                ))
            }
        }
    }

    /// Cancelling already removes partial data; this catches a leftover from
    /// a failed removal. The row is gone, so a failure here is only logged.
    async fn forget_partial(&self, id: Uuid) {
        if let Err(e) = self.engine.remove_partial(id).await {
            tracing::warn!(%id, error = %e, "could not remove partial file of deleted download");
        }
    }

    /// Resume a paused download.
    pub async fn resume(&self, id: Uuid) -> AppResult<Download> {
        self.requeue(id, &[TransferState::Paused], "resume").await
    }

    /// Retry a failed, blocked or cancelled download from the queue,
    /// optionally with a new download password.
    pub async fn retry(&self, id: Uuid, password: Option<&str>) -> AppResult<Download> {
        if let Some(password) = normalize_password(password)? {
            let download = self.get(id).await?;
            let supported = self
                .providers
                .get(&download.provider)
                .is_some_and(|p| p.supports_password());
            if !supported {
                return Err(AppError::invalid(
                    "this kind of link does not use a download password",
                ));
            }
            if !self
                .store
                .set_download_password(id, password, Utc::now())
                .await?
            {
                return Err(AppError::conflict(format!(
                    "cannot retry a download that is {}",
                    download.state.as_str()
                )));
            }
        }
        self.requeue(
            id,
            &[
                TransferState::Failed,
                TransferState::Blocked,
                TransferState::Cancelled,
            ],
            "retry",
        )
        .await
    }

    async fn stop(
        &self,
        id: Uuid,
        target: TransferState,
        reason: StopReason,
        verb: &str,
    ) -> AppResult<Download> {
        for _ in 0..MAX_STATE_RACES {
            let download = self.get(id).await?;
            match download.state {
                state if state == target => return Ok(download),
                TransferState::Downloading => {
                    if self.engine.stop_running(id, reason).await {
                        return self.get(id).await;
                    }
                    // Claimed but not yet registered with the engine, or just
                    // finished: look again shortly.
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                state if state.can_transition_to(target) => {
                    match self
                        .store
                        .transition(id, state, target, TransitionDetails::default(), Utc::now())
                        .await
                    {
                        Ok(updated) => {
                            if reason == StopReason::Cancel {
                                self.engine.remove_partial(id).await?;
                            }
                            tracing::info!(%id, state = target.as_str(), "download {verb}d by user");
                            return Ok(updated);
                        }
                        Err(e) if e.kind == ErrorKind::Conflict => continue,
                        Err(e) => return Err(e),
                    }
                }
                state => {
                    return Err(AppError::conflict(format!(
                        "cannot {verb} a download that is {}",
                        state.as_str()
                    )));
                }
            }
        }
        Err(AppError::conflict(
            "download state is changing rapidly; try again",
        ))
    }

    async fn requeue(
        &self,
        id: Uuid,
        allowed_from: &[TransferState],
        verb: &str,
    ) -> AppResult<Download> {
        let download = self.get(id).await?;
        if !allowed_from.contains(&download.state) {
            return Err(AppError::conflict(format!(
                "cannot {verb} a download that is {}",
                download.state.as_str()
            )));
        }
        let updated = self
            .store
            .transition(
                id,
                download.state,
                TransferState::Queued,
                TransitionDetails {
                    message: Some(format!("{verb} requested by user")),
                    ..Default::default()
                },
                Utc::now(),
            )
            .await?;
        tracing::info!(%id, "download {verb} requested");
        self.engine.wake();
        Ok(updated)
    }
}

const MAX_PASSWORD_CHARS: usize = 256;

/// Treat an empty password as none and reject unusable ones.
fn expired() -> AppError {
    AppError::not_found("this confirmation has expired; paste the link and click Download again")
}

fn normalize_password(password: Option<&str>) -> AppResult<Option<&str>> {
    let Some(password) = password.filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    if password.chars().count() > MAX_PASSWORD_CHARS {
        return Err(AppError::invalid(format!(
            "download password must be at most {MAX_PASSWORD_CHARS} characters"
        )));
    }
    if password.chars().any(char::is_control) {
        return Err(AppError::invalid(
            "download password must not contain control characters",
        ));
    }
    Ok(Some(password))
}
