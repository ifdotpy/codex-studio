//! Synchronous transaction adapter for notification capture and scheduler gates.
//! Call inside RuntimeStore::transaction_with. No provider/file work is performed.
use crate::budget::{
    self, AdmissionDecision, AdmissionDenial, BudgetError, BudgetSnapshot, CaptureSource,
    HistoryObservation, UsageObservation,
};
use crate::budget_wait::{self, WaitDecision, WaitSnapshot};
use serde_json::Value;
use std::collections::BTreeMap;
use studio_runtime_store::{
    AgentId, Table, Transaction,
    budget::{BudgetRecord, BudgetRepository},
};

fn atomic<T>(
    tx: &Transaction<'_>,
    run: impl FnOnce() -> Result<T, BudgetError>,
) -> Result<T, BudgetError> {
    tx.execute_batch("SAVEPOINT budget_scope")
        .map_err(studio_runtime_store::StoreError::from)?;
    let result = run();
    match result {
        Ok(value) => {
            tx.execute_batch("RELEASE budget_scope")
                .map_err(studio_runtime_store::StoreError::from)?;
            Ok(value)
        }
        Err(error) => {
            tx.execute_batch("ROLLBACK TO budget_scope; RELEASE budget_scope")
                .map_err(studio_runtime_store::StoreError::from)?;
            Err(error)
        }
    }
}
fn snapshot(tx: &Transaction<'_>, agent: &Value, now: f64) -> Result<BudgetSnapshot, BudgetError> {
    let id = agent
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| BudgetError::Invalid("runtime_agents/id".into()))?;
    let durable = BudgetRepository::agent(tx, id)?;
    let state = Table::<BudgetRecord>::new(tx).get(&AgentId::from(id))?;
    BudgetSnapshot::from_records(agent, &durable, state.as_ref().map(|s| &s.0), now)
}
fn save_state(
    tx: &Transaction<'_>,
    id: &str,
    state: &budget::BudgetState,
) -> Result<(), BudgetError> {
    Table::<BudgetRecord>::new(tx).put_at(&id.into(), &BudgetRecord(state.record()?))?;
    BudgetRepository::patch_budget(tx, id, &state.patch())?;
    Ok(())
}
/// Live notification and historical capture share the same durable response key.
/// `now` is collection time (initial cutoff); `at` is the usage record timestamp.
/// The transaction's durable agent owns identity. The supplied copy needs only
/// `id`; any supplied root/account/thread/turn/epoch must still match that row.
/// Observation fields in `params` belong to the usage record, including historical
/// response/turn IDs. They never replace the supplied copy's ownership assertions.
/// Only budget fields in the supplied copy are changed, after all writes succeed.
pub fn capture(
    tx: &Transaction<'_>,
    agent: &mut Value,
    params: &Value,
    at: f64,
    source: CaptureSource,
    now: f64,
) -> Result<u64, BudgetError> {
    let (state, changed) = atomic(tx, || {
        let snapshot = snapshot(tx, agent, now)?;
        let observation = UsageObservation::from_record(&snapshot, params, at, source)?;
        let previous = BudgetRepository::usage(tx, observation.key())?;
        let decision = budget::capture(&snapshot, &observation, previous.as_ref())?;
        if let Some(row) = &decision.usage {
            BudgetRepository::insert_usage(tx, row)?;
        }
        if decision.changed {
            save_state(tx, snapshot.agent_id(), &decision.state)?;
        }
        Ok((decision.state, decision.changed))
    })?;
    if !changed {
        return state.patch()["tokensUsed"]
            .as_u64()
            .ok_or_else(|| BudgetError::Invalid("runtime_budget/spent".into()));
    }
    let patch = state.patch();
    agent["tokensUsed"] = patch["tokensUsed"].clone();
    agent["tokenUsageAccounting"] = patch["tokenUsageAccounting"].clone();
    patch["tokensUsed"]
        .as_u64()
        .ok_or_else(|| BudgetError::Invalid("runtime_budget/spent".into()))
}

/// Re-read all same-root members including deleted workers in the owning submit
/// transaction. A denial does not consume events or submit provider work.
pub fn admission(
    tx: &Transaction<'_>,
    agent_id: &str,
    histories: &BTreeMap<String, HistoryObservation>,
    now: f64,
) -> Result<AdmissionDecision, BudgetError> {
    atomic(tx, || {
        let agent = BudgetRepository::agent(tx, agent_id)?;
        let root_id = agent
            .get("rootId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(agent_id);
        let root = BudgetRepository::agent(tx, root_id)?;
        // Unlimited teams do not initialize ledger rows.
        if root.get("tokenBudget").is_none_or(Value::is_null) {
            return budget::admission(&agent, &root, &[], histories);
        }
        let members = team_snapshots(tx, root_id, now)?;
        budget::admission(&agent, &root, &members, histories)
    })
}
fn wait_snapshot(
    tx: &Transaction<'_>,
    agent_id: &str,
    closed: bool,
) -> Result<WaitSnapshot, BudgetError> {
    let agent = BudgetRepository::agent(tx, agent_id)?;
    // Bound before querying. Malformed reservations are rejected by the core;
    // loading an unbounded or duplicate event batch is unnecessary.
    let ids: Vec<String> = agent
        .get("startAttempt")
        .and_then(|v| v.get("events"))
        .and_then(Value::as_array)
        .filter(|ids| ids.len() <= 32)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let ids: Vec<_> = ids
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let rows = BudgetRepository::events(tx, &ids)?;
    WaitSnapshot::from_records(&agent, &rows, closed)
}
fn apply_wait(tx: &Transaction<'_>, decision: &WaitDecision) -> Result<(), BudgetError> {
    let Some(agent) = &decision.agent else {
        return Ok(());
    };
    for event in &decision.events {
        BudgetRepository::set_event(tx, &event.id, &event.status, event.status == "pending")?;
    }
    if let Some(action) = &decision.retired_action {
        BudgetRepository::retire_action(tx, &action.request_id, &action.attempt_id)?;
    }
    BudgetRepository::save_agent(tx, agent)?;
    let id = agent
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| BudgetError::Invalid("runtime_agents/id".into()))?;
    let fields: [(&str, Value); 5] = ["status", "inFlight", "turnEpoch", "error", "startAttempt"]
        .map(|key| (key, agent.get(key).cloned().unwrap_or(Value::Null)));
    BudgetRepository::sync_fields(tx, id, &fields)?;
    Ok(())
}
/// A denial is pinned to its agent/account/thread/epoch/attempt. Unknown native
/// writes never create retryable waits. Replayed denials retain the first time.
pub fn defer_wait(
    tx: &Transaction<'_>,
    agent_id: &str,
    attempt_id: &str,
    denial: &AdmissionDenial,
    unknown: bool,
    closed: bool,
    now: f64,
) -> Result<bool, BudgetError> {
    atomic(tx, || {
        let decision = budget_wait::defer(
            &wait_snapshot(tx, agent_id, closed)?,
            attempt_id,
            denial,
            unknown,
            now,
        )?;
        apply_wait(tx, &decision)?;
        Ok(decision.agent.is_some())
    })
}
/// Scheduler calls this only after capacity and budget admission in the same
/// transaction. The job retains the exact attempt; final submit checks still run.
pub fn claim_wait(
    tx: &Transaction<'_>,
    agent_id: &str,
    closed: bool,
    now: f64,
) -> Result<Option<Value>, BudgetError> {
    atomic(tx, || {
        let decision = budget_wait::claim(&wait_snapshot(tx, agent_id, closed)?, now)?;
        apply_wait(tx, &decision)?;
        Ok(decision.job)
    })
}

fn team_snapshots(
    tx: &Transaction<'_>,
    root_id: &str,
    now: f64,
) -> Result<Vec<BudgetSnapshot>, BudgetError> {
    let mut members = Vec::new();
    for member in BudgetRepository::team(tx, root_id)? {
        let snapshot = snapshot(tx, &member, now)?;
        if snapshot.initialized() {
            save_state(tx, snapshot.agent_id(), snapshot.state())?;
        }
        members.push(snapshot);
    }
    Ok(members)
}
/// Read/display totals, initializing absent ledgers from durable lifetime floors.
pub fn status(
    tx: &Transaction<'_>,
    agent_id: &str,
    histories: &BTreeMap<String, HistoryObservation>,
    check_coverage: bool,
    now: f64,
) -> Result<budget::BudgetStatus, BudgetError> {
    atomic(tx, || {
        let agent = BudgetRepository::agent(tx, agent_id)?;
        let root_id = agent
            .get("rootId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(agent_id);
        let root = BudgetRepository::agent(tx, root_id)?;
        budget::team_status(
            &root,
            &team_snapshots(tx, root_id, now)?,
            histories,
            check_coverage,
        )
    })
}
