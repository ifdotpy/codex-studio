//! Host adapters for budget decisions and current session-cost reads.
//!
//! Native history is captured before the caller opens its write transaction.
//! Admission, defer, claim and usage capture then reuse that caller's transaction.
//! AdmissionHistoryPreReadWindow: Python's `Runtime.lock` (codex_budget.py:1)
//! encloses the stat/read/stat checks (codex_budget.py:190-211), excluding
//! concurrent Studio writers but not provider appends. Rust's window spans the
//! pre-read to commit, so a provider append after pre-read is not detected; the
//! next live or history capture accounts for it before a later admission.
//! ResponseReplayContentConflict is a deliberate compatibility tightening:
//! Python records `conflictingResponseUsage` and saves the changed replay
//! (codex_budget.py:101-108,147-151); Rust refuses it without writes.

use crate::budget::{
    AdmissionDecision, AdmissionDenial, BudgetError, CaptureSource, HistoryFileObservation,
    HistoryObservation, HistoryRuntimeBinding,
};
use rusqlite::OptionalExtension;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::UNIX_EPOCH;
use studio_runtime_store::analytics_capture::CaptureTable;
use studio_runtime_store::{
    AgentId, AnalyticsRepository, RuntimeStore, StoreError, Table, Transaction,
    budget::{BudgetRecord, BudgetRepository},
};
use thiserror::Error;

const MAX_HISTORY_TAIL: u64 = 65_536;
const MAX_HISTORY_READ_ATTEMPTS: usize = 3;

#[derive(Debug, Error)]
pub enum BudgetRuntimeError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Analytics(#[from] studio_runtime_store::AnalyticsStoreError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Prepare current history evidence for every member of a team.
///
/// This method does filesystem and read-only analytics work only. Its bounded
/// stat/read/stat proof is bound to the raw checkpoint, agent and budget rows;
/// [`admission`] compares those durable rows inside the owner transaction.
/// A provider append after this pre-read and before commit remains undetected,
/// as it also does while Python's Runtime.lock excludes only Studio writers.
pub fn read_histories(
    store: &RuntimeStore,
    root_id: &str,
) -> Result<BTreeMap<String, HistoryObservation>, BudgetRuntimeError> {
    let members = store.read_transaction(|tx| {
        BudgetRepository::agent(tx, root_id)?;
        let members = BudgetRepository::team(tx, root_id)?;
        let mut snapshots = Vec::with_capacity(members.len());
        for agent in members {
            let id = agent["id"]
                .as_str()
                .ok_or_else(|| StoreError::SchemaMismatch("runtime_agents/id".into()))?;
            let budget = Table::<BudgetRecord>::new(tx)
                .get(&AgentId::from(id))?
                .map(|record| record.0);
            snapshots.push((agent, budget));
        }
        Ok(snapshots)
    })?;
    let repository = AnalyticsRepository::new(store.root());
    let db = repository.open_read_only()?;
    let has_history = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.sqlite_master WHERE type='table' AND name='analytics_history')",
        [],
        |row| row.get::<_, bool>(0),
    )?;

    let mut observations = BTreeMap::new();
    for (agent, budget) in members {
        let id = agent
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| BudgetError::Invalid("runtime_agents/id".into()))?;
        let account = agent
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default");
        let thread = agent
            .get("threadId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let checkpoint_id = format!("{id}:{account}:{thread}");
        let checkpoint_raw = if has_history {
            let raw = db
                .query_row(
                    "SELECT record FROM analytics_history WHERE id=?1",
                    [&checkpoint_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if let Some(raw) = &raw {
                let value: Value = serde_json::from_str(raw)?;
                CaptureTable::History.validate(&value)?;
                if value["id"].as_str() != Some(&checkpoint_id) {
                    return Err(StoreError::SchemaMismatch("analytics_history/id".into()).into());
                }
            }
            raw
        } else {
            None
        };
        let history = checkpoint_raw
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()?;
        let file = match history.as_ref() {
            Some(history) => read_history_file(history)?,
            None => None,
        };
        let mut observation =
            HistoryObservation::from_records(&agent, history.as_ref(), file.as_ref())?;
        observation.bind_runtime(HistoryRuntimeBinding {
            agent: agent.clone(),
            budget,
            checkpoint_id,
            checkpoint_record: checkpoint_raw,
            file,
        });
        observations.insert(id.to_owned(), observation);
    }
    Ok(observations)
}

fn read_history_file(history: &Value) -> Result<Option<HistoryFileObservation>, BudgetError> {
    read_history_file_bounded_with(history, || {})
}

fn read_history_file_bounded_with(
    history: &Value,
    mut after_read: impl FnMut(),
) -> Result<Option<HistoryFileObservation>, BudgetError> {
    for _ in 0..MAX_HISTORY_READ_ATTEMPTS {
        let mut changed = false;
        let observation = read_history_file_with(history, &mut after_read, &mut changed);
        if !changed {
            return Ok(observation);
        }
    }
    Err(BudgetError::StaleHistory)
}

fn read_history_file_with(
    history: &Value,
    after_read: &mut impl FnMut(),
    changed: &mut bool,
) -> Option<HistoryFileObservation> {
    let path = Path::new(history.get("path")?.as_str()?);
    let offset = history
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let mut file = File::open(path).ok()?;
    let opened = file.metadata().ok()?;
    let named = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => {
            *changed = true;
            return None;
        }
    };
    let identity = file_identity(&opened)?;
    let named_identity = file_identity(&named)?;
    let size = opened.len();
    let modified = opened.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    let named_modified = named.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    if named_identity != identity || named.len() != size || named_modified != modified {
        *changed = true;
        return None;
    }
    let remaining = size.saturating_sub(offset);
    let bytes = if offset <= size && remaining <= MAX_HISTORY_TAIL {
        let mut bytes = Vec::with_capacity(remaining as usize);
        file.seek(SeekFrom::Start(offset)).ok()?;
        if (&mut file)
            .take(MAX_HISTORY_TAIL + 1)
            .read_to_end(&mut bytes)
            .is_err()
        {
            *changed = true;
            return None;
        }
        bytes
    } else {
        Vec::new()
    };
    after_read();
    let final_opened = match file.metadata() {
        Ok(metadata) => metadata,
        Err(_) => {
            *changed = true;
            return None;
        }
    };
    let final_named = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => {
            *changed = true;
            return None;
        }
    };
    let final_identity = file_identity(&final_opened)?;
    let final_named_identity = file_identity(&final_named)?;
    let final_modified = final_opened
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?;
    if final_identity != identity
        || final_named_identity != identity
        || final_opened.len() != size
        || final_named.len() != size
        || final_modified != modified
    {
        *changed = true;
        return None;
    }
    Some(HistoryFileObservation {
        identity,
        size,
        final_size: final_opened.len(),
        tail: String::from_utf8(bytes.clone()).ok()?,
        modified_seconds: modified.as_secs(),
        modified_nanos: modified.subsec_nanos(),
        content_hash: studio_runtime_store::sha256_hex(&bytes),
    })
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> Option<[u64; 2]> {
    use std::os::unix::fs::MetadataExt;
    Some([metadata.dev(), metadata.ino()])
}

#[cfg(not(unix))]
fn file_identity(_: &fs::Metadata) -> Option<[u64; 2]> {
    None
}

/// Check admission from the caller's transaction using history prepared before it.
pub fn admission(
    store: &RuntimeStore,
    tx: &Transaction<'_>,
    agent: &Value,
    root: &Value,
    histories: &BTreeMap<String, HistoryObservation>,
    now: f64,
) -> Result<AdmissionDecision, BudgetError> {
    admission_with_analytics_path(
        &store.root().join("analytics.sqlite3"),
        tx,
        agent,
        root,
        histories,
        now,
    )
}

fn admission_with_analytics_path(
    analytics_path: &Path,
    tx: &Transaction<'_>,
    agent: &Value,
    root: &Value,
    histories: &BTreeMap<String, HistoryObservation>,
    now: f64,
) -> Result<AdmissionDecision, BudgetError> {
    recheck_history_bindings(analytics_path, tx, histories)?;
    let agent_id = agent
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| BudgetError::Invalid("runtime_agents/id".into()))?;
    let root_id = root
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| BudgetError::Invalid("runtime_agents/id".into()))?;
    let decision = crate::budget_service::admission(tx, agent_id, histories, now)?;
    if decision.status.root_id != root_id {
        return Err(BudgetError::Invalid("runtime_agents/rootId".into()));
    }
    Ok(decision)
}

fn recheck_history_bindings(
    analytics_path: &Path,
    tx: &Transaction<'_>,
    histories: &BTreeMap<String, HistoryObservation>,
) -> Result<(), BudgetError> {
    if !histories
        .values()
        .any(|history| history.runtime_binding().is_some())
    {
        return Ok(());
    }
    let attached: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_database_list WHERE name='p09_analytics')",
            [],
            |row| row.get(0),
        )
        .map_err(StoreError::from)?;
    if !attached {
        let uri = read_only_uri(analytics_path)?;
        tx.execute("ATTACH DATABASE ?1 AS p09_analytics", [uri])
            .map_err(StoreError::from)?;
    }
    for history in histories.values() {
        let binding = history.runtime_binding().ok_or(BudgetError::StaleHistory)?;
        let id = binding.agent["id"]
            .as_str()
            .ok_or(BudgetError::StaleHistory)?;
        let current_agent_raw: Option<String> = tx
            .query_row(
                "SELECT record FROM runtime_agents WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)?;
        let current_agent = current_agent_raw
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()
            .map_err(StoreError::from)?;
        let current_budget_raw: Option<String> = tx
            .query_row(
                "SELECT record FROM runtime_budget WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)?;
        let current_budget = current_budget_raw
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()
            .map_err(StoreError::from)?;
        if current_agent.as_ref() != Some(&binding.agent) || current_budget != binding.budget {
            return Err(BudgetError::StaleHistory);
        }
        let table_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM p09_analytics.sqlite_master WHERE type='table' AND name='analytics_history')",
            [],
            |row| row.get(0),
        )
        .map_err(StoreError::from)?;
        let current_checkpoint: Option<String> =
            if table_exists && !binding.checkpoint_id.is_empty() {
                tx.query_row(
                    "SELECT record FROM p09_analytics.analytics_history WHERE id=?1",
                    [&binding.checkpoint_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(StoreError::from)?
            } else {
                None
            };
        if current_checkpoint != binding.checkpoint_record {
            return Err(BudgetError::StaleHistory);
        }
        let _file_proof = &binding.file;
    }
    Ok(())
}

fn read_only_uri(path: &Path) -> Result<String, BudgetError> {
    let raw = path
        .to_str()
        .ok_or_else(|| BudgetError::Invalid("analytics/path".into()))?;
    let mut uri = String::from("file:");
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || b"/:_-~.".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri.push_str("?mode=ro");
    Ok(uri)
}

/// Capture one live or historical observation inside its owner's transaction.
pub fn capture(
    tx: &Transaction<'_>,
    agent: &mut Value,
    params: &Value,
    at: f64,
    source: CaptureSource,
    now: f64,
) -> Result<u64, BudgetError> {
    crate::budget_service::capture(tx, agent, params, at, source, now)
}

/// Defer a proven-unsubmitted budget denial in the caller's transaction.
pub fn defer_wait(
    tx: &Transaction<'_>,
    agent_id: &str,
    attempt_id: &str,
    denial: &AdmissionDenial,
    unknown: bool,
    closed: bool,
    now: f64,
) -> Result<bool, BudgetError> {
    crate::budget_service::defer_wait(tx, agent_id, attempt_id, denial, unknown, closed, now)
}

/// Claim a previously deferred attempt after the scheduler's same-transaction gates.
pub fn claim_wait(
    tx: &Transaction<'_>,
    agent_id: &str,
    closed: bool,
    now: f64,
) -> Result<Option<Value>, BudgetError> {
    crate::budget_service::claim_wait(tx, agent_id, closed, now)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_HISTORY_READ_ATTEMPTS, admission_with_analytics_path, read_history_file,
        read_history_file_bounded_with,
    };
    use crate::budget::{HistoryObservation, HistoryRuntimeBinding};
    use rusqlite::Connection;
    use serde_json::{Value, json};
    use std::{cell::Cell, collections::BTreeMap, fs, io::Write};

    #[test]
    fn reads_bounded_history_tail_from_checkpoint_offset() {
        let directory = tempfile::tempdir().expect("temporary history directory");
        let path = directory.path().join("rollout.jsonl");
        fs::write(&path, b"{}\n{}").expect("write fixture history");
        let metadata = fs::metadata(&path).expect("history metadata");
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            json!([metadata.dev(), metadata.ino()])
        };
        #[cfg(not(unix))]
        let identity = json!([0, 0]);
        let observation = read_history_file(&json!({
            "path": path,
            "offset": 3,
            "identity": identity,
        }))
        .expect("stable history")
        .expect("read bounded tail");
        assert_eq!(observation.tail, "{}");
        assert_eq!(observation.size, observation.final_size);
        assert!(!observation.content_hash.is_empty());
    }

    #[test]
    fn refuses_a_file_that_changes_during_the_pre_read() {
        let directory = tempfile::tempdir().expect("temporary history directory");
        let path = directory.path().join("rollout.jsonl");
        fs::write(&path, b"{}\n").expect("write fixture history");
        let metadata = fs::metadata(&path).expect("history metadata");
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            json!([metadata.dev(), metadata.ino()])
        };
        #[cfg(not(unix))]
        let identity = json!([0, 0]);
        let attempts = Cell::new(0);
        let result = read_history_file_bounded_with(
            &json!({"path":path,"offset":0,"identity":identity}),
            || {
                attempts.set(attempts.get() + 1);
                fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .expect("open append")
                    .write_all(b"{}\n")
                    .expect("append during read")
            },
        );
        assert!(matches!(
            result,
            Err(crate::budget::BudgetError::StaleHistory)
        ));
        assert_eq!(attempts.get(), MAX_HISTORY_READ_ATTEMPTS);
    }

    fn admission_fixture() -> (
        Connection,
        Value,
        BTreeMap<String, HistoryObservation>,
        String,
    ) {
        let cases: Value = serde_json::from_str(include_str!("../tests/budget/cases.json"))
            .expect("budget fixture");
        let agent = cases[0]["agents"][0].clone();
        let checkpoint_id = "a:account:thread".to_owned();
        let raw = json!({"id":checkpoint_id,"agent":"a","status":"current"}).to_string();
        let mut history = HistoryObservation::from_records(&agent, None, None).expect("history");
        history.bind_runtime(HistoryRuntimeBinding {
            agent: agent.clone(),
            budget: None,
            checkpoint_id: checkpoint_id.clone(),
            checkpoint_record: Some(raw.clone()),
            file: None,
        });
        let mut histories = BTreeMap::new();
        histories.insert("a".into(), history);
        let db = Connection::open_in_memory().expect("canvas db");
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);CREATE TABLE runtime_budget(id TEXT PRIMARY KEY,record TEXT NOT NULL);CREATE TABLE runtime_budget_usage(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,tokens INTEGER,at REAL);")
            .expect("canvas schema");
        db.execute(
            "INSERT INTO runtime_agents VALUES ('a',?1)",
            [agent.to_string()],
        )
        .expect("agent row");
        (db, agent, histories, raw)
    }

    fn analytics_fixture(directory: &std::path::Path, raw: &str) -> std::path::PathBuf {
        let path = directory.join("analytics.sqlite3");
        let db = Connection::open(&path).expect("analytics db");
        db.execute_batch(
            "CREATE TABLE analytics_history(id TEXT PRIMARY KEY,agent TEXT,record TEXT NOT NULL);",
        )
        .expect("analytics schema");
        db.execute(
            "INSERT INTO analytics_history VALUES ('a:account:thread','a',?1)",
            [raw],
        )
        .expect("checkpoint row");
        path
    }

    #[test]
    fn admission_rechecks_checkpoint_and_accepts_the_identical_snapshot() {
        let directory = tempfile::tempdir().expect("state directory");
        let (mut db, agent, histories, raw) = admission_fixture();
        let path = analytics_fixture(directory.path(), &raw);
        let tx = db.transaction().expect("transaction");
        let decision = admission_with_analytics_path(&path, &tx, &agent, &agent, &histories, 100.)
            .expect("unchanged source remains admissible");
        assert_eq!(decision.status.root_id, "a");
        tx.commit().expect("commit");
    }

    #[test]
    fn admission_refuses_when_checkpoint_moves_after_the_pre_read() {
        let directory = tempfile::tempdir().expect("state directory");
        let (mut db, agent, histories, raw) = admission_fixture();
        let path = analytics_fixture(directory.path(), &raw);
        let writer = Connection::open(&path).expect("checkpoint writer");
        writer
            .execute(
                "UPDATE analytics_history SET record=?1 WHERE id='a:account:thread'",
                [
                    json!({"id":"a:account:thread","agent":"a","status":"current","offset":4})
                        .to_string(),
                ],
            )
            .expect("move checkpoint");
        let before = state_snapshot(&db, &path);
        let tx = db.transaction().expect("transaction");
        let result = admission_with_analytics_path(&path, &tx, &agent, &agent, &histories, 100.);
        assert!(matches!(
            result,
            Err(crate::budget::BudgetError::StaleHistory)
        ));
        tx.commit().expect("commit refused transaction");
        assert_eq!(
            state_snapshot(&db, &path),
            before,
            "stale refusal writes nothing"
        );
    }

    fn state_snapshot(db: &Connection, analytics_path: &std::path::Path) -> Value {
        let mut agents = db
            .prepare("SELECT id,record FROM runtime_agents ORDER BY id")
            .expect("agent snapshot query");
        let agents = agents
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("agent snapshot rows")
            .map(|row| {
                let (id, raw) = row.expect("agent snapshot row");
                (
                    id,
                    serde_json::from_str::<Value>(&raw).expect("agent record"),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut budgets = db
            .prepare("SELECT id,record FROM runtime_budget ORDER BY id")
            .expect("budget snapshot query");
        let budgets = budgets
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("budget snapshot rows")
            .map(|row| {
                let (id, raw) = row.expect("budget snapshot row");
                (
                    id,
                    serde_json::from_str::<Value>(&raw).expect("budget record"),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut usage = db
            .prepare("SELECT id,agent,kind,tokens,at FROM runtime_budget_usage ORDER BY id")
            .expect("usage snapshot query");
        let usage = usage
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, f64>(4)?,
                ))
            })
            .expect("usage snapshot rows")
            .map(|row| row.expect("usage snapshot row"))
            .collect::<Vec<_>>();
        let analytics = Connection::open(analytics_path).expect("analytics snapshot db");
        let checkpoint: String = analytics
            .query_row(
                "SELECT record FROM analytics_history WHERE id='a:account:thread'",
                [],
                |row| row.get(0),
            )
            .expect("checkpoint snapshot");
        json!({"agents":agents,"budgets":budgets,"usage":usage,"checkpoint":checkpoint})
    }
}
