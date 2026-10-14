//! Bounded, value-free runtime diagnostics collectors.
//!
//! Python's interpreter lock metric has no Rust equivalent. Its nearest honest
//! measure is the store writer mutex wait and transaction hold duration. Rust
//! records HTTP route templates, never request URLs, and SQLite profile
//! durations, never SQL text or bound values. Slow transaction history comes
//! from the actual serialized store boundary; SQLite statement profiles are
//! never labeled as transactions. Startup memory checkpoints contain stage
//! names and byte counts only; allocation traces are intentionally not collected.
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
//! SQL is discarded. Python stack frames, holder identity, transaction journals,
//! and SQLite busy-error counters are not available. Startup captures opt-in
//! Linux current RSS only; peak RSS and allocation traces are unavailable.

mod host;
mod http;
mod locks;
mod sqlite;
mod startup;
mod store;

pub use host::{current_process_id, process_kind, system_snapshot};
pub use http::{http_request_metrics, record_http_request};
pub use locks::{record_store_writer_transaction, runtime_lock_operations, runtime_lock_samples};
pub use sqlite::{
    DatabaseKind, SqliteStatement, TransactionOutcome, TransactionTrace, begin_sqlite_transaction,
    finish_sqlite_transaction, record_sqlite_profile, sqlite_contention, sqlite_history,
};
pub use startup::{StartupMemoryMark, mark_startup_memory, startup_memory_marks};
pub use store::{durable_input_pending, execution_ledger, migration_status};

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Bridge the store-owned optional observer into the nonblocking collectors.
pub fn observe_store_event(event: studio_runtime_store::StoreDiagnosticsEvent) {
    match event {
        studio_runtime_store::StoreDiagnosticsEvent::WriterTransaction { wait, hold } => {
            record_store_writer_transaction(wait, hold);
        }
        studio_runtime_store::StoreDiagnosticsEvent::SqlProfile { database, elapsed } => {
            let database = match database {
                studio_runtime_store::StoreDatabaseKind::Canvas => DatabaseKind::Canvas,
                studio_runtime_store::StoreDatabaseKind::Analytics => DatabaseKind::Analytics,
                studio_runtime_store::StoreDatabaseKind::Other => DatabaseKind::Other,
            };
            record_sqlite_profile(database, elapsed);
        }
    }
}

const LOCK_SAMPLE_LIMIT: usize = 512;
const ROUTE_SAMPLE_LIMIT: usize = 2048;
const SITE_LIMIT: usize = 256;

/// Provider-owned snapshot required to fill the diagnostics schema's account
/// and loaded-thread fields. Implementations must return cached/local state;
/// diagnostics must not initiate native provider requests.
pub trait ProviderDiagnosticsPort: Send + Sync {
    fn snapshot(&self) -> ProviderDiagnosticsSnapshot;
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
    use super::Ring;

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
}
