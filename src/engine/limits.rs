//! Limits the user can change while downloads run: an overall speed limit
//! shared by all transfers, and how many transfers run at once.
//!
//! The speed limit is a shared schedule (GCRA): each chunk a transfer
//! receives reserves its share of time at the limit, and the transfer sleeps
//! until its reservation is due. Reading slower lets TCP slow the sender down.
//! A short burst is allowed after idle time so small chunks are not delayed.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

/// Credit an idle limiter grants, so a transfer resuming after a pause does
/// not wait on its first chunks.
const BURST: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub struct Limits(Arc<Shared>);

struct Shared {
    /// Bytes per second for all transfers together; 0 means no limit.
    speed: AtomicU64,
    /// When the bandwidth reserved so far is used up.
    next_free: Mutex<Option<Instant>>,
    max_concurrent: watch::Sender<usize>,
    default_max_concurrent: usize,
}

impl Limits {
    /// No speed limit, and `max_concurrent` transfers at once (also the
    /// default reported to users).
    pub fn new(max_concurrent: usize) -> Self {
        Self(Arc::new(Shared {
            speed: AtomicU64::new(0),
            next_free: Mutex::new(None),
            max_concurrent: watch::Sender::new(max_concurrent),
            default_max_concurrent: max_concurrent,
        }))
    }

    pub fn max_concurrent(&self) -> usize {
        *self.0.max_concurrent.borrow()
    }

    /// The configured value, before any change made on the settings page.
    pub fn default_max_concurrent(&self) -> usize {
        self.0.default_max_concurrent
    }

    /// Takes effect immediately. When lowered, running transfers finish and
    /// no new one starts until fewer than `n` run.
    pub fn set_max_concurrent(&self, n: usize) {
        self.0.max_concurrent.send_if_modified(|current| {
            let changed = *current != n;
            *current = n;
            changed
        });
    }

    pub(super) fn watch_max_concurrent(&self) -> watch::Receiver<usize> {
        self.0.max_concurrent.subscribe()
    }

    /// Bytes per second for all transfers together, if limited.
    pub fn speed_limit(&self) -> Option<u64> {
        Some(self.0.speed.load(Ordering::Relaxed)).filter(|&s| s > 0)
    }

    pub fn set_speed_limit(&self, bytes_per_sec: Option<u64>) {
        self.0
            .speed
            .store(bytes_per_sec.unwrap_or(0), Ordering::Relaxed);
        // Forget reservations made at the old rate.
        *self.next_free() = None;
    }

    /// Reserve bandwidth for `bytes` just received; returns how long to wait
    /// before reading more.
    pub(super) fn reserve(&self, bytes: u64) -> Duration {
        self.reserve_at(bytes, Instant::now())
    }

    fn reserve_at(&self, bytes: u64, now: Instant) -> Duration {
        let Some(limit) = self.speed_limit() else {
            return Duration::ZERO;
        };
        let cost = Duration::from_secs_f64(bytes as f64 / limit as f64);
        let earliest = now.checked_sub(BURST).unwrap_or(now);
        let mut next_free = self.next_free();
        let start = next_free.map_or(earliest, |t| t.max(earliest));
        let end = start + cost;
        *next_free = Some(end);
        end.saturating_duration_since(now)
    }

    fn next_free(&self) -> std::sync::MutexGuard<'_, Option<Instant>> {
        // Only a timestamp: still consistent after a panic elsewhere.
        self.0.next_free.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl std::fmt::Debug for Limits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Limits")
            .field("speed_limit", &self.speed_limit())
            .field("max_concurrent", &self.max_concurrent())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn unlimited_never_waits() {
        let limits = Limits::new(3);
        assert_eq!(limits.speed_limit(), None);
        assert_eq!(limits.reserve(u64::MAX), Duration::ZERO);
    }

    #[test]
    fn sustained_rate_matches_the_limit() {
        let limits = Limits::new(3);
        limits.set_speed_limit(Some(1000));
        let now = Instant::now();
        // The burst covers the first 250 bytes; then every 100 bytes costs
        // 100 ms more, shared by however many transfers ask.
        assert_eq!(limits.reserve_at(250, now), Duration::ZERO);
        assert_eq!(limits.reserve_at(100, now), 100 * MS);
        assert_eq!(limits.reserve_at(100, now), 200 * MS);
        // Time passing pays the debt off.
        assert_eq!(limits.reserve_at(100, now + 300 * MS), Duration::ZERO);
    }

    #[test]
    fn idle_time_earns_only_a_short_burst() {
        let limits = Limits::new(3);
        limits.set_speed_limit(Some(1000));
        let now = Instant::now();
        let _ = limits.reserve_at(1000, now);
        let later = now + Duration::from_secs(60);
        assert_eq!(limits.reserve_at(1000, later), 750 * MS);
    }

    #[test]
    fn changing_the_limit_forgets_old_reservations() {
        let limits = Limits::new(3);
        limits.set_speed_limit(Some(10));
        let now = Instant::now();
        assert!(limits.reserve_at(10_000, now) > Duration::from_secs(900));
        limits.set_speed_limit(Some(1_000_000));
        assert_eq!(limits.reserve_at(10_000, now), Duration::ZERO);
        limits.set_speed_limit(None);
        assert_eq!(limits.reserve_at(u64::MAX, now), Duration::ZERO);
    }

    #[test]
    fn concurrency_changes_are_announced() {
        let limits = Limits::new(3);
        let mut rx = limits.watch_max_concurrent();
        assert_eq!(*rx.borrow_and_update(), 3);
        limits.set_max_concurrent(3);
        assert!(!rx.has_changed().unwrap(), "same value: no wake-up");
        limits.set_max_concurrent(5);
        assert!(rx.has_changed().unwrap());
        assert_eq!(limits.max_concurrent(), 5);
        assert_eq!(limits.default_max_concurrent(), 3);
    }
}
