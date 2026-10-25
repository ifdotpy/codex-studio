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
enum SnapshotIdentity {
    Live,
    History,
}

fn snapshot(
    tx: &Transaction<'_>,
    agent: &Value,
    identity: SnapshotIdentity,
    now: f64,
) -> Result<BudgetSnapshot, BudgetError> {
    let id = agent
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| BudgetError::Invalid("runtime_agents/id".into()))?;
    let durable = BudgetRepository::agent(tx, id)?;
    let state = Table::<BudgetRecord>::new(tx).get(&AgentId::from(id))?;
    match identity {
        SnapshotIdentity::Live => {
            BudgetSnapshot::from_records(agent, &durable, state.as_ref().map(|s| &s.0), now)
        }
        SnapshotIdentity::History => {
            BudgetSnapshot::from_history_records(agent, &durable, state.as_ref().map(|s| &s.0), now)
        }
    }
}
/// Store the budget state; with `patch_agent`, also the agent row's budget
/// fields and their sync view.
fn save_state(
    tx: &Transaction<'_>,
    id: &str,
    state: &budget::BudgetState,
    patch_agent: bool,
) -> Result<(), BudgetError> {
    Table::<BudgetRecord>::new(tx).put_at(&id.into(), &BudgetRecord(state.record()?))?;
    if patch_agent {
        BudgetRepository::patch_budget(tx, id, &state.patch())?;
    }
    Ok(())
}
/// Live notification and historical capture share the same durable response key.
/// `now` is collection time (initial cutoff); `at` is the usage record timestamp.
/// The transaction's durable agent owns identity. Live capture requires every
/// supplied identity field to match it. History capture allows the supplied
/// rollout-context turn to differ; params carries the historical event turn.
/// All other supplied identity fields must still match the durable row.
/// Only budget fields in the supplied copy are changed, after all writes succeed.
pub fn capture(
    tx: &Transaction<'_>,
    agent: &mut Value,
    params: &Value,
    at: f64,
    source: CaptureSource,
    now: f64,
) -> Result<u64, BudgetError> {
    capture_into(
        tx,
        agent,
        params,
        at,
        source,
        now,
        CaptureMode::Notification,
    )
    .map(|(tokens, _)| tokens)
}

/// [`capture`] for a caller that persists the supplied agent copy in the same
/// transaction (the notification dispatcher): the agent row is written once,
/// by that caller, instead of here and again there.
pub fn capture_for_persisting_caller(
    tx: &Transaction<'_>,
    agent: &mut Value,
    params: &Value,
    at: f64,
    source: CaptureSource,
    now: f64,
) -> Result<budget::UsageAcceptance, BudgetError> {
    capture_into(
        tx,
        agent,
        params,
        at,
        source,
        now,
        CaptureMode::PersistingCaller,
    )
    .map(|(_, acceptance)| acceptance)
}

/// Replay a persisted typed analytics row using its first-capture receipt.
pub fn capture_persisted(
    tx: &Transaction<'_>,
    agent: &mut Value,
    row: &Value,
    at: f64,
    now: f64,
) -> Result<u64, BudgetError> {
    capture_into(
        tx,
        agent,
        row,
        at,
        CaptureSource::History,
        now,
        CaptureMode::Persisted,
    )
    .map(|(tokens, _)| tokens)
}

pub(crate) fn capture_acceptance(
    tx: &Transaction<'_>,
    agent: &mut Value,
    params: &Value,
    at: f64,
    source: CaptureSource,
    now: f64,
) -> Result<budget::UsageAcceptance, BudgetError> {
    capture_into(
        tx,
        agent,
        params,
        at,
        source,
        now,
        CaptureMode::Notification,
    )
    .map(|(_, acceptance)| acceptance)
}
pub(crate) fn capture_persisted_acceptance(
    tx: &Transaction<'_>,
    agent: &mut Value,
    row: &Value,
    at: f64,
    now: f64,
) -> Result<budget::UsageAcceptance, BudgetError> {
    capture_into(
        tx,
        agent,
        row,
        at,
        CaptureSource::History,
        now,
        CaptureMode::Persisted,
    )
    .map(|(_, acceptance)| acceptance)
}

enum CaptureMode {
    Notification,
    PersistingCaller,
    Persisted,
}

fn capture_into(
    tx: &Transaction<'_>,
    agent: &mut Value,
    params: &Value,
    at: f64,
    source: CaptureSource,
    now: f64,
    mode: CaptureMode,
) -> Result<(u64, budget::UsageAcceptance), BudgetError> {
    let (state, changed, acceptance) = atomic(tx, || {
        let identity = match source {
            CaptureSource::Live => SnapshotIdentity::Live,
            CaptureSource::History => SnapshotIdentity::History,
        };
        let snapshot = snapshot(tx, agent, identity, now)?;
        let mut observation = if matches!(mode, CaptureMode::Persisted) {
            UsageObservation::from_persisted(&snapshot, params, at, source)?
        } else {
            UsageObservation::from_record(&snapshot, params, at, source)?
        };
        let mut receipts = Vec::new();
        for key in observation.receipt_candidates() {
            if let Some(row) = BudgetRepository::usage(tx, key)?
                && observation.accepts_receipt(&row)
                && !receipts
                    .iter()
                    .any(|previous: &budget::UsageRow| previous.id == row.id)
            {
                receipts.push(row);
            }
        }
        let previous = if receipts.len() == 1 {
            let row = receipts.remove(0);
            observation.reuse_receipt(row.id.clone());
            Some(row)
        } else {
            if receipts.len() > 1 {
                observation.mark_unknown_receipt();
            }
            None
        };
        let decision = budget::capture(&snapshot, &observation, previous.as_ref())?;
        if let Some(row) = &decision.usage {
            BudgetRepository::insert_usage(tx, row)?;
        }
        if decision.changed {
            save_state(
                tx,
                snapshot.agent_id(),
                &decision.state,
                !matches!(mode, CaptureMode::PersistingCaller),
            )?;
        }
        Ok((decision.state, decision.changed, decision.acceptance))
    })?;
    if !changed {
        return state.patch()["tokensUsed"]
            .as_u64()
            .ok_or_else(|| BudgetError::Invalid("runtime_budget/spent".into()))
            .map(|tokens| (tokens, acceptance));
    }
    let patch = state.patch();
    agent["tokensUsed"] = patch["tokensUsed"].clone();
    agent["tokenUsageAccounting"] = patch["tokenUsageAccounting"].clone();
    patch["tokensUsed"]
        .as_u64()
        .ok_or_else(|| BudgetError::Invalid("runtime_budget/spent".into()))
        .map(|tokens| (tokens, acceptance))
}

/// Establish old gap identities only from a complete bounded analytics window.
/// A truncated or empty gap inventory leaves coverage explicitly unknown.
async fn prepare_legacy_gap_coverage(
    store: &studio_runtime_store::RuntimeStore,
    agents: Vec<String>,
) -> Result<(), BudgetError> {
    let states = store
        .read_async("budget/legacy-gap-states", move |tx| {
            let mut states = Vec::new();
            for id in agents {
                if let Some(record) =
                    Table::<BudgetRecord>::new(tx).get(&AgentId::from(id.as_str()))?
                    && budget::needs_legacy_gap_coverage(&record.0)?
                {
                    states.push((id, record.0));
                }
            }
            Ok::<_, BudgetError>(states)
        })
        .await?;
    if states.is_empty() {
        return Ok(());
    }
    let root = store.root().to_owned();
    let proofs = studio_runtime_process::blocking_pool::spawn_blocking(
        "budget/legacy-gap-identities",
        move || {
            let db = studio_runtime_store::AnalyticsRepository::new(root).open_read_only()?;
            let mut proofs = Vec::new();
            for (agent, state) in states {
                let window = studio_runtime_store::cost_aggregate::reconciliation_usage_window(
                    &db,
                    &agent,
                    studio_runtime_schema::usage::ledger::LEGACY_GAP_COVERAGE_ROWS,
                )?;
                if window.truncated {
                    continue;
                }
                let gaps = window
                    .observations
                    .iter()
                    .filter_map(|observation| match observation {
                        studio_runtime_schema::usage::UsageObservation::Gap {
                            response_id, ..
                        } => Some(response_id.clone()),
                        _ => None,
                    })
                    .collect::<std::collections::BTreeSet<_>>();
                if gaps.is_empty() {
                    continue;
                }
                let finals = window
                    .responses
                    .iter()
                    .filter(|row| {
                        row.evidence != studio_runtime_schema::usage::UsageEvidence::Legacy
                    })
                    .map(|row| row.response_id.as_str())
                    .collect::<std::collections::BTreeSet<_>>();
                let pending = gaps
                    .iter()
                    .filter(|id| !finals.contains(id.as_str()))
                    .map(|id| {
                        studio_runtime_schema::usage::ledger::response_receipt_key(&agent, id)
                    })
                    .collect();
                proofs.push((agent, state, pending));
            }
            Ok::<_, BudgetError>(proofs)
        },
    )
    .await
    .map_err(|_| studio_runtime_store::StoreError::ReadTaskFailed)??;
    if proofs.is_empty() {
        return Ok(());
    }
    store
        .transaction_with(move |tx| {
            for (id, expected, pending) in proofs {
                let actual = Table::<BudgetRecord>::new(tx).get(&AgentId::from(id.as_str()))?;
                if actual.as_ref().map(|record| &record.0) == Some(&expected) {
                    let state = budget::establish_gap_coverage(&expected, pending)?;
                    save_state(tx, &id, &state, true)?;
                }
            }
            Ok::<_, BudgetError>(())
        })
        .await?;
    Ok(())
}

/// Enrich only old scalar receipts before admitting the capture writer. All analytics
/// IO occurs outside that writer; the metadata transaction compares the exact old row.
/// It changes no amount, placement totals, agent state, or notification receipt.
pub async fn prepare_legacy_corrections(
    store: &studio_runtime_store::RuntimeStore,
    captures: Vec<(Value, Value)>,
) -> Result<(), BudgetError> {
    let agents = captures
        .iter()
        .filter_map(|(agent, _)| agent["id"].as_str().map(str::to_owned))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    prepare_legacy_gap_coverage(store, agents).await?;
    use studio_runtime_schema::usage::ledger::UsageLedgerRow;
    use studio_runtime_schema::usage::{
        UsageEvidence, UsageObservation as ParsedUsage, UsageProvider, parse_token_usage,
    };
    let candidates = store
        .read_async("budget/legacy-receipts", move |tx| {
            let mut candidates = Vec::new();
            for (agent, params) in captures {
                let provider = if agent["provider"] == "claude" {
                    UsageProvider::Claude
                } else {
                    UsageProvider::Codex
                };
                let Ok(observation) = parse_token_usage(provider, &params) else {
                    continue;
                };
                let restore = params.get("agentId").is_some();
                let ParsedUsage::Response {
                    response_id,
                    evidence,
                    ..
                } = &observation
                else {
                    continue;
                };
                if *evidence == UsageEvidence::Legacy && !restore {
                    continue;
                }
                let Some(id) = agent["id"].as_str() else {
                    continue;
                };
                let incoming = UsageLedgerRow::capture(
                    id,
                    agent["accountKey"].as_str().unwrap_or("default"),
                    params["threadId"]
                        .as_str()
                        .or_else(|| agent["threadId"].as_str()),
                    None,
                    &params,
                    observation.clone(),
                );
                let Some(old) = BudgetRepository::usage(
                    tx,
                    incoming.receipt_key.as_deref().ok_or_else(|| {
                        studio_runtime_store::StoreError::SchemaMismatch(
                            "response receipt is missing".into(),
                        )
                    })?,
                )?
                else {
                    continue;
                };
                if old.agent != id || old.details.is_some() {
                    continue;
                }
                let state = Table::<BudgetRecord>::new(tx).get(&AgentId::from(id))?;
                let cutoff = state
                    .as_ref()
                    .and_then(|row| row.0["cutoff"].as_f64())
                    .unwrap_or(old.at);
                candidates.push((
                    old,
                    params["threadId"]
                        .as_str()
                        .or_else(|| agent["threadId"].as_str())
                        .map(str::to_owned),
                    response_id.clone(),
                    cutoff,
                ));
            }
            Ok::<_, studio_runtime_store::StoreError>(candidates)
        })
        .await?;
    if candidates.is_empty() {
        return Ok(());
    }
    let root = store.root().to_owned();
    let proven = studio_runtime_process::blocking_pool::spawn_blocking(
        "budget/legacy-provenance",
        move || {
            let db = studio_runtime_store::AnalyticsRepository::new(&root).open_read_only()?;
            let mut proven = Vec::new();
            for (old, thread, response, cutoff) in candidates {
                let provenance=studio_runtime_store::analytics_capture::AnalyticsCaptureRepository::usage_provenance(&db,&old.agent,thread.as_deref(),&response)?;
                let Some((mut ledger,estimated)) = provenance else { continue };
                let consistent=match &ledger.studio_usage {
                    ParsedUsage::Response { tokens,.. } => old.kind=="response" && tokens.total()==Some(old.tokens) && old.breakdown.as_ref().is_none_or(|previous|previous==tokens),
                    ParsedUsage::Gap {..} => old.kind=="notice" && old.tokens==0 && old.breakdown.is_none(),
                    _=>false,
                };
                if !consistent || ledger.usage_identity != studio_runtime_schema::usage::ledger::UsageIdentity::new(&old.agent, thread.as_deref(), &ledger.studio_usage) { continue; }
                ledger.receipt_key = Some(old.id.clone());
                let before_cutoff = estimated || old.at <= cutoff;
                proven.push((old, studio_runtime_store::budget::BudgetUsageDetails { ledger, before_cutoff }));
            }
            Ok::<_, BudgetError>(proven)
        },
    ).await.map_err(|_| studio_runtime_store::StoreError::ReadTaskFailed)??;
    if proven.is_empty() {
        return Ok(());
    }
    store
        .transaction(move |tx| {
            for (old, details) in proven {
                if BudgetRepository::usage(tx, &old.id)?.as_ref() == Some(&old) {
                    let mut enriched = old;
                    enriched.details = Some(details);
                    BudgetRepository::insert_usage(tx, &enriched)?;
                }
            }
            Ok(())
        })
        .await?;
    Ok(())
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
        let members = team_snapshots(tx, root_id, true, now)?;
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
    check_coverage: bool,
    now: f64,
) -> Result<Vec<BudgetSnapshot>, BudgetError> {
    let mut members = Vec::new();
    for member in BudgetRepository::team(tx, root_id)? {
        let mut snapshot = snapshot(tx, &member, SnapshotIdentity::Live, now)?;
        if check_coverage && snapshot.needs_live_usage() {
            snapshot.bind_live_usage(BudgetRepository::response_tokens(tx, snapshot.agent_id())?);
        }
        if snapshot.initialized() {
            save_state(tx, snapshot.agent_id(), snapshot.state(), true)?;
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
            &team_snapshots(
                tx,
                root_id,
                check_coverage
                    && root
                        .get("tokenBudget")
                        .is_some_and(|value| !value.is_null()),
                now,
            )?,
            histories,
            check_coverage,
        )
    })
}
