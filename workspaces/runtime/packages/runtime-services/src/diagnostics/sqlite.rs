use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};

use super::{Ring, try_with_state};

const SLOW_TRANSACTION: Duration = Duration::from_secs(1);
const ACTIVE_LIMIT: usize = 256;
const HISTORY_LIMIT: usize = 64;
const LONGEST_LIMIT: usize = 8;
const LONG_TRANSACTION_MS: f64 = 100.0;
const WRITE_WAIT_MS: f64 = 1.0;

#[derive(Clone, Copy, Debug, Ord, PartialOrd, Eq, PartialEq, Serialize)]
pub enum DatabaseKind {
    #[serde(rename = "canvas.sqlite3")]
    Canvas,
    #[serde(rename = "analytics.sqlite3")]
    Analytics,
    #[serde(rename = "other")]
    Other,
}

#[derive(Clone, Copy, Debug, Ord, PartialOrd, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SqliteStatement {
    Begin,
    Insert,
    Update,
    Delete,
    Replace,
    Create,
    Drop,
    Alter,
    With,
    #[serde(rename = "SAVEPOINT")]
    Savepoint,
    #[serde(rename = "other")]
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TransactionOutcome {
    Committed,
    RolledBack,
    Failed,
}

#[derive(Clone, Debug)]
struct TraceEntry {
    id: u64,
    started_at: Instant,
}

#[derive(Debug)]
struct SqliteState {
    active: BTreeMap<u64, TraceEntry>,
    recent: VecDeque<Value>,
    longest: Vec<Value>,
    profile_by_database: BTreeMap<DatabaseKind, (u64, Ring)>,
    transaction_count: u64,
    long_transactions: u64,
    write_waits: u64,
    write_wait_ms: f64,
    write_wait_durations_ms: Ring,
    longest_transaction_ms: f64,
    transaction_durations_ms: Ring,
    coverage_started_at: f64,
    untracked: u64,
}

impl Default for SqliteState {
    fn default() -> Self {
        Self {
            active: BTreeMap::new(),
            recent: VecDeque::with_capacity(HISTORY_LIMIT),
            longest: Vec::with_capacity(LONGEST_LIMIT),
            profile_by_database: BTreeMap::new(),
            transaction_count: 0,
            long_transactions: 0,
            write_waits: 0,
            write_wait_ms: 0.0,
            write_wait_durations_ms: Ring::new(HISTORY_LIMIT),
            longest_transaction_ms: 0.0,
            transaction_durations_ms: Ring::new(HISTORY_LIMIT),
            coverage_started_at: epoch_seconds(),
            untracked: 0,
        }
    }
}

/// Observe one serialized store transaction for the SQLite contention view.
pub fn record_store_transaction(wait: Duration, hold: Duration) {
    let wait_ms = wait.as_secs_f64() * 1000.0;
    let hold_ms = hold.as_secs_f64() * 1000.0;
    let _ = try_with_state(state(), |state| {
        state.transaction_count = state.transaction_count.saturating_add(1);
        state.long_transactions = state
            .long_transactions
            .saturating_add(u64::from(hold_ms >= LONG_TRANSACTION_MS));
        if wait_ms >= WRITE_WAIT_MS {
            state.write_waits = state.write_waits.saturating_add(1);
            state.write_wait_ms += wait_ms;
        }
        if wait_ms >= WRITE_WAIT_MS {
            state
                .write_wait_durations_ms
                .record(wait.as_millis().min(u128::from(u64::MAX)) as u64);
        }
        state.longest_transaction_ms = state.longest_transaction_ms.max(hold_ms);
        state
            .transaction_durations_ms
            .record(hold.as_millis().min(u128::from(u64::MAX)) as u64);
        if hold >= SLOW_TRANSACTION
            && let Some(thread_id) = native_thread_id()
        {
            let elapsed_seconds = hold.as_secs_f64();
            let finished_at = epoch_seconds();
            let entry = json!({
                "id": format!("rust:{}:{}", std::process::id(), NEXT_ID.fetch_add(1, Ordering::Relaxed)),
                "pid": std::process::id(),
                "site": "runtime_store.writer_transaction",
                "database": DatabaseKind::Canvas,
                "threadId": thread_id,
                "nativeThreadId": thread_id,
                "threadName": "runtime",
                "originFrames": [],
                "firstStatement": SqliteStatement::Begin,
                "originRecorded": false,
                "startedAtMonotonic": process_monotonic_seconds() - elapsed_seconds,
                "startedAt": finished_at - elapsed_seconds,
                "finishedAt": finished_at,
                "durationMs": elapsed_seconds * 1000.0,
                "observations": [],
                "state": "ended",
            });
            push_history(state, entry);
        }
    });
}

/// Accept one rusqlite profile event without retaining its SQL text.
pub fn record_sqlite_profile(database: DatabaseKind, elapsed: Duration) {
    let _ = try_with_state(state(), |state| {
        let (count, samples) = state
            .profile_by_database
            .entry(database)
            .or_insert_with(|| (0, Ring::new(HISTORY_LIMIT)));
        *count = count.saturating_add(1);
        samples.record(elapsed.as_millis().min(u128::from(u64::MAX)) as u64);
    });
}

fn push_history(state: &mut SqliteState, entry: Value) {
    if state.recent.len() == HISTORY_LIMIT {
        state.recent.pop_front();
    }
    state.recent.push_back(entry.clone());
    state.longest.push(entry);
    state.longest.sort_by(|left, right| {
        right["durationMs"]
            .as_f64()
            .unwrap_or_default()
            .total_cmp(&left["durationMs"].as_f64().unwrap_or_default())
    });
    state.longest.truncate(LONGEST_LIMIT);
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static SQLITE: OnceLock<Mutex<SqliteState>> = OnceLock::new();

fn state() -> &'static Mutex<SqliteState> {
    SQLITE.get_or_init(|| Mutex::new(SqliteState::default()))
}

/// Opaque transaction token. It carries no connection, SQL, path, or parameter.
#[derive(Debug)]
pub struct TransactionTrace(TraceEntry);

/// Start tracing one transaction without waiting if diagnostics are contended.
pub fn begin_sqlite_transaction(
    _database: DatabaseKind,
    _statement: SqliteStatement,
) -> Option<TransactionTrace> {
    let entry = TraceEntry {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        started_at: Instant::now(),
    };
    let token = TransactionTrace(entry.clone());
    let recorded = try_with_state(state(), |state| track_active(state, entry));
    recorded.then_some(token)
}

fn track_active(state: &mut SqliteState, entry: TraceEntry) {
    if state.active.len() >= ACTIVE_LIMIT {
        state.untracked = state.untracked.saturating_add(1);
    } else {
        state.active.insert(entry.id, entry);
    }
}

/// Finish an owned transaction; only slow scopes enter the bounded history.
pub fn finish_sqlite_transaction(token: TransactionTrace, outcome: TransactionOutcome) {
    let elapsed = token.0.started_at.elapsed();
    let id = token.0.id;
    let _ = try_with_state(state(), |state| {
        state.active.remove(&id);
        let _ = (elapsed, outcome);
    });
}

/// A value-free history view for operators and the diagnostics HTTP adapter.
pub fn sqlite_history() -> Value {
    let mut result = json!({
        "coverageStartedAt": 0,
        "thresholdMs": SLOW_TRANSACTION.as_millis(),
        "recent": [],
        "longest": [],
        "journal": {"status": "notWritten"},
        "activeTracking": {"tracked": 0, "limit": ACTIVE_LIMIT, "untrackedStarts": 0},
    });
    let _ = try_with_state(state(), |state| {
        result["coverageStartedAt"] = json!(state.coverage_started_at);
        result["recent"] = json!(state.recent);
        result["longest"] = json!(state.longest);
        result["activeTracking"] = json!({
            "tracked": state.active.len(),
            "limit": ACTIVE_LIMIT,
            "untrackedStarts": state.untracked,
        });
    });
    result
}

/// Build the committed OpenAPI `SqliteContention` shape from store-owned
/// observations. `writeWait` is the Rust writer mutex wait, the nearest
/// available equivalent to Python's SQLite write-lock wait.
pub fn sqlite_contention() -> Value {
    let mut result = json!({
        "activeTransactions": [],
        "activeTransactionScan": {"threads": 0, "frames": 0, "locals": 0, "truncated": false},
        "slowTransactions": sqlite_history(),
        "transaction": null,
        "writeWait": null,
        "lockError": null,
    });
    let _ = try_with_state(state(), |state| {
        let (_, durations) = state.transaction_durations_ms.snapshot();
        let metric = json!({
            "transactions": state.transaction_count,
            "longTransactions": state.long_transactions,
            "writeWaits": state.write_waits,
            "writeWaitMs": state.write_wait_ms,
            "lockErrors": 0,
            "longestTransactionMs": state.longest_transaction_ms,
            "p50TransactionMs": quantile(&durations, 50),
            "p95TransactionMs": quantile(&durations, 95),
            "sites": {
                "runtime_store.writer_transaction": {
                    "transactions": state.transaction_count,
                    "longTransactions": state.long_transactions,
                    "writeWaits": state.write_waits,
                    "writeWaitMs": state.write_wait_ms,
                    "lockErrors": 0,
                    "longestTransactionMs": state.longest_transaction_ms,
                    "p50TransactionMs": quantile(&durations, 50),
                    "p95TransactionMs": quantile(&durations, 95),
                    "sites": {}
                }
            }
        });
        result["transaction"] = metric;
        let (_, waits) = state.write_wait_durations_ms.snapshot();
        result["writeWait"] = json!({
            "transactions": 0,
            "longTransactions": 0,
            "writeWaits": state.write_waits,
            "writeWaitMs": state.write_wait_ms,
            "lockErrors": 0,
            "longestTransactionMs": waits.iter().max().copied().unwrap_or_default() as f64,
            "p50TransactionMs": quantile(&waits, 50),
            "p95TransactionMs": quantile(&waits, 95),
            "sites": {}
        });
    });
    result
}

fn quantile(values: &[u64], percentile: usize) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut values = values.to_vec();
    values.sort_unstable();
    let index = (values.len() - 1) * percentile / 100;
    Some(values[index] as f64)
}

fn epoch_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or_default()
}

static PROCESS_MONOTONIC_START: OnceLock<Instant> = OnceLock::new();

fn process_monotonic_seconds() -> f64 {
    PROCESS_MONOTONIC_START
        .get_or_init(Instant::now)
        .elapsed()
        .as_secs_f64()
}

#[cfg(target_os = "linux")]
fn native_thread_id() -> Option<u64> {
    u64::try_from(nix::unistd::gettid().as_raw()).ok()
}

#[cfg(not(target_os = "linux"))]
fn native_thread_id() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_record_contains_only_closed_metadata() {
        let token = begin_sqlite_transaction(DatabaseKind::Canvas, SqliteStatement::Update)
            .expect("collector available");
        finish_sqlite_transaction(token, TransactionOutcome::Committed);
        let value = sqlite_history();
        let text = value.to_string();
        assert!(!text.contains("SELECT"));
        assert!(!text.contains("home"));
        assert_eq!(value["activeTracking"]["limit"], ACTIVE_LIMIT);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn slow_statements_do_not_masquerade_as_slow_transactions() {
        for _ in 0..=HISTORY_LIMIT {
            record_sqlite_profile(DatabaseKind::Canvas, Duration::from_secs(2));
        }
        let history = sqlite_history();
        let recent = history["recent"].as_array().expect("slow profile history");
        assert!(recent.is_empty());
        assert!(history["longest"].as_array().expect("longest").is_empty());
    }

    #[test]
    fn actual_writer_transactions_fill_only_bounded_slow_history() {
        for _ in 0..=HISTORY_LIMIT {
            record_store_transaction(Duration::ZERO, Duration::from_secs(2));
        }
        let history = sqlite_history();
        let recent = history["recent"]
            .as_array()
            .expect("slow transaction history");
        let entry = recent.last().expect("actual transaction record");
        assert_eq!(entry["site"], "runtime_store.writer_transaction");
        assert_eq!(entry["database"], "canvas.sqlite3");
        assert_eq!(entry["firstStatement"], "BEGIN");
        assert_eq!(entry["state"], "ended");
        assert_eq!(recent.len(), HISTORY_LIMIT);
        assert_eq!(
            history["longest"].as_array().map(Vec::len),
            Some(LONGEST_LIMIT)
        );
        let serialized = history.to_string();
        assert!(!serialized.contains("SELECT"));
        assert!(!serialized.contains("home"));
    }

    #[test]
    fn active_transaction_tracking_stops_at_its_fixed_capacity() {
        let mut state = SqliteState::default();
        for id in 0..=ACTIVE_LIMIT {
            track_active(
                &mut state,
                TraceEntry {
                    id: id as u64,
                    started_at: Instant::now(),
                },
            );
        }
        assert_eq!(state.active.len(), ACTIVE_LIMIT);
        assert_eq!(state.untracked, 1);
    }
}
