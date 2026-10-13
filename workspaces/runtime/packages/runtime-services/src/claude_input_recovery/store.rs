//! Account-worker entry points. The caller settles its journal cursor only after
//! the surrounding RuntimeStore transaction commits these effects.
use super::*;
use studio_runtime_store::{ClaudeRetryRepository, StoreError, StoreResult, Transaction};

pub fn recover_rejected_start(
    tx: &Transaction<'_>,
    agent_id: &str,
    attempt_id: &str,
    transport: TransportSnapshot,
    runtime_closed: bool,
    rejection: &Rejection,
) -> StoreResult<Option<RecoveryOutcome>> {
    let Some(rows) = ClaudeRetryRepository::load(tx, agent_id, attempt_id)? else {
        return Ok(None);
    };
    let attempt = rows
        .agent
        .get("startAttempt")
        .cloned()
        .unwrap_or(Value::Null);
    if attempt.is_null() {
        return Ok(None);
    }
    let work = match rejection {
        Rejection::Turn { turn } => {
            if let (Some(turn), Some(since)) = (
                turn.get("id").and_then(Value::as_str),
                attempt.get("created").and_then(Value::as_f64),
            ) {
                ClaudeRetryRepository::has_turn_work(tx, agent_id, turn, since)?
            } else {
                true
            }
        }
        Rejection::Rpc { .. } => false,
    };
    let snapshot = RecoverySnapshot::from_records(
        rows.agent,
        attempt,
        rows.attempt,
        rows.events,
        transport,
        runtime_closed,
        work,
    )
    .map_err(|_| invalid(agent_id))?;
    let Some(decision) = snapshot.recover(rejection) else {
        return Ok(None);
    };
    let events = decision
        .events
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()?;
    ClaudeRetryRepository::apply(tx, &decision.agent, &decision.attempt, &events)?;
    Ok(Some(decision.outcome))
}

pub fn retry_batch(
    tx: &Transaction<'_>,
    agent_id: &str,
    attempt_id: &str,
    transport: TransportSnapshot,
    runtime_closed: bool,
) -> StoreResult<Option<Vec<String>>> {
    let Some(rows) = ClaudeRetryRepository::load(tx, agent_id, attempt_id)? else {
        return Ok(None);
    };
    let later = later_user(tx, &rows.agent, &rows.attempt)?;
    let snapshot = RecoverySnapshot::from_records(
        rows.agent,
        rows.attempt.clone(),
        rows.attempt,
        rows.events,
        transport,
        runtime_closed,
        false,
    )
    .map_err(|_| invalid(agent_id))?;
    Ok(snapshot.retry_batch(later))
}

pub fn retry_input(
    tx: &Transaction<'_>,
    agent_id: &str,
    attempt_id: &str,
    transport: TransportSnapshot,
    runtime_closed: bool,
) -> StoreResult<Option<Value>> {
    let Some(rows) = ClaudeRetryRepository::load(tx, agent_id, attempt_id)? else {
        return Err(invalid(agent_id));
    };
    let item = rows
        .attempt
        .pointer("/claudeInputRequest/transcriptItemId")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(agent_id))?;
    let (transcript, fulltext) =
        ClaudeRetryRepository::transcript(tx, agent_id, item)?.ok_or_else(|| invalid(agent_id))?;
    let later = later_user(tx, &rows.agent, &rows.attempt)?;
    let snapshot = RecoverySnapshot::from_records(
        rows.agent,
        rows.attempt.clone(),
        rows.attempt,
        rows.events,
        transport,
        runtime_closed,
        false,
    )
    .map_err(|_| invalid(agent_id))?;
    snapshot
        .retry_input(later, &transcript, fulltext.as_deref())
        .map_err(|_| invalid(agent_id))
}
fn later_user(tx: &Transaction<'_>, agent: &Value, attempt: &Value) -> StoreResult<bool> {
    let Some(since) = attempt.get("created").and_then(Value::as_f64) else {
        return Ok(false);
    };
    let id = agent
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("agent"))?;
    let epoch = agent
        .get("epoch")
        .and_then(Value::as_i64)
        .ok_or_else(|| invalid(id))?;
    ClaudeRetryRepository::later_user(tx, id, epoch, since)
}
fn invalid(id: &str) -> StoreError {
    StoreError::InvalidStoredRecord {
        object: "claude retry",
        id: id.into(),
    }
}

/// Called during stop recovery, or before ordinary admission when a marker exists.
pub fn retire_stopped_retry(
    tx: &Transaction<'_>,
    agent_id: &str,
    attempt_id: &str,
    transport: TransportSnapshot,
    runtime_closed: bool,
) -> StoreResult<bool> {
    let Some(rows) = ClaudeRetryRepository::load(tx, agent_id, attempt_id)? else {
        return Ok(false);
    };
    let reservation = if let Some(id) = rows
        .agent
        .pointer("/startAttempt/id")
        .and_then(Value::as_str)
    {
        ClaudeRetryRepository::load(tx, agent_id, id)?.map(|row| row.attempt)
    } else {
        None
    };
    let snapshot = RecoverySnapshot::from_records(
        rows.agent,
        rows.attempt.clone(),
        rows.attempt.clone(),
        rows.events,
        transport,
        runtime_closed,
        false,
    )
    .map_err(|_| invalid(agent_id))?;
    let Some(decision) = snapshot.retire_stopped(reservation.as_ref()) else {
        return Ok(false);
    };
    let events = decision
        .events
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()?;
    ClaudeRetryRepository::apply(tx, &decision.agent, &rows.attempt, &events)?;
    if let Some(reservation) = decision.reservation {
        ClaudeRetryRepository::replace_attempt(tx, agent_id, &reservation)?;
    }
    Ok(true)
}
pub fn resume_held_retry(
    tx: &Transaction<'_>,
    agent_id: &str,
    attempt_id: &str,
    transport: TransportSnapshot,
    runtime_closed: bool,
) -> StoreResult<bool> {
    let Some(rows) = ClaudeRetryRepository::load(tx, agent_id, attempt_id)? else {
        return Ok(false);
    };
    let Some(item) = rows
        .attempt
        .pointer("/claudeInputRequest/transcriptItemId")
        .and_then(Value::as_str)
    else {
        return Ok(false);
    };
    let Some((transcript, fulltext)) = ClaudeRetryRepository::transcript(tx, agent_id, item)?
    else {
        return Ok(false);
    };
    let later = later_user(tx, &rows.agent, &rows.attempt)?;
    let snapshot = RecoverySnapshot::from_records(
        rows.agent,
        rows.attempt.clone(),
        rows.attempt,
        rows.events,
        transport,
        runtime_closed,
        false,
    )
    .map_err(|_| invalid(agent_id))?;
    let Some(agent) = snapshot.resume_held(later, &transcript, fulltext.as_deref()) else {
        return Ok(false);
    };
    studio_runtime_store::AgentRepository::put(
        tx,
        &studio_runtime_store::AgentRecord::from_value(agent)?,
    )?;
    Ok(true)
}
