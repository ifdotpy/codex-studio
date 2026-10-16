//! Synchronous monitor consumer: ownership checks and all row writes share one transaction.
//! Native I/O is returned as an effect, and must run only after a successful commit.
//! Startup recovery for remote-owned monitors records its owner exit event locally as
//! `stored_only`; that row claims no delivery and is not resent. The remote owner is not notified
//! until `cross-server/remote-events` lands.

use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, EventWriteRepository, MonitorsRepository, QueueEvent,
    QueueEventId, RequestRecord, RuntimeStore, SchedulerRepository, SyncDocumentsRepository, Table,
    Transaction, monitor_runtime as repository,
};

use crate::event_write::{self, python_json};
use crate::monitor_policy::{
    self as policy, Decision, Effect, Input, InputOutcome, MonitorError, MonitorSnapshot, Operation,
};

pub const SESSION_DEPENDENCY: &str = "provider-session/monitor-commands";
pub const RULE_DEPENDENCY: &str = "rule-driver/monitor-finish";
pub const REMOTE_EVENTS_DEPENDENCY: &str = "cross-server/remote-events";
const STARTUP_RECEIPT_BATCH: usize = 64;
/// Maximum running monitor rows evaluated per keyset page.
pub(crate) const MONITOR_TICK_BATCH: usize = 64;
const RESTART_EVENT_ERROR: &str = "Server restarted before delivery acknowledgement";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StartupRecovery {
    pub receipts: usize,
    pub requests: usize,
    pub events: usize,
    pub monitors: usize,
}

/// Recover durable monitor results first, then expire requests and mark only
/// monitors with no surviving process as lost. All row and projection writes
/// share one transaction; result files are handled before that transaction.
pub async fn recover_on_startup(
    store: &RuntimeStore,
    now: f64,
) -> Result<StartupRecovery, MonitorError> {
    if !now.is_finite() || now < 0.0 {
        return Err(MonitorError::Refused("Invalid startup recovery clock"));
    }
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
                match error {
                    studio_runtime_store::StoreError::InvalidSyncObject { scope, id }
                        if scope == "runtime_agents" =>
                    {
                        eprintln!(
                            "startup scheduler reservation skipped row id={id}: invalid batch"
                        );
                    }
                    error => return Err(MonitorError::Store(error)),
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
    let mut agent_statement = tx
        .prepare_cached("SELECT id,record FROM runtime_agents ORDER BY id")
        .map_err(studio_runtime_store::StoreError::from)?;
    let agent_rows = agent_statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(studio_runtime_store::StoreError::from)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(studio_runtime_store::StoreError::from)?;
    drop(agent_statement);
    let mut agents = std::collections::BTreeMap::new();
    for (id, raw) in agent_rows {
        match serde_json::from_str::<Value>(&raw) {
            Ok(agent) if agent.is_object() => {
                agents.insert(id, agent);
            }
            _ => eprintln!("startup request recovery skipped owner row id={id}: invalid record"),
        }
    }
    let mut request_statement = tx
        .prepare_cached("SELECT id,record FROM runtime_requests ORDER BY id")
        .map_err(studio_runtime_store::StoreError::from)?;
    let request_rows = request_statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(studio_runtime_store::StoreError::from)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(studio_runtime_store::StoreError::from)?;
    drop(request_statement);
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
            Some("pending") if !retained_question(&value, &agents) => {
                value["status"] = json!("expired");
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

fn retained_question(request: &Value, agents: &std::collections::BTreeMap<String, Value>) -> bool {
    if request.get("method").and_then(Value::as_str) != Some("agent/asyncQuestion") {
        return false;
    }
    let Some(agent) = request
        .get("agent")
        .and_then(Value::as_str)
        .and_then(|id| agents.get(id))
    else {
        return false;
    };
    let same_epoch = agent.get("epoch") == request.get("epoch");
    if event_write::truthy(agent.get("deletedAt")) || !same_epoch {
        return false;
    }
    if event_write::truthy(agent.get("autoWake")) {
        return true;
    }
    agent.get("restartRecovery").is_some_and(|marker| {
        marker.get("stage").and_then(Value::as_str) == Some("pending")
            && event_write::truthy(marker.get("autoWake"))
            && marker.get("epoch") == agent.get("epoch")
    })
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
            let decision = policy::restart_loss(&snapshot)?;
            let remote = event_write::truthy(actor.extra.get("remoteWorker"))
                || event_write::truthy(actor.extra.get("remoteAnchor"));
            if remote {
                if event_write::truthy(actor.extra.get("remoteAnchor")) {
                    // Anchors have a cross-server event adapter; preserve that
                    // delivery while marking the local monitor as lost.
                    for effect in &decision.effects {
                        if let Effect::ExitEvent { wake } = effect {
                            exit_event(tx, &actor, &decision.record, *wake, now)?;
                        }
                    }
                    tx.execute(
                        "UPDATE runtime_events SET status='stored_only' WHERE id=?1 AND status='pending'",
                        [format!("monitor:{id}")],
                    )
                    .map_err(studio_runtime_store::StoreError::from)?;
                } else {
                    // Remote workers have no event delivery adapter. Preserve
                    // the lost result locally without putting it in a queue.
                    store_remote_exit_event(tx, &actor, &decision.record, now)?;
                }
                MonitorSnapshot::checked(
                    decision.record.clone(),
                    serde_json::to_value(&actor).map_err(studio_runtime_store::StoreError::from)?,
                )?;
                repository::put(tx, decision.record)?;
                crate::sync_policy::maintain_monitor_window(tx, &|_, _, view, deleted| {
                    deleted
                        || studio_runtime_schema::validate("MonitorEntityDto", view)
                            .unwrap_or(false)
                })?;
            } else {
                apply(tx, &snapshot, &actor, &decision, now)?;
            }
            Ok::<(), MonitorError>(())
        })();
        if let Err(error) = result {
            if startup_store_failure(&error) {
                return Err(error);
            }
            eprintln!("startup monitor recovery skipped row id={id}: {error}");
            continue;
        }
        recovered += 1;
    }
    Ok(recovered)
}

fn active_monitor_ids(tx: &Transaction<'_>) -> Result<Vec<String>, MonitorError> {
    let mut statement = tx
        .prepare_cached("SELECT id,record FROM runtime_monitors ORDER BY id")
        .map_err(studio_runtime_store::StoreError::from)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(studio_runtime_store::StoreError::from)?;
    let mut ids = Vec::new();
    for row in rows {
        let (id, raw) = row.map_err(studio_runtime_store::StoreError::from)?;
        match serde_json::from_str::<RecoverySelection>(&raw) {
            Ok(RecoverySelection {
                status:
                    Some(
                        policy::MonitorStatus::Running
                        | policy::MonitorStatus::Starting
                        | policy::MonitorStatus::Approval,
                    ),
            }) => ids.push(id),
            Ok(_) => {}
            Err(_) => eprintln!("startup monitor recovery skipped row id={id}: invalid record"),
        }
    }
    Ok(ids)
}

struct RecoverySelectionVisitor;

impl<'de> Visitor<'de> for RecoverySelectionVisitor {
    type Value = RecoverySelection;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a persisted monitor record")
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut status = None;
        while let Some(key) = map.next_key::<RecoveryField>()? {
            if matches!(key, RecoveryField::Status) {
                let raw = map.next_value::<&serde_json::value::RawValue>()?;
                status = serde_json::from_str::<policy::MonitorStatus>(raw.get()).ok();
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(RecoverySelection { status })
    }
}

#[derive(Deserialize)]
#[serde(field_identifier)]
enum RecoveryField {
    #[serde(rename = "status")]
    Status,
    #[serde(other)]
    Other,
}

struct RecoverySelection {
    status: Option<policy::MonitorStatus>,
}

impl<'de> Deserialize<'de> for RecoverySelection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(RecoverySelectionVisitor)
    }
}

fn store_remote_exit_event(
    tx: &Transaction<'_>,
    actor: &AgentRecord,
    monitor: &Value,
    now: f64,
) -> Result<(), MonitorError> {
    let id = monitor
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("?", "id"))?;
    let payload: serde_json::Map<String, Value> = [
        "id", "command", "status", "exitCode", "error", "tail", "log", "bytes",
    ]
    .into_iter()
    .map(|key| (key.into(), monitor.get(key).cloned().unwrap_or(Value::Null)))
    .collect();
    let event_id = format!("monitor:{id}");
    let text = python_json(&Value::Object(payload));
    let epoch = monitor
        .get("epoch")
        .and_then(Value::as_i64)
        .or_else(|| monitor.pointer("/operation/epoch").and_then(Value::as_i64))
        .unwrap_or(actor.epoch);

    EventWriteRepository::insert_event(
        tx,
        &QueueEvent {
            id: QueueEventId::from(event_id.as_str()),
            agent: actor.id.clone(),
            kind: "monitor_exit".into(),
            text: text.clone(),
            status: "stored_only".into(),
            created: now,
            epoch,
            turn_id: None,
            error: None,
        },
    )?;
    tx.execute(
        "UPDATE runtime_events SET text=?1,status='stored_only',created=?2,epoch=?3,error=NULL \
         WHERE id=?4 AND agent=?5 AND kind='monitor_exit' \
         AND status IN ('pending','cancelled','stored_only')",
        rusqlite::params![text, now, epoch, event_id, actor.id.0.as_str()],
    )
    .map_err(studio_runtime_store::StoreError::from)?;
    Ok(())
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
        _ => false,
    }
}

pub fn snapshot(
    transaction: &Transaction<'_>,
    id: &str,
) -> Result<(MonitorSnapshot, AgentRecord), MonitorError> {
    let row = MonitorsRepository::get(transaction, id)?
        .ok_or(MonitorError::Refused("Unknown command watch"))?;
    let agent_id = row
        .get("agent")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(id, "agent"))?;
    let actor = AgentRepository::get(transaction, &AgentId::from(agent_id))?
        .ok_or(MonitorError::Refused("Unknown agent"))?;
    let actor_value =
        serde_json::to_value(&actor).map_err(studio_runtime_store::StoreError::from)?;
    let snapshot = MonitorSnapshot::checked(row.value().clone(), actor_value)?;
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
                || actor.extra.get("autoWake") != Some(&json!(true))
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
                    let rule_snapshot = crate::rules_engine::RuleSnapshot::from_records(
                        rule.value().clone(),
                        actor.value(),
                    )
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
                } else {
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
            repository::put(tx, monitor)?;
            crate::sync_policy::maintain_monitor_window(tx, &|_, _, view, deleted| {
                deleted
                    || studio_runtime_schema::validate("MonitorEntityDto", view).unwrap_or(false)
            })?;
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
            let request_view =
                crate::sync_views::project_persisted_record("request", request.value()).ok_or(
                    MonitorError::Refused("Monitor approval projection is invalid"),
                )?;
            SyncDocumentsRepository::put_entity(tx, "request", &request_id, request_view, false)?;
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
            let decision = policy::begin_input(&snapshot, &operation, &attempt_id, &input, now)?;
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
    store.transaction_with(move |tx| {
        let mut statement = tx.prepare_cached(
            "SELECT id FROM runtime_monitors WHERE json_extract(record,'$.status')='running' AND id>?1 ORDER BY id LIMIT ?2",
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

            _ => (),
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
    MonitorSnapshot::checked(
        decision.record.clone(),
        serde_json::to_value(actor).map_err(studio_runtime_store::StoreError::from)?,
    )?;
    for effect in &decision.effects {
        match effect {
            Effect::RuleFinished => rules.finish(tx, before.record(), &decision.record, now)?,
            Effect::ExitEvent { wake } => exit_event(tx, actor, &decision.record, *wake, now)?,
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
            _ => (),
        }
    }
    if before.record() != &decision.record {
        repository::put(tx, decision.record.clone())?;
        crate::sync_policy::maintain_monitor_window(tx, &|_, _, view, deleted| {
            deleted || studio_runtime_schema::validate("MonitorEntityDto", view).unwrap_or(false)
        })?;
    }
    Ok(())
}

fn exit_event(
    tx: &Transaction<'_>,
    actor: &AgentRecord,
    monitor: &Value,
    wake: bool,
    now: f64,
) -> Result<(), MonitorError> {
    let id = monitor
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("?", "id"))?;
    let payload: serde_json::Map<String, Value> = [
        "id", "command", "status", "exitCode", "error", "tail", "log", "bytes",
    ]
    .into_iter()
    .map(|key| (key.into(), monitor.get(key).cloned().unwrap_or(Value::Null)))
    .collect();
    let payload = Value::Object(payload);
    let event_id = format!("monitor:{id}");
    let monitor_epoch = monitor
        .get("epoch")
        .and_then(Value::as_i64)
        .or_else(|| monitor.pointer("/operation/epoch").and_then(Value::as_i64));
    let can_wake = wake
        && monitor_epoch == Some(actor.epoch)
        && !event_write::truthy(actor.extra.get("deletedAt"));
    if let Some(existing) = repository::exit_event(tx, &event_id)? {
        let previous: Value = serde_json::from_str(&existing.text).unwrap_or(Value::Null);
        if previous.get("status").and_then(Value::as_str) != Some("lost")
            || monitor.get("status").and_then(Value::as_str) == Some("lost")
        {
            return Ok(());
        }
        if existing.status == "pending" {
            repository::replace_pending_exit(tx, &event_id, &python_json(&payload))?;
            return Ok(());
        }
        if existing.status == "cancelled"
            && existing.kind == "monitor_exit"
            && existing.agent == actor.id
            && Some(existing.epoch) == monitor_epoch
            && previous.get("id").and_then(Value::as_str) == Some(id)
            && previous.get("exitCode").is_none_or(Value::is_null)
            && (existing.error.as_deref()
                == Some("Native command reattached; discard the provisional disconnect notice.")
                || legacy_reattach_notice(actor, monitor, &existing, &previous))
            && monitor.pointer("/operation/agent").and_then(Value::as_str)
                == Some(actor.id.0.as_str())
            && monitor.pointer("/operation/epoch").and_then(Value::as_i64) == monitor_epoch
            && monitor
                .pointer("/operation/accountKey")
                .and_then(Value::as_str)
                == Some(
                    actor
                        .extra
                        .get("accountKey")
                        .and_then(Value::as_str)
                        .unwrap_or("default"),
                )
            && monitor
                .pointer("/operation/connectionId")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty())
        {
            let authorized = can_wake
                && actor.status != studio_runtime_store::AgentStatus::Paused
                && (actor.extra.get("autoWake").and_then(Value::as_bool) == Some(true)
                    || event_write::recovery_pending(actor));
            repository::restore_cancelled_notice(
                tx,
                &event_id,
                &python_json(&payload),
                authorized,
            )?;
            if authorized {
                event_write::enqueue_recovery_event(
                    tx,
                    actor,
                    event_id,
                    "monitor_exit",
                    python_json(&payload),
                    now,
                )?;
            }
            return Ok(());
        }
        let correction = json!({"monitorId": id, "corrects": event_id, "message": "This exact result corrects the earlier unknown monitor outcome. Do not repeat the command.", "result": payload});
        return enqueue(
            tx,
            actor,
            &format!("monitor-correction:{id}"),
            "monitor_correction",
            &correction,
            (can_wake, monitor_epoch),
            now,
        );
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

// Python's old restore path did not stamp cancelled notices. Exact restart/turn
// evidence proves these identities too; unrelated cancelled events stay cancelled.
fn legacy_reattach_notice(
    actor: &studio_runtime_store::AgentRecord,
    monitor: &Value,
    event: &studio_runtime_store::QueueEvent,
    previous: &Value,
) -> bool {
    let created = event.created;
    if event.error.is_some()
        || event.turn_id.is_some()
        || previous["error"] != "Server restarted. Command outcome unknown; not rerun."
        || !created.is_finite()
        || !monitor["created"]
            .as_f64()
            .is_some_and(|at| at.is_finite() && at > 0.0 && at <= created)
    {
        return false;
    }
    let epoch = &monitor["epoch"];
    let default_account = json!("default");
    let account = actor.extra.get("accountKey").unwrap_or(&default_account);
    let scoped_account = |receipt: &Value| {
        receipt.get("epoch") == Some(epoch)
            && receipt.get("accountKey").unwrap_or(&default_account) == account
    };
    let recent = |at: &Value| {
        at.as_f64()
            .is_some_and(|at| at.is_finite() && at >= created)
    };
    let receipt = &monitor["reattachRecovery"];
    if receipt["status"] == "running"
        && scoped_account(receipt)
        && recent(&receipt["cancelledNoticeLostAt"])
    {
        return true;
    }
    if monitor["status"] == "lost" {
        return false;
    }
    let scoped_turn = |receipt: &Value, at: &Value| {
        scoped_account(receipt)
            && recent(at)
            && actor
                .extra
                .get("threadId")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            && receipt.get("threadId") == actor.extra.get("threadId")
            && receipt["turnId"].as_str().is_some_and(|s| !s.is_empty())
    };
    let restore = actor.extra.get("supervisorRestore").unwrap_or(&Value::Null);
    let restart = actor.extra.get("restartRecovery").unwrap_or(&Value::Null);
    (restore["status"] == "restored"
        && restore["reason"] == "live_handle_resumed"
        && scoped_turn(restore, &restore["at"]))
        || (matches!(restart["stage"].as_str(), Some("reattached" | "finished"))
            && event_write::truthy(restart.get("autoWake"))
            && scoped_turn(restart, &restart["reattachedAt"]))
}

#[derive(Clone)]
pub struct PreparedStart {
    pub monitor: Value,
    pub actor: Value,
    pub operation: Operation,
    pub params: Value,
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
            if snapshot.record() != &prepared.monitor
                || serde_json::to_value(&actor).map_err(studio_runtime_store::StoreError::from)?
                    != prepared.actor
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

pub async fn reattach(
    store: &RuntimeStore,
    id: &str,
    proof: &policy::ReattachProof,
    now: f64,
) -> Result<(), MonitorError> {
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
            let decision = policy::reattach(&snapshot, &proof)?;
            apply(tx, &snapshot, &actor, &decision, now)?;
            repository::cancel_lost_notice(tx, &id)?;
            Ok(())
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
                Some(previous) if policy::same_request(previous.value(), &record) => {
                    Ok(Some(previous.value().clone()))
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
        if !policy::same_request(previous.value(), &record) {
            return Err(MonitorError::Refused(
                "This monitor request id has different content",
            ));
        }
        return Ok(previous.value().clone());
    }
    if actor.epoch != epoch || actor.extra.get("autoWake") != Some(&json!(true)) {
        return Err(MonitorError::Refused("The agent turn was stopped"));
    }
    workspace.check(tx, &actor)?;
    if repository::active_count(tx)? >= 64 {
        return Err(MonitorError::Refused("Maximum 64 active command watches"));
    }
    repository::put(tx, record.clone())?;
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
    crate::sync_policy::maintain_monitor_window(tx, &|_, _, view, deleted| {
        deleted || studio_runtime_schema::validate("MonitorEntityDto", view).unwrap_or(false)
    })?;
    Ok(record)
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
    let record = MonitorsRepository::get(tx, monitor_id)?
        .ok_or(MonitorError::Refused("Unknown command watch"))?;
    let mut value = record.value().clone();
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
    repository::put(tx, value.clone())?;
    crate::sync_policy::maintain_monitor_window(tx, &|_, _, view, deleted| {
        deleted || studio_runtime_schema::validate("MonitorEntityDto", view).unwrap_or(false)
    })?;
    Ok(value)
}

#[cfg(test)]
mod recovery_selection_tests {
    use super::active_monitor_ids;
    use rusqlite::Connection;

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
        let tx = connection.transaction().expect("fixture transaction");

        assert_eq!(
            active_monitor_ids(&tx).expect("active monitor selection"),
            vec![
                "a-approval",
                "b-starting",
                "duplicate-escaped-key",
                "duplicate-invalid-first",
                "z-running"
            ]
        );
    }
}
