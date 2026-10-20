//! Aggregate transfer throughput for the dashboard graph.
//!
//! A sampler ticks once per second and folds the progress of all running jobs
//! into a fixed-size history. Only bytes written since the previous tick
//! count, so resumed jobs do not report their existing partial data as new
//! traffic. Bytes written in the last second before a job stops are not
//! counted; the graph is informational, not an accounting record.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;
use uuid::Uuid;

/// Seconds of history kept for the graph.
pub const HISTORY_SECS: usize = 300;
/// How often the sampler runs.
pub(super) const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// Free disk space is checked every this many samples.
pub(super) const DISK_EVERY: u32 = 10;

#[derive(Debug, Default)]
pub(super) struct Throughput {
    /// Bytes per second, oldest first.
    history: VecDeque<u64>,
    /// Downloaded bytes of each running job at the previous tick.
    last: HashMap<Uuid, u64>,
    peak: u64,
    total: u64,
    free_bytes: Option<u64>,
}

impl Throughput {
    /// Record one tick given each running job's downloaded byte count.
    pub(super) fn record(&mut self, running: &[(Uuid, u64)], elapsed: Duration) {
        let bytes: u64 = running
            .iter()
            .filter_map(|(id, now)| self.last.get(id).map(|prev| now.saturating_sub(*prev)))
            .sum();
        self.last = running.iter().copied().collect();
        let secs = elapsed.as_secs_f64().max(0.001);
        let rate = (bytes as f64 / secs) as u64;
        if self.history.len() == HISTORY_SECS {
            self.history.pop_front();
        }
        self.history.push_back(rate);
        self.peak = self.peak.max(rate);
        self.total = self.total.saturating_add(bytes);
    }

    pub(super) fn set_free_bytes(&mut self, free: Option<u64>) {
        self.free_bytes = free;
    }

    pub(super) fn snapshot(&self) -> ThroughputSnapshot {
        ThroughputSnapshot {
            samples: self.history.iter().copied().collect(),
            current: self.history.back().copied().unwrap_or(0),
            peak: self.peak,
            total: self.total,
            free_bytes: self.free_bytes,
        }
    }
}

/// Throughput since the service started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThroughputSnapshot {
    /// Bytes per second for each of the last (up to) [`HISTORY_SECS`]
    /// seconds, oldest first.
    pub samples: Vec<u64>,
    /// Bytes per second during the last second.
    pub current: u64,
    /// Highest one-second rate since start.
    pub peak: u64,
    /// Bytes transferred since start.
    pub total: u64,
    /// Free space in the download directory, once checked.
    pub free_bytes: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: Duration = Duration::from_secs(1);

    #[test]
    fn counts_only_new_bytes() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut t = Throughput::default();
        // A resumed job appears with existing partial data: not new traffic.
        t.record(&[(a, 5_000)], SEC);
        assert_eq!(t.snapshot().current, 0);
        t.record(&[(a, 6_000), (b, 100)], SEC);
        assert_eq!(t.snapshot().current, 1_000);
        t.record(&[(a, 6_500), (b, 400)], SEC);
        let s = t.snapshot();
        assert_eq!(s.current, 800);
        assert_eq!(s.peak, 1_000);
        assert_eq!(s.total, 1_800);
        assert_eq!(s.samples, vec![0, 1_000, 800]);
    }

    #[test]
    fn restarted_job_never_counts_negative() {
        let a = Uuid::new_v4();
        let mut t = Throughput::default();
        t.record(&[(a, 9_000)], SEC);
        t.record(&[(a, 0)], SEC);
        t.record(&[(a, 300)], SEC);
        assert_eq!(t.snapshot().total, 300);
    }

    #[test]
    fn rate_accounts_for_elapsed_time() {
        let a = Uuid::new_v4();
        let mut t = Throughput::default();
        t.record(&[(a, 0)], SEC);
        t.record(&[(a, 4_000)], Duration::from_secs(2));
        assert_eq!(t.snapshot().current, 2_000);
    }

    #[test]
    fn history_is_bounded() {
        let mut t = Throughput::default();
        for _ in 0..HISTORY_SECS + 50 {
            t.record(&[], SEC);
        }
        assert_eq!(t.snapshot().samples.len(), HISTORY_SECS);
    }
}
