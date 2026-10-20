//! The download engine: a dispatcher that claims queued jobs from the store
//! and runs at most `max_concurrent` transfers at a time.
//!
//! * Jobs are claimed with an atomic UPDATE, so a job never runs twice.
//! * Each running job has a stop channel (pause, cancel, shutdown) and a "done"
//!   signal that callers can wait on.
//! * Failures are classified by [`Disposition`]: transient errors are retried
//!   with exponential backoff up to `max_retries`, errors needing user action
//!   block the job, and permanent errors fail it.
//! * Partial data lives in `incomplete_dir/<id>.part`. Its name never derives
//!   from untrusted input. Once complete, it is moved without overwriting
//!   into `download_dir` under a sanitized, collision-free name.

#[cfg(test)]
mod tests;
mod throughput;
mod transfer;

use crate::config::Config;
use crate::domain::{Download, DownloadError, TransferState};
use crate::error::{Disposition, ErrorKind, TransferError};
use crate::fsutil;
use crate::net::NetworkPolicy;
use crate::providers::{Providers, ResolveRequest};
use crate::store::{Store, TransitionDetails};
use chrono::Utc;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use throughput::Throughput;
pub use throughput::{HISTORY_SECS, ThroughputSnapshot};
use tokio::sync::{Notify, Semaphore, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use transfer::{Attempt, AttemptOutcome, remove_if_exists};
use url::Url;
use uuid::Uuid;

/// Upper bound for a server-requested `Retry-After`.
const MAX_SERVER_RETRY_DELAY: Duration = Duration::from_secs(24 * 3600);
/// How long the dispatcher sleeps when idle with nothing scheduled.
const IDLE_POLL: Duration = Duration::from_secs(60);
/// How long shutdown waits for running transfers to save their state.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
/// How long a pause/cancel request waits for the worker to stop.
const STOP_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct EngineSettings {
    pub download_dir: PathBuf,
    pub incomplete_dir: PathBuf,
    pub max_concurrent: usize,
    pub max_retries: u32,
    pub retry_base_delay: Duration,
    pub retry_max_delay: Duration,
    /// Free space to keep available on the incomplete-download filesystem.
    pub min_free_bytes: u64,
}

impl EngineSettings {
    pub fn from_config(config: &Config) -> Self {
        Self {
            download_dir: config.storage.download_dir.clone(),
            incomplete_dir: config.storage.incomplete_dir.clone(),
            max_concurrent: config.downloads.max_concurrent,
            max_retries: config.downloads.max_retries,
            retry_base_delay: Duration::from_secs(config.downloads.retry_base_delay_secs),
            retry_max_delay: Duration::from_secs(config.downloads.retry_max_delay_secs),
            min_free_bytes: config.storage.min_free_space_mb.saturating_mul(1024 * 1024),
        }
    }

    /// Delay before retry number `retry_count + 1`: base·2ⁿ, capped.
    fn backoff(&self, retry_count: i64) -> Duration {
        let exp = u32::try_from(retry_count).unwrap_or(u32::MAX).min(30);
        self.retry_base_delay
            .saturating_mul(1u32 << exp)
            .min(self.retry_max_delay)
    }
}

/// Why a running transfer is being stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Pause,
    Cancel,
    Shutdown,
}

/// Lock-free live progress of a running transfer, for the UI.
#[derive(Debug)]
pub struct LiveProgress {
    downloaded: AtomicU64,
    /// `u64::MAX` means unknown.
    total: AtomicU64,
    bytes_per_sec: AtomicU64,
}

impl Default for LiveProgress {
    fn default() -> Self {
        Self {
            downloaded: AtomicU64::new(0),
            total: AtomicU64::new(u64::MAX),
            bytes_per_sec: AtomicU64::new(0),
        }
    }
}

impl LiveProgress {
    fn set_total(&self, total: Option<u64>) {
        self.total
            .store(total.unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    fn set_downloaded(&self, bytes: u64) {
        self.downloaded.store(bytes, Ordering::Relaxed);
    }

    /// Record progress and fold the instantaneous rate into a smoothed speed.
    fn record(&self, bytes: u64, rate: f64) {
        self.downloaded.store(bytes, Ordering::Relaxed);
        let previous = self.bytes_per_sec.load(Ordering::Relaxed) as f64;
        let smoothed = if previous == 0.0 {
            rate
        } else {
            0.7 * previous + 0.3 * rate
        };
        self.bytes_per_sec.store(smoothed as u64, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> LiveSnapshot {
        let total = self.total.load(Ordering::Relaxed);
        LiveSnapshot {
            downloaded_bytes: self.downloaded.load(Ordering::Relaxed),
            total_bytes: (total != u64::MAX).then_some(total),
            bytes_per_sec: self.bytes_per_sec.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveSnapshot {
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub bytes_per_sec: u64,
}

struct RunningJob {
    stop: Arc<watch::Sender<Option<StopReason>>>,
    /// Closes (sender dropped) when the worker has fully finished.
    done: watch::Receiver<()>,
    live: Arc<LiveProgress>,
}

struct Inner {
    store: Store,
    providers: Providers,
    client: reqwest::Client,
    policy: NetworkPolicy,
    settings: EngineSettings,
    wake: Notify,
    shutdown: CancellationToken,
    running: Mutex<HashMap<Uuid, RunningJob>>,
    throughput: Mutex<Throughput>,
}

impl Inner {
    fn part_path(&self, id: Uuid) -> PathBuf {
        self.settings.incomplete_dir.join(format!("{id}.part"))
    }

    fn running(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, RunningJob>> {
        // A poisoned lock only means a worker panicked mid-update of a plain
        // map; the data is still consistent.
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn throughput(&self) -> std::sync::MutexGuard<'_, Throughput> {
        self.throughput.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

impl Engine {
    pub fn new(
        store: Store,
        providers: Providers,
        client: reqwest::Client,
        policy: NetworkPolicy,
        settings: EngineSettings,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                providers,
                client,
                policy,
                settings,
                wake: Notify::new(),
                shutdown: CancellationToken::new(),
                running: Mutex::new(HashMap::new()),
                throughput: Mutex::new(Throughput::default()),
            }),
        }
    }

    /// Spawn the dispatcher. Call [`Engine::shutdown`] with the returned
    /// handle to stop it.
    pub fn start(&self) -> JoinHandle<()> {
        // The sampler exits on its own when shutdown is signalled.
        tokio::spawn(sample_throughput(self.inner.clone()));
        tokio::spawn(dispatch(self.inner.clone()))
    }

    /// Signal that new work may be available.
    pub fn wake(&self) {
        self.inner.wake.notify_one();
    }

    /// Stop all transfers (they return to the queue with partial data kept)
    /// and wait for the dispatcher to exit.
    pub async fn shutdown(&self, handle: JoinHandle<()>) {
        self.inner.shutdown.cancel();
        if let Err(e) = handle.await {
            tracing::error!(error = %e, "download dispatcher terminated abnormally");
        }
    }

    pub fn is_running(&self, id: Uuid) -> bool {
        self.inner.running().contains_key(&id)
    }

    pub fn live(&self, id: Uuid) -> Option<LiveSnapshot> {
        self.inner.running().get(&id).map(|job| job.live.snapshot())
    }

    /// Aggregate throughput since the engine started.
    pub fn throughput(&self) -> ThroughputSnapshot {
        self.inner.throughput().snapshot()
    }

    /// Ask a running transfer to stop and wait until its worker has recorded
    /// the resulting state. Returns `false` if the job is not running here.
    pub async fn stop_running(&self, id: Uuid, reason: StopReason) -> bool {
        let (stop, mut done) = {
            let running = self.inner.running();
            let Some(job) = running.get(&id) else {
                return false;
            };
            (job.stop.clone(), job.done.clone())
        };
        let _ = stop.send(Some(reason));
        let finished =
            tokio::time::timeout(STOP_WAIT, async { while done.changed().await.is_ok() {} }).await;
        if finished.is_err() {
            tracing::warn!(%id, "timed out waiting for transfer to stop");
        }
        true
    }

    /// Delete the partial file of a job that is not running.
    pub async fn remove_partial(&self, id: Uuid) -> Result<(), TransferError> {
        remove_if_exists(&self.inner.part_path(id)).await
    }
}

/// Sample aggregate throughput once per second until shutdown.
async fn sample_throughput(inner: Arc<Inner>) {
    let mut ticker = tokio::time::interval(throughput::SAMPLE_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut previous = tokio::time::Instant::now();
    let mut tick: u32 = 0;
    loop {
        tokio::select! {
            () = inner.shutdown.cancelled() => break,
            now = ticker.tick() => {
                let running: Vec<(Uuid, u64)> = inner
                    .running()
                    .iter()
                    .map(|(id, job)| (*id, job.live.snapshot().downloaded_bytes))
                    .collect();
                inner.throughput().record(&running, now - previous);
                previous = now;
            }
        }
        if tick.is_multiple_of(throughput::DISK_EVERY) {
            let dir = inner.settings.download_dir.clone();
            let free = tokio::task::spawn_blocking(move || fsutil::available_space(&dir))
                .await
                .ok()
                .and_then(Result::ok);
            inner.throughput().set_free_bytes(free);
        }
        tick = tick.wrapping_add(1);
    }
}

/// Resolve when a stop is requested. Never resolves if the sender is gone.
async fn wait_for_stop(stop: &mut watch::Receiver<Option<StopReason>>) -> StopReason {
    loop {
        if let Some(reason) = *stop.borrow_and_update() {
            return reason;
        }
        if stop.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

async fn dispatch(inner: Arc<Inner>) {
    let slots = Arc::new(Semaphore::new(inner.settings.max_concurrent));
    let mut workers = JoinSet::new();
    let mut tasks: HashMap<tokio::task::Id, Uuid> = HashMap::new();
    tracing::info!(
        max_concurrent = inner.settings.max_concurrent,
        "download engine started"
    );

    loop {
        while let Some(result) = workers.try_join_next_with_id() {
            reap(&inner, &mut tasks, result).await;
        }

        let permit = tokio::select! {
            () = inner.shutdown.cancelled() => break,
            Some(result) = workers.join_next_with_id(), if !workers.is_empty() => {
                reap(&inner, &mut tasks, result).await;
                continue;
            }
            permit = slots.clone().acquire_owned() => permit.expect("semaphore is never closed"),
        };

        match inner.store.claim_next(Utc::now()).await {
            Ok(Some(download)) => {
                let id = download.id;
                let (stop_tx, stop_rx) = watch::channel(None);
                let (done_tx, done_rx) = watch::channel(());
                let live = Arc::new(LiveProgress::default());
                inner.running().insert(
                    id,
                    RunningJob {
                        stop: Arc::new(stop_tx),
                        done: done_rx,
                        live: live.clone(),
                    },
                );
                let inner = inner.clone();
                let handle = workers.spawn(async move {
                    let _permit = permit;
                    run_job(&inner, download, stop_rx, &live).await;
                    inner.running().remove(&id);
                    drop(done_tx);
                    inner.wake.notify_one();
                });
                tasks.insert(handle.id(), id);
            }
            Ok(None) => {
                drop(permit);
                let idle = match inner.store.next_retry_at().await {
                    Ok(Some(at)) => (at - Utc::now()).to_std().unwrap_or(Duration::ZERO),
                    _ => IDLE_POLL,
                }
                .min(IDLE_POLL);
                tokio::select! {
                    () = inner.shutdown.cancelled() => break,
                    () = inner.wake.notified() => {}
                    () = tokio::time::sleep(idle) => {}
                    Some(result) = workers.join_next_with_id(), if !workers.is_empty() => {
                        reap(&inner, &mut tasks, result).await;
                    }
                }
            }
            Err(e) => {
                drop(permit);
                tracing::error!(error = %e, "failed to claim next download");
                tokio::select! {
                    () = inner.shutdown.cancelled() => break,
                    () = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
            }
        }
    }

    // Shutdown: ask every running transfer to stop and save its state.
    let senders: Vec<_> = inner
        .running()
        .values()
        .map(|job| job.stop.clone())
        .collect();
    tracing::info!(running = senders.len(), "stopping download engine");
    for stop in senders {
        let _ = stop.send(Some(StopReason::Shutdown));
    }
    let drained = tokio::time::timeout(SHUTDOWN_GRACE, async {
        while let Some(result) = workers.join_next_with_id().await {
            reap(&inner, &mut tasks, result).await;
        }
    })
    .await;
    if drained.is_err() {
        tracing::warn!("some transfers did not stop in time; they will be recovered on next start");
        workers.abort_all();
    }
}

/// Handle a finished worker task; a panicked worker's job is marked failed so
/// it does not stay "downloading" forever.
async fn reap(
    inner: &Inner,
    tasks: &mut HashMap<tokio::task::Id, Uuid>,
    result: Result<(tokio::task::Id, ()), tokio::task::JoinError>,
) {
    let task_id = match &result {
        Ok((id, ())) => *id,
        Err(e) => e.id(),
    };
    let Some(download_id) = tasks.remove(&task_id) else {
        return;
    };
    if let Err(e) = result
        && e.is_panic()
    {
        tracing::error!(id = %download_id, "transfer worker panicked");
        inner.running().remove(&download_id);
        let details = TransitionDetails {
            error: Some(DownloadError {
                kind: ErrorKind::Internal,
                message: "internal error during transfer".into(),
            }),
            ..Default::default()
        };
        let _ = inner
            .store
            .transition(
                download_id,
                TransferState::Downloading,
                TransferState::Failed,
                details,
                Utc::now(),
            )
            .await;
    }
}

enum JobEnd {
    Completed,
    Stopped(StopReason),
}

async fn run_job(
    inner: &Inner,
    download: Download,
    mut stop: watch::Receiver<Option<StopReason>>,
    live: &LiveProgress,
) {
    let id = download.id;
    tracing::info!(%id, url = %download.display_url, attempt = download.retry_count + 1, "transfer started");
    let result = attempt(inner, &download, &mut stop, live).await;

    let now = Utc::now();
    let (to, details) = match result {
        Ok(JobEnd::Completed) => {
            tracing::info!(%id, "transfer completed");
            (TransferState::Completed, TransitionDetails::default())
        }
        Ok(JobEnd::Stopped(StopReason::Pause)) => {
            (TransferState::Paused, TransitionDetails::default())
        }
        Ok(JobEnd::Stopped(StopReason::Cancel)) => {
            if let Err(e) = remove_if_exists(&inner.part_path(id)).await {
                tracing::warn!(%id, error = %e, "could not remove partial file of cancelled download");
            }
            (TransferState::Cancelled, TransitionDetails::default())
        }
        Ok(JobEnd::Stopped(StopReason::Shutdown)) => (
            TransferState::Queued,
            TransitionDetails {
                message: Some("transfer interrupted by shutdown; re-queued".into()),
                ..Default::default()
            },
        ),
        Err(err) => failure_transition(&inner.settings, &download, err, now),
    };
    if let Err(e) = inner
        .store
        .transition(id, TransferState::Downloading, to, details, now)
        .await
    {
        // On restart, recovery re-queues anything left in `downloading`.
        tracing::error!(%id, error = %e, target = to.as_str(), "failed to record transfer result");
    }
}

/// Decide the next state for a failed attempt.
fn failure_transition(
    settings: &EngineSettings,
    download: &Download,
    err: TransferError,
    now: chrono::DateTime<Utc>,
) -> (TransferState, TransitionDetails) {
    let id = download.id;
    let error = |message: String| {
        Some(DownloadError {
            kind: err.kind,
            message,
        })
    };
    match err.disposition {
        Disposition::Retry { .. } if download.retry_count >= i64::from(settings.max_retries) => {
            tracing::warn!(%id, error = %err, "transfer failed; retries exhausted");
            (
                TransferState::Failed,
                TransitionDetails {
                    error: error(format!(
                        "{} (gave up after {} retries)",
                        err.message, download.retry_count
                    )),
                    ..Default::default()
                },
            )
        }
        Disposition::Retry { after } => {
            let delay = match after {
                Some(after) => after
                    .min(MAX_SERVER_RETRY_DELAY)
                    .max(settings.retry_base_delay),
                None => settings.backoff(download.retry_count),
            };
            let next = chrono::Duration::from_std(delay)
                .ok()
                .and_then(|d| now.checked_add_signed(d))
                .unwrap_or(chrono::DateTime::<Utc>::MAX_UTC);
            tracing::info!(%id, error = %err, retry_in_secs = delay.as_secs(), "transfer failed; will retry");
            (
                TransferState::Retrying,
                TransitionDetails {
                    error: error(err.message.clone()),
                    next_attempt_at: Some(next),
                    message: Some(format!("{}; retrying in {}s", err.message, delay.as_secs())),
                },
            )
        }
        Disposition::Block => {
            tracing::warn!(%id, error = %err, "transfer blocked; needs attention");
            (
                TransferState::Blocked,
                TransitionDetails {
                    error: error(err.message.clone()),
                    ..Default::default()
                },
            )
        }
        Disposition::Fail => {
            tracing::warn!(%id, error = %err, "transfer failed");
            (
                TransferState::Failed,
                TransitionDetails {
                    error: error(err.message.clone()),
                    ..Default::default()
                },
            )
        }
    }
}

async fn attempt(
    inner: &Inner,
    download: &Download,
    stop: &mut watch::Receiver<Option<StopReason>>,
    live: &LiveProgress,
) -> Result<JobEnd, TransferError> {
    let part_path = inner.part_path(download.id);
    if finalized_before_crash(download, &part_path).await {
        tracing::info!(id = %download.id, "completed file found from an interrupted run");
        return Ok(JobEnd::Completed);
    }

    let provider = inner.providers.get(&download.provider).ok_or_else(|| {
        TransferError::fail(
            ErrorKind::UnsupportedProvider,
            format!("provider '{}' is not available", download.provider),
        )
    })?;
    let url = Url::parse(&download.url)
        .map_err(|_| TransferError::fail(ErrorKind::InvalidRequest, "stored URL is invalid"))?;
    let resource = tokio::select! {
        biased;
        reason = wait_for_stop(stop) => return Ok(JobEnd::Stopped(reason)),
        r = provider.resolve(ResolveRequest {
            url: &url,
            password: download.download_password.as_deref(),
        }) => r?,
    };

    let outcome = Attempt {
        inner,
        download,
        resource: &resource,
        part_path: &part_path,
        live,
        stop,
    }
    .run()
    .await?;

    match outcome {
        AttemptOutcome::Stopped(reason) => Ok(JobEnd::Stopped(reason)),
        AttemptOutcome::Finished { bytes, filename } => {
            finalize(inner, download, part_path, &filename).await?;
            tracing::debug!(id = %download.id, bytes, "file moved to download directory");
            Ok(JobEnd::Completed)
        }
    }
}

/// True if a previous run moved the completed file into place but stopped
/// before recording completion.
async fn finalized_before_crash(download: &Download, part_path: &std::path::Path) -> bool {
    let (Some(path), Some(total)) = (&download.file_path, download.total_bytes) else {
        return false;
    };
    if download.downloaded_bytes != total || tokio::fs::try_exists(part_path).await.unwrap_or(true)
    {
        return false;
    }
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) => meta.is_file() && i64::try_from(meta.len()) == Ok(total),
        Err(_) => false,
    }
}

/// Move the completed partial file into `download_dir`. The destination is
/// recorded first, so an interruption between the move and the state update
/// can be recognized on restart.
async fn finalize(
    inner: &Inner,
    download: &Download,
    part_path: PathBuf,
    filename: &str,
) -> Result<(), TransferError> {
    let dir = inner.settings.download_dir.clone();
    for _ in 0..5 {
        let (dir, name) = (dir.clone(), filename.to_owned());
        let dest = tokio::task::spawn_blocking(move || fsutil::unique_destination(&dir, &name))
            .await
            .map_err(|_| TransferError::retry(ErrorKind::Internal, "internal task failed"))?
            .map_err(|e| TransferError::from_io(&e, "choosing a destination file name"))?;
        inner
            .store
            .set_file_path(download.id, &dest.to_string_lossy(), Utc::now())
            .await
            .map_err(TransferError::from)?;

        let (src, dst) = (part_path.clone(), dest.clone());
        let moved = tokio::task::spawn_blocking(move || fsutil::move_no_clobber(&src, &dst))
            .await
            .map_err(|_| TransferError::retry(ErrorKind::Internal, "internal task failed"))?;
        match moved {
            Ok(()) => return Ok(()),
            // Someone created the same name in the meantime; pick another.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(TransferError::from_io(&e, "moving the completed file")),
        }
    }
    Err(TransferError::block(
        ErrorKind::Filesystem,
        "could not find a free destination file name",
    ))
}
