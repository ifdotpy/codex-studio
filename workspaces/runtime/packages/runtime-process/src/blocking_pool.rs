//! Opt-in measurements for Tokio's blocking pool.
//!
//! The wrapper adds one cached flag read when disabled. Set
//! `CODEX_RUNTIME_BLOCKING_POOL_DIAGNOSTICS=1` to collect per-group queue wait,
//! closure duration, and occupancy measurements.
//!
//! Measurement procedure: run the isolated `fixture_reports_pool_wait_duration_and_occupancy`
//! test with `-- --nocapture`; it uses a one-worker Tokio blocking pool, holds
//! one fixture closure at a channel gate, queues a second, releases the gate,
//! and reports the snapshot. For a runtime sample, start the Rust server with
//! the environment flag above and read `GET /api/diagnostics` field
//! `blockingPoolMetrics`. No live backend is used by the fixture test.

use std::{
    collections::BTreeMap,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

const DIAGNOSTICS_ENV: &str = "CODEX_RUNTIME_BLOCKING_POOL_DIAGNOSTICS";
const NANOS_PER_MILLISECOND: f64 = 1_000_000.0;

static ENABLED: OnceLock<bool> = OnceLock::new();
static METRICS: OnceLock<Metrics> = OnceLock::new();

struct Metrics {
    queued: AtomicUsize,
    running: AtomicUsize,
    peak_occupancy: AtomicUsize,
    groups: Mutex<BTreeMap<&'static str, GroupMetrics>>,
}

#[derive(Clone, Copy, Default)]
struct GroupMetrics {
    count: u64,
    queue_wait_total_ns: u128,
    queue_wait_max_ns: u128,
    duration_total_ns: u128,
    duration_max_ns: u128,
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
    pub closure_avg_ms: f64,
    pub closure_max_ms: f64,
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
        return tokio::task::spawn_blocking(work);
    }

    let metrics = metrics();
    let submitted_at = Instant::now();
    metrics.queued.fetch_add(1, Ordering::Relaxed);
    tokio::task::spawn_blocking(move || {
        let queue_wait = submitted_at.elapsed();
        metrics.queued.fetch_sub(1, Ordering::Relaxed);
        let running = metrics.running.fetch_add(1, Ordering::Relaxed) + 1;
        update_peak(metrics, running + metrics.queued.load(Ordering::Relaxed));
        let started_at = Instant::now();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
        let duration = started_at.elapsed();
        metrics.running.fetch_sub(1, Ordering::Relaxed);

        let mut groups = metrics
            .groups
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = groups.entry(group).or_default();
        entry.count += 1;
        entry.queue_wait_total_ns += queue_wait.as_nanos();
        entry.queue_wait_max_ns = entry.queue_wait_max_ns.max(queue_wait.as_nanos());
        entry.duration_total_ns += duration.as_nanos();
        entry.duration_max_ns = entry.duration_max_ns.max(duration.as_nanos());
        drop(groups);
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
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let groups = groups
        .iter()
        .map(|(group, value)| {
            let count = value.count.max(1) as f64;
            BlockingPoolGroupSnapshot {
                group,
                count: value.count,
                queue_wait_avg_ms: value.queue_wait_total_ns as f64 / count / NANOS_PER_MILLISECOND,
                queue_wait_max_ms: value.queue_wait_max_ns as f64 / NANOS_PER_MILLISECOND,
                closure_avg_ms: value.duration_total_ns as f64 / count / NANOS_PER_MILLISECOND,
                closure_max_ms: value.duration_max_ns as f64 / NANOS_PER_MILLISECOND,
            }
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
            .unwrap();
        let (release_tx, release_rx) = mpsc::channel();
        let (entered_tx, entered_rx) = mpsc::channel();
        runtime.block_on(async {
            let first = submit(
                "fixture/held",
                move || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                },
                true,
            );
            entered_rx.recv().unwrap();
            let second = submit("fixture/queued", || 17usize, true);
            // The only blocking worker is held at a channel gate, so the
            // second closure queues without sleeps or synthetic load.
            let occupied = snapshot_with_collection(true);
            assert_eq!(occupied.queued, 1);
            assert_eq!(occupied.running, 1);
            assert_eq!(occupied.peak_occupancy, 1);
            assert!(
                occupied
                    .groups
                    .iter()
                    .all(|group| group.group != "fixture/queued"),
                "the queued fixture closure cannot start before the release gate"
            );
            release_tx.send(()).unwrap();
            first.await.unwrap();
            assert_eq!(second.await.unwrap(), 17);
        });
        let snapshot = snapshot_with_collection(true);
        println!("blocking-pool fixture snapshot: {snapshot:?}");
        assert!(snapshot.enabled);
        assert_eq!(snapshot.peak_occupancy, 1);
        let queued = snapshot
            .groups
            .iter()
            .find(|group| group.group == "fixture/queued")
            .unwrap();
        assert_eq!(queued.count, 1);
        assert!(queued.queue_wait_max_ms > 0.0);
        assert!(queued.closure_max_ms >= 0.0);
    }
}
