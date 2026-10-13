//! Synchronous monitor consumer: ownership checks and all row writes share one transaction.
//! Native I/O is returned as an effect, and must run only after a successful commit.

use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, EventWriteRepository, MonitorsRepository, QueueEvent,
    QueueEventId, RuntimeStore, SyncDocumentsRepository, Table, Transaction,
    monitor_runtime as repository,
};

use crate::event_write::{self, python_json};
use crate::monitor_policy::{
    self as policy, Decision, Effect, Input, InputOutcome, MonitorError, MonitorSnapshot, Operation,
};

pub const SESSION_DEPENDENCY: &str = "provider-session/monitor-commands";
pub const RULE_DEPENDENCY: &str = "rule-driver/monitor-finish";

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

pub fn cancel(
    store: &RuntimeStore,
    id: &str,
    owner: Option<&str>,
    now: f64,
) -> Result<Value, MonitorError> {
    store.transaction_with(|tx| cancel_in_transaction(tx, id, owner, now))
}

pub fn cancel_in_transaction(
    tx: &Transaction<'_>,
    id: &str,
    owner: Option<&str>,
    now: f64,
) -> Result<Value, MonitorError> {
    let (snapshot, actor) = snapshot(tx, id)?;
    let decision = policy::cancel(&snapshot, owner, now)?;
    refuse_unavailable(&decision)?;
    apply(tx, &snapshot, &actor, &decision, now)?;
    Ok(decision.response)
}

pub fn input_without_session(
    store: &RuntimeStore,
    id: &str,
    input: &Input,
    now: f64,
) -> Result<Value, MonitorError> {
    store.transaction_with(|tx| {
        let (snapshot, _) = snapshot(tx, id)?;
        let operation = snapshot.operation().ok_or(MonitorError::Refused(
            "This interactive monitor is not active",
        ))?;
        policy::begin_input(&snapshot, operation, "unavailable", input, now)?;
        Err(MonitorError::Unavailable(SESSION_DEPENDENCY))
    })
}

/// The provider adapter calls once, after it has obtained its exact transport ticket.
/// A commit error is Unknown: do not send. An inputAttempt already present is never replayed.
pub fn reserve_input(
    store: &RuntimeStore,
    id: &str,
    operation: &Operation,
    attempt_id: &str,
    input: &Input,
    now: f64,
) -> Result<Effect, MonitorError> {
    store.transaction_with(|tx| {
        let (snapshot, actor) = snapshot(tx, id)?;
        let decision = policy::begin_input(&snapshot, operation, attempt_id, input, now)?;
        let effect = decision
            .effects
            .first()
            .cloned()
            .ok_or(MonitorError::Refused("Missing monitor input"))?;
        apply(tx, &snapshot, &actor, &decision, now)?;
        Ok(effect)
    })
}

pub fn settle_input(
    store: &RuntimeStore,
    id: &str,
    operation: &Operation,
    attempt_id: &str,
    outcome: InputOutcome,
    now: f64,
) -> Result<(), MonitorError> {
    store.transaction_with(|tx| {
        let (snapshot, actor) = snapshot(tx, id)?;
        let decision = policy::settle_input(&snapshot, operation, attempt_id, outcome, now)?;
        apply(tx, &snapshot, &actor, &decision, now)
    })
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

pub fn tick(store: &RuntimeStore, now: f64) -> Result<Vec<(String, Effect)>, MonitorError> {
    store.transaction_with(|tx| {
        let mut effects = Vec::new();
        for id in repository::running_ids(tx)? {
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
        Ok(effects)
    })
}

pub fn complete_probe(
    store: &RuntimeStore,
    id: &str,
    generation: i64,
    result: Value,
    now: f64,
) -> Result<(), MonitorError> {
    store.transaction_with(|tx| {
        let (snapshot, actor) = snapshot(tx, id)?;
        let decision = policy::probe_result(&snapshot, generation, result, now)?;
        apply(tx, &snapshot, &actor, &decision, now)
    })
}

fn refuse_unavailable(decision: &Decision) -> Result<(), MonitorError> {
    for effect in &decision.effects {
        match effect {
            Effect::Terminate | Effect::Native { .. } => {
                return Err(MonitorError::Unavailable(SESSION_DEPENDENCY));
            }
            Effect::RuleFinished => return Err(MonitorError::Unavailable(RULE_DEPENDENCY)),
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
    apply_with_rule(tx, before, actor, decision, now, &UnavailableRules)
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
            && existing.error.as_deref()
                == Some("Native command reattached; discard the provisional disconnect notice.")
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

pub struct PreparedStart {
    pub monitor: Value,
    pub actor: Value,
    pub operation: Operation,
    pub params: Value,
}

pub fn reserve_start(
    store: &RuntimeStore,
    id: &str,
    prepared: &PreparedStart,
    now: f64,
) -> Result<Effect, MonitorError> {
    store.transaction_with(|tx| {
        let (snapshot, actor) = snapshot(tx, id)?;
        if snapshot.record() != &prepared.monitor
            || serde_json::to_value(&actor).map_err(studio_runtime_store::StoreError::from)?
                != prepared.actor
        {
            return Err(MonitorError::Refused("Monitor changed during preparation"));
        }
        let decision = policy::start(&snapshot, &prepared.operation, prepared.params.clone(), now)?;
        let effect = decision
            .effects
            .first()
            .cloned()
            .ok_or(MonitorError::Refused("Missing monitor command"))?;
        apply(tx, &snapshot, &actor, &decision, now)?;
        Ok(effect)
    })
}

pub fn reattach(
    store: &RuntimeStore,
    id: &str,
    proof: &policy::ReattachProof,
    now: f64,
) -> Result<(), MonitorError> {
    store.transaction_with(|tx| {
        let (snapshot, actor) = snapshot(tx, id)?;
        let decision = policy::reattach(&snapshot, proof)?;
        apply(tx, &snapshot, &actor, &decision, now)?;
        repository::cancel_lost_notice(tx, id)?;
        Ok(())
    })
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

/// Implemented by workspace admission. This check performs only transactional
/// reads: preparation, filesystem and provider calls have already completed.
pub trait WorkspaceAdmission {
    fn check(&self, tx: &Transaction<'_>, actor: &AgentRecord) -> Result<(), MonitorError>;
}

pub fn create_in_transaction(
    tx: &Transaction<'_>,
    admission: &MonitorAdmission<'_>,
    workspace: &impl WorkspaceAdmission,
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
    // Workspace admission is supplied by the turn path; until installed this API can
    // persist approval requests only, which have no command side effect.
    if record["status"] == "starting" {
        return Err(MonitorError::Unavailable(
            "workspace-state/monitor-admission",
        ));
    }
    repository::put(tx, record.clone())?;
    let approval = studio_runtime_store::RequestRecord::from_value(json!({
        "id":format!("monitor-approval:{id}"), "method":"monitor/approve", "agent":actor_id,
        "params":{"monitorId":id,"command":record["command"],"cwd":record["cwd"]}, "status":"pending", "createdAt":now
    })).map_err(studio_runtime_store::StoreError::from)?;
    Table::new(tx).put(&approval)?;
    let view = crate::sync_views::project_persisted_record("request", approval.value())
        .ok_or_else(|| invalid(id, "request"))?;
    SyncDocumentsRepository::put_entity(tx, "request", approval.id(), view, false)?;
    crate::sync_policy::maintain_monitor_window(tx, &|_, _, view, deleted| {
        deleted || studio_runtime_schema::validate("MonitorEntityDto", view).unwrap_or(false)
    })?;
    Ok(record)
}
