//! Bounded, value-free runtime diagnostics collectors.
//!
//! Python's interpreter lock metric has no Rust equivalent. Its nearest honest
//! measure is the store writer mutex wait and transaction hold duration. Rust
//! records HTTP route templates, never request URLs, and SQLite profile
//! durations, never SQL text or bound values. Slow transaction history comes
//! from the actual serialized store boundary; SQLite statement profiles are
//! never labeled as transactions. Startup marks contain stage names and byte
//! counts; explicit diagnostics requests can include bounded numeric GNU
//! allocator aggregate counters, never raw XML or allocation traces.
//!
//! Python source parity: `codex_diagnostics.py:130-209,255` (migration and
//! snapshot fields), `codex_execution.py:388-400` (execution chain),
//! `codex_lock_metrics.py:14-60,76-186`, `codex_http_traces.py:50-83`,
//! `codex_sqlite_traces.py:59-298`, and `codex_startup_memory.py:34-65`.
//! Registered meaning differences: lock samples observe Rust store writer
//! transactions rather than `Runtime.lock`; `holder` is always null. Process
//! snapshots use sysinfo's current cross-platform values; CPU usage is sampled
//! once and therefore does not match Python `ps` interval usage. Missing
//! sysinfo values use the schema's null form. HTTP response bytes use the response body's exact size hint
//! and otherwise zero. SQLite profile durations and writer wait/hold aggregates
//! are bounded in memory; slow profile rows use `firstStatement: other` because
//! SQL is discarded. Python stack frames, holder identity, and transaction
//! journals are not available. Rust counts terminal SQLite busy/locked errors
//! crossing the RuntimeStore typed error boundary; raw rusqlite errors handled
//! directly through `ReadConnection` and service-specific error conversions are
//! outside that boundary. A zero count means none were observed at that boundary,
//! not that all-process count is zero. Startup marks capture
//! opt-in Linux current RSS; peak RSS and allocation traces are unavailable.

use futures_util::future::BoxFuture;
use std::future::Future;
use std::time::Instant;

mod allocator;
mod host;
mod http;
mod locks;
mod sqlite;
mod startup;
mod store;

/// Opt-in switch for runtime stage timings: `CODEX_RUNTIME_STAGE_TIMINGS=1`.
/// The environment is read once.
fn runtime_stage_timings_enabled() -> bool {
    use std::sync::OnceLock;

    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("CODEX_RUNTIME_STAGE_TIMINGS").is_ok_and(|value| value == "1")
    })
}

/// The only writer of the `runtime_stage` line. Unit for `elapsed_ms`: milliseconds.
fn log_runtime_stage(name: &'static str, started: Instant) {
    eprintln!(
        "runtime_stage name={name} elapsed_ms={}",
        started.elapsed().as_millis()
    );
}

/// Record one fixed-name runtime stage duration only when explicitly enabled.
/// The flag is evaluated once; measurements contain no request, account, or
/// record identities. Unit for `elapsed_ms`: milliseconds.
pub(crate) fn measure_runtime_stage<T>(name: &'static str, operation: impl FnOnce() -> T) -> T {
    if !runtime_stage_timings_enabled() {
        return operation();
    }
    let started = Instant::now();
    let result = operation();
    log_runtime_stage(name, started);
    result
}

/// Async form of [`measure_runtime_stage`] for awaited phases. The operation is
/// awaited exactly once; its output and error are returned unchanged.
pub(crate) async fn measure_runtime_stage_async<T>(
    name: &'static str,
    operation: impl Future<Output = T>,
) -> T {
    if !runtime_stage_timings_enabled() {
        return operation.await;
    }
    let started = Instant::now();
    let result = operation.await;
    log_runtime_stage(name, started);
    result
}

pub use allocator::log_on_demand_memory_snapshot;
pub use host::{current_process_id, process_kind, system_snapshot};
pub use http::{http_request_metrics, record_http_request};
pub use locks::{record_store_writer_transaction, runtime_lock_operations, runtime_lock_samples};
use sqlite::record_sqlite_lock_error_snapshot;
pub use sqlite::{
    DatabaseKind, SqliteStatement, TransactionOutcome, TransactionTrace, begin_sqlite_transaction,
    finish_sqlite_transaction, record_commit_profile, record_sqlite_profile,
    record_store_admission, record_store_waiter_occupancy, record_wal_checkpoint,
    sqlite_contention, sqlite_history,
};
pub use startup::{StartupMemoryMark, mark_startup_memory, startup_memory_marks};
pub use store::{durable_input_pending, execution_ledger, migration_status};

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Bridge the store-owned optional observer into the nonblocking collectors.
pub fn observe_store_event(event: studio_runtime_store::StoreDiagnosticsEvent) {
    match event {
        studio_runtime_store::StoreDiagnosticsEvent::WriterTransaction { wait, hold, origin } => {
            record_store_writer_transaction(wait, hold, origin);
        }
        studio_runtime_store::StoreDiagnosticsEvent::WriterAdmission {
            wait,
            waited,
            waiting_threads,
            peak_waiters,
        } => record_store_admission(false, waited, wait, waiting_threads, peak_waiters, 0),
        studio_runtime_store::StoreDiagnosticsEvent::WriterWaiters {
            waiting_threads,
            peak_waiters,
        } => record_store_waiter_occupancy(false, waiting_threads, peak_waiters, 0),
        studio_runtime_store::StoreDiagnosticsEvent::ReadAdmission {
            wait,
            waited,
            waiting_threads,
            peak_waiters,
            opened_connections,
        } => record_store_admission(
            true,
            waited,
            wait,
            waiting_threads,
            peak_waiters,
            opened_connections,
        ),
        studio_runtime_store::StoreDiagnosticsEvent::ReadWaiters {
            waiting_threads,
            peak_waiters,
            opened_connections,
        } => record_store_waiter_occupancy(true, waiting_threads, peak_waiters, opened_connections),
        studio_runtime_store::StoreDiagnosticsEvent::SqlProfile { database, elapsed } => {
            let database = match database {
                studio_runtime_store::StoreDatabaseKind::Canvas => DatabaseKind::Canvas,
                studio_runtime_store::StoreDatabaseKind::Analytics => DatabaseKind::Analytics,
                studio_runtime_store::StoreDatabaseKind::Other => DatabaseKind::Other,
            };
            record_sqlite_profile(database, elapsed);
        }
        studio_runtime_store::StoreDiagnosticsEvent::CommitDuration { database, elapsed } => {
            let database = match database {
                studio_runtime_store::StoreDatabaseKind::Canvas => DatabaseKind::Canvas,
                studio_runtime_store::StoreDatabaseKind::Analytics => DatabaseKind::Analytics,
                studio_runtime_store::StoreDatabaseKind::Other => DatabaseKind::Other,
            };
            record_commit_profile(database, elapsed);
        }
        studio_runtime_store::StoreDiagnosticsEvent::SqliteLockError { total } => {
            record_sqlite_lock_error_snapshot(total);
        }
        studio_runtime_store::StoreDiagnosticsEvent::WalCheckpoint {
            database,
            wal_size_bytes,
            busy,
            log_frames,
            checkpointed_frames,
        } => {
            let database = match database {
                studio_runtime_store::StoreDatabaseKind::Canvas => DatabaseKind::Canvas,
                studio_runtime_store::StoreDatabaseKind::Analytics => DatabaseKind::Analytics,
                studio_runtime_store::StoreDatabaseKind::Other => DatabaseKind::Other,
            };
            record_wal_checkpoint(
                database,
                wal_size_bytes,
                busy,
                log_frames,
                checkpointed_frames,
            );
        }
    }
}

const LOCK_SAMPLE_LIMIT: usize = 512;
const ROUTE_SAMPLE_LIMIT: usize = 2048;
const SITE_LIMIT: usize = 256;

/// Provider-owned snapshot required to fill the diagnostics schema's account,
/// loaded-thread, and queue fields. Implementations may issue bounded read-only
/// requests through already-retained provider sessions; they must never launch
/// a CLI, open another connection, or perform a provider mutation. Local file
/// and SQLite snapshots stay on a blocking pool; provider requests are awaited.
pub trait ProviderDiagnosticsPort: Send + Sync {
    fn snapshot<'a>(&'a self) -> BoxFuture<'a, ProviderDiagnosticsSnapshot>;
}

#[derive(Clone, Debug)]
pub struct ProviderDiagnosticsSnapshot {
    pub native_accounts: serde_json::Value,
    pub studio_loaded_threads: u64,
    pub queues: serde_json::Map<String, serde_json::Value>,
}

/// Supervisor status is owned by runtime-server, which injects this snapshot
/// without diagnostics reaching into process supervision internals.
pub trait SupervisorDiagnosticsPort: Send + Sync {
    fn snapshot(&self) -> SupervisorDiagnosticsSnapshot;
}

#[derive(Clone, Debug)]
pub struct SupervisorDiagnosticsSnapshot {
    pub mode: bool,
    pub fallback: bool,
    pub notice: Option<String>,
    pub health: Option<serde_json::Value>,
}

#[derive(Clone, Debug)]
struct Ring {
    limit: usize,
    values: Vec<u64>,
    index: usize,
    count: u64,
}

impl Ring {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            values: vec![0; limit],
            index: 0,
            count: 0,
        }
    }

    fn record(&mut self, value: u64) {
        self.values[self.index] = value;
        self.index = (self.index + 1) % self.limit;
        self.count = self.count.saturating_add(1);
    }

    fn snapshot(&self) -> (u64, Vec<u64>) {
        let used = usize::try_from(self.count.min(self.limit as u64)).unwrap_or(self.limit);
        let values = if used < self.limit {
            self.values[..used].to_vec()
        } else {
            self.values[self.index..]
                .iter()
                .chain(self.values[..self.index].iter())
                .copied()
                .collect()
        };
        (self.count, values)
    }
}

fn quantiles(values: &[u64], include_p99: bool, divisor: f64) -> serde_json::Value {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let at = |percent: usize| -> f64 {
        sorted
            .get(
                ((sorted.len().saturating_sub(1)) * percent / 100)
                    .min(sorted.len().saturating_sub(1)),
            )
            .copied()
            .unwrap_or_default() as f64
            / divisor
    };
    let mut summary = serde_json::json!({
        "samples": sorted.len(),
        "p50": at(50),
        "p95": at(95),
        "max": sorted.last().copied().unwrap_or_default() as f64 / divisor,
    });
    if include_p99 {
        summary["p99"] = serde_json::json!(at(99));
    }
    summary
}

fn try_with_state<T>(state: &std::sync::Mutex<T>, f: impl FnOnce(&mut T)) -> bool {
    match state.try_lock() {
        Ok(mut guard) => {
            f(&mut guard);
            true
        }
        Err(std::sync::TryLockError::Poisoned(error)) => {
            f(&mut error.into_inner());
            true
        }
        Err(std::sync::TryLockError::WouldBlock) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{Ring, measure_runtime_stage, measure_runtime_stage_async};

    #[test]
    fn ring_keeps_only_the_newest_fixed_capacity_samples() {
        let mut ring = Ring::new(512);
        for value in 0..=512 {
            ring.record(value);
        }

        let (count, samples) = ring.snapshot();
        assert_eq!(count, 513);
        assert_eq!(samples.len(), 512);
        assert_eq!(samples.first(), Some(&1));
        assert_eq!(samples.last(), Some(&512));
    }

    #[test]
    fn sync_stage_wrapper_runs_once_and_returns_value_and_error() {
        let mut calls = 0;
        let value = measure_runtime_stage("test.sync", || {
            calls += 1;
            7
        });
        assert_eq!((calls, value), (1, 7));

        let failed: Result<u8, &str> = measure_runtime_stage("test.sync", || Err("failed"));
        assert_eq!(failed, Err("failed"));
    }

    #[tokio::test]
    async fn async_stage_wrapper_awaits_once_and_returns_value_and_error() {
        let mut calls = 0;
        let value = measure_runtime_stage_async("test.async", async {
            calls += 1;
            7
        })
        .await;
        assert_eq!((calls, value), (1, 7));

        let failed: Result<u8, &str> =
            measure_runtime_stage_async("test.async", async { Err("failed") }).await;
        assert_eq!(failed, Err("failed"));
    }
}
