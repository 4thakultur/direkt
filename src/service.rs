//! Application service for downloads.
//!
//! This is the single entry point for download operations, so the API, the
//! web UI and future integrations share the same validation and state rules.

use crate::domain::{Download, DownloadEvent, TransferState};
use crate::engine::{Engine, LiveSnapshot, StopReason};
use crate::error::{AppError, AppResult, ErrorKind};
use crate::net::{NetworkPolicy, redact_url, validate_submission_url};
use crate::providers::Providers;
use crate::store::{NewDownload, Store, TransitionDetails};
use chrono::Utc;
use std::time::Duration;
use uuid::Uuid;

/// Bound on optimistic-concurrency retries when a download changes state
/// while a user action is being applied.
const MAX_STATE_RACES: usize = 40;

#[derive(Clone)]
pub struct DownloadService {
    store: Store,
    engine: Engine,
    providers: Providers,
    policy: NetworkPolicy,
}

impl DownloadService {
    pub fn new(store: Store, engine: Engine, providers: Providers, policy: NetworkPolicy) -> Self {
        Self {
            store,
            engine,
            providers,
            policy,
        }
    }

    /// Validate a URL, pick its provider, and queue it. `password` is for
    /// password-protected files on providers that support it.
    pub async fn submit(&self, raw_url: &str, password: Option<&str>) -> AppResult<Download> {
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
        let download = self
            .store
            .insert_download(
                NewDownload {
                    display_url: redact_url(&url),
                    url: url.into(),
                    provider: provider.id().to_owned(),
                    password: password.map(str::to_owned),
                },
                Utc::now(),
            )
            .await?;
        tracing::info!(
            id = %download.id,
            url = %download.display_url,
            provider = %download.provider,
            "download submitted"
        );
        self.engine.wake();
        Ok(download)
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
