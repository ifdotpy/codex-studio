//! Host adapters for budget decisions and current session-cost reads.
//!
//! Native history is captured before the caller opens its write transaction.
//! Admission, defer, claim and usage capture then reuse that caller's transaction.

use crate::budget::{
    AdmissionDecision, AdmissionDenial, BudgetError, CaptureSource, HistoryFileObservation,
    HistoryObservation,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use studio_runtime_store::analytics_capture::AnalyticsCaptureRepository;
use studio_runtime_store::{
    AnalyticsRepository, RuntimeStore, StoreError, Transaction, analytics_capture::CaptureTable,
    budget::BudgetRepository,
};
use thiserror::Error;

const MAX_HISTORY_TAIL: u64 = 65_536;

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
}

/// Prepare current history evidence for every member of a team.
///
/// This method does filesystem and read-only analytics work only. Call it
/// before the transaction that invokes [`admission`]. Missing or unreadable
/// history is represented as incomplete evidence, matching the budget policy.
pub fn read_histories(
    store: &RuntimeStore,
    root_id: &str,
) -> Result<BTreeMap<String, HistoryObservation>, BudgetRuntimeError> {
    let members = store.read_transaction(|tx| {
        BudgetRepository::agent(tx, root_id)?;
        BudgetRepository::team(tx, root_id)
    })?;
    let repository = AnalyticsRepository::new(store.root());
    let db = repository.open_read_only()?;
    let has_history = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.sqlite_master WHERE type='table' AND name='analytics_history')",
        [],
        |row| row.get::<_, bool>(0),
    )?;

    let mut observations = BTreeMap::new();
    for agent in members {
        let id = agent
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| BudgetError::Invalid("runtime_agents/id".into()))?;
        let history = if has_history {
            let account = agent
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default");
            let thread = agent
                .get("threadId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let key = format!("{id}:{account}:{thread}");
            AnalyticsCaptureRepository::read(&db, CaptureTable::History, &key)?
        } else {
            None
        };
        let file = history.as_ref().and_then(read_history_file);
        let observation =
            HistoryObservation::from_records(&agent, history.as_ref(), file.as_ref())?;
        observations.insert(id.to_owned(), observation);
    }
    Ok(observations)
}

fn read_history_file(history: &Value) -> Option<HistoryFileObservation> {
    let path = Path::new(history.get("path")?.as_str()?);
    let offset = history
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let mut file = File::open(path).ok()?;
    let opened = file.metadata().ok()?;
    let identity = file_identity(&opened)?;
    let size = opened.len();
    let remaining = size.saturating_sub(offset);
    let tail = if offset <= size && remaining <= MAX_HISTORY_TAIL {
        let mut bytes = Vec::with_capacity(remaining as usize);
        file.seek(SeekFrom::Start(offset)).ok()?;
        file.take(MAX_HISTORY_TAIL + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        String::from_utf8(bytes).ok()?
    } else {
        String::new()
    };
    let final_metadata = fs::metadata(path).ok()?;
    let final_identity = file_identity(&final_metadata)?;
    if final_identity != identity {
        return None;
    }
    Some(HistoryFileObservation {
        identity,
        size,
        final_size: final_metadata.len(),
        tail,
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
    tx: &Transaction<'_>,
    agent: &Value,
    root: &Value,
    histories: &BTreeMap<String, HistoryObservation>,
    now: f64,
) -> Result<AdmissionDecision, BudgetError> {
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

/// Refresh session cost when the persisted source signature is stale.
///
/// The cost service returns a valid persisted snapshot when its source still
/// matches, otherwise it recomputes from the current usage rows. Missing local
/// pricing and Claude local usage remain named dependencies.
pub fn session_cost(
    store: &RuntimeStore,
    agent_id: &str,
    account_providers: &BTreeMap<String, String>,
) -> Result<Value, crate::costs::CostsError> {
    crate::costs::session_cost_with_accounts(store, agent_id, account_providers)
}

#[cfg(test)]
mod tests {
    use super::read_history_file;
    use serde_json::json;
    use std::fs;

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
        .expect("read bounded tail");
        assert_eq!(observation.tail, "{}");
        assert_eq!(observation.size, observation.final_size);
    }
}
