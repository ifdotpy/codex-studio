//! Isolation of one agent's invalid stored data from its account worker.
//!
//! A journal frame, a provider reply or a turn preparation (with the durable
//! effect that started it) of one agent can fail on a store invariant: a
//! record that does not decode, or a receipt that conflicts with the saved
//! one. Such a failure belongs to the agent the record names. Its part of the
//! transaction rolls back to a savepoint, the agent gets a visible durable
//! hold, and the account worker keeps serving every other agent. The frame or
//! effect is settled, so it neither blocks the account journal nor runs
//! again. Storage failures (SQLite, I/O, writer or commit outcome), and data
//! errors that name no record or agent, stay fatal for the worker.
use super::*;
use rusqlite::OptionalExtension;
use studio_runtime_store::{AgentStatus, CachedSql, EventWriteRepository};

/// Savepoint around one agent's part of an account transaction.
const AGENT_SCOPE_SAVEPOINT: &str = "account_worker_agent_scope";
/// Savepoint around the hold write itself.
const AGENT_HOLD_SAVEPOINT: &str = "account_worker_agent_hold";
/// Savepoint around returning an unsubmitted claim's input to the queue.
const AGENT_REQUEUE_SAVEPOINT: &str = "account_worker_agent_requeue";

/// Where the failing data was applied; part of the visible hold text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FaultStage {
    JournalFrame,
    ProviderReply,
    TurnPreparation,
}
impl FaultStage {
    fn label(self) -> &'static str {
        match self {
            Self::JournalFrame => "a provider event",
            Self::ProviderReply => "a provider reply",
            Self::TurnPreparation => "turn preparation",
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
    /// Log a fault when its hold is new, and otherwise once per agent and
    /// worker (`reported`). A parked frame or effect does not run again, so a
    /// held agent's later frames do not log in a loop.
    pub(super) fn report(&self, account: &str, reported: &mut BTreeSet<String>) {
        if reported.insert(self.agent_id.clone()) || self.hold == HoldResult::Placed {
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

/// The stored record (scope, id) that a data error names, or `None` for any
/// other error: SQLite, I/O, writer, commit outcome, session or policy, and a
/// decode error that names no record, since nothing ties it to one agent.
pub(super) fn data_fault(error: &WorkerError) -> Option<(String, String)> {
    let named = match error {
        WorkerError::Store(error)
        | WorkerError::Scheduler(scheduler::SchedulerError::Store(error))
        | WorkerError::Notification(
            crate::notification_dispatch::NotificationDispatchError::Store(error),
        ) => match error {
            StoreError::InvalidSyncObject { scope, id }
            | StoreError::InvalidSyncObjectField { scope, id, .. } => (scope.clone(), id.clone()),
            StoreError::InvalidStoredRecord { object, id } => ((*object).to_owned(), id.clone()),
            StoreError::InvalidStoredObject { table, id } => ((*table).to_owned(), id.clone()),
            _ => return None,
        },
        WorkerError::Notification(
            crate::notification_dispatch::NotificationDispatchError::InvalidStoredObject {
                table,
                id,
            },
        ) => (table.clone(), id.clone()),
        _ => return None,
    };
    (!named.1.is_empty()).then_some(named)
}

/// Run `apply` in a savepoint. The outer error is a savepoint failure; the
/// inner one is `apply`'s error after its writes were rolled back.
pub(super) fn in_savepoint<T>(
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
            // A failed rollback leaves the transaction state unknown: return
            // the first error as fatal, so nothing more is written in it.
            if tx
                .execute_batch(&format!(
                    "ROLLBACK TO SAVEPOINT {name}; RELEASE SAVEPOINT {name}"
                ))
                .is_err()
            {
                return Err(error);
            }
            Ok(Err(error))
        }
    }
}

/// Apply one agent's part of an account transaction. `subject` is the agent
/// that `apply` works for once it knows it. A data error rolls back `apply`,
/// runs `settle_failed` (the step's own settlement without its effects), and
/// holds the agent of the failing record (see [`fault_agent`]). Every other
/// error, and one that names no agent, is returned unchanged.
pub(super) fn isolate<T>(
    tx: &studio_runtime_store::Transaction<'_>,
    server_id: Option<&str>,
    stage: FaultStage,
    subject: &mut Option<String>,
    apply: impl FnOnce(&mut Option<String>) -> Result<T, WorkerError>,
    settle_failed: impl FnOnce() -> Result<(), WorkerError>,
) -> Result<Result<T, AgentFault>, WorkerError> {
    let error = match in_savepoint(tx, AGENT_SCOPE_SAVEPOINT, || apply(subject))? {
        Ok(value) => return Ok(Ok(value)),
        Err(error) => error,
    };
    let Some((scope, id)) = data_fault(&error) else {
        return Err(error);
    };
    let Some(agent_id) = fault_agent(tx, &scope, &id, subject.as_deref())? else {
        // Data that names no agent is account-scoped: keep it fatal.
        return Err(error);
    };
    settle_failed()?;
    let detail = error.to_string();
    let hold = hold_agent(tx, server_id, &agent_id, stage, &detail)?;
    Ok(Err(AgentFault {
        agent_id,
        stage,
        detail,
        hold,
    }))
}

/// The agent to hold for a failing record: the agent the record belongs to,
/// else `subject` when the record is one of its own (its row, an attempt,
/// its turn preparation or one of its request receipts). A record of the
/// account (a journal or publication receipt, the workspace) names no agent.
fn fault_agent(
    tx: &studio_runtime_store::Transaction<'_>,
    scope: &str,
    id: &str,
    subject: Option<&str>,
) -> Result<Option<String>, WorkerError> {
    if let Some(agent) = record_agent(tx, scope, id)? {
        return Ok(Some(agent));
    }
    let own = matches!(
        scope,
        "runtime_agents" | "runtime_execution_attempts" | "turn_preparation"
    ) || (scope == "account_worker" && id.starts_with("account:"));
    Ok(subject.filter(|_| own).map(str::to_owned))
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
    if matches!(scope, "runtime_events" | "runtime_event_meta") {
        return Ok(tx
            .query_row_cached(
                "SELECT agent FROM runtime_events WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?);
    }
    // A run row names its agent. A turn outcome that finds no run reports
    // the agent id itself as `runtime_execution_runs/<agent id>`.
    if scope == "runtime_execution_runs"
        && let Some(agent) = tx
            .query_row_cached(
                "SELECT agent FROM runtime_execution_runs WHERE id=?1",
                [id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
    {
        return Ok(Some(agent));
    }
    // `runtime_agents/<id>`, the worker's own `invalid(<agent id>)`, and a
    // turn outcome without a run.
    if matches!(
        scope,
        "runtime_agents" | "account_worker" | "runtime_execution_runs"
    ) {
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
/// agent that is already held or paused keeps its state and first cause. An
/// unsubmitted claim first returns its input to the queue: that input never
/// reached the provider, and a reserved event would block every later claim.
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
            Ok(Some(agent)) => {
                if agent.status == AgentStatus::Paused
                    || studio_runtime_store::truthy(agent.extra.get("nativeFailureHold"))
                    || agent
                        .extra
                        .get("startOutcomeHold")
                        .is_some_and(|hold| !hold.is_null())
                {
                    return Ok(HoldResult::AlreadyHeld);
                }
                // A thread request still in flight releases the input itself,
                // with its reply or its unknown outcome.
                if let Some(identity) = unsubmitted_attempt(&agent)
                    && !store::has_pending_thread_start(tx, agent_id)?
                {
                    match in_savepoint(tx, AGENT_REQUEUE_SAVEPOINT, || {
                        Ok(SchedulerRepository::requeue_unsubmitted_attempt(
                            tx, &identity,
                        )?)
                    })? {
                        Ok(_) => {}
                        // The attempt itself is the invalid data: its input
                        // stays reserved with the held agent.
                        Err(error) if data_fault(&error).is_some() => {}
                        Err(error) => return Err(error),
                    }
                }
                let Some(mut agent) = AgentRepository::get(tx, &AgentId::from(agent_id))? else {
                    return Ok(HoldResult::Missing);
                };
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

/// The current unsubmitted start attempt of `agent`, if it has one.
fn unsubmitted_attempt(
    agent: &studio_runtime_store::AgentRecord,
) -> Option<studio_runtime_store::TurnAttemptIdentity> {
    let attempt = agent.extra.get("startAttempt")?;
    (attempt["submitted"] == false).then(|| studio_runtime_store::TurnAttemptIdentity {
        agent_id: agent.id.0.clone(),
        epoch: attempt["epoch"].as_i64().unwrap_or(agent.epoch),
        attempt_id: attempt["id"].as_str().unwrap_or_default().to_owned(),
    })
}

/// Hold the agent of a failed turn preparation in its own transaction (see
/// [`fault_agent`]); `None` when the failing record names no agent.
/// `record` is the [`data_fault`] of the error that `detail` describes.
pub(super) async fn hold_after(
    store: &RuntimeStore,
    subject: String,
    stage: FaultStage,
    record: (String, String),
    detail: String,
) -> Result<Option<AgentFault>, WorkerError> {
    let server_id = store.local_server_id()?;
    let hold_detail = detail.clone();
    let held = scheduler::lifecycle_transaction_with(store, move |tx| {
        let (scope, id) = record;
        let Some(agent_id) = fault_agent(tx, &scope, &id, Some(&subject))? else {
            return Ok(None);
        };
        let hold = hold_agent(tx, server_id.as_deref(), &agent_id, stage, &hold_detail)?;
        Ok::<_, WorkerError>(Some((agent_id, hold)))
    })
    .await?;
    Ok(held.map(|(agent_id, hold)| AgentFault {
        agent_id,
        stage,
        detail,
        hold,
    }))
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
