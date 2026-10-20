//! Isolation of one agent's invalid stored data from its account worker.
//!
//! A journal frame, a provider reply, a turn preparation or a durable effect
//! of one agent can fail on a store invariant: a record that does not decode,
//! or a receipt that conflicts with the saved one. Such a failure belongs to
//! that agent. Its part of the transaction rolls back to a savepoint, the
//! agent gets a visible durable hold, and the account worker keeps serving
//! every other agent. The frame or effect is settled, so it neither blocks
//! the account journal nor runs again. Storage failures (SQLite, I/O, writer
//! or commit outcome) stay fatal for the worker.
use super::*;
use rusqlite::OptionalExtension;
use studio_runtime_store::{AgentStatus, CachedSql, EventWriteRepository};

/// Savepoint around one agent's part of an account transaction.
const AGENT_SCOPE_SAVEPOINT: &str = "account_worker_agent_scope";
/// Savepoint around the hold write itself.
const AGENT_HOLD_SAVEPOINT: &str = "account_worker_agent_hold";

/// Where the failing data was applied; part of the visible hold text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FaultStage {
    JournalFrame,
    ProviderReply,
    TurnPreparation,
    DurableEffect,
}
impl FaultStage {
    fn label(self) -> &'static str {
        match self {
            Self::JournalFrame => "a provider event",
            Self::ProviderReply => "a provider reply",
            Self::TurnPreparation => "turn preparation",
            Self::DurableEffect => "a queued follow-up step",
        }
    }
}

/// One agent-scoped failure that was rolled back and parked.
#[derive(Debug)]
pub(super) struct AgentFault {
    pub agent_id: String,
    pub stage: FaultStage,
    pub detail: String,
    pub hold: HoldResult,
}
impl AgentFault {
    /// Log a fault once: when its hold is new, or when no hold can be kept.
    /// A parked frame or effect does not run again, so this is not a loop.
    pub(super) fn report(&self, account: &str) {
        if self.hold != HoldResult::AlreadyHeld {
            eprintln!(
                "account worker {account} held agent {} after {} failed: {} ({:?})",
                self.agent_id,
                self.stage.label(),
                self.detail,
                self.hold,
            );
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HoldResult {
    /// This fault placed the hold.
    Placed,
    /// An earlier fault holds the agent; its first cause stays visible.
    AlreadyHeld,
    /// The agent row is gone; nothing remains to hold.
    Missing,
    /// The agent row cannot be read or written; the frame or effect is
    /// parked without a hold.
    Unavailable,
}

/// The record a data error names, or `None` for an error that is not about
/// stored data (SQLite, I/O, writer, commit outcome, session, policy).
pub(super) fn data_fault(error: &WorkerError) -> Option<(String, String)> {
    let store = match error {
        WorkerError::Store(error)
        | WorkerError::Scheduler(scheduler::SchedulerError::Store(error))
        | WorkerError::Notification(
            crate::notification_dispatch::NotificationDispatchError::Store(error),
        ) => error,
        WorkerError::Scheduler(scheduler::SchedulerError::InvalidAttemptTimestamp) => {
            return Some((String::new(), String::new()));
        }
        WorkerError::Notification(error) => {
            use crate::notification_dispatch::NotificationDispatchError as E;
            return match error {
                E::InvalidStoredObject { table, id } => Some((table.clone(), id.clone())),
                E::InvalidNotification
                | E::ConflictingDelivery
                | E::ConflictingInput
                | E::Json(_) => Some((String::new(), String::new())),
                _ => None,
            };
        }
        _ => return None,
    };
    match store {
        StoreError::InvalidSyncObject { scope, id }
        | StoreError::InvalidSyncObjectField { scope, id, .. } => Some((scope.clone(), id.clone())),
        StoreError::InvalidStoredRecord { object, id } => Some(((*object).to_owned(), id.clone())),
        StoreError::InvalidStoredObject { table, id } => Some(((*table).to_owned(), id.clone())),
        StoreError::Json(_) | StoreError::SchemaValidation(_) | StoreError::RecordContract(_) => {
            Some((String::new(), String::new()))
        }
        _ => None,
    }
}

/// Run `apply` in a savepoint. The outer error is a savepoint failure; the
/// inner one is `apply`'s error after its writes were rolled back.
fn in_savepoint<T>(
    tx: &studio_runtime_store::Transaction<'_>,
    name: &str,
    apply: impl FnOnce() -> Result<T, WorkerError>,
) -> Result<Result<T, WorkerError>, WorkerError> {
    tx.execute_batch(&format!("SAVEPOINT {name}"))?;
    match apply() {
        Ok(value) => {
            tx.execute_batch(&format!("RELEASE SAVEPOINT {name}"))?;
            Ok(Ok(value))
        }
        Err(error) => {
            tx.execute_batch(&format!(
                "ROLLBACK TO SAVEPOINT {name}; RELEASE SAVEPOINT {name}"
            ))?;
            Ok(Err(error))
        }
    }
}

/// Apply one agent's part of an account transaction. `subject` is the agent
/// that `apply` works for once it knows it. A data error of that agent (or of
/// the agent the failing record belongs to) rolls back `apply` and holds the
/// agent; every other error is returned unchanged.
pub(super) fn isolate<T>(
    tx: &studio_runtime_store::Transaction<'_>,
    server_id: Option<&str>,
    stage: FaultStage,
    subject: &mut Option<String>,
    apply: impl FnOnce(&mut Option<String>) -> Result<T, WorkerError>,
) -> Result<Result<T, AgentFault>, WorkerError> {
    let error = match in_savepoint(tx, AGENT_SCOPE_SAVEPOINT, || apply(subject))? {
        Ok(value) => return Ok(Ok(value)),
        Err(error) => error,
    };
    let Some((scope, id)) = data_fault(&error) else {
        return Err(error);
    };
    let agent_id = match subject.clone() {
        Some(agent) => agent,
        None => match record_agent(tx, &scope, &id)? {
            Some(agent) => agent,
            // Data that names no agent is account-scoped: keep it fatal.
            None => return Err(error),
        },
    };
    let detail = error.to_string();
    let hold = hold_agent(tx, server_id, &agent_id, stage, &detail)?;
    Ok(Err(AgentFault {
        agent_id,
        stage,
        detail,
        hold,
    }))
}

/// The agent a failing record belongs to, if the record names one.
fn record_agent(
    tx: &studio_runtime_store::Transaction<'_>,
    scope: &str,
    id: &str,
) -> Result<Option<String>, WorkerError> {
    if id.is_empty() {
        return Ok(None);
    }
    if scope == "runtime_execution_attempts" {
        return Ok(tx
            .query_row_cached(
                "SELECT r.agent FROM runtime_execution_attempts AS a \
                 JOIN runtime_execution_runs AS r ON r.id=a.run WHERE a.id=?1",
                [id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten());
    }
    // `runtime_agents/<id>`, and the worker's own `invalid(<agent id>)`.
    if matches!(scope, "runtime_agents" | "account_worker") {
        let exists: bool = tx.query_row_cached(
            "SELECT EXISTS(SELECT 1 FROM runtime_agents WHERE id=?1)",
            [id],
            |row| row.get(0),
        )?;
        return Ok(exists.then(|| id.to_owned()));
    }
    Ok(None)
}

/// The visible error of a held agent.
pub(super) fn hold_message(stage: FaultStage, detail: &str) -> String {
    format!(
        "Held after {} failed on invalid stored data: {detail}. \
         Other agents on this account keep running. Send a message to retry.",
        stage.label()
    )
}

/// Hold `agent_id` durably: failed, not woken, not in flight, with the cause
/// as its error. A manual message clears the hold (see `coordination`). An
/// agent that is already held keeps its first cause.
pub(super) fn hold_agent(
    tx: &studio_runtime_store::Transaction<'_>,
    server_id: Option<&str>,
    agent_id: &str,
    stage: FaultStage,
    detail: &str,
) -> Result<HoldResult, WorkerError> {
    let message = hold_message(stage, detail);
    let held = in_savepoint(tx, AGENT_HOLD_SAVEPOINT, || {
        match AgentRepository::get(tx, &AgentId::from(agent_id)) {
            Ok(None) => Ok(HoldResult::Missing),
            Ok(Some(mut agent)) => {
                if agent.status == AgentStatus::Failed
                    && agent.extra.get("nativeFailureHold") == Some(&Value::Bool(true))
                    && agent.extra.get("autoWake") == Some(&Value::Bool(false))
                {
                    return Ok(HoldResult::AlreadyHeld);
                }
                agent.status = AgentStatus::Failed;
                agent.extra.insert("nativeFailureHold".into(), json!(true));
                agent.extra.insert("autoWake".into(), json!(false));
                agent.extra.insert("inFlight".into(), json!(false));
                agent.extra.insert("error".into(), json!(message));
                AgentRepository::put(tx, &agent)?;
                Ok(HoldResult::Placed)
            }
            // The row itself does not decode: keep its bytes and hold it by
            // receipt, as worker recovery does.
            Err(StoreError::InvalidSyncObjectField {
                scope,
                id,
                pointer,
                reason,
            }) if scope == "runtime_agents" && id == agent_id => {
                let receipt_id = format!(
                    "{}{agent_id}",
                    studio_runtime_store::INVALID_AGENT_HOLD_RECEIPT_PREFIX
                );
                if EventWriteRepository::receipt(tx, &receipt_id)?.is_some_and(|receipt| {
                    receipt.signature == studio_runtime_store::INVALID_AGENT_HOLD_RECEIPT_SIGNATURE
                }) {
                    return Ok(HoldResult::AlreadyHeld);
                }
                let diagnostic = StoreError::InvalidSyncObjectField {
                    scope,
                    id,
                    pointer: pointer.clone(),
                    reason: reason.clone(),
                }
                .to_string();
                store::hold_invalid_agent_record_in(
                    tx,
                    server_id,
                    agent_id,
                    &pointer,
                    &reason,
                    &diagnostic,
                )?;
                Ok(HoldResult::Placed)
            }
            Err(error) => Err(error.into()),
        }
    })?;
    match held {
        Ok(held) => Ok(held),
        Err(error) if data_fault(&error).is_some() => Ok(HoldResult::Unavailable),
        Err(error) => Err(error),
    }
}

/// Hold `agent_id` in its own transaction, after a step that ran outside an
/// account transaction (a turn preparation or a durable effect) failed.
pub(super) async fn hold_after(
    store: &RuntimeStore,
    agent_id: String,
    stage: FaultStage,
    error: &WorkerError,
) -> Result<AgentFault, WorkerError> {
    let detail = error.to_string();
    let server_id = store.local_server_id()?;
    let hold_detail = detail.clone();
    let hold_agent_id = agent_id.clone();
    let hold = scheduler::lifecycle_transaction_with(store, move |tx| {
        hold_agent(
            tx,
            server_id.as_deref(),
            &hold_agent_id,
            stage,
            &hold_detail,
        )
    })
    .await?;
    Ok(AgentFault {
        agent_id,
        stage,
        detail,
        hold,
    })
}

/// The agent of a durable effect, when it has one.
pub(super) fn effect_agent(effect: &Effect) -> Option<&str> {
    match effect {
        Effect::Submit(intent) => Some(intent.agent_id.as_str()),
        Effect::Interrupt { agent, .. } | Effect::Wake(agent, _) => Some(agent.as_str()),
        Effect::Callback(..) => None,
    }
}

/// The thread of a provider `turn/completed` journal entry.
pub(super) fn completion_thread(entry: &JournalEntry) -> Option<&str> {
    let params = match &entry.payload {
        JournalPayload::Frame(Frame::Notification(frame))
            if frame.method.as_str() == "turn/completed" =>
        {
            frame.params.as_ref()?
        }
        JournalPayload::Frame(Frame::UnknownMethod(frame))
            if frame.method.as_str() == "turn/completed" && !frame.fields.contains_key("id") =>
        {
            frame.fields.get("params")?
        }
        _ => return None,
    };
    params.get("threadId").and_then(Value::as_str)
}
