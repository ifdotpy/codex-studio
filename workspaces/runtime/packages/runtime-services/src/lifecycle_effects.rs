//! Durable production adapters for stop and delete lifecycle effects.
//!
//! Interrupt intents are written with the agent lifecycle transaction. The
//! provider broker is called only after commit, and an ambiguous result remains
//! Unknown and is never retried by this adapter.
//!
//! Ported effects: Runtime.stop in `codex_runtime.py:10984-11021`,
//! `capacity_reset` in `codex_capacity_retry.py:72-80`, and
//! `usage_resume_cancel` in `codex_usage_resume.py:375-381`;
//! Runtime.delete_conversation and
//! `_settle_deleted_conversations` in `codex_runtime.py:5121-5250`;
//! `image_workspace_summary` / `child_stopped_event` in
//! `codex_runtime.py:7671-7745`; and
//! `release_failed_work` in `codex_work.py:1313-1355`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, ContinuationRepository, ContinuationTable, RuntimeStore,
    SchedulerRepository, Transaction,
};
use tokio::sync::Notify;

use crate::{
    account_session_runtime::{
        AccountSessionError, AccountSessionPort, ConnectionScope, ProviderRequest,
        ProviderResponse, RequestCertainty,
    },
    account_worker::retained::RequestActor,
    agent_lifecycle_runtime::{LifecycleEffect, LifecycleEffectKind, LifecyclePorts},
    agent_management::{Action, ManagementError},
    event_write,
    monitor_lifecycle::MonitorLifecyclePorts,
    sync_policy, work_write,
};

const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InterruptWriteResult {
    Accepted,
    Rejected,
    NotSent,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterruptRequest {
    pub ticket: String,
    pub agent: String,
    pub epoch: i64,
    pub thread: String,
    pub turn: String,
    pub scope: ConnectionScope,
}

/// Production LifecyclePorts constructor used by stop/delete callers.
pub struct ProductionLifecyclePorts {
    store: Arc<RuntimeStore>,
    monitors: MonitorLifecyclePorts,
    monitor_wake: Arc<Notify>,
    lifecycle_wake: Arc<Notify>,
    interrupt_broker: Arc<dyn AccountSessionPort>,
    prepared: Mutex<Vec<InterruptRequest>>,
}

impl ProductionLifecyclePorts {
    pub fn new(
        store: Arc<RuntimeStore>,
        monitor_wake: Arc<Notify>,
        lifecycle_wake: Arc<Notify>,
        interrupt_broker: Arc<dyn AccountSessionPort>,
    ) -> Self {
        let ports = Self {
            store,
            monitors: MonitorLifecyclePorts::new(monitor_wake.clone()),
            monitor_wake,
            lifecycle_wake,
            interrupt_broker,
            prepared: Mutex::new(Vec::new()),
        };
        // A tombstone and unsettled fields are the durable reconciliation
        // intent. Scan on construction so process restart cannot lose it.
        let _ = ports.reconcile_deleted_tombstones();
        ports
    }

    /// Run the production stop/delete adapter. Effect intents and lifecycle
    /// rows commit together; broker dispatch begins only after that commit.
    pub fn execute(&self, action: &Action, now: f64) -> Result<Value, ManagementError> {
        // Retry startup settlement here and surface storage failures before
        // making a new lifecycle decision.
        self.reconcile_deleted_tombstones()?;
        let reader = self.store.read()?;
        let rooms = crate::sync_entity_context::RoomDirectory::prepare(&reader)?;
        drop(reader);
        let result = self.store.transaction_with_error(|tx| {
            crate::agent_management_store::execute_with_ports(tx, action, now, &rooms, self)
        })?;
        self.committed()?;
        Ok(result)
    }

    fn stage_interrupt(
        &self,
        tx: &Transaction<'_>,
        effect: &LifecycleEffect,
    ) -> Result<(), ManagementError> {
        let before = effect.before();
        let agent = required_str(before, "id", "Invalid interrupt owner")?;
        let Some(thread) = before
            .get("threadId")
            .and_then(Value::as_str)
            .filter(|thread| !thread.is_empty())
        else {
            return Ok(());
        };
        let attempt = before.get("startAttempt").unwrap_or(&Value::Null);
        let is_review = before.get("nativeReview").and_then(Value::as_bool) == Some(true)
            && attempt.get("action").and_then(Value::as_str) == Some("review")
            && attempt.get("submitted").and_then(Value::as_bool) == Some(true)
            && truthy(before.get("inFlight"))
            && attempt.get("threadId").and_then(Value::as_str) == Some(thread)
            && before.get("turnId").is_none_or(Value::is_null);
        let current_turn = before
            .get("turnId")
            .and_then(Value::as_str)
            .filter(|turn| !turn.is_empty() && truthy(before.get("inFlight")));
        let Some(turn) = current_turn.or_else(|| is_review.then_some("")) else {
            // Python only interrupts an active native turn (or its special
            // submitted review). Preparation and idle stops need no provider IO.
            return Ok(());
        };
        if attempt.get("interrupt").is_some_and(|saved| {
            saved.get("threadId").and_then(Value::as_str) == Some(thread)
                && saved.get("turnId").and_then(Value::as_str) == Some(turn)
                && matches!(
                    saved.get("outcome").and_then(Value::as_str),
                    Some("unknown" | "accepted")
                )
        }) {
            // SchedulerRepository also refuses uncertain tickets. Treat the
            // existing intent as already handled locally so a repeated stop
            // can commit without resending it.
            return Ok(());
        }
        let generation = before
            .get("connectionGeneration")
            .and_then(Value::as_u64)
            .or_else(|| attempt.get("connectionGeneration").and_then(Value::as_u64))
            .ok_or(ManagementError::Unavailable(
                "account-worker/agent-interrupt",
            ))?;
        let account = attempt
            .get("accountKey")
            .and_then(Value::as_str)
            .or_else(|| before.get("accountKey").and_then(Value::as_str))
            .unwrap_or("default")
            .to_owned();
        let connection = attempt
            .get("connectionId")
            .and_then(Value::as_str)
            .or_else(|| before.get("connectionId").and_then(Value::as_str))
            .ok_or(ManagementError::Unavailable(
                "account-worker/agent-interrupt",
            ))?
            .to_owned();
        let stopped_epoch = effect
            .stopped()
            .get("epoch")
            .and_then(Value::as_i64)
            .ok_or(ManagementError::Invalid("Invalid stopped epoch"))?;
        let request_epoch = before
            .get("epoch")
            .and_then(Value::as_i64)
            .ok_or(ManagementError::Invalid("Invalid interrupt actor epoch"))?;
        let ticket = format!(
            "lifecycle-interrupt:{}",
            event_write::request_signature(&json!([
                agent,
                stopped_epoch,
                generation,
                attempt.get("id"),
                &account,
                &connection,
                thread,
                turn
            ]))
        );
        if !SchedulerRepository::prepare_interrupt(tx, agent, thread, turn, generation, &ticket)? {
            return Err(ManagementError::Unavailable(
                "account-worker/agent-interrupt",
            ));
        }
        let request = InterruptRequest {
            ticket,
            agent: agent.to_owned(),
            epoch: request_epoch,
            thread: thread.to_owned(),
            turn: turn.to_owned(),
            scope: ConnectionScope {
                account,
                connection,
                generation,
            },
        };
        self.prepared
            .lock()
            .map_err(|_| ManagementError::Invalid("Lifecycle interrupt queue unavailable"))?
            .push(request);
        Ok(())
    }

    fn dispatch_prepared(&self) -> Result<(), ManagementError> {
        let pending = {
            let mut queue = self
                .prepared
                .lock()
                .map_err(|_| ManagementError::Invalid("Lifecycle interrupt queue unavailable"))?;
            std::mem::take(&mut *queue)
        };
        let mut dispatched = HashSet::new();
        for request in pending {
            // A failed transaction can leave only its in-memory candidate.
            // If the exact action is then retried and committed, coalesce the
            // duplicate ticket so one durable identity is dispatched once.
            if !dispatched.insert(request.ticket.clone()) {
                continue;
            }
            // A rollback leaves no durable matching receipt, so discard the
            // in-memory candidate without contacting the provider.
            let persisted = {
                let reader = self.store.read()?;
                let Some(agent) =
                    AgentRepository::get(&reader, &AgentId::from(request.agent.as_str()))?
                else {
                    continue;
                };
                let interrupt =
                    &agent.extra.get("startAttempt").unwrap_or(&Value::Null)["interrupt"];
                interrupt["requestTicket"].as_str() == Some(request.ticket.as_str())
                    && interrupt["outcome"].as_str() == Some("unknown")
                    && agent.epoch == request.epoch.saturating_add(1)
            };
            if !persisted {
                continue;
            }
            let outcome = request_interrupt(self.interrupt_broker.as_ref(), &request);
            let (state, error) = match outcome {
                InterruptWriteResult::Accepted => ("accepted", None),
                InterruptWriteResult::Rejected => ("rejected", None),
                InterruptWriteResult::NotSent => ("unsent", None),
                InterruptWriteResult::Unknown => ("unknown", None),
            };
            self.store.transaction_with_error(|tx| {
                SchedulerRepository::settle_interrupt_result(
                    tx,
                    &request.ticket,
                    request.scope.generation,
                    state,
                    error,
                )?;
                Ok::<_, ManagementError>(())
            })?;
        }
        Ok(())
    }

    fn reconcile_deleted_tombstones(&self) -> Result<(), ManagementError> {
        let pending = self.store.read_transaction(|tx| {
            let mut pending = Vec::new();
            for (id, raw) in sync_policy::stored_records(tx, "runtime_agents")? {
                let record: Value =
                    serde_json::from_str(&raw).map_err(studio_runtime_store::StoreError::from)?;
                if needs_deleted_settlement(&record) {
                    pending.push(id);
                }
            }
            Ok::<_, studio_runtime_store::StoreError>(pending)
        })?;
        if pending.is_empty() {
            return Ok(());
        }
        self.store.transaction_with_error(|tx| {
            for id in &pending {
                finalize_deleted(tx, id)?;
            }
            Ok::<_, ManagementError>(())
        })?;
        Ok(())
    }
}

fn request_interrupt(
    port: &dyn AccountSessionPort,
    request: &InterruptRequest,
) -> InterruptWriteResult {
    let expected = request.scope.clone();
    let receipt = port.provider_request(ProviderRequest {
        account: request.scope.account.clone(),
        operation: request.ticket.clone(),
        actor: Some(RequestActor {
            agent: request.agent.clone(),
            epoch: request.epoch,
        }),
        expected_scope: Some(expected.clone()),
        method: studio_runtime_providers::Method::TurnInterrupt,
        params: json!({"threadId":request.thread,"turnId":request.turn}),
        deadline: Instant::now() + INTERRUPT_TIMEOUT,
    });
    match receipt {
        Ok(receipt)
            if receipt.operation == request.ticket && receipt.scope.account == expected.account =>
        {
            match receipt.certainty {
                RequestCertainty::Written
                    if receipt.scope == expected
                        && receipt.ticket.as_ref().is_some_and(|ticket| {
                        ticket.generation == expected.generation
                            && ticket.method == studio_runtime_providers::Method::TurnInterrupt
                            && matches!(&ticket.id, studio_runtime_providers::RequestId::String(id) if id == &request.ticket)
                    }) => match receipt.result {
                        Some(ProviderResponse::Reply(_)) => InterruptWriteResult::Accepted,
                        Some(ProviderResponse::Rejected(_)) => InterruptWriteResult::Rejected,
                        None => InterruptWriteResult::Unknown,
                    },
                RequestCertainty::NotSent => InterruptWriteResult::NotSent,
                RequestCertainty::Written | RequestCertainty::Unknown => InterruptWriteResult::Unknown,
            }
        }
        Err(AccountSessionError::Requires(_)) | Err(AccountSessionError::Refused(_)) => {
            InterruptWriteResult::NotSent
        }
        Err(_) | Ok(_) => InterruptWriteResult::Unknown,
    }
}

impl LifecyclePorts for ProductionLifecyclePorts {
    fn stage(&self, tx: &Transaction<'_>, effect: &LifecycleEffect) -> Result<(), ManagementError> {
        match effect.kind() {
            LifecycleEffectKind::Interrupt => self.stage_interrupt(tx, effect),
            LifecycleEffectKind::CancelResume => cancel_resume(tx, effect),
            LifecycleEffectKind::CancelMonitors => self.monitors.stage(tx, effect),
            LifecycleEffectKind::NotifyParent => notify_parent(tx, effect),
            LifecycleEffectKind::ReleaseWork => release_work(tx, effect),
            LifecycleEffectKind::FinalizeDeleted => {
                // The decision writes the tombstone and still-active fields
                // in this transaction; post-commit/restart scans consume it.
                required_str(effect.before(), "id", "Invalid deleted agent")?;
                Ok(())
            }
        }
    }

    fn committed(&self) -> Result<(), ManagementError> {
        self.monitor_wake.notify_one();
        self.lifecycle_wake.notify_one();
        self.reconcile_deleted_tombstones()?;
        self.dispatch_prepared()?;
        self.reconcile_deleted_tombstones()
    }
}

fn cancel_resume(tx: &Transaction<'_>, effect: &LifecycleEffect) -> Result<(), ManagementError> {
    let before = effect.before();
    let agent_id = required_str(before, "id", "Invalid continuation owner")?;
    for (table, projection) in [
        (ContinuationTable::Usage, "usageResume"),
        (ContinuationTable::Capacity, "capacityRetry"),
    ] {
        let Some(record) = before.get(projection).filter(|record| record.is_object()) else {
            continue;
        };
        let id = required_str(record, "id", "Invalid continuation identity")?;
        let mut stored = ContinuationRepository::snapshot(tx, agent_id, table, Some(id))?
            .get("record")
            .cloned()
            .ok_or(ManagementError::Invalid("Invalid stored continuation"))?;
        let status = stored.get("status").and_then(Value::as_str).unwrap_or("");
        let should_cancel = if projection == "usageResume" {
            matches!(status, "scheduled" | "started")
        } else {
            true
        };
        if !should_cancel {
            continue;
        }
        stored["status"] = json!(if projection == "capacityRetry" {
            "finished"
        } else {
            "cancelled"
        });
        stored["dueAt"] = Value::Null;
        stored["reason"] = json!("The agent was stopped.");
        ContinuationRepository::put_record(tx, table, agent_id, &stored)?;
        if projection == "usageResume"
            && let Some(mut current) = AgentRepository::get(tx, &AgentId::from(agent_id))?
        {
            let mut updated = current
                .extra
                .get(projection)
                .cloned()
                .unwrap_or(Value::Null);
            if updated.get("id").and_then(Value::as_str) == Some(id) {
                updated["status"] = json!("cancelled");
                updated["dueAt"] = Value::Null;
                updated["reason"] = stored["reason"].clone();
                current.extra.insert(projection.to_owned(), updated);
                AgentRepository::put(tx, &current)?;
            }
        }
    }
    Ok(())
}

fn release_work(tx: &Transaction<'_>, effect: &LifecycleEffect) -> Result<(), ManagementError> {
    let mut owners = HashSet::new();
    for (_, raw) in sync_policy::stored_records(tx, "runtime_work")? {
        let work: Value =
            serde_json::from_str(&raw).map_err(studio_runtime_store::StoreError::from)?;
        if work
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| ["ready", "running", "blocked"].contains(&status))
            && let Some(owner) = work.get("owner").and_then(Value::as_str)
        {
            owners.insert(owner.to_owned());
        }
    }
    let mut agents = Vec::new();
    for (_, raw) in sync_policy::stored_records(tx, "runtime_agents")? {
        let value: Value =
            serde_json::from_str(&raw).map_err(studio_runtime_store::StoreError::from)?;
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            continue;
        };
        let eligible = value
            .get("parentId")
            .is_some_and(|parent| !parent.is_null())
            && (truthy(value.get("deletedAt"))
                || value.get("status").and_then(Value::as_str) == Some("failed"));
        if eligible && owners.contains(id) {
            agents.push(
                AgentRecord::from_value(value).map_err(studio_runtime_store::StoreError::from)?,
            );
        }
    }
    let _ = work_write::release_failed_work(tx, &agents, effect.at())
        .map_err(|_| ManagementError::Invalid("Unable to release deleted worker work"))?;
    Ok(())
}

fn notify_parent(tx: &Transaction<'_>, effect: &LifecycleEffect) -> Result<(), ManagementError> {
    let agent = effect.stopped();
    if agent.get("isLead").and_then(Value::as_bool) == Some(true)
        || agent.get("parentId").and_then(Value::as_str).is_none()
        || truthy(agent.get("deletedAt"))
        || truthy(agent.get("agentArchive"))
    {
        return Ok(());
    }
    let id = required_str(agent, "id", "Invalid stopped child")?;
    let parent_id = required_str(agent, "parentId", "Invalid stopped child parent")?;
    let parent = AgentRepository::get(tx, &AgentId::from(parent_id))?
        .ok_or(ManagementError::Invalid("Invalid stopped child parent"))?;
    let mut tasks = sync_policy::stored_records(tx, "runtime_work")?
        .into_iter()
        .filter_map(|(_, raw)| serde_json::from_str::<Value>(&raw).ok())
        .filter(|work| {
            work.get("owner").and_then(Value::as_str) == Some(id)
                && work.get("rootId").and_then(Value::as_str)
                    == agent.get("rootId").and_then(Value::as_str)
                && work
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| {
                        ["ready", "running", "blocked", "review"].contains(&status)
                    })
        })
        .collect::<Vec<_>>();
    tasks.sort_by(|left, right| {
        right
            .get("updated")
            .and_then(Value::as_f64)
            .unwrap_or_default()
            .total_cmp(
                &left
                    .get("updated")
                    .and_then(Value::as_f64)
                    .unwrap_or_default(),
            )
    });
    let tasks = tasks.into_iter().map(|work| {
        let results = work.get("results").and_then(Value::as_array);
        let current_result = results.and_then(|values| values.last());
        let decisions = work.get("decisions").and_then(Value::as_array);
        let latest = decisions.and_then(|values| values.last());
        let current_assignment = current_result
            .and_then(|result| result.get("agent").and_then(Value::as_str)) == Some(id);
        let undecided = current_assignment
            && work.get("status").and_then(Value::as_str) == Some("review")
            && latest.and_then(|decision| decision.get("resultId"))
                != current_result.and_then(|result| result.get("id"));
        json!({
            "task_id": work.get("id").cloned().unwrap_or(Value::Null),
            "status": work.get("status").cloned().unwrap_or(Value::Null),
            "current_result_id": current_result.and_then(|result| result.get("id")).cloned().unwrap_or(Value::Null),
            "latest_decision": latest.and_then(|decision| decision.get("decision")).cloned().unwrap_or(Value::Null),
            "result_submitted": undecided,
            "needs_resubmission": current_assignment
                && latest.and_then(|decision| decision.get("decision")).and_then(Value::as_str) == Some("reject")
                && latest.and_then(|decision| decision.get("resultId"))
                    == current_result.and_then(|result| result.get("id")),
        })
    }).collect::<Vec<_>>();
    let reason = agent.get("error").cloned().unwrap_or(Value::Null);
    let reason_text = reason
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| reason.to_string());
    let activity = agent.get("activity").and_then(Value::as_object);
    let last_activity = activity
        .and_then(|value| value.get("at"))
        .filter(|value| !value.is_null() && value.as_f64() != Some(0.0))
        .or_else(|| {
            agent
                .get("lastEvent")
                .filter(|value| !value.is_null() && value.as_f64() != Some(0.0))
        })
        .or_else(|| {
            agent
                .get("lastUpdated")
                .filter(|value| !value.is_null() && value.as_f64() != Some(0.0))
        })
        .or_else(|| agent.get("created"))
        .cloned()
        .unwrap_or(Value::Null);
    let mut payload = json!({
        "agent_id": id,
        "name": agent.get("name").cloned().unwrap_or(Value::Null),
        "status": agent.get("status").cloned().unwrap_or(json!("paused")),
        "reason": reason,
        "result": reason_text,
        "last_activity": last_activity,
        "task_id": tasks.first().and_then(|task| task.get("task_id")).cloned().unwrap_or(Value::Null),
        "result_submitted": tasks.iter().any(|task| task.get("result_submitted").and_then(Value::as_bool) == Some(true)),
        "tasks": tasks,
        "next_step": "send",
        "requested_by_lead": false,
        "cwd": agent.get("cwd").cloned().unwrap_or(Value::Null),
        "branch": agent.get("branch").cloned().unwrap_or(Value::Null),
    });
    payload["workspace"] = image_workspace_summary(agent);
    let stopped_turn = effect
        .before()
        .get("turnId")
        .and_then(Value::as_str)
        .filter(|turn| !turn.is_empty())
        .or_else(|| {
            effect
                .before()
                .pointer("/startAttempt/id")
                .and_then(Value::as_str)
        })
        .unwrap_or("stop");
    let event_id = format!(
        "child-stop:{id}:{}:stop:{stopped_turn}",
        effect.stopped()["epoch"].as_i64().unwrap_or_default()
    );
    event_write::enqueue_recovery_event(
        tx,
        &parent,
        event_id,
        "child_result",
        payload.to_string(),
        effect.at(),
    )
    .map_err(|error| {
        ManagementError::Invalid(match error {
            event_write::EventWriteError::RemoteEventsUnavailable => {
                "Remote child event unavailable"
            }
            _ => "Unable to write child stop event",
        })
    })?;
    Ok(())
}

fn image_workspace_summary(agent: &Value) -> Value {
    if agent.get("executionMode").and_then(Value::as_str) == Some("vm") {
        return json!({
            "workspace":"layr",
            "environment":"linux",
            "projectId":agent.get("layrProjectId").cloned().unwrap_or(Value::Null),
            "line":agent.get("layrLine").cloned().unwrap_or(Value::Null),
            "stateId":agent.get("layrStateId").cloned().unwrap_or(Value::Null),
            "path":agent.get("cwd").cloned().unwrap_or(Value::Null),
        });
    }
    let phase = agent.get("imageWorkspacePhase");
    let mode = agent
        .get("workspaceMode")
        .and_then(Value::as_str)
        .or_else(|| {
            if truthy(agent.get("imageWorkspace")) {
                Some("image")
            } else if truthy(agent.get("worktree")) {
                Some("worktree")
            } else {
                Some("shared")
            }
        })
        .unwrap_or("shared");
    let mut result = json!({"mode":mode,"path":agent.get("cwd").cloned().unwrap_or(Value::Null)});
    let image = truthy(agent.get("imageWorkspace"));
    let fallback = phase.and_then(Value::as_str) == Some("fallback");
    if !image && !fallback && mode != "image" {
        return result;
    }
    let ready = truthy(agent.get("imageWorkspaceReady"));
    let state = agent
        .get("imageWorkspaceBaseState")
        .filter(|value| !value.is_null())
        .cloned()
        .or_else(|| {
            if ready {
                Some(json!("ready"))
            } else if phase.and_then(Value::as_str) == Some("read_only") {
                Some(json!("building"))
            } else {
                phase.cloned()
            }
        })
        .unwrap_or(Value::Null);
    let taken_at = agent
        .get("imageWorkspaceCreatedAt")
        .and_then(Value::as_f64)
        .and_then(|created| chrono::DateTime::from_timestamp(created as i64, 0))
        .map(|created| created.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    result["source"] = agent
        .get("imageWorkspaceRepo")
        .cloned()
        .unwrap_or(Value::Null);
    result["state"] = state;
    result["takenAt"] = taken_at.map_or(Value::Null, |at| json!(at));
    result["includesUncommittedChanges"] = json!(matches!(mode, "image" | "shared"));
    if let Some(error) = agent.get("imageWorkspaceError") {
        result["error"] = error.clone();
    }
    result
}

fn finalize_deleted(tx: &Transaction<'_>, id: &str) -> Result<(), ManagementError> {
    let Some(mut agent) = AgentRepository::get(tx, &AgentId::from(id))? else {
        return Ok(());
    };
    if !truthy(agent.extra.get("deletedAt")) {
        return Ok(());
    }
    let agent_value =
        serde_json::to_value(&agent).map_err(studio_runtime_store::StoreError::from)?;
    if !needs_deleted_settlement(&agent_value) {
        return Ok(());
    }
    agent.status = studio_runtime_store::AgentStatus::Paused;
    agent.extra.insert("inFlight".to_owned(), json!(false));
    agent.extra.insert("turnId".to_owned(), Value::Null);
    agent.extra.insert("activity".to_owned(), Value::Null);
    agent.extra.insert("activeTools".to_owned(), json!([]));
    if let Some(attempt) = agent.extra.get_mut("startAttempt")
        && truthy(attempt.get("activeAtReservation"))
    {
        attempt["activeAtReservation"] = json!(false);
    }
    AgentRepository::put(tx, &agent)?;
    Ok(())
}

fn needs_deleted_settlement(agent: &Value) -> bool {
    truthy(agent.get("deletedAt"))
        && (truthy(agent.get("inFlight"))
            || truthy(agent.get("turnId"))
            || truthy(agent.pointer("/startAttempt/activeAtReservation"))
            || agent
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| ["running", "starting", "approval"].contains(&status)))
}

fn required_str<'a>(
    value: &'a Value,
    key: &str,
    error: &'static str,
) -> Result<&'a str, ManagementError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(ManagementError::Invalid(error))
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Null) | None => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_session_runtime::{NotSentReason, SessionReceipt};
    use studio_runtime_providers::{RequestId, RequestTicket};

    struct Broker {
        scope: Option<ConnectionScope>,
        certainty: RequestCertainty,
        response: Option<ProviderResponse>,
        observed: Mutex<Option<ProviderRequest>>,
    }

    impl AccountSessionPort for Broker {
        fn current_scope(&self, _: &str) -> Option<ConnectionScope> {
            self.scope.clone()
        }

        fn provider_request(
            &self,
            request: ProviderRequest,
        ) -> Result<SessionReceipt, AccountSessionError> {
            let certainty = if self.scope.is_none() {
                RequestCertainty::NotSent
            } else {
                self.certainty
            };
            let receipt = SessionReceipt {
                scope: self.scope.clone().unwrap_or_else(|| {
                    request
                        .expected_scope
                        .clone()
                        .expect("test broker request expected scope")
                }),
                operation: request.operation.clone(),
                ticket: Some(RequestTicket {
                    generation: request
                        .expected_scope
                        .as_ref()
                        .expect("test broker request expected scope")
                        .generation,
                    id: RequestId::String(request.operation.clone()),
                    method: request.method,
                }),
                certainty,
                result: self.response.clone(),
                not_sent_reason: (certainty == RequestCertainty::NotSent)
                    .then_some(NotSentReason::WorkerNotSent),
            };
            *self.observed.lock().expect("observed request lock") = Some(request);
            Ok(receipt)
        }
    }

    fn request() -> InterruptRequest {
        InterruptRequest {
            ticket: "lifecycle-interrupt:request".into(),
            agent: "agent".into(),
            epoch: 4,
            thread: "thread".into(),
            turn: "turn".into(),
            scope: ConnectionScope {
                account: "acct".into(),
                connection: "connection".into(),
                generation: 8,
            },
        }
    }

    #[test]
    fn interrupt_dispatch_binds_expected_scope_and_actor() {
        let request = request();
        let broker = Broker {
            scope: Some(request.scope.clone()),
            certainty: RequestCertainty::Written,
            response: Some(ProviderResponse::Reply(json!({"turnId":"turn"}))),
            observed: Mutex::new(None),
        };

        assert_eq!(
            request_interrupt(&broker, &request),
            InterruptWriteResult::Accepted
        );
        let observed = broker.observed.lock().expect("observed request lock");
        let observed = observed.as_ref().expect("broker records request");
        assert_eq!(observed.expected_scope.as_ref(), Some(&request.scope));
        assert_eq!(observed.operation, request.ticket);
        assert_eq!(
            observed.actor.as_ref().map(|actor| actor.agent.as_str()),
            Some("agent")
        );
        assert_eq!(observed.actor.as_ref().map(|actor| actor.epoch), Some(4));
        assert_eq!(
            observed.method,
            studio_runtime_providers::Method::TurnInterrupt
        );
        assert_eq!(
            observed.params,
            json!({"threadId":"thread","turnId":"turn"})
        );
    }

    #[test]
    fn interrupt_uses_broker_scope_guard_and_preserves_unknown() {
        let request = request();
        let broker = Broker {
            scope: None,
            certainty: RequestCertainty::Unknown,
            response: None,
            observed: Mutex::new(None),
        };
        assert_eq!(
            request_interrupt(&broker, &request),
            InterruptWriteResult::NotSent
        );
        let observed = broker.observed.lock().expect("observed request lock");
        assert_eq!(
            observed
                .as_ref()
                .and_then(|request| request.expected_scope.as_ref()),
            Some(&request.scope)
        );
        drop(observed);

        let broker = Broker {
            scope: Some(request.scope.clone()),
            certainty: RequestCertainty::Unknown,
            response: None,
            observed: Mutex::new(None),
        };
        assert_eq!(
            request_interrupt(&broker, &request),
            InterruptWriteResult::Unknown
        );
    }

    #[test]
    fn interrupt_rejection_is_written_not_unknown() {
        let request = request();
        let broker = Broker {
            scope: Some(request.scope.clone()),
            certainty: RequestCertainty::Written,
            response: Some(ProviderResponse::Rejected(json!({"code":"rejected"}))),
            observed: Mutex::new(None),
        };
        assert_eq!(
            request_interrupt(&broker, &request),
            InterruptWriteResult::Rejected
        );
    }
}
