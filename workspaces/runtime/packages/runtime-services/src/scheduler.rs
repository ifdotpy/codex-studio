//! Durable pending-event handoff between the scheduler and the future turn driver.

use serde_json::Value;
use studio_runtime_store::{AgentStatus, RuntimeStore, SchedulerRepository, StoreError};
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
    pub events: Vec<TurnEvent>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TurnStart {
    Accepted { turn_id: String },
    NotSubmitted,
    Unknown,
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

/// Consume one wake, claim a batch, then hand it off. Startup recovery is separate so a
/// concurrent wake cannot reclaim an in-process attempt between its claim and driver call.
/// Every store transaction is committed before the turn-driver port is called. The submitted
/// marker is durable first, so a crash across the port call can never make this batch replayable.
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

    let submitted = store.transaction(|transaction| {
        SchedulerRepository::mark_submitted(transaction, agent_id, epoch, attempt_id)
    })?;
    if !submitted {
        let event_ids = claimed
            .events
            .iter()
            .map(|event| event.id.0.clone())
            .collect::<Vec<_>>();
        store.transaction(|transaction| {
            SchedulerRepository::cancel_unsubmitted_claim(
                transaction,
                agent_id,
                epoch,
                attempt_id,
                &event_ids,
            )
        })?;
        return Ok(true);
    }

    let intent = TurnIntent {
        agent_id: agent_id.to_owned(),
        epoch,
        attempt_id: attempt_id.to_owned(),
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
        let may_continue = agent.epoch == epoch
            && agent.status == studio_runtime_store::AgentStatus::Starting
            && agent.extra.get("autoWake").and_then(Value::as_bool) == Some(true)
            && agent.extra.get("inFlight").and_then(Value::as_bool) == Some(true);
        if !may_continue {
            let (status, turn_id) = match outcome {
                TurnStart::NotSubmitted => ("cancelled", None),
                TurnStart::Accepted { ref turn_id } => ("uncertain", Some(turn_id.as_str())),
                TurnStart::Unknown => ("uncertain", None),
            };
            let mut changed = 0;
            for event in &claimed.events {
                changed += transaction.execute(
                    "UPDATE runtime_events SET status=?1,turn_id=?2 \
                     WHERE id=?3 AND agent=?4 AND epoch=?5 AND status IN ('reserved','dispatching')",
                    (status, turn_id, event.id.0.as_str(), agent_id, epoch),
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
            TurnStart::Accepted { ref turn_id } => {
                let mut changed = 0;
                for event in &claimed.events {
                    changed += transaction.execute(
                        "UPDATE runtime_events SET status='delivered',turn_id=?1,error=NULL \
                         WHERE id=?2 AND agent=?3 AND epoch=?4 AND status IN ('reserved','dispatching')",
                        (turn_id.as_str(), event.id.0.as_str(), agent_id, epoch),
                    )?;
                }
                if changed != claimed.events.len() {
                    return Err(StoreError::SchemaMismatch(
                        "scheduler event claim changed before turn acceptance".to_owned(),
                    ));
                }
                agent.status = studio_runtime_store::AgentStatus::Running;
                agent
                    .extra
                    .insert("turnId".to_owned(), Value::String(turn_id.clone()));
                if let Some(attempt) = agent
                    .extra
                    .get_mut("startAttempt")
                    .and_then(Value::as_object_mut)
                {
                    attempt.insert("submitted".to_owned(), Value::Bool(true));
                }
            }
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
                if let Some(attempt) = agent
                    .extra
                    .get_mut("startAttempt")
                    .and_then(Value::as_object_mut)
                {
                    attempt.insert("submitted".to_owned(), Value::Bool(true));
                }
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
