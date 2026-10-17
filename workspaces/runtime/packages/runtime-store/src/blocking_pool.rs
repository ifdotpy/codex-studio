//! The single entry point to Tokio's blocking pool, with opt-in measurements.
//!
//! Every blocking closure is submitted through [`spawn_blocking`] with a static
//! call-site label; Clippy rejects direct `tokio::task::spawn_blocking` calls.
//!
//! The wrapper adds one cached flag read when disabled. Set
//! `CODEX_RUNTIME_BLOCKING_POOL_DIAGNOSTICS=1` to collect per-label queue wait,
//! closure duration, and occupancy measurements.
//!
//! Measurement procedure: run the isolated `fixture_reports_pool_wait_duration_and_occupancy`
//! test with `-- --nocapture`; it uses a one-worker Tokio blocking pool, holds
//! one fixture closure at a channel gate, queues a second, releases the gate,
//! and reports the snapshot. For a runtime sample, start the Rust server with
//! the environment flag above and read `GET /api/diagnostics` field
//! `blockingPoolMetrics`. No live backend is used by the fixture test.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

const DIAGNOSTICS_ENV: &str = "CODEX_RUNTIME_BLOCKING_POOL_DIAGNOSTICS";
const NANOS_PER_MILLISECOND: f64 = 1_000_000.0;
/// Upper bound on concurrently running blocking-pool threads in the runtime
/// server. Waiting for a SQLite reader is a future (`RuntimeStore::read_async`),
/// so this bounds thread count without bounding read admission. Unit: threads.
pub const MAX_THREADS: usize = 32;
/// How long an idle blocking-pool thread is kept for reuse before it exits, so
/// bursts reuse threads instead of creating and freeing them. Unit: time.
pub const THREAD_KEEP_ALIVE: Duration = Duration::from_secs(300);
/// Retained samples per call-site label for p95/p99 summaries.
const SAMPLE_LIMIT_PER_GROUP: usize = 512;

static ENABLED: OnceLock<bool> = OnceLock::new();
static METRICS: OnceLock<Metrics> = OnceLock::new();

struct Metrics {
    queued: AtomicUsize,
    running: AtomicUsize,
    peak_occupancy: AtomicUsize,
    /// Per-label measurements, keyed by the call site's static label.
    groups: Mutex<BTreeMap<&'static str, Arc<GroupMetrics>>>,
}

#[derive(Default)]
struct GroupMetrics {
    count: AtomicU64,
    queue_wait_total_ns: AtomicU64,
    queue_wait_max_ns: AtomicU64,
    duration_total_ns: AtomicU64,
    duration_max_ns: AtomicU64,
    recent: Mutex<VecDeque<(u64, u64)>>,
}

struct QueuedGuard {
    metrics: &'static Metrics,
    queued: bool,
}

impl QueuedGuard {
    fn started(&mut self) {
        if self.queued {
            self.metrics.queued.fetch_sub(1, Ordering::Relaxed);
            self.queued = false;
        }
    }
}

impl Drop for QueuedGuard {
    fn drop(&mut self) {
        if self.queued {
            self.metrics.queued.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

fn record_max(target: &AtomicU64, sample: u64) {
    target.fetch_max(sample, Ordering::Relaxed);
}

fn nanos(duration: std::time::Duration) -> u64 {
    duration.as_nanos().min(u64::MAX as u128) as u64
}

/// A bounded diagnostic snapshot. Durations are in milliseconds.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockingPoolSnapshot {
    pub enabled: bool,
    pub queued: usize,
    pub running: usize,
    pub peak_occupancy: usize,
    pub groups: Vec<BlockingPoolGroupSnapshot>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BlockingPoolGroupSnapshot {
    pub group: &'static str,
    pub count: u64,
    pub queue_wait_avg_ms: f64,
    pub queue_wait_max_ms: f64,
    pub queue_wait_p95_ms: f64,
    pub queue_wait_p99_ms: f64,
    pub closure_avg_ms: f64,
    pub closure_max_ms: f64,
    pub closure_p95_ms: f64,
    pub closure_p99_ms: f64,
}

fn enabled() -> bool {
    *ENABLED.get_or_init(|| std::env::var(DIAGNOSTICS_ENV).is_ok_and(|value| value == "1"))
}

fn metrics() -> &'static Metrics {
    METRICS.get_or_init(|| Metrics {
        queued: AtomicUsize::new(0),
        running: AtomicUsize::new(0),
        peak_occupancy: AtomicUsize::new(0),
        groups: Mutex::new(BTreeMap::new()),
    })
}

fn update_peak(metrics: &Metrics, occupancy: usize) {
    let mut peak = metrics.peak_occupancy.load(Ordering::Relaxed);
    while occupancy > peak {
        match metrics.peak_occupancy.compare_exchange_weak(
            peak,
            occupancy,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(current) => peak = current,
        }
    }
}

/// Submit blocking work and optionally record queue wait, runtime, and occupancy.
pub fn spawn_blocking<F, R>(group: &'static str, work: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    submit(group, work, enabled())
}

fn submit<F, R>(group: &'static str, work: F, collect: bool) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    if !collect {
        #[allow(clippy::disallowed_methods)] // The single entry point to Tokio's blocking pool.
        return tokio::task::spawn_blocking(work);
    }

    let metrics = metrics();
    let entry = Arc::clone(
        metrics
            .groups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(group)
            .or_default(),
    );
    let submitted_at = Instant::now();
    let queued = metrics.queued.fetch_add(1, Ordering::Relaxed) + 1;
    update_peak(metrics, metrics.running.load(Ordering::Relaxed) + queued);
    let mut queued_guard = QueuedGuard {
        metrics,
        queued: true,
    };
    #[allow(clippy::disallowed_methods)] // The single entry point to Tokio's blocking pool.
    tokio::task::spawn_blocking(move || {
        let queue_wait = submitted_at.elapsed();
        queued_guard.started();
        let running = metrics.running.fetch_add(1, Ordering::Relaxed) + 1;
        update_peak(metrics, running + metrics.queued.load(Ordering::Relaxed));
        let started_at = Instant::now();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
        let duration = started_at.elapsed();
        metrics.running.fetch_sub(1, Ordering::Relaxed);

        let queue_wait_ns = nanos(queue_wait);
        let duration_ns = nanos(duration);
        entry
            .queue_wait_total_ns
            .fetch_add(queue_wait_ns, Ordering::Relaxed);
        record_max(&entry.queue_wait_max_ns, queue_wait_ns);
        entry
            .duration_total_ns
            .fetch_add(duration_ns, Ordering::Relaxed);
        record_max(&entry.duration_max_ns, duration_ns);
        if let Ok(mut recent) = entry.recent.try_lock() {
            if recent.len() == SAMPLE_LIMIT_PER_GROUP {
                recent.pop_front();
            }
            recent.push_back((queue_wait_ns, duration_ns));
        }
        entry.count.fetch_add(1, Ordering::Relaxed);
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    })
}

/// Return the opt-in measurements accumulated by this process.
pub fn snapshot() -> BlockingPoolSnapshot {
    snapshot_with_collection(enabled())
}

fn snapshot_with_collection(collect: bool) -> BlockingPoolSnapshot {
    if !collect {
        return BlockingPoolSnapshot {
            enabled: false,
            queued: 0,
            running: 0,
            peak_occupancy: 0,
            groups: Vec::new(),
        };
    }

    let metrics = metrics();
    let groups = metrics
        .groups
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter_map(|(group, value)| {
            let count = value.count.load(Ordering::Relaxed);
            if count == 0 {
                return None;
            }
            let divisor = count as f64;
            Some(BlockingPoolGroupSnapshot {
                group,
                count,
                queue_wait_avg_ms: value.queue_wait_total_ns.load(Ordering::Relaxed) as f64
                    / divisor
                    / NANOS_PER_MILLISECOND,
                queue_wait_max_ms: value.queue_wait_max_ns.load(Ordering::Relaxed) as f64
                    / NANOS_PER_MILLISECOND,
                queue_wait_p95_ms: group_quantile(value, |sample| sample.0, 95),
                queue_wait_p99_ms: group_quantile(value, |sample| sample.0, 99),
                closure_avg_ms: value.duration_total_ns.load(Ordering::Relaxed) as f64
                    / divisor
                    / NANOS_PER_MILLISECOND,
                closure_max_ms: value.duration_max_ns.load(Ordering::Relaxed) as f64
                    / NANOS_PER_MILLISECOND,
                closure_p95_ms: group_quantile(value, |sample| sample.1, 95),
                closure_p99_ms: group_quantile(value, |sample| sample.1, 99),
            })
        })
        .collect();
    BlockingPoolSnapshot {
        enabled: true,
        queued: metrics.queued.load(Ordering::Relaxed),
        running: metrics.running.load(Ordering::Relaxed),
        peak_occupancy: metrics.peak_occupancy.load(Ordering::Relaxed),
        groups,
    }
}

fn group_quantile(
    entry: &GroupMetrics,
    select: impl Fn((u64, u64)) -> u64,
    percentile: usize,
) -> f64 {
    let Ok(recent) = entry.recent.try_lock() else {
        return 0.0;
    };
    if recent.is_empty() {
        return 0.0;
    }
    let mut values = recent.iter().copied().map(select).collect::<Vec<_>>();
    values.sort_unstable();
    values[(values.len() - 1) * percentile / 100] as f64 / NANOS_PER_MILLISECOND
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const FIXTURE_BLOCKING_THREADS: usize = 1;

    #[test]
    fn fixture_reports_pool_wait_duration_and_occupancy() {
        // Exercise a fixture workload only; never connect to the live backend.
        // Force collection directly so this test does not mutate process env.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(FIXTURE_BLOCKING_THREADS)
            .build()
            .expect("fixture runtime builds");
        let (release_tx, release_rx) = mpsc::channel();
        let (entered_tx, entered_rx) = mpsc::channel();
        runtime.block_on(async {
            let first = submit(
                "fixture/held",
                move || {
                    entered_tx.send(()).expect("test receiver is alive");
                    release_rx.recv().expect("test releases held worker");
                },
                true,
            );
            entered_rx.recv().expect("held worker entered");
            let second = submit("fixture/queued", || 17usize, true);
            let aborted = submit("fixture/aborted", || (), true);
            aborted.abort();
            // The only blocking worker is held at a channel gate, so the
            // second and third closures queue without sleeps or synthetic
            // load. Tokio drops an aborted queued closure when a worker can
            // take it, so release the held worker before awaiting cancellation.
            let occupied = snapshot_with_collection(true);
            assert_eq!(occupied.queued, 2);
            assert_eq!(occupied.running, 1);
            assert_eq!(occupied.peak_occupancy, 3);
            assert!(
                occupied
                    .groups
                    .iter()
                    .all(|group| group.group != "fixture/queued"),
                "the queued fixture closure cannot start before the release gate"
            );
            release_tx.send(()).expect("held worker is waiting");
            first.await.expect("held blocking task completes");
            assert_eq!(second.await.expect("queued task completes"), 17);
            assert!(
                aborted
                    .await
                    .expect_err("queued task was aborted")
                    .is_cancelled()
            );
        });
        let snapshot = snapshot_with_collection(true);
        println!("blocking-pool fixture snapshot: {snapshot:?}");
        assert!(snapshot.enabled);
        assert_eq!(snapshot.queued, 0);
        assert_eq!(snapshot.peak_occupancy, 3);
        let queued = snapshot
            .groups
            .iter()
            .find(|group| group.group == "fixture/queued")
            .expect("queued fixture metrics are retained");
        assert_eq!(queued.count, 1);
        assert!(queued.queue_wait_max_ms > 0.0);
        assert!(queued.queue_wait_p95_ms > 0.0);
        assert!(queued.queue_wait_p99_ms > 0.0);
        assert!(queued.closure_max_ms >= 0.0);
        assert!(
            snapshot
                .groups
                .iter()
                .all(|group| group.group != "fixture/aborted")
        );
    }
}
