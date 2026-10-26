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
    commit_profile_by_database: BTreeMap<DatabaseKind, (u64, Ring)>,
    read_admission: StoreAdmissionSamples,
    writer_admission: StoreAdmissionSamples,
    wal_checkpoint_by_database: BTreeMap<DatabaseKind, Value>,
    transaction_count: u64,
    long_transactions: u64,
    lock_error_snapshot: u64,
    write_waits: u64,
    write_wait_ms: f64,
    write_wait_durations_ns: Ring,
    longest_transaction_ms: f64,
    transaction_durations_ns: Ring,
    coverage_started_at: f64,
    untracked: u64,
}

#[derive(Debug, Default)]
struct StoreAdmissionSamples {
    operations: u64,
    waits: u64,
    active_waiters: usize,
    peak_waiters: usize,
    opened_connections: usize,
    wait_ns: Option<Ring>,
}

impl Default for SqliteState {
    fn default() -> Self {
        Self {
            active: BTreeMap::new(),
            recent: VecDeque::with_capacity(HISTORY_LIMIT),
            longest: Vec::with_capacity(LONGEST_LIMIT),
            profile_by_database: BTreeMap::new(),
            commit_profile_by_database: BTreeMap::new(),
            read_admission: StoreAdmissionSamples::default(),
            writer_admission: StoreAdmissionSamples::default(),
            wal_checkpoint_by_database: BTreeMap::new(),
            transaction_count: 0,
            long_transactions: 0,
            lock_error_snapshot: 0,
            write_waits: 0,
            write_wait_ms: 0.0,
            write_wait_durations_ns: Ring::new(HISTORY_LIMIT),
            longest_transaction_ms: 0.0,
            transaction_durations_ns: Ring::new(HISTORY_LIMIT),
            coverage_started_at: epoch_seconds(),
            untracked: 0,
        }
    }
}

/// Observe one serialized store transaction for the SQLite contention view.
/// `wait` includes the writer actor queue; `hold` is the SQLite job alone.
pub fn record_store_transaction(
    wait: Duration,
    hold: Duration,
    origin: &'static std::panic::Location<'static>,
) {
    let _ = try_with_state(state(), |state| {
        record_store_transaction_in_state(state, wait, hold, origin);
    });
}

fn record_store_transaction_in_state(
    state: &mut SqliteState,
    wait: Duration,
    hold: Duration,
    origin: &'static std::panic::Location<'static>,
) {
    let wait_ms = wait.as_secs_f64() * 1000.0;
    let hold_ms = hold.as_secs_f64() * 1000.0;
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
            .write_wait_durations_ns
            .record(wait.as_nanos().min(u128::from(u64::MAX)) as u64);
    }
    state.longest_transaction_ms = state.longest_transaction_ms.max(hold_ms);
    state
        .transaction_durations_ns
        .record(hold.as_nanos().min(u128::from(u64::MAX)) as u64);
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
            "originFrames": [{
                "file": origin.file(),
                "function": "store writer caller",
                "line": origin.line(),
            }],
            "firstStatement": SqliteStatement::Begin,
            "originRecorded": true,
            "startedAtMonotonic": process_monotonic_seconds() - elapsed_seconds,
            "startedAt": finished_at - elapsed_seconds,
            "finishedAt": finished_at,
            "durationMs": elapsed_seconds * 1000.0,
            "observations": [],
            "state": "ended",
        });
        push_history(state, entry);
    }
}

/// Accept one rusqlite profile event without retaining its SQL text.
pub fn record_sqlite_profile(database: DatabaseKind, elapsed: Duration) {
    let _ = try_with_state(state(), |state| {
        record_sqlite_profile_in_state(state, database, elapsed);
    });
}

pub fn record_store_admission(
    read: bool,
    waited: bool,
    elapsed: Duration,
    waiting_threads: usize,
    peak_waiters: usize,
    opened_connections: usize,
) {
    let _ = try_with_state(state(), |state| {
        record_store_admission_in_state(
            state,
            read,
            waited,
            elapsed,
            waiting_threads,
            peak_waiters,
            opened_connections,
        );
    });
}

fn record_store_admission_in_state(
    state: &mut SqliteState,
    read: bool,
    waited: bool,
    elapsed: Duration,
    waiting_threads: usize,
    peak_waiters: usize,
    opened_connections: usize,
) {
    let samples = if read {
        &mut state.read_admission
    } else {
        &mut state.writer_admission
    };
    samples.operations = samples.operations.saturating_add(1);
    samples.active_waiters = waiting_threads;
    samples.peak_waiters = samples.peak_waiters.max(peak_waiters);
    samples.opened_connections = opened_connections;
    if waited {
        samples.waits = samples.waits.saturating_add(1);
        samples
            .wait_ns
            .get_or_insert_with(|| Ring::new(HISTORY_LIMIT))
            .record(elapsed.as_nanos().min(u128::from(u64::MAX)) as u64);
    }
}

/// Update queue occupancy without counting a new admission or latency sample.
pub fn record_store_waiter_occupancy(
    read: bool,
    waiting_threads: usize,
    peak_waiters: usize,
    opened_connections: usize,
) {
    let _ = try_with_state(state(), |state| {
        record_store_waiter_occupancy_in_state(
            state,
            read,
            waiting_threads,
            peak_waiters,
            opened_connections,
        );
    });
}

fn record_store_waiter_occupancy_in_state(
    state: &mut SqliteState,
    read: bool,
    waiting_threads: usize,
    peak_waiters: usize,
    opened_connections: usize,
) {
    let samples = if read {
        &mut state.read_admission
    } else {
        &mut state.writer_admission
    };
    samples.active_waiters = waiting_threads;
    samples.peak_waiters = samples.peak_waiters.max(peak_waiters);
    if read {
        samples.opened_connections = opened_connections;
    }
}

pub fn record_commit_profile(database: DatabaseKind, elapsed: Duration) {
    let _ = try_with_state(state(), |state| {
        let (count, samples) = state
            .commit_profile_by_database
            .entry(database)
            .or_insert_with(|| (0, Ring::new(HISTORY_LIMIT)));
        *count = count.saturating_add(1);
        samples.record(elapsed.as_nanos().min(u128::from(u64::MAX)) as u64);
    });
}

/// Count one terminal SQLite busy or locked error reported by RuntimeStore.
pub(super) fn record_sqlite_lock_error_snapshot(total: u64) {
    let _ = try_with_state(state(), |state| {
        record_sqlite_lock_error_snapshot_in_state(state, total);
    });
}

fn record_sqlite_lock_error_snapshot_in_state(state: &mut SqliteState, total: u64) {
    state.lock_error_snapshot = state.lock_error_snapshot.max(total);
}

/// Retain the latest bounded WAL size and passive-checkpoint result per file.
pub fn record_wal_checkpoint(
    database: DatabaseKind,
    wal_size_bytes: u64,
    busy: i64,
    log_frames: i64,
    checkpointed_frames: i64,
) {
    let _ = try_with_state(state(), |state| {
        record_wal_checkpoint_in_state(
            state,
            database,
            wal_size_bytes,
            busy,
            log_frames,
            checkpointed_frames,
        );
    });
}

fn record_wal_checkpoint_in_state(
    state: &mut SqliteState,
    database: DatabaseKind,
    wal_size_bytes: u64,
    busy: i64,
    log_frames: i64,
    checkpointed_frames: i64,
) {
    state.wal_checkpoint_by_database.insert(
        database,
        json!({
            "walSizeBytes": wal_size_bytes,
            "busy": busy,
            "logFrames": log_frames.max(0),
            "checkpointedFrames": checkpointed_frames.max(0),
        }),
    );
}

fn record_sqlite_profile_in_state(
    state: &mut SqliteState,
    database: DatabaseKind,
    elapsed: Duration,
) {
    let (count, samples) = state
        .profile_by_database
        .entry(database)
        .or_insert_with(|| (0, Ring::new(HISTORY_LIMIT)));
    *count = count.saturating_add(1);
    samples.record(elapsed.as_nanos().min(u128::from(u64::MAX)) as u64);
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
        result = sqlite_history_from_state(state);
    });
    result
}

fn sqlite_history_from_state(state: &SqliteState) -> Value {
    json!({
        "coverageStartedAt": state.coverage_started_at,
        "thresholdMs": SLOW_TRANSACTION.as_millis(),
        "recent": state.recent,
        "longest": state.longest,
        "journal": {"status": "notWritten"},
        "activeTracking": {
            "tracked": state.active.len(),
            "limit": ACTIVE_LIMIT,
            "untrackedStarts": state.untracked,
        },
    })
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
        "walTelemetry": null,
        "storeAdmission": empty_store_admission(),
        "sqlProfile": {},
        "commitProfile": {},
    });
    let _ = try_with_state(state(), |state| {
        let lock_errors =
            studio_runtime_store::sqlite_lock_error_count().max(state.lock_error_snapshot);
        result = sqlite_contention_from_state(state, lock_errors);
    });
    result
}

fn sqlite_contention_from_state(state: &SqliteState, lock_errors: u64) -> Value {
    let (_, durations) = state.transaction_durations_ns.snapshot();
    let transaction = json!({
        "transactions": state.transaction_count,
        "longTransactions": state.long_transactions,
        "writeWaits": state.write_waits,
        "writeWaitMs": state.write_wait_ms,
        "lockErrors": lock_errors,
        "longestTransactionMs": state.longest_transaction_ms,
        "p50TransactionMs": quantile_ms(&durations, 50),
        "p95TransactionMs": quantile_ms(&durations, 95),
        "p99TransactionMs": quantile_ms(&durations, 99),
        "sites": {
            "runtime_store.writer_transaction": {
                "transactions": state.transaction_count,
                "longTransactions": state.long_transactions,
                "writeWaits": state.write_waits,
                "writeWaitMs": state.write_wait_ms,
                "lockErrors": lock_errors,
                "longestTransactionMs": state.longest_transaction_ms,
                "p50TransactionMs": quantile_ms(&durations, 50),
                "p95TransactionMs": quantile_ms(&durations, 95),
                "p99TransactionMs": quantile_ms(&durations, 99),
                "sites": {}
            }
        }
    });
    let (_, waits) = state.write_wait_durations_ns.snapshot();
    let write_wait = json!({
        "transactions": 0,
        "longTransactions": 0,
        "writeWaits": state.write_waits,
        "writeWaitMs": state.write_wait_ms,
        "lockErrors": lock_errors,
        "longestTransactionMs": waits.iter().max().copied().unwrap_or_default() as f64 / 1_000_000.0,
        "p50TransactionMs": quantile_ms(&waits, 50),
        "p95TransactionMs": quantile_ms(&waits, 95),
        "p99TransactionMs": quantile_ms(&waits, 99),
        "sites": {}
    });
    let lock_error = json!({
        "transactions": 0,
        "longTransactions": 0,
        "writeWaits": 0,
        "writeWaitMs": 0.0,
        "lockErrors": lock_errors,
        "longestTransactionMs": 0.0,
        "p50TransactionMs": null,
        "p95TransactionMs": null,
        "p99TransactionMs": null,
        "sites": {}
    });
    json!({
        "activeTransactions": [],
        "activeTransactionScan": {"threads": 0, "frames": 0, "locals": 0, "truncated": false},
        "slowTransactions": sqlite_history_from_state(state),
        "transaction": transaction,
        "writeWait": write_wait,
        "lockError": lock_error,
        "walTelemetry": wal_telemetry_from_state(state),
        "storeAdmission": {
            "reads": admission_metric(&state.read_admission),
            "writes": admission_metric(&state.writer_admission),
        },
        "sqlProfile": profile_metrics(&state.profile_by_database),
        "commitProfile": profile_metrics(&state.commit_profile_by_database),
    })
}

fn empty_store_admission() -> Value {
    let empty = StoreAdmissionSamples::default();
    json!({
        "reads": admission_metric(&empty),
        "writes": admission_metric(&empty),
    })
}

fn wal_telemetry_from_state(state: &SqliteState) -> Value {
    json!({
        "canvas": state.wal_checkpoint_by_database.get(&DatabaseKind::Canvas),
        "analytics": state.wal_checkpoint_by_database.get(&DatabaseKind::Analytics),
    })
}

fn admission_metric(samples: &StoreAdmissionSamples) -> Value {
    let waits = samples
        .wait_ns
        .as_ref()
        .map(|ring| ring.snapshot().1)
        .unwrap_or_default();
    let mut summary =
        json!({"samples": waits.len(), "p50Ms": null, "p95Ms": null, "p99Ms": null, "maxMs": 0.0});
    if !waits.is_empty() {
        let values = quantile_summary(&waits, 1_000_000.0);
        summary = values;
    }
    json!({
        "operations": samples.operations,
        "waits": samples.waits,
        "activeWaiters": samples.active_waiters,
        "peakWaiters": samples.peak_waiters,
        "openedConnections": samples.opened_connections,
        "waitMs": summary,
    })
}

fn profile_metrics(profiles: &BTreeMap<DatabaseKind, (u64, Ring)>) -> Value {
    let mut result = serde_json::Map::new();
    for (database, (count, ring)) in profiles {
        let samples = ring.snapshot().1;
        result.insert(
            format!("{database:?}").to_ascii_lowercase(),
            json!({"count": count, "p50Ms": quantile_ms(&samples, 50), "p95Ms": quantile_ms(&samples, 95), "p99Ms": quantile_ms(&samples, 99)}),
        );
    }
    Value::Object(result)
}

fn quantile_summary(values: &[u64], divisor: f64) -> Value {
    json!({
        "samples": values.len(),
        "p50Ms": quantile(values, 50).map(|value| value / divisor),
        "p95Ms": quantile(values, 95).map(|value| value / divisor),
        "p99Ms": quantile(values, 99).map(|value| value / divisor),
        "maxMs": values.iter().max().map(|value| *value as f64 / divisor).unwrap_or_default(),
    })
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

fn quantile_ms(values: &[u64], percentile: usize) -> Option<f64> {
    quantile(values, percentile).map(|value| value / 1_000_000.0)
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
        let mut state = SqliteState::default();
        record_store_transaction_in_state(
            &mut state,
            Duration::ZERO,
            Duration::from_secs(2),
            std::panic::Location::caller(),
        );
        let value = sqlite_history_from_state(&state);
        let text = value.to_string();
        assert!(!text.contains("SELECT"));
        assert!(!text.contains("home"));
        assert_eq!(value["activeTracking"]["limit"], ACTIVE_LIMIT);
    }

    #[test]
    fn wal_telemetry_reports_size_and_passive_checkpoint_counts() {
        let mut state = SqliteState::default();
        let telemetry = wal_telemetry_from_state(&state);
        assert!(telemetry["canvas"].is_null());

        record_wal_checkpoint_in_state(&mut state, DatabaseKind::Canvas, 4096, 0, 12, 12);
        let telemetry = wal_telemetry_from_state(&state);
        assert_eq!(telemetry["canvas"]["walSizeBytes"], 4096);
        assert_eq!(telemetry["canvas"]["checkpointedFrames"], 12);
        assert!(telemetry["analytics"].is_null());
    }

    #[test]
    fn empty_contention_fallback_keeps_store_admission_typed() {
        let admission = empty_store_admission();
        for side in ["reads", "writes"] {
            assert_eq!(admission[side]["operations"], 0);
            assert_eq!(admission[side]["waits"], 0);
            assert_eq!(admission[side]["openedConnections"], 0);
            assert_eq!(admission[side]["waitMs"]["samples"], 0);
            assert!(admission[side]["waitMs"]["p50Ms"].is_null());
            assert!(admission[side]["waitMs"]["p95Ms"].is_null());
            assert!(admission[side]["waitMs"]["p99Ms"].is_null());
            assert_eq!(admission[side]["waitMs"]["maxMs"], 0.0);
        }
    }

    #[test]
    fn lock_error_projections_share_the_reported_count() {
        let mut state = SqliteState::default();
        record_sqlite_lock_error_snapshot_in_state(&mut state, 2);
        record_sqlite_lock_error_snapshot_in_state(&mut state, 1);

        let value = sqlite_contention_from_state(&state, 2);
        assert_eq!(value["lockError"]["lockErrors"], 2);
        assert_eq!(value["transaction"]["lockErrors"], 2);
        assert_eq!(
            value["transaction"]["sites"]["runtime_store.writer_transaction"]["lockErrors"],
            2
        );
        assert_eq!(value["writeWait"]["lockErrors"], 2);
    }

    #[test]
    fn store_admission_reports_only_contended_waits_and_bounded_peaks() {
        let mut state = SqliteState::default();
        record_store_admission_in_state(&mut state, true, false, Duration::ZERO, 0, 0, 1);
        record_store_waiter_occupancy_in_state(&mut state, true, 1, 3, 4);
        record_store_admission_in_state(&mut state, true, true, Duration::from_millis(2), 0, 3, 4);
        record_store_admission_in_state(&mut state, true, true, Duration::from_millis(4), 0, 3, 4);
        let metric = admission_metric(&state.read_admission);
        assert_eq!(metric["operations"], 3);
        assert_eq!(metric["waits"], 2);
        assert_eq!(metric["activeWaiters"], 0);
        assert_eq!(metric["peakWaiters"], 3);
        assert_eq!(metric["openedConnections"], 4);
        assert_eq!(metric["waitMs"]["p95Ms"], 2.0);
        assert_eq!(metric["waitMs"]["p99Ms"], 2.0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn slow_statements_do_not_masquerade_as_slow_transactions() {
        let mut state = SqliteState::default();
        for _ in 0..=HISTORY_LIMIT {
            record_sqlite_profile_in_state(
                &mut state,
                DatabaseKind::Canvas,
                Duration::from_secs(2),
            );
        }
        let history = sqlite_history_from_state(&state);
        let recent = history["recent"].as_array().expect("slow profile history");
        assert!(recent.is_empty());
        assert!(history["longest"].as_array().expect("longest").is_empty());
        assert_eq!(
            state.profile_by_database[&DatabaseKind::Canvas].0,
            HISTORY_LIMIT as u64 + 1
        );
    }

    // Native thread IDs, required by slow-history entries, are Linux-only.
    #[cfg(target_os = "linux")]
    #[test]
    fn actual_writer_transactions_fill_only_bounded_slow_history() {
        let mut state = SqliteState::default();
        for _ in 0..=HISTORY_LIMIT {
            record_store_transaction_in_state(
                &mut state,
                Duration::ZERO,
                Duration::from_secs(2),
                std::panic::Location::caller(),
            );
        }
        let history = sqlite_history_from_state(&state);
        let recent = history["recent"]
            .as_array()
            .expect("slow transaction history");
        let entry = recent.last().expect("actual transaction record");
        assert_eq!(entry["site"], "runtime_store.writer_transaction");
        assert_eq!(entry["database"], "canvas.sqlite3");
        assert_eq!(entry["firstStatement"], "BEGIN");
        assert_eq!(entry["state"], "ended");
        assert_eq!(entry["originRecorded"], true);
        assert!(
            entry["originFrames"][0]["file"]
                .as_str()
                .is_some_and(|file| file.ends_with("diagnostics/sqlite.rs"))
        );
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
