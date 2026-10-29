//! Synchronous monitor consumer: ownership checks and all row writes share one transaction.
//! Native I/O is returned as an effect, and must run only after a successful commit.
//! Startup reconciliation preserves active commands until a retained result or a failed
//! reattach decides their terminal outcome.

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, EventWriteRepository, MonitorsRepository, QueueEvent,
    QueueEventId, RequestRecord, RuntimeStore, SchedulerRepository, StartupRecoveryRepository,
    SyncDocumentsRepository, Table, Transaction, monitor_runtime as repository,
};

use crate::event_write::{self, python_json};
use crate::monitor_policy::{
    self as policy, Decision, Effect, Input, InputOutcome, MonitorError, MonitorSnapshot, Operation,
};

pub const SESSION_DEPENDENCY: &str = "provider-session/monitor-commands";
pub const RULE_DEPENDENCY: &str = "rule-driver/monitor-finish";
pub const REMOTE_EVENTS_DEPENDENCY: &str = "cross-server/remote-events";
const STARTUP_RECEIPT_BATCH: usize = 64;
/// Maximum monitor rows evaluated per keyset page.
pub(crate) const MONITOR_TICK_BATCH: usize = 64;
const RESTART_EVENT_ERROR: &str = "Server restarted before delivery acknowledgement";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StartupRecovery {
    pub receipts: usize,
    pub requests: usize,
    pub events: usize,
    pub monitors: usize,
}

/// Recover durable results first, then expire requests and stage active monitors
/// for retained-process reconciliation. Only a decided failed reattach marks a
/// command lost. Row and projection writes share a transaction; result files are
/// handled before that transaction.
pub async fn recover_on_startup(
    store: &RuntimeStore,
    now: f64,
) -> Result<StartupRecovery, MonitorError> {
    if !now.is_finite() || now < 0.0 {
        return Err(MonitorError::Refused("Invalid startup recovery clock"));
    }
    store
        .transaction_with(move |tx| crate::task_gate::recover(tx, now))
        .await?;
    let mut recovery = StartupRecovery::default();
    loop {
        let count = crate::monitor_receipts::recover_pending(store, STARTUP_RECEIPT_BATCH).await?;
        recovery.receipts += count;
        if count < STARTUP_RECEIPT_BATCH {
            break;
        }
    }
    let (requests, events, monitors) = store
        .transaction_with::<_, MonitorError>(move |tx| {
            // Use the scheduler's canonical exact-batch validator. The general
            // event pass below owns only the claims left after that operation.
            if let Err(error) = SchedulerRepository::recover_unsubmitted(tx) {
                if let studio_runtime_store::StoreError::InvalidSyncObject { scope, id } = &error
                    && scope == "runtime_agents"
                {
                    eprintln!("startup scheduler reservation skipped row id={id}: invalid batch");
                } else {
                    return Err(MonitorError::Store(error));
                }
            }
            let requests = recover_requests(tx)?;
            let events = recover_uncovered_events(tx)?;
            let monitors = recover_active_monitors(tx, now)?;
            Ok((requests, events, monitors))
        })
        .await?;
    recovery.requests = requests;
    recovery.events = events;
    recovery.monitors = monitors;
    Ok(recovery)
}

fn recover_requests(tx: &Transaction<'_>) -> Result<usize, MonitorError> {
    let request_rows = StartupRecoveryRepository::unsettled_requests(tx)?;
    let mut count = 0;
    for (id, raw) in request_rows {
        let mut value = match serde_json::from_str::<Value>(&raw) {
            Ok(value) if value.is_object() => value,
            _ => {
                eprintln!("startup request recovery skipped row id={id}: invalid record");
                continue;
            }
        };
        match value.get("status").and_then(Value::as_str) {
            Some("answering") => {
                value["status"] = json!("uncertain");
                value["answerError"] = json!("Server restarted before answer delivery completed");
            }
            Some("pending")
                if !retained_question(tx, &value)? && !retained_monitor_approval(tx, &value)? =>
            {
                value["status"] = json!("expired");
                // A sign-in request lives in its bridge across the restart; the card poll gives
                // the card back while that request is open (`browser_cards::apply`).
                if value.get("method").and_then(Value::as_str) == Some(crate::browser_cards::METHOD)
                {
                    value["closeReason"] = json!(crate::browser_cards::RESTART_CLOSE_REASON);
                }
            }
            _ => continue,
        }
        let request = match RequestRecord::from_value(value) {
            Ok(request) => request,
            Err(error) => {
                eprintln!("startup request recovery skipped row id={id}: {error}");
                continue;
            }
        };
        tx.execute_batch("SAVEPOINT startup_request_recovery_row")
            .map_err(studio_runtime_store::StoreError::from)?;
        let apply = (|| {
            Table::<RequestRecord>::new(tx).put(&request)?;
            let view = crate::sync_views::project_persisted_record("request", request.value())
                .ok_or(MonitorError::Refused("Invalid request projection"))?;
            SyncDocumentsRepository::put_entity(
                tx,
                "request",
                request.id(),
                view,
                request.value()["status"] != "pending",
            )?;
            Ok::<(), MonitorError>(())
        })();
        if let Err(error) = apply {
            tx.execute_batch(
                "ROLLBACK TO startup_request_recovery_row; RELEASE startup_request_recovery_row",
            )
            .map_err(studio_runtime_store::StoreError::from)?;
            if startup_store_failure(&error) {
                return Err(error);
            }
            eprintln!("startup request recovery skipped row id={id}: {error}");
            continue;
        }
        tx.execute_batch("RELEASE startup_request_recovery_row")
            .map_err(studio_runtime_store::StoreError::from)?;
        count += 1;
    }
    Ok(count)
}

/// Monitor approvals are local durable intents, independent of a native turn.
fn retained_monitor_approval(tx: &Transaction<'_>, request: &Value) -> Result<bool, MonitorError> {
    #[derive(Deserialize)]
    enum Method {
        #[serde(rename = "monitor/approve")]
        Approve,
    }
    #[derive(Deserialize)]
    struct Params {
        #[serde(rename = "monitorId")]
        monitor_id: String,
    }
    #[derive(Deserialize)]
    struct Approval {
        #[serde(rename = "method")]
        _method: Method,
        agent: String,
        params: Params,
    }
    let Ok(approval) = serde_json::from_value::<Approval>(request.clone()) else {
        return Ok(false);
    };
    match snapshot(tx, &approval.params.monitor_id) {
        Ok((monitor, _)) => Ok(monitor.retains_approval_for(&approval.agent)),
        Err(error) if startup_store_failure(&error) => Err(error),
        Err(_) => Ok(false),
    }
}

fn retained_question(tx: &Transaction<'_>, request: &Value) -> Result<bool, MonitorError> {
    if request.get("method").and_then(Value::as_str) != Some("agent/asyncQuestion") {
        return Ok(false);
    }
    let Some(id) = request.get("agent").and_then(Value::as_str) else {
        return Ok(false);
    };
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()
        .map_err(studio_runtime_store::StoreError::from)?;
    let agent = match raw.map(|raw| serde_json::from_str::<Value>(&raw)) {
        None => return Ok(false),
        Some(Ok(agent)) if agent.is_object() => agent,
        Some(_) => {
            eprintln!("startup request recovery skipped owner row id={id}: invalid record");
            return Ok(false);
        }
    };
    let same_epoch = agent.get("epoch") == request.get("epoch");
    if event_write::truthy(agent.get("deletedAt")) || !same_epoch {
        return Ok(false);
    }
    if event_write::truthy(agent.get("autoWake")) {
        return Ok(true);
    }
    Ok(agent.get("restartRecovery").is_some_and(|marker| {
        marker.get("stage").and_then(Value::as_str) == Some("pending")
            && event_write::truthy(marker.get("autoWake"))
            && marker.get("epoch") == agent.get("epoch")
    }))
}

fn recover_uncovered_events(tx: &Transaction<'_>) -> Result<usize, MonitorError> {
    // Exact unsubmitted batches were handled by SchedulerRepository above.
    // Every claim still present has no validated scheduler ownership.
    Ok(tx
        .execute(
            "UPDATE runtime_events AS e SET status='uncertain',error=?1 \
         WHERE e.status IN ('reserved','dispatching')",
            [RESTART_EVENT_ERROR],
        )
        .map_err(studio_runtime_store::StoreError::from)?)
}

fn recover_active_monitors(tx: &Transaction<'_>, now: f64) -> Result<usize, MonitorError> {
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_monitors')",
        [],
        |row| row.get(0),
    ).map_err(studio_runtime_store::StoreError::from)?;
    if !exists {
        return Ok(0);
    }
    let ids = active_monitor_ids(tx)?;
    let mut recovered = 0;
    for id in &ids {
        let result = (|| {
            let (snapshot, actor) = snapshot(tx, id)?;
            if snapshot.recovering() || snapshot.unsubmitted_intent() {
                return Ok(false);
            }
            let decision = policy::restart_recovery(&snapshot, now)?;
            let after = MonitorSnapshot::checked(decision.record.clone(), actor.value())?;
            repository::put(tx, decision.record)?;
            crate::agent_progress::publish_owner(tx, &actor.id.0)?;
            crate::sync_policy::publish_monitor_change(tx, Some(&snapshot), &after)?;
            Ok::<bool, MonitorError>(true)
        })();
        if let Err(error) = result {
            if startup_store_failure(&error) {
                return Err(error);
            }
            eprintln!("startup monitor recovery skipped row id={id}: {error}");
            continue;
        }
        recovered += usize::from(result.unwrap_or(false));
    }
    Ok(recovered)
}

fn active_monitor_ids(tx: &Transaction<'_>) -> Result<Vec<String>, MonitorError> {
    Ok(StartupRecoveryRepository::active_monitors(tx)?
        .into_iter()
        .map(|(id, _)| id)
        .collect())
}

pub(crate) fn startup_store_failure(error: &MonitorError) -> bool {
    fn store_failure(error: &studio_runtime_store::StoreError) -> bool {
        matches!(
            error,
            studio_runtime_store::StoreError::Sqlite(_)
                | studio_runtime_store::StoreError::CommitOutcomeUnknown(_)
                | studio_runtime_store::StoreError::WriterPoisoned
                | studio_runtime_store::StoreError::StateDirectory(_)
                | studio_runtime_store::StoreError::AlreadyOwned
                | studio_runtime_store::StoreError::MissingDatabase(_)
                | studio_runtime_store::StoreError::UnsafeStateDirectory
                | studio_runtime_store::StoreError::SchemaMismatch(_)
                | studio_runtime_store::StoreError::SchemaObjectMismatch { .. }
        )
    }
    match error {
        MonitorError::Store(error) => store_failure(error),
        MonitorError::Event(crate::event_write::EventWriteError::Store(error)) => {
            store_failure(error)
        }
        MonitorError::Rules(crate::rules_tick::RulesTickError::Store(error)) => {
            store_failure(error)
        }
        MonitorError::Rules(crate::rules_tick::RulesTickError::Event(
            crate::event_write::EventWriteError::Store(error),
        )) => store_failure(error),
        MonitorError::InvalidStored { .. }
        | MonitorError::InvalidResultReceipt(_)
        | MonitorError::InvalidDeliveryIdentity(_)
        | MonitorError::InvalidOutputReceipt(_)
        | MonitorError::TaskFailed
        | MonitorError::Expired
        | MonitorError::InputOutcomeUnknown
        | MonitorError::Refused(_)
        | MonitorError::Unavailable(_)
        | MonitorError::ProviderRequest { .. }
        | MonitorError::ConfigurationRejected { .. }
        | MonitorError::ConfigurationReceipt { .. }
        | MonitorError::Event(_)
        | MonitorError::Rules(_)
        | MonitorError::Io(_) => false,
    }
}

pub fn snapshot(
    transaction: &Transaction<'_>,
    id: &str,
) -> Result<(MonitorSnapshot, AgentRecord), MonitorError> {
    let row = MonitorsRepository::get(transaction, id)?
        .ok_or(MonitorError::Refused("Unknown command watch"))?;
    let agent_id = row.agent().ok_or_else(|| invalid(id, "agent"))?;
    let actor = AgentRepository::get(transaction, &AgentId::from(agent_id))?
        .ok_or(MonitorError::Refused("Unknown agent"))?;
    let actor_value =
        serde_json::to_value(&actor).map_err(studio_runtime_store::StoreError::from)?;
    let snapshot = MonitorSnapshot::checked_stored(&row, actor_value)?;
    Ok((snapshot, actor))
}

pub async fn cancel(
    store: &RuntimeStore,
    id: &str,
    owner: Option<&str>,
    now: f64,
) -> Result<Value, MonitorError> {
    let id = id.to_owned();
    let owner = owner.map(str::to_owned);
    store
        .transaction_with(move |tx| cancel_in_transaction(tx, &id, owner.as_deref(), now))
        .await
}

pub fn cancel_in_transaction(
    tx: &Transaction<'_>,
    id: &str,
    owner: Option<&str>,
    now: f64,
) -> Result<Value, MonitorError> {
    let (snapshot, actor) = snapshot(tx, id)?;
    let decision = policy::cancel(&snapshot, owner, now)?;
    apply(tx, &snapshot, &actor, &decision, now)?;
    Ok(decision.response)
}

pub struct MonitorApprovalAnswer {
    pub response: Value,
    pub accepted: bool,
}

/// Commit the local monitor decision and its request receipt together. The HTTP
/// caller wakes retained start dispatch only after this transaction succeeds.
pub async fn answer_approval(
    store: &RuntimeStore,
    request_id: &str,
    data: &Value,
    now: f64,
    start_dispatch_available: bool,
) -> Result<Option<MonitorApprovalAnswer>, MonitorError> {
    if !now.is_finite() || now < 0.0 {
        return Err(MonitorError::Refused("Invalid monitor approval clock"));
    }
    let request_id = request_id.to_owned();
    let data = data.clone();
    store
        .transaction_with(move |tx| {
            let Some(request) = Table::<RequestRecord>::new(tx).get(&request_id.to_owned())? else {
                return Ok(None);
            };
            let mut request = request.value().clone();
            if request.get("method").and_then(Value::as_str) != Some("monitor/approve") {
                return Ok(None);
            }
            let decision = data
                .get("decision")
                .and_then(Value::as_str)
                .filter(|decision| matches!(*decision, "accept" | "decline" | "cancel"))
                .ok_or(MonitorError::Refused("Invalid monitor approval decision"))?;
            let accepted = decision == "accept";
            let monitor_id = request
                .pointer("/params/monitorId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or(MonitorError::InvalidStored {
                    id: request_id.clone(),
                    location: "params/monitorId".into(),
                })?;
            let (snapshot, actor) = snapshot(tx, monitor_id)?;
            event_write::refuse_remote(&actor)?;
            if request.get("agent").and_then(Value::as_str) != Some(snapshot.agent()) {
                return Err(MonitorError::Refused("Monitor approval identity changed"));
            }
            if request["status"] == "answered" {
                let saved = request
                    .get("answerDecision")
                    .and_then(Value::as_str)
                    .or_else(|| match snapshot.record()["status"].as_str() {
                        Some("starting" | "running" | "completed" | "failed") => Some("accept"),
                        Some("cancelled") => Some("decline"),
                        _ => None,
                    });
                if saved != Some(decision) {
                    return Err(MonitorError::Refused(
                        "This monitor approval already has a different decision",
                    ));
                }
                if accepted && !start_dispatch_available {
                    return Err(MonitorError::Unavailable("monitor-driver/start-dispatch"));
                }
                return Ok(Some(MonitorApprovalAnswer {
                    response: json!({"status":"answered","replayed":true}),
                    accepted,
                }));
            }
            if request.get("status").and_then(Value::as_str) != Some("pending")
                || snapshot.record()["status"] != "approval"
                || snapshot.record()["epoch"].as_i64() != Some(actor.epoch)
                || actor.auto_wake != Some(true)
                || event_write::truthy(actor.extra.get("deletedAt"))
            {
                return Err(MonitorError::Refused("Command watch was stopped"));
            }
            if accepted && !start_dispatch_available {
                return Err(MonitorError::Unavailable("monitor-driver/start-dispatch"));
            }

            let mut monitor = snapshot.record().clone();
            if accepted {
                monitor["status"] = json!("starting");
            } else {
                monitor["status"] = json!("cancelled");
                monitor["finished"] = json!(now);
                monitor["configurationPending"] = json!(false);
                if let Some(rule_id) = monitor.get("ruleId").and_then(Value::as_str) {
                    let rule = studio_runtime_store::RulesRepository::get(tx, rule_id)?
                        .ok_or(MonitorError::Refused("Unknown monitor rule"))?;
                    let rule_snapshot =
                        crate::rules_engine::RuleSnapshot::from_stored(&rule, actor.value())
                            .map_err(crate::rules_tick::RulesTickError::from)?;
                    let check = rule_snapshot.check();
                    if check.monitor_id != monitor_id {
                        return Err(MonitorError::Refused("Monitor rule identity changed"));
                    }
                    crate::rules_tick::finish_in_transaction(
                        tx,
                        &check,
                        crate::rules_engine::CheckResult {
                            code: None,
                            error: Some("User declined the command"),
                            cancelled: false,
                            output: monitor
                                .get("tail")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                        },
                        now,
                    )?;
                } else if !crate::task_gate::settle(tx, &monitor, now)? {
                    event_write::enqueue_event(
                        tx,
                        &actor,
                        format!("monitor-declined:{monitor_id}"),
                        "monitor_cancelled",
                        format!("User declined command {monitor_id}"),
                        now,
                    )?;
                }
            }
            let after = MonitorSnapshot::checked(monitor.clone(), actor.value())?;
            repository::put(tx, monitor)?;
            crate::agent_progress::publish_owner(tx, &actor.id.0)?;
            crate::sync_policy::publish_monitor_change(tx, Some(&snapshot), &after)?;
            request["status"] = json!("answered");
            request["answerDecision"] = json!(decision);
            request["answerSignature"] = json!(event_write::request_signature(
                &json!({"decision":decision})
            ));
            request["answeredBy"] = json!("user");
            request["answeredAt"] = json!(now);
            request["decision"] = json!(decision);
            let request = RequestRecord::from_value(request.clone())
                .map_err(studio_runtime_store::StoreError::from)?;
            Table::<RequestRecord>::new(tx).put(&request)?;
            SyncDocumentsRepository::put_entity(tx, "request", &request_id, json!({}), true)?;
            Ok(Some(MonitorApprovalAnswer {
                response: json!({"status":"answered"}),
                accepted,
            }))
        })
        .await
}

pub async fn input_without_session(
    store: &RuntimeStore,
    id: &str,
    input: &Input,
    now: f64,
) -> Result<Value, MonitorError> {
    let id = id.to_owned();
    let input = input.clone();
    store
        .transaction_with(move |tx| {
            let (snapshot, _) = snapshot(tx, &id)?;
            let operation = snapshot.operation().ok_or(MonitorError::Refused(
                "This interactive monitor is not active",
            ))?;
            policy::begin_input(&snapshot, operation, "unavailable", &input, now)?;
            Err(MonitorError::Unavailable(SESSION_DEPENDENCY))
        })
        .await
}

/// The provider adapter calls once, after it has obtained its exact transport ticket.
/// A commit error is Unknown: do not send. An inputAttempt already present is never replayed.
pub async fn reserve_input(
    store: &RuntimeStore,
    id: &str,
    operation: &Operation,
    attempt_id: &str,
    input: &Input,
    now: f64,
) -> Result<Effect, MonitorError> {
    let id = id.to_owned();
    let operation = operation.clone();
    let attempt_id = attempt_id.to_owned();
    let input = input.clone();
    store
        .transaction_with(move |tx| {
            let (snapshot, actor) = snapshot(tx, &id)?;
            let mut decision =
                policy::begin_input(&snapshot, &operation, &attempt_id, &input, now)?;
            initialize_input_identity(
                &mut decision,
                &studio_runtime_schema::persistence::RequestIdentity::issue_root(),
            );
            let effect = decision
                .effects
                .first()
                .cloned()
                .ok_or(MonitorError::Refused("Missing monitor input"))?;
            apply(tx, &snapshot, &actor, &decision, now)?;
            Ok(effect)
        })
        .await
}

pub async fn settle_input(
    store: &RuntimeStore,
    id: &str,
    operation: &Operation,
    attempt_id: &str,
    outcome: InputOutcome,
    now: f64,
) -> Result<(), MonitorError> {
    let id = id.to_owned();
    let operation = operation.clone();
    let attempt_id = attempt_id.to_owned();
    store
        .transaction_with(move |tx| {
            let (snapshot, actor) = snapshot(tx, &id)?;
            crate::monitor_http_input::settle(tx, &id, &operation, &attempt_id, &outcome)?;
            let decision = policy::settle_input(&snapshot, &operation, &attempt_id, outcome, now)?;
            apply(tx, &snapshot, &actor, &decision, now)
        })
        .await
}

pub fn finish_in_transaction(
    tx: &Transaction<'_>,
    id: &str,
    operation: Option<&Operation>,
    code: Option<i64>,
    error: Option<&str>,
    now: f64,
) -> Result<(), MonitorError> {
    let (snapshot, actor) = snapshot(tx, id)?;
    let decision = policy::finish(&snapshot, operation, code, error, now)?;
    refuse_unavailable(&decision)?;
    apply(tx, &snapshot, &actor, &decision, now)
}

/// Preserve essential command completion before its provider obligation retires.
/// The existing stream queue keeps this exit behind already committed output.
/// False holds a relevant call whose exact completion proof is unavailable.
pub fn settle_provider_call(
    tx: &Transaction<'_>,
    call: &crate::account_worker::retained::ProviderCallIntent,
    result: &crate::account_worker::retained::RequestResult,
) -> Result<bool, MonitorError> {
    use crate::account_worker::retained::RequestResult;
    use studio_runtime_schema::persistence::RequestIdentity;
    if call.method != studio_runtime_providers::Method::CommandExec {
        return Ok(true);
    }
    let Some(id) = call.params.get("processId").and_then(Value::as_str) else {
        return Ok(true);
    };
    if call.operation != format!("monitor:{id}") {
        return Ok(true);
    }
    let record = match MonitorsRepository::get(tx, id) {
        Ok(record) => record,
        Err(error) if malformed_monitor_owner(&error) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let Some(_record) = record else {
        return Ok(true);
    };
    #[derive(Deserialize)]
    struct StoredOperation {
        operation: Operation,
    }
    let Some((_, raw)) =
        Table::<studio_runtime_store::MonitorRecord>::new(tx).get_with_raw(&id.to_owned())?
    else {
        return Ok(false);
    };
    let Ok(StoredOperation { operation }) = serde_json::from_str(&raw) else {
        return Ok(false);
    };
    let Some(actor) = &call.actor else {
        return Ok(false);
    };
    let Some(token) = &call.admitted_request else {
        return Ok(false);
    };
    if operation.agent != actor.agent
        || operation.epoch != actor.epoch
        || operation.account_key != call.scope.account
        || operation.connection_id != call.scope.connection
        || operation.evidence.provider_generation != Some(call.scope.generation)
        || token.signature() != call.fingerprint(&call.scope.account, call.method.as_str())
    {
        return Ok(false);
    }
    if let Some(saved) = &operation.evidence.provider_request {
        if saved != token {
            return Ok(false);
        }
    } else {
        let root = operation.evidence.root_request_id.as_deref();
        if root != Some(call.root_request_id.as_str()) {
            return Ok(false);
        }
        let epoch = actor.epoch.to_string();
        let identity = RequestIdentity::new(
            "provider-call",
            &[&call.scope.account, &actor.agent, &epoch],
            &call.root_request_id,
            &[&call.operation],
        );
        if !identity.is_ok_and(|identity| identity.id() == token.id()) {
            return Ok(false);
        }
    }
    let (code, error) = match result {
        RequestResult::Reply(value) => {
            if !value.is_object() {
                return Ok(false);
            }
            let code = match value.get("exitCode") {
                None | Some(Value::Null) => None,
                Some(value) => match value.as_i64() {
                    Some(code) => Some(code),
                    None => return Ok(false),
                },
            };
            let error = match value.get("error") {
                None | Some(Value::Null) => None,
                Some(value) => match value.as_str() {
                    Some(error) => Some(error.to_owned()),
                    None => return Ok(false),
                },
            };
            if code.is_none() && error.as_deref().is_none_or(str::is_empty) {
                return Ok(false);
            }
            (code, error)
        }
        RequestResult::Rejected(value) => (None, Some(value.to_string())),
        RequestResult::NotSent => return Ok(true),
        RequestResult::Unknown => return Ok(false),
    };
    crate::monitor_event_queue::enqueue_exit_in_transaction(
        tx,
        id,
        &operation,
        &format!("provider-result:monitor:{id}"),
        code,
        error.as_deref(),
        crate::event_write::unix_time()?,
    )
}

fn malformed_monitor_owner(error: &studio_runtime_store::StoreError) -> bool {
    use studio_runtime_store::StoreError;
    matches!(
        error,
        StoreError::InvalidStoredRecord { .. }
            | StoreError::InvalidStoredObject { .. }
            | StoreError::InvalidSyncObject { .. }
            | StoreError::InvalidSyncObjectField { .. }
            | StoreError::Json(_)
            | StoreError::SchemaValidation(_)
            | StoreError::Sqlite(
                rusqlite::Error::FromSqlConversionFailure(..)
                    | rusqlite::Error::InvalidColumnType(..)
            )
    )
}

pub struct TickPage {
    pub effects: Vec<(String, Effect)>,
    /// Continue the ordered running-monitor scan after this ID when set.
    pub next_after: Option<String>,
}

pub async fn tick_page(
    store: &RuntimeStore,
    now: f64,
    after: Option<&str>,
) -> Result<TickPage, MonitorError> {
    let after = after.map(str::to_owned);
    // No running monitor after the cursor means nothing to decide: stay off the writer lane.
    let start = after.clone();
    let any_running = store.read_async("runtime-services/monitor_tick_page", move |tx| {
        tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_monitors WHERE json_extract(record,'$.status')='running' AND COALESCE(json_extract(record,'$.reattachRecovery.status'),'')!='running' AND id>?1)",
            [start.as_deref().unwrap_or("")],
            |row| row.get::<_, bool>(0),
        ).map_err(|error| MonitorError::from(studio_runtime_store::StoreError::from(error)))
    }).await?;
    if !any_running {
        return Ok(TickPage {
            effects: Vec::new(),
            next_after: None,
        });
    }
    store.transaction_with(move |tx| {
        let mut statement = tx.prepare_cached(
            "SELECT id FROM runtime_monitors WHERE json_extract(record,'$.status')='running' AND COALESCE(json_extract(record,'$.reattachRecovery.status'),'')!='running' AND id>?1 ORDER BY id LIMIT ?2",
        ).map_err(studio_runtime_store::StoreError::from)?;
        let ids = statement
            .query_map((after.as_deref().unwrap_or(""), (MONITOR_TICK_BATCH + 1) as i64), |row| {
                row.get::<_, String>(0)
            })
            .map_err(studio_runtime_store::StoreError::from)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(studio_runtime_store::StoreError::from)?;
        drop(statement);
        let has_more = ids.len() > MONITOR_TICK_BATCH;
        let mut ids = ids.into_iter().take(MONITOR_TICK_BATCH).collect::<Vec<_>>();
        let next_after = has_more.then(|| ids.last().cloned()).flatten();
        let mut effects = Vec::new();
        for id in ids.drain(..) {
            let (snapshot, actor) = snapshot(tx, &id)?;
            let decision = policy::tick(&snapshot, now)?;
            apply(tx, &snapshot, &actor, &decision, now)?;
            effects.extend(
                decision
                    .effects
                    .into_iter()
                    .filter(|e| matches!(e, Effect::Probe { .. }))
                    .map(|effect| (id.clone(), effect)),
            );
        }
        Ok(TickPage { effects, next_after })
    }).await
}

pub async fn complete_probe(
    store: &RuntimeStore,
    id: &str,
    generation: i64,
    result: Value,
    now: f64,
) -> Result<(), MonitorError> {
    let id = id.to_owned();
    store
        .transaction_with(move |tx| {
            let (snapshot, actor) = snapshot(tx, &id)?;
            let decision = policy::probe_result(&snapshot, generation, result, now)?;
            apply(tx, &snapshot, &actor, &decision, now)
        })
        .await
}

fn refuse_unavailable(decision: &Decision) -> Result<(), MonitorError> {
    for effect in &decision.effects {
        match effect {
            Effect::Terminate | Effect::Native { .. } => {
                return Err(MonitorError::Unavailable(SESSION_DEPENDENCY));
            }

            Effect::OutputEvent { .. }
            | Effect::ExitEvent
            | Effect::StallEvent { .. }
            | Effect::Probe { .. }
            | Effect::ExpireApprovals
            | Effect::RuleFinished => (),
        }
    }
    Ok(())
}

pub fn apply(
    tx: &Transaction<'_>,
    before: &MonitorSnapshot,
    actor: &AgentRecord,
    decision: &Decision,
    now: f64,
) -> Result<(), MonitorError> {
    apply_with_rule(
        tx,
        before,
        actor,
        decision,
        now,
        &crate::rules_tick::MonitorRuleFinish,
    )
}

/// The rules adapter implements this using rules_tick::finish_in_transaction.
/// A false/stale rule identity must return an error, rolling back the monitor too.
pub trait RuleFinish {
    fn finish(
        &self,
        tx: &Transaction<'_>,
        before: &Value,
        after: &Value,
        now: f64,
    ) -> Result<(), MonitorError>;
}
pub struct UnavailableRules;
impl RuleFinish for UnavailableRules {
    fn finish(
        &self,
        _: &Transaction<'_>,
        _: &Value,
        _: &Value,
        _: f64,
    ) -> Result<(), MonitorError> {
        Err(MonitorError::Unavailable(RULE_DEPENDENCY))
    }
}

pub fn apply_with_rule(
    tx: &Transaction<'_>,
    before: &MonitorSnapshot,
    actor: &AgentRecord,
    decision: &Decision,
    now: f64,
    rules: &impl RuleFinish,
) -> Result<(), MonitorError> {
    event_write::refuse_remote(actor)?;
    // Validate before any write, including all optional fields preserved from Python.
    let after = MonitorSnapshot::checked(
        decision.record.clone(),
        serde_json::to_value(actor).map_err(studio_runtime_store::StoreError::from)?,
    )?;
    for effect in &decision.effects {
        match effect {
            Effect::OutputEvent { id, payload } => {
                event_write::enqueue_recovery_event(
                    tx,
                    actor,
                    id.clone(),
                    "monitor_output",
                    python_json(payload),
                    now,
                )?;
            }
            Effect::RuleFinished => rules.finish(tx, before.record(), &decision.record, now)?,
            Effect::ExitEvent => exit_event(tx, actor, &decision.record, now)?,
            Effect::StallEvent {
                generation,
                payload,
            } => {
                event_write::enqueue_recovery_event(
                    tx,
                    actor,
                    format!("monitor-stall:{}:{generation}", before.id()),
                    "monitor_stall",
                    python_json(payload),
                    now,
                )?;
            }
            Effect::ExpireApprovals => {
                for mut request in repository::pending_approvals(tx, before.id())? {
                    request.set("status", json!("expired"));
                    Table::new(tx).put(&request)?;
                    SyncDocumentsRepository::put_entity(
                        tx,
                        "request",
                        request.id(),
                        json!({}),
                        true,
                    )?;
                }
            }
            Effect::Probe { .. } | Effect::Terminate | Effect::Native { .. } => (),
        }
    }
    if before.record() != &decision.record {
        repository::put(tx, decision.record.clone())?;
        crate::agent_progress::publish_owner(tx, &actor.id.0)?;
        crate::sync_policy::publish_monitor_change(tx, Some(before), &after)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum TerminalStatus {
    Completed,
    Failed,
    Cancelled,
    Lost,
}

// This is a projection of a full persisted monitor, so unrelated monitor fields
// are intentionally ignored during deserialization.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct TerminalPayload {
    id: String,
    command: String,
    status: TerminalStatus,
    #[serde(default)]
    exit_code: Option<i64>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    tail: String,
    #[serde(default)]
    log: Option<String>,
    #[serde(default)]
    bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_match_summary: Option<OutputMatchSummary>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OutputMatchSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    suppressed_matches: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_wakes_paused: Option<bool>,
    events: u64,
    matches: Vec<OutputMatch>,
    pending: Vec<OutputMatch>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OutputMatch {
    stream: OutputStream,
    pattern: String,
    count: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TerminalOwner {
    #[serde(default)]
    epoch: Option<i64>,
    #[serde(default)]
    operation: Option<Operation>,
}

fn exit_event(
    tx: &Transaction<'_>,
    actor: &AgentRecord,
    monitor: &Value,
    now: f64,
) -> Result<(), MonitorError> {
    if crate::task_gate::settle(tx, monitor, now)? {
        return Ok(());
    }
    let mut payload: TerminalPayload =
        serde_json::from_value(monitor.clone()).map_err(studio_runtime_store::StoreError::from)?;
    payload.output_match_summary = crate::monitor_matches::summary(monitor)
        .map(serde_json::from_value)
        .transpose()
        .map_err(studio_runtime_store::StoreError::from)?;
    let owner: TerminalOwner =
        serde_json::from_value(monitor.clone()).map_err(studio_runtime_store::StoreError::from)?;
    // The late answer of a deadline loss is a second outcome for the owner.
    let event_id = if monitor.get("lateAnswerAt").is_some_and(|at| !at.is_null()) {
        format!("monitor:{}:late-answer", payload.id)
    } else {
        format!("monitor:{}", payload.id)
    };
    let monitor_epoch = owner
        .epoch
        .or_else(|| owner.operation.map(|operation| operation.epoch));
    let can_wake =
        monitor_epoch == Some(actor.epoch) && !event_write::truthy(actor.extra.get("deletedAt"));
    if repository::exit_event(tx, &event_id)?.is_some() {
        return Ok(());
    }
    let mut payload =
        serde_json::to_value(payload).map_err(studio_runtime_store::StoreError::from)?;
    if monitor["shellPressureCleanup"].is_object() {
        payload["stopCause"] = json!("memory_pressure");
        payload["message"] = json!(format!(
            "Studio requested this monitor stop under critical memory pressure after {} idle minutes.",
            studio_runtime_schema::shell_policy::PRESSURE_IDLE_MS / 60_000
        ));
    }
    enqueue(
        tx,
        actor,
        &event_id,
        "monitor_exit",
        &payload,
        (can_wake, monitor_epoch),
        now,
    )
}

fn enqueue(
    tx: &Transaction<'_>,
    actor: &AgentRecord,
    id: &str,
    kind: &str,
    payload: &Value,
    wake: (bool, Option<i64>),
    now: f64,
) -> Result<(), MonitorError> {
    if wake.0 {
        event_write::enqueue_recovery_event(tx, actor, id.into(), kind, python_json(payload), now)?;
    } else {
        EventWriteRepository::insert_event(
            tx,
            &QueueEvent {
                id: QueueEventId::from(id),
                agent: actor.id.clone(),
                kind: kind.into(),
                text: python_json(payload),
                status: "cancelled".into(),
                created: now,
                epoch: wake.1.unwrap_or(actor.epoch),
                turn_id: None,
                error: None,
            },
        )?;
    }
    Ok(())
}

fn invalid(id: &str, location: &str) -> MonitorError {
    MonitorError::InvalidStored {
        id: id.into(),
        location: location.into(),
    }
}

const MONITOR_START_ACTOR_INPUTS: &[&str] = &[
    "/threadId",
    "/cwd",
    "/provider",
    "/executionMode",
    "/yoloMode",
    "/approvalPolicy",
    "/sandbox",
    "/profile/id",
    "/role",
    "/isLead",
    "/workspaceMode",
    "/layrScratch",
];

#[derive(Clone)]
pub struct PreparedStart {
    pub monitor: Value,
    pub actor: Value,
    pub operation: Operation,
    pub params: Value,
}

/// Persist the root before preparation can perform any provider operation.
/// A committed operation always keeps its captured identity, including imports.
pub async fn provider_root(store: &RuntimeStore, id: &str) -> Result<String, MonitorError> {
    let id = id.to_owned();
    store
        .transaction_with(move |tx| {
            let (snapshot, actor) = snapshot(tx, &id)?;
            if let Some(operation) = snapshot.operation() {
                return operation
                    .evidence
                    .root_request_id
                    .clone()
                    .ok_or(MonitorError::Refused(
                        "Monitor provider request identity is unavailable",
                    ));
            }
            if let Some(root) = snapshot.record().get("rootRequestId") {
                return root
                    .as_str()
                    .map(str::to_owned)
                    .ok_or(MonitorError::Refused(
                        "Monitor provider request identity is invalid",
                    ));
            }
            if !snapshot.active_owner() {
                return Err(MonitorError::Refused("Monitor owner is no longer active"));
            }
            let root = studio_runtime_schema::persistence::RequestIdentity::issue_root();
            let mut record = snapshot.record().clone();
            record["rootRequestId"] = json!(root);
            // The root is owner state; no delivery or status transition is implied.
            let typed = studio_runtime_store::MonitorRecord::from_value(record)
                .map_err(studio_runtime_store::StoreError::from)?;
            studio_runtime_store::Table::new(tx).put(&typed)?;
            drop(actor);
            Ok(root)
        })
        .await
}

pub async fn reserve_start(
    store: &RuntimeStore,
    id: &str,
    prepared: &PreparedStart,
    now: f64,
) -> Result<Effect, MonitorError> {
    let id = id.to_owned();
    let prepared = prepared.clone();
    store
        .transaction_with(move |tx| {
            let (snapshot, actor) = snapshot(tx, &id)?;
            if !restart_ready(tx, snapshot.record())? {
                return Err(MonitorError::Refused(
                    "Wait for the previous command to exit before restarting",
                ));
            }
            let current_actor = actor.value();
            if snapshot.record() != &prepared.monitor
                || crate::legacy_image_workspace::is_legacy(&current_actor)
                    != crate::legacy_image_workspace::is_legacy(&prepared.actor)
                || MONITOR_START_ACTOR_INPUTS
                    .iter()
                    .any(|path| current_actor.pointer(path) != prepared.actor.pointer(path))
            {
                return Err(MonitorError::Refused("Monitor changed during preparation"));
            }
            let decision =
                policy::start(&snapshot, &prepared.operation, prepared.params.clone(), now)?;
            let effect = decision
                .effects
                .first()
                .cloned()
                .ok_or(MonitorError::Refused("Missing monitor command"))?;
            apply(tx, &snapshot, &actor, &decision, now)?;
            Ok(effect)
        })
        .await
}

pub(crate) fn initialize_input_identity(decision: &mut Decision, root: &str) {
    decision.record["inputAttempt"]["rootRequestId"] = json!(root);
    if let Some(attempt) = decision.record["inputAttempt"].as_object_mut() {
        attempt.remove("providerRequest");
    }
}

/// Resolve a provider mutation from its current owner; a retry never allocates another root.
pub async fn provider_request_identity(
    store: &RuntimeStore,
    id: &str,
    probe_generation: Option<i64>,
    operation: &Operation,
    request_operation: &str,
    method: studio_runtime_providers::Method,
    params: &Value,
) -> Result<
    (
        String,
        Option<studio_runtime_store::request_results::AdmittedRequest>,
    ),
    MonitorError,
> {
    use studio_runtime_providers::Method;
    let id = id.to_owned();
    let operation = operation.clone();
    let request_operation = request_operation.to_owned();
    let params = params.clone();
    store.transaction_with(move |tx| {
        let (snapshot,_)=snapshot(tx,&id)?;
        if snapshot.operation().is_some_and(|saved| saved!=&operation) {return Err(MonitorError::Refused("Monitor provider operation changed"));}
        if probe_generation.is_none() && method==Method::CommandExec {
            let root=operation.evidence.root_request_id.ok_or(MonitorError::Refused("Monitor provider request identity is unavailable"))?;
            return Ok((root,operation.evidence.provider_request));
        }
        let mut record=snapshot.record().clone();
        let identity = if let Some(generation)=probe_generation {
            if record["stallProbeGeneration"].as_i64()!=Some(generation) {return Err(MonitorError::Refused("Monitor liveness generation changed"));}
            if record["livenessProbe"]["generation"].as_i64()!=Some(generation) {
                record["livenessProbe"]=json!({"generation":generation,"rootRequestId":studio_runtime_schema::persistence::RequestIdentity::issue_root()});
                Table::new(tx).put(&studio_runtime_store::MonitorRecord::from_value(record.clone()).map_err(studio_runtime_store::StoreError::from)?)?;
            }
            record["livenessProbe"].clone()
        } else if method==Method::CommandExecWrite || method==Method::CommandExecResize {
            let ticket=policy::input_effect_ticket(&id,&request_operation).ok_or(MonitorError::Refused("Monitor input provider identity changed"))?;
            let attempt=&record["inputAttempt"];
            if attempt["id"]!=ticket || attempt["params"]!=params || attempt["method"]!=method.as_str() {return Err(MonitorError::Refused("Monitor input provider identity changed"));}
            attempt.clone()
        } else if method==Method::CommandExecTerminate {
            json!({"rootRequestId":record["cancelRootRequestId"],"providerRequest":record["cancelProviderRequest"]})
        } else {return Err(MonitorError::Refused("Monitor provider method has no mutation owner"));};
        let root=identity["rootRequestId"].as_str().ok_or(MonitorError::Refused("Monitor provider request identity is unavailable"))?.to_owned();
        let admitted = if identity.get("providerRequest").is_some_and(|value| !value.is_null()) {
            let path = if probe_generation.is_some() { "$.livenessProbe.providerRequest" } else if method == Method::CommandExecTerminate { "$.cancelProviderRequest" } else { "$.inputAttempt.providerRequest" };
            let raw: String = tx.query_row("SELECT record -> ?2 FROM runtime_monitors WHERE id=?1", rusqlite::params![id, path], |row| row.get(0)).map_err(studio_runtime_store::StoreError::from)?;
            Some(serde_json::from_str(&raw).map_err(studio_runtime_store::StoreError::from)?)
        } else { None };
        Ok((root,admitted))
    }).await
}

/// Unknown submission and backend restart share the same durable reconciliation.
pub async fn reconcile_unknown(
    store: &RuntimeStore,
    id: &str,
    now: f64,
) -> Result<(), MonitorError> {
    let id = id.to_owned();
    store
        .transaction_with(move |tx| {
            let (snapshot, actor) = snapshot(tx, &id)?;
            // A late exact receipt may already have settled the command.
            if !snapshot.active() {
                return Ok(());
            }
            let decision = policy::restart_recovery(&snapshot, now)?;
            apply(tx, &snapshot, &actor, &decision, now)
        })
        .await
}

/// Settle an unanswered retained command in one transaction. No intermediate
/// recovery is visible to a monitor tick. A result that already committed wins.
pub async fn settle_unanswered(
    store: &RuntimeStore,
    id: &str,
    operation: &Operation,
    reason: &str,
    now: f64,
) -> Result<(), MonitorError> {
    let id = id.to_owned();
    let operation = operation.clone();
    let reason: String = reason.chars().take(12000).collect();
    store
        .transaction_with(move |tx| {
            let (before, actor) = snapshot(tx, &id)?;
            if before.operation() != Some(&operation) {
                return Err(MonitorError::Refused("Monitor operation identity changed"));
            }
            if !before.active() {
                return Ok(());
            }
            let recovery = policy::restart_recovery(&before, now)?;
            let recovering = MonitorSnapshot::checked(
                recovery.record,
                serde_json::to_value(&actor).map_err(studio_runtime_store::StoreError::from)?,
            )?;
            let loss = policy::restart_lost(&recovering, &reason, now)?;
            apply(tx, &before, &actor, &loss, now)
        })
        .await
}

/// Deadline losses that may still be in their late-answer window at `now`.
/// The policy (`awaits_late_answer`) decides; this query only narrows the rows.
pub async fn late_answer_ids(
    store: &RuntimeStore,
    after: Option<&str>,
    now: f64,
) -> Result<Vec<String>, MonitorError> {
    let after = after.unwrap_or_default().to_owned();
    store
        .read_async("runtime-services/monitor_late_answers", move |connection| {
            let mut statement = connection.prepare_cached(
                "SELECT id FROM runtime_monitors WHERE json_extract(record,'$.status')='lost' AND json_extract(record,'$.reattachRecovery.proof')=?1 AND json_extract(record,'$.finished')>?2 AND id>?3 ORDER BY id LIMIT ?4",
            )?;
            let ids = statement
                .query_map(
                    (
                        policy::DEADLINE_PASSED,
                        now - policy::MONITOR_LATE_ANSWER_SECONDS,
                        after,
                        MONITOR_TICK_BATCH as i64,
                    ),
                    |row| row.get::<_, String>(0),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<_, studio_runtime_store::StoreError>(ids)
        })
        .await
        .map_err(MonitorError::from)
}

/// Running commands with a native operation, no rule, no reattach in progress
/// and no queued event, after `after` in ID order. These are the only commands
/// whose exit depends on a live waiter in the monitor port.
pub async fn unqueued_running_ids(
    store: &RuntimeStore,
    after: Option<&str>,
) -> Result<Vec<String>, MonitorError> {
    let after = after.unwrap_or_default().to_owned();
    store
        .read_async("runtime-services/monitor_orphans", move |connection| {
            let mut statement = connection.prepare_cached(
                "SELECT id FROM runtime_monitors WHERE json_extract(record,'$.status')='running' AND json_type(record,'$.operation')='object' AND json_extract(record,'$.ruleId') IS NULL AND COALESCE(json_extract(record,'$.reattachRecovery.status'),'')!='running' AND COALESCE(json_array_length(record,'$.streamQueue'),0)=0 AND id>?1 ORDER BY id LIMIT ?2",
            )?;
            let ids = statement
                .query_map((after.as_str(), MONITOR_TICK_BATCH as i64), |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<_, studio_runtime_store::StoreError>(ids)
        })
        .await
        .map_err(MonitorError::from)
}

/// Stage the exact-identity reattach for a running command whose exit has no
/// owner in this process: its request future ended, failed or was lost with an
/// account worker. The next reattach pass asks the provider about the exact
/// `monitor:<id>` operation and settles a gone process once. Returns whether
/// the command was staged. A command whose state changed is left untouched.
pub async fn reconcile_orphan(
    store: &RuntimeStore,
    id: &str,
    now: f64,
) -> Result<bool, MonitorError> {
    let id = id.to_owned();
    store
        .transaction_with(move |tx| {
            let (snapshot, actor) = snapshot(tx, &id)?;
            let record = snapshot.record();
            if record["status"] != "running"
                || snapshot.operation().is_none()
                || snapshot.recovering()
                || record.get("ruleId").is_some_and(|rule| !rule.is_null())
                || record
                    .get("streamQueue")
                    .and_then(Value::as_array)
                    .is_some_and(|queue| !queue.is_empty())
            {
                return Ok(false);
            }
            let decision = policy::restart_recovery(&snapshot, now)?;
            apply(tx, &snapshot, &actor, &decision, now)?;
            Ok(true)
        })
        .await
}

/// Decide an active command's failed reattach as lost and enqueue its one result.
/// Idempotent: a command with no running recovery is left untouched.
pub async fn settle_reattach(
    store: &RuntimeStore,
    id: &str,
    reason: &str,
    now: f64,
) -> Result<(), MonitorError> {
    let id = id.to_owned();
    let reason: String = reason.chars().take(12000).collect();
    store
        .transaction_with(move |tx| {
            let (snapshot, actor) = snapshot(tx, &id)?;
            let decision = policy::restart_lost(&snapshot, &reason, now)?;
            apply(tx, &snapshot, &actor, &decision, now)
        })
        .await
}

pub async fn reattach(
    store: &RuntimeStore,
    id: &str,
    proof: &policy::ReattachProof,
    now: f64,
) -> Result<policy::ReattachDisposition, MonitorError> {
    let id = id.to_owned();
    let proof = policy::ReattachProof {
        operation: proof.operation.clone(),
        operation_id: proof.operation_id.clone(),
        native_id: proof.native_id,
        current_connection: proof.current_connection.clone(),
    };
    store
        .transaction_with(move |tx| {
            let (snapshot, actor) = snapshot(tx, &id)?;
            // A provider read runs outside this transaction. Its exact proof
            // can arrive after an exit or another reattach already committed.
            if !snapshot.recovering()
                && snapshot.operation() == Some(&proof.operation)
                && proof.operation_id == format!("monitor:{}", snapshot.id())
                && !proof.current_connection.is_empty()
            {
                return Ok(policy::ReattachDisposition::Superseded);
            }
            let decision = policy::reattach(&snapshot, &proof)?;
            apply(tx, &snapshot, &actor, &decision, now)?;
            Ok(if decision.effects.contains(&Effect::Terminate) {
                policy::ReattachDisposition::Cancel
            } else {
                policy::ReattachDisposition::AwaitExit
            })
        })
        .await
}

/// Admission for the dynamic monitor caller. Return the durable identity promptly;
/// its preparation worker calls reserve_start after applying the workspace/provider policy.
pub struct MonitorAdmission<'a> {
    pub actor_id: &'a str,
    pub epoch: i64,
    pub request_id: &'a str,
    pub request: &'a Value,
    pub log_root: &'a str,
}

/// Return an exact prior create result without workspace preparation. This is
/// a read-only fast path; `create_in_transaction` repeats the identity check
/// under the committing transaction to close the race with a concurrent create.
pub async fn stored_create_result(
    store: &RuntimeStore,
    admission: &MonitorAdmission<'_>,
    now: f64,
) -> Result<Option<Value>, MonitorError> {
    let actor_id = admission.actor_id.to_owned();
    let request = admission.request.clone();
    let request_id = admission.request_id.to_owned();
    let log_root = admission.log_root.to_owned();
    store
        .transaction_with(move |tx| {
            let actor = event_write::checked_agent(tx, &actor_id)?;
            let actor_value =
                serde_json::to_value(&actor).map_err(studio_runtime_store::StoreError::from)?;
            let record =
                policy::create_record(&actor_value, &request, &request_id, &log_root, now)?;
            let id = record
                .get("id")
                .and_then(Value::as_str)
                .ok_or(MonitorError::Refused("Invalid monitor identity"))?;
            match MonitorsRepository::get(tx, id)? {
                Some(previous) if policy::same_request(&previous.value(), &record) => {
                    Ok(Some(previous.value()))
                }
                Some(_) => Err(MonitorError::Refused(
                    "This monitor request id has different content",
                )),
                None => Ok(None),
            }
        })
        .await
}

/// Implemented by workspace admission. This check performs only transactional
/// reads: preparation, filesystem and provider calls have already completed.
pub trait WorkspaceAdmission {
    fn check(&self, tx: &Transaction<'_>, actor: &AgentRecord) -> Result<(), MonitorError>;
}

pub fn create_in_transaction(
    tx: &Transaction<'_>,
    admission: &MonitorAdmission<'_>,
    workspace: &dyn WorkspaceAdmission,
    now: f64,
) -> Result<Value, MonitorError> {
    let MonitorAdmission {
        actor_id,
        epoch,
        request_id,
        request,
        log_root,
    } = *admission;
    let actor = event_write::checked_agent(tx, actor_id)?;
    event_write::refuse_remote(&actor)?;
    let actor_value =
        serde_json::to_value(&actor).map_err(studio_runtime_store::StoreError::from)?;
    let record = policy::create_record(&actor_value, request, request_id, log_root, now)?;
    let id = record
        .get("id")
        .and_then(Value::as_str)
        .ok_or(MonitorError::Refused("Invalid monitor identity"))?;
    if let Some(previous) = MonitorsRepository::get(tx, id)? {
        if !policy::same_request(&previous.value(), &record) {
            return Err(MonitorError::Refused(
                "This monitor request id has different content",
            ));
        }
        return Ok(previous.value());
    }
    if actor.epoch != epoch || actor.auto_wake != Some(true) {
        return Err(MonitorError::Refused(
            studio_runtime_schema::shared_values::MONITOR_OWNER_STOPPED_ERROR,
        ));
    }
    workspace.check(tx, &actor)?;
    if repository::active_count(tx)? >= 64 {
        return Err(MonitorError::Refused("Maximum 64 active command watches"));
    }
    repository::put(tx, record.clone())?;
    crate::agent_progress::publish_owner(tx, &actor.id.0)?;
    if record["status"] == "approval" {
        let approval = studio_runtime_store::RequestRecord::from_value(json!({
            "id":format!("monitor-approval:{id}"), "method":"monitor/approve", "agent":actor_id,
            "params":{"monitorId":id,"command":record["command"],"cwd":record["cwd"]}, "status":"pending", "createdAt":now
        })).map_err(studio_runtime_store::StoreError::from)?;
        Table::new(tx).put(&approval)?;
        let view = crate::sync_views::project_persisted_record("request", approval.value())
            .ok_or_else(|| invalid(id, "request"))?;
        SyncDocumentsRepository::put_entity(tx, "request", approval.id(), view, false)?;
    }
    let after = MonitorSnapshot::checked(record.clone(), actor_value)?;
    crate::sync_policy::publish_monitor_change(tx, None, &after)?;
    Ok(record)
}

/// Checked restart fields and the digest of the exact validated JSON request.
#[derive(Debug)]
pub struct RestartRequest {
    id: String,
    agent: String,
    epoch: i64,
    request_id: String,
    account_key: String,
    output_patterns: Vec<String>,
    signature: String,
}

impl RestartRequest {
    pub fn agent(&self) -> &str {
        &self.agent
    }
}

impl<'de> serde::Deserialize<'de> for RestartRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            id: String,
            agent: String,
            epoch: i64,
            request_id: String,
            account_key: String,
            output_patterns: Vec<String>,
        }
        let raw = <Value as serde::Deserialize>::deserialize(deserializer)?;
        let signature = crate::process_controls::digest(&raw);
        let wire: Wire = serde::Deserialize::deserialize(raw).map_err(D::Error::custom)?;
        Ok(Self {
            id: wire.id,
            agent: wire.agent,
            epoch: wire.epoch,
            request_id: wire.request_id,
            account_key: wire.account_key,
            output_patterns: wire.output_patterns,
            signature,
        })
    }
}

/// A confirmed restart admits one replacement and requests cancellation atomically.
/// The replacement cannot start until the source has a confirmed terminal state.
pub fn restart_in_transaction(
    tx: &Transaction<'_>,
    body: &RestartRequest,
    workspace: &dyn WorkspaceAdmission,
    log_root: &str,
    now: f64,
) -> Result<Value, MonitorError> {
    let id = body.id.as_str();
    let owner = body.agent.as_str();
    let epoch = body.epoch;
    let request_id = format!("monitor-restart:{owner}:{}", body.request_id);
    let replacement_id =
        uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, request_id.as_bytes()).to_string();
    let actor = event_write::checked_agent(tx, owner)?;
    if actor.epoch != epoch
        || actor.account_key.as_str() != body.account_key
    {
        return Err(MonitorError::Refused("The monitor owner changed"));
    }
    let signature = &body.signature;
    if let Some(previous) = MonitorsRepository::get(tx, &replacement_id)? {
        if previous.value()["restartSignature"] != signature.as_str() {
            return Err(MonitorError::Refused(
                "This restart request has different content",
            ));
        }
        return Ok(previous.value());
    }
    let (source, _) = snapshot(tx, id)?;
    let old = source.record();
    if old["agent"] != owner
        || old["epoch"] != epoch
        || old["cancelRequested"] == true
        || !matches!(
            old["status"].as_str(),
            Some("running" | "starting" | "approval")
        )
    {
        return Err(MonitorError::Refused(
            "The monitor changed or finished. Refresh its details",
        ));
    }
    let patterns = crate::monitor_matches::patterns(Some(&json!(body.output_patterns)))?;
    if json!(patterns) == old["outputPatterns"] {
        return Err(MonitorError::Refused(
            "Use narrower output patterns before restarting",
        ));
    }
    let mut request = json!({"command":old["command"],"output_patterns":patterns});
    for (saved, input) in [
        ("timeout_ms", "timeout_ms"),
        ("interactive", "interactive"),
        ("wakeOn", "wake_on"),
        ("successExitCodes", "success_exit_codes"),
        ("stallTimeoutSeconds", "stall_timeout_seconds"),
        ("livenessCommand", "liveness_command"),
    ] {
        if let Some(value) = old.get(saved) {
            request[input] = value.clone();
        }
    }
    let admission = MonitorAdmission {
        actor_id: owner,
        epoch,
        request_id: &request_id,
        request: &request,
        log_root,
    };
    let mut replacement = create_in_transaction(tx, &admission, workspace, now)?;
    replacement["restartAfter"] = json!(id);
    replacement["restartSignature"] = json!(signature);
    repository::put(tx, replacement.clone())?;
    cancel_in_transaction(tx, id, Some(owner), now)?;
    Ok(replacement)
}

/// Missing or uncertain source outcomes never authorize a replacement command.
pub fn restart_ready(
    connection: &studio_runtime_store::Connection,
    record: &Value,
) -> Result<bool, MonitorError> {
    let Some(id) = record["restartAfter"].as_str() else {
        return Ok(true);
    };
    Ok(
        MonitorsRepository::get(connection, id)?.is_some_and(|source| {
            matches!(
                source.value()["status"].as_str(),
                Some("completed" | "failed" | "cancelled")
            )
        }),
    )
}

/// Attach a rule-check identity and retained account to a monitor admitted by
/// the shared workspace path. The update shares the admission transaction and
/// renderer projection so callback and output consumers see one persisted row.
pub fn bind_rule_in_transaction(
    tx: &Transaction<'_>,
    monitor_id: &str,
    rule_id: &str,
    account_key: &str,
) -> Result<Value, MonitorError> {
    let (before, actor) = snapshot(tx, monitor_id)?;
    let mut value = before.record().clone();
    if !matches!(value["status"].as_str(), Some("starting" | "approval"))
        || (value
            .get("ruleId")
            .and_then(Value::as_str)
            .is_some_and(|saved| !saved.is_empty() && saved != rule_id))
        || value
            .get("accountKey")
            .and_then(Value::as_str)
            .is_some_and(|saved| !saved.is_empty() && saved != account_key)
    {
        return Err(MonitorError::Refused(
            "Rule monitor admission identity changed",
        ));
    }
    value["ruleId"] = json!(rule_id);
    value["accountKey"] = json!(account_key);
    let after = MonitorSnapshot::checked(value.clone(), actor.value())?;
    repository::put(tx, value.clone())?;
    crate::sync_policy::publish_monitor_change(tx, Some(&before), &after)?;
    Ok(value)
}

#[cfg(test)]
mod recovery_selection_tests {
    use super::active_monitor_ids;
    use rusqlite::Connection;

    #[test]
    fn receipt_pivot_late_monitor_result_commits_exit_behind_output_without_replay_cache() {
        use crate::account_worker::retained::{ProviderCallIntent, RequestResult};
        use serde_json::json;
        use studio_runtime_schema::persistence::{REQUEST_RESULT_TTL_MS, RequestIdentity};
        use studio_runtime_store::request_results::{Admission, RequestResults, ResultEnvelope};
        let mut db = Connection::open_in_memory().expect("database");
        studio_runtime_store::receipt_schema::create(&db).expect("schema");
        db.execute_batch("CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL);CREATE TABLE runtime_orchestration_settings(id TEXT PRIMARY KEY,record TEXT NOT NULL)").expect("owners");
        let issued = REQUEST_RESULT_TTL_MS * 2;
        let root =
            uuid::Uuid::from_u128(((issued as u128) << 80) | (7 << 76) | (2 << 62)).to_string();
        let mut call = ProviderCallIntent {
            root_request_id: root.clone(),
            admitted_request: None,
            scope: crate::account_session_runtime::ConnectionScope {
                account: "account".into(),
                connection: "connection".into(),
                generation: 7,
            },
            operation: "monitor:watch".into(),
            actor: Some(crate::account_worker::retained::RequestActor {
                agent: "agent".into(),
                epoch: 3,
            }),
            transfer: None,
            deleted_actor_session: None,
            method: studio_runtime_providers::Method::CommandExec,
            params: json!({"processId":"watch"}),
        };
        let identity = RequestIdentity::new(
            "provider-call",
            &["account", "agent", "3"],
            &root,
            &["monitor:watch"],
        )
        .expect("identity");
        let signature = call.fingerprint("account", call.method.as_str());
        let tx = db.transaction().expect("transaction");
        let Admission::Fresh(token) =
            RequestResults::admit(&tx, &identity, &signature, issued).expect("admission")
        else {
            panic!("fresh");
        };
        call.admitted_request = Some(token.clone());
        let operation = json!({"agent":"agent","epoch":3,"accountKey":"account","connectionId":"connection","providerGeneration":7,"rootRequestId":root});
        let record = json!({"id":"watch","agent":"agent","status":"running","operation":operation,"streamQueue":[{"cursor":"output:1","operation":operation,"offset":0,"byteLength":1,"deltaBase64":"YQ=="}]});
        tx.execute(
            "INSERT INTO runtime_monitors(id,record) VALUES('watch',?1)",
            [record.to_string()],
        )
        .expect("owner and prior output");
        RequestResults::gc(&tx, issued + REQUEST_RESULT_TTL_MS + 1).expect("expire root");
        assert!(
            !super::settle_provider_call(
                &tx,
                &call,
                &RequestResult::Reply(json!({"processId":"watch"}))
            )
            .expect("invalid positive proof held")
        );
        let result = RequestResult::Reply(json!({"exitCode":0}));
        assert!(super::settle_provider_call(&tx, &call, &result).expect("late exact result"));
        assert!(super::settle_provider_call(&tx, &call, &result).expect("duplicate result"));
        RequestResults::settle(
            &tx,
            &token,
            &ResultEnvelope {
                actor: Some("agent".into()),
                reply: result,
                references: vec![],
            },
        )
        .expect("late response settlement allowed");
        assert!(
            RequestResults::replay(
                &tx,
                token.id(),
                &signature,
                issued + REQUEST_RESULT_TTL_MS + 1
            )
            .is_err(),
            "transport replay already expired"
        );
        let saved = studio_runtime_store::MonitorsRepository::get(&tx, "watch")
            .expect("read")
            .expect("owner");
        let saved_value = saved.value();
        let queue = saved_value["streamQueue"].as_array().expect("queue");
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0]["cursor"], "output:1");
        assert_eq!(queue[1]["kind"], "exit");
        assert_eq!(queue[1]["code"], 0);
        call.scope.generation += 1;
        assert!(
            !super::settle_provider_call(&tx, &call, &RequestResult::Reply(json!({"exitCode":0})))
                .expect("old generation cannot settle another owner")
        );
        tx.execute(
            "UPDATE runtime_monitors SET record='{' WHERE id='watch'",
            [],
        )
        .expect("malformed owner");
        assert!(
            !super::settle_provider_call(&tx, &call, &RequestResult::Reply(json!({"exitCode":0})))
                .expect("invalid owner holds only its call")
        );
        assert_eq!(
            tx.query_row(
                "SELECT record FROM runtime_monitors WHERE id='watch'",
                [],
                |row| row.get::<_, String>(0)
            )
            .expect("raw owner preserved"),
            "{"
        );
        tx.execute_batch("ALTER TABLE runtime_monitors RENAME COLUMN record TO broken")
            .expect("SQL failure boundary");
        assert!(
            super::settle_provider_call(&tx, &call, &RequestResult::Reply(json!({"exitCode":0})))
                .is_err(),
            "SQL errors remain real transaction failures"
        );
    }

    #[test]
    fn startup_monitor_selection_keeps_only_active_statuses_in_id_order() {
        let mut connection = Connection::open_in_memory().expect("in-memory database");
        connection
            .execute_batch(
                "CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                 INSERT INTO runtime_monitors(id, record) VALUES
                    ('z-running', '{\"status\":\"running\",\"futureField\":{\"enabled\":true}}'),
                    ('b-starting', '{\"status\":\"starting\"}'),
                    ('a-approval', '{\"status\":\"approval\"}'),
                    ('terminal-completed', '{\"status\":\"completed\"}'),
                    ('terminal-failed', '{\"status\":\"failed\"}'),
                    ('terminal-cancelled', '{\"status\":\"cancelled\"}'),
                    ('terminal-lost', '{\"status\":\"lost\"}'),
                    ('unknown', '{\"status\":\"future-status\"}'),
                    ('duplicate-invalid-first', '{\"status\":17,\"status\":\"running\"}'),
                    ('duplicate-invalid-last', '{\"status\":\"running\",\"status\":17}'),
                    ('duplicate-unknown-last', '{\"status\":\"running\",\"status\":\"future-status\"}'),
                    ('duplicate-escaped-key', '{\"status\":\"completed\",\"sta\\u0074us\":\"approval\"}'),
                    ('malformed', '{invalid json}');",
            )
            .expect("monitor fixture");
        studio_runtime_store::StartupRecoveryRepository::prepare_indexes(&connection)
            .expect("startup recovery indexes");
        let tx = connection.transaction().expect("fixture transaction");

        // Selection uses SQLite's JSON reader through the startup partial
        // index: a duplicated key resolves to its first literal occurrence.
        // No writer produces duplicate keys; malformed rows stay unselected.
        assert_eq!(
            active_monitor_ids(&tx).expect("active monitor selection"),
            vec![
                "a-approval",
                "b-starting",
                "duplicate-invalid-last",
                "duplicate-unknown-last",
                "z-running"
            ]
        );
    }
}
