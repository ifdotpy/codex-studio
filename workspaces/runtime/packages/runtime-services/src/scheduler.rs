//! Durable pending-event handoff between the scheduler and the future turn driver.

use serde_json::Value;
use studio_runtime_store::{
    AgentStatus, RuntimeStore, SchedulerRepository, StoreError, TurnAttemptIdentity,
};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq)]
pub struct TurnEvent {
    pub id: String,
    pub kind: String,
    pub text: String,
    pub created: f64,
    pub epoch: i64,
    pub error: Option<String>,
    pub metadata: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnIntent {
    pub agent_id: String,
    pub epoch: i64,
    pub attempt_id: String,
    pub account_key: String,
    pub provider: String,
    pub thread_id: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub events: Vec<TurnEvent>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TurnStart {
    Accepted { turn_id: String },
    Rejected { error: Value },
    NotSubmitted,
    Unknown,
    Blocked { reason: String },
}

#[derive(Clone, Debug, PartialEq)]
pub enum TurnOutcome {
    Completed,
    Failed { error: Option<Value> },
    Interrupted { error: Option<Value> },
}

/// Edge-owned wake signal adapter. `consume` must clear exactly one pending wake.
pub trait WakeSignal {
    fn consume(&mut self) -> bool;
}

/// The actual provider/process implementation is owned by the later turn-driver slice.
pub trait TurnDriver {
    fn start(&mut self, intent: TurnIntent) -> TurnStart;
}

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("scheduler attempt timestamp is not finite")]
    InvalidAttemptTimestamp,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Run once during scheduler startup to release only persisted claims proven unsubmitted.
pub fn recover_on_startup(store: &RuntimeStore) -> Result<usize, SchedulerError> {
    Ok(store.transaction(|transaction| {
        let unsubmitted = SchedulerRepository::recover_unsubmitted(transaction)?;
        let submitted = SchedulerRepository::recover_submitted(transaction)?;
        Ok(unsubmitted + submitted)
    })?)
}

/// Persist a provider start callback in its own transaction. Late callbacks
/// are harmless because the repository checks both attempt and epoch identity.
pub fn settle_started(
    store: &RuntimeStore,
    intent: &TurnIntent,
    turn_id: &str,
) -> Result<bool, SchedulerError> {
    Ok(store.transaction(|transaction| {
        SchedulerRepository::settle_started(
            transaction,
            &intent.agent_id,
            intent.epoch,
            &intent.attempt_id,
            turn_id,
        )
    })?)
}

/// Persist one terminal provider outcome in its own transaction.
pub fn settle_terminal(
    store: &RuntimeStore,
    intent: &TurnIntent,
    turn_id: &str,
    outcome: TurnOutcome,
) -> Result<bool, SchedulerError> {
    let (status, error) = match outcome {
        TurnOutcome::Completed => (AgentStatus::Completed, None),
        TurnOutcome::Failed { error } => (AgentStatus::Failed, error),
        TurnOutcome::Interrupted { error } => (AgentStatus::Interrupted, error),
    };
    Ok(store.transaction(|transaction| {
        SchedulerRepository::settle_terminal(
            transaction,
            &intent.agent_id,
            intent.epoch,
            &intent.attempt_id,
            turn_id,
            status,
            error,
        )
    })?)
}

pub fn settle_started_attempt(
    store: &RuntimeStore,
    identity: &TurnAttemptIdentity,
    turn_id: &str,
) -> Result<bool, SchedulerError> {
    Ok(store.transaction(|transaction| {
        SchedulerRepository::settle_started(
            transaction,
            &identity.agent_id,
            identity.epoch,
            &identity.attempt_id,
            turn_id,
        )
    })?)
}

pub fn settle_started_provider(
    store: &RuntimeStore,
    intent: &TurnIntent,
    request_id: &str,
    generation: u64,
    turn_id: &str,
) -> Result<bool, SchedulerError> {
    let Some(thread_id) = intent.thread_id.as_deref() else {
        return Ok(false);
    };
    Ok(store.transaction(|transaction| {
        SchedulerRepository::settle_started_provider(
            transaction,
            &intent.agent_id,
            intent.epoch,
            &intent.attempt_id,
            request_id,
            &intent.account_key,
            generation,
            thread_id,
            turn_id,
        )
    })?)
}

pub fn settle_start_rejected_provider(
    store: &RuntimeStore,
    intent: &TurnIntent,
    request_id: &str,
    generation: u64,
    error: Value,
) -> Result<bool, SchedulerError> {
    let Some(thread_id) = intent.thread_id.as_deref() else {
        return Ok(false);
    };
    Ok(store.transaction(|transaction| {
        SchedulerRepository::settle_start_rejected_provider(
            transaction,
            &intent.agent_id,
            intent.epoch,
            &intent.attempt_id,
            request_id,
            &intent.account_key,
            generation,
            thread_id,
            error,
        )
    })?)
}

pub fn settle_terminal_attempt(
    store: &RuntimeStore,
    identity: &TurnAttemptIdentity,
    turn_id: &str,
    outcome: TurnOutcome,
) -> Result<bool, SchedulerError> {
    let (status, error) = match outcome {
        TurnOutcome::Completed => (AgentStatus::Completed, None),
        TurnOutcome::Failed { error } => (AgentStatus::Failed, error),
        TurnOutcome::Interrupted { error } => (AgentStatus::Interrupted, error),
    };
    Ok(store.transaction(|transaction| {
        SchedulerRepository::settle_terminal(
            transaction,
            &identity.agent_id,
            identity.epoch,
            &identity.attempt_id,
            turn_id,
            status,
            error,
        )
    })?)
}

pub fn attempt_can_prepare(
    store: &RuntimeStore,
    intent: &TurnIntent,
) -> Result<bool, SchedulerError> {
    Ok(store.read().and_then(|connection| {
        SchedulerRepository::attempt_can_prepare(
            &connection,
            &intent.agent_id,
            intent.epoch,
            &intent.attempt_id,
        )
    })?)
}

pub fn mark_submitted_for_provider(
    store: &RuntimeStore,
    intent: &TurnIntent,
    request_id: &str,
    generation: u64,
) -> Result<bool, SchedulerError> {
    let Some(thread_id) = intent.thread_id.as_deref() else {
        return Ok(false);
    };
    Ok(store.transaction(|transaction| {
        SchedulerRepository::mark_submitted_identity(
            transaction,
            &intent.agent_id,
            intent.epoch,
            &intent.attempt_id,
            request_id,
            &intent.account_key,
            generation,
            thread_id,
        )
    })?)
}

/// Claim a batch and hand it to the driver. The provider actor owns the submission boundary:
/// it marks the attempt submitted only after attach, initialize, and resume have succeeded.
pub fn consume_wake(
    store: &RuntimeStore,
    wake: &mut impl WakeSignal,
    agent_id: &str,
    epoch: i64,
    attempt_id: &str,
    attempt_created: f64,
    driver: &mut impl TurnDriver,
) -> Result<bool, SchedulerError> {
    if !attempt_created.is_finite() {
        return Err(SchedulerError::InvalidAttemptTimestamp);
    }
    if !wake.consume() {
        return Ok(false);
    }

    let claimed = store.transaction(|transaction| {
        SchedulerRepository::claim(
            transaction,
            agent_id,
            epoch,
            attempt_id,
            attempt_created,
            32,
        )
    })?;
    let Some(claimed) = claimed else {
        return Ok(true);
    };

    let intent = TurnIntent {
        agent_id: agent_id.to_owned(),
        epoch,
        attempt_id: attempt_id.to_owned(),
        account_key: claimed
            .agent
            .extra
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            .to_owned(),
        provider: claimed
            .agent
            .extra
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("codex")
            .to_owned(),
        thread_id: claimed
            .agent
            .extra
            .get("threadId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        cwd: claimed
            .agent
            .extra
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_owned),
        model: claimed
            .agent
            .extra
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned),
        events: claimed
            .events
            .iter()
            .map(|event| TurnEvent {
                id: event.id.0.clone(),
                kind: event.kind.clone(),
                text: event.text.clone(),
                created: event.created,
                epoch: event.epoch,
                error: event.error.clone(),
                metadata: claimed
                    .metadata
                    .get(&event.id.0)
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
            })
            .collect(),
    };
    let outcome = driver.start(intent);
    // Acceptance has one authority: the account actor commits it before notifying this caller.
    if matches!(
        outcome,
        TurnStart::Accepted { .. } | TurnStart::Rejected { .. }
    ) {
        return Ok(true);
    }
    store.transaction(|transaction| {
        let Some(mut agent) = studio_runtime_store::AgentRepository::get(
            transaction,
            &studio_runtime_store::AgentId::from(agent_id),
        )?
        else {
            return Ok(());
        };
        if agent
            .extra
            .get("startAttempt")
            .and_then(Value::as_object)
            .and_then(|attempt| attempt.get("id"))
            .and_then(Value::as_str)
            != Some(attempt_id)
        {
            return Ok(());
        }
        if matches!(outcome, TurnStart::Unknown)
            && !agent.extra.get("startAttempt").and_then(Value::as_object).is_some_and(|attempt| {
                attempt.get("submitted").and_then(Value::as_bool) == Some(true)
                    && attempt.get("requestTicket").and_then(Value::as_str).is_some()
                    && attempt.get("accountKey").and_then(Value::as_str).is_some()
                    && attempt.get("connectionGeneration").and_then(Value::as_u64).is_some()
                    && attempt.get("threadId").and_then(Value::as_str).is_some()
            })
        {
            return Err(StoreError::SchemaMismatch(
                "provider returned an unknown outcome without a committed request identity".into(),
            ));
        }
        if matches!(outcome, TurnStart::Unknown)
            && agent.epoch == epoch
            && agent
                .extra
                .get("startAttempt")
                .and_then(Value::as_object)
                .is_some_and(|attempt| {
                    attempt.get("id").and_then(Value::as_str) == Some(attempt_id)
                        && attempt
                            .get("turnId")
                            .and_then(Value::as_str)
                            .is_some_and(|turn_id| !turn_id.is_empty())
                        || attempt
                            .get("executionOutcome")
                            .and_then(Value::as_str)
                            .is_some_and(|outcome| matches!(outcome, "failed" | "completed" | "interrupted"))
                })
        {
            return Ok(());
        }
        let may_continue = agent.epoch == epoch
            && agent.status == studio_runtime_store::AgentStatus::Starting
            && agent.extra.get("autoWake").and_then(Value::as_bool) == Some(true)
            && agent.extra.get("inFlight").and_then(Value::as_bool) == Some(true);
        if !may_continue {
            let status = match outcome {
                TurnStart::NotSubmitted | TurnStart::Blocked { .. } => "cancelled",
                TurnStart::Accepted { .. } => return Ok(()),
                TurnStart::Rejected { .. } => return Ok(()),
                TurnStart::Unknown => "uncertain",
            };
            let mut changed = 0;
            for event in &claimed.events {
                changed += transaction.execute(
                    "UPDATE runtime_events SET status=?1,turn_id=?2 \
                     WHERE id=?3 AND agent=?4 AND epoch=?5 AND status IN ('reserved','dispatching')",
                    (status, Option::<&str>::None, event.id.0.as_str(), agent_id, epoch),
                )?;
            }
            if changed != claimed.events.len() {
                return Err(StoreError::SchemaMismatch(
                    "scheduler event claim changed while its agent stopped".to_owned(),
                ));
            }
            return Ok(());
        }
        match outcome {
            TurnStart::Accepted { .. } => return Ok(()),
            TurnStart::Rejected { .. } => return Ok(()),
            TurnStart::NotSubmitted => {
                let mut changed = 0;
                for event in &claimed.events {
                    changed += transaction.execute(
                        "UPDATE runtime_events SET status='pending',turn_id=NULL,error=NULL \
                         WHERE id=?1 AND agent=?2 AND epoch=?3 AND status IN ('reserved','dispatching')",
                        (event.id.0.as_str(), agent_id, epoch),
                    )?;
                }
                if changed != claimed.events.len() {
                    return Err(StoreError::SchemaMismatch(
                        "scheduler event claim changed before safe retry".to_owned(),
                    ));
                }
                agent.status = studio_runtime_store::AgentStatus::Queued;
                agent
                    .extra
                    .insert("inFlight".to_owned(), Value::Bool(false));
                agent.extra.insert("turnId".to_owned(), Value::Null);
                agent.extra.remove("startAttempt");
            }
            TurnStart::Unknown => {
                let mut changed = 0;
                for event in &claimed.events {
                    changed += transaction.execute(
                        "UPDATE runtime_events SET status='uncertain' \
                         WHERE id=?1 AND agent=?2 AND epoch=?3 AND status IN ('reserved','dispatching')",
                        (event.id.0.as_str(), agent_id, epoch),
                    )?;
                }
                if changed != claimed.events.len() {
                    return Err(StoreError::SchemaMismatch(
                        "scheduler event claim changed before uncertain result".to_owned(),
                    ));
                }
                agent.status = studio_runtime_store::AgentStatus::Waiting;
            }
            TurnStart::Blocked { ref reason } => {
                let mut changed = 0;
                for event in &claimed.events {
                    changed += transaction.execute(
                        "UPDATE runtime_events SET status='pending',turn_id=NULL,error=NULL \
                         WHERE id=?1 AND agent=?2 AND epoch=?3 AND status IN ('reserved','dispatching')",
                        (event.id.0.as_str(), agent_id, epoch),
                    )?;
                }
                if changed != claimed.events.len() {
                    return Err(StoreError::SchemaMismatch(
                        "scheduler event claim changed before blocked outcome".to_owned(),
                    ));
                }
                agent.status = studio_runtime_store::AgentStatus::Waiting;
                agent
                    .extra
                    .insert("inFlight".to_owned(), Value::Bool(false));
                agent.extra.insert("turnId".to_owned(), Value::Null);
                agent
                    .extra
                    .insert("nativeFailureHold".to_owned(), Value::Bool(true));
                agent
                    .extra
                    .insert("error".to_owned(), Value::String(reason.clone()));
                agent.extra.insert(
                    "startOutcomeHold".to_owned(),
                    serde_json::json!({"stage":"held","attemptId":attempt_id,
                        "epoch":epoch,"reason":reason}),
                );
                agent.extra.remove("startAttempt");
            }
        }
        studio_runtime_store::AgentRepository::put(transaction, &agent)?;
        let sync_status = match agent.status {
            studio_runtime_store::AgentStatus::Running => "running",
            studio_runtime_store::AgentStatus::Queued => "queued",
            studio_runtime_store::AgentStatus::Waiting => "waiting",
            _ => return Err(StoreError::SchemaMismatch("invalid scheduler status".into())),
        };
        let in_flight = agent
            .extra
            .get("inFlight")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let turn_id = agent.extra.get("turnId").and_then(Value::as_str);
        studio_runtime_store::EventWriteRepository::set_agent_sync_lifecycle(
            transaction,
            agent_id,
            sync_status,
            in_flight,
            turn_id,
        )?;
        Ok(())
    })?;
    Ok(true)
}
