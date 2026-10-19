//! Submission helpers for the account worker's existing transaction. The
//! Submitted transaction calls `submission::commit_start`
//! with the exact prepared wire params and request generation. No provider call or journal ACK belongs here.
use super::{TransportSnapshot, capture_input};
use serde_json::{Value, json};
use studio_runtime_store::{
    AgentRecord, AgentRepository, ClaudeRetryRecords, ClaudeRetryRepository, StoreError,
    StoreResult, Transaction,
};

pub mod submission;

/// Identity of the retained session that prepared the native request. The
/// generation must come from that request, not a newly resolved account session.
pub struct SubmittedBinding {
    pub transport: TransportSnapshot,
    pub generation: u64,
}

fn invalid(agent: &str) -> StoreError {
    StoreError::InvalidStoredRecord {
        object: "claude submission",
        id: agent.into(),
    }
}
fn reserved(
    tx: &Transaction<'_>,
    agent: &str,
    attempt: &str,
    transport: &TransportSnapshot,
) -> StoreResult<ClaudeRetryRecords> {
    let rows = ClaudeRetryRepository::load(tx, agent, attempt)?.ok_or_else(|| invalid(agent))?;
    let current = rows
        .agent
        .get("startAttempt")
        .ok_or_else(|| invalid(agent))?;
    if rows.agent.get("provider") != Some(&json!("claude"))
        || rows.agent.get("autoWake") != Some(&json!(true))
        || rows.agent.get("status") == Some(&json!("paused"))
        || [
            "deletedAt",
            "agentArchive",
            "nativeFailureHold",
            "accountTransferId",
            "workspaceOperation",
        ]
        .iter()
        .any(|key| crate::event_write::truthy(rows.agent.get(*key)))
        || crate::sync_views::native_thread_block(&rows.agent).is_some()
        || current.get("id").and_then(Value::as_str) != Some(attempt)
        || current.get("epoch") != rows.agent.get("epoch")
        || current.get("submitted") != Some(&json!(false))
        || rows.attempt.get("submitted") != Some(&json!(false))
        || super::RECEIPT_FIELDS
            .iter()
            .any(|key| current.get(*key) != rows.attempt.get(*key))
        || transport.closed
        || rows
            .agent
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            != transport.account_key
        || rows.events.is_empty()
    {
        return Err(invalid(agent));
    }
    for event in &rows.events {
        if event.get("agent").and_then(Value::as_str) != Some(agent)
            || event.get("epoch") != rows.agent.get("epoch")
            || !matches!(
                event.get("status").and_then(Value::as_str),
                Some("reserved" | "dispatching")
            )
            || event.get("turn_id").is_some_and(|v| !v.is_null())
        {
            return Err(invalid(agent));
        }
    }
    Ok(rows)
}

/// The prepared params must match the reserved attempt exactly.
/// Dynamic tools alone may refresh. Newer queued events cannot join this batch.
fn resolve(agent: &str, rows: &ClaudeRetryRecords, params: &Value) -> StoreResult<()> {
    let object = params.as_object().ok_or_else(|| invalid(agent))?;
    if object.get("threadId") != rows.agent.get("threadId")
        || object
            .get("threadId")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || object.get("clientUserMessageId")
            != rows.events.first().and_then(|event| event.get("id"))
        || !object.get("input").is_some_and(Value::is_array)
    {
        return Err(invalid(agent));
    }
    // Legacy Python retry state (an exact-retry attempt or a pending retry) has
    // no Rust retry path: such a record stays held and visible, never resent.
    if rows
        .agent
        .pointer("/startAttempt/claudeRetryOf")
        .is_some_and(|value| !value.is_null())
        || rows
            .agent
            .get("claudePreInputRetry")
            .is_some_and(|value| !value.is_null())
    {
        return Err(invalid(agent));
    }
    Ok(())
}

/// Freeze beside the worker's Submitted receipt, in its transaction. Roll back
/// the entire transaction if the worker's final ownership check subsequently fails.
/// Call only with the exact params that SessionControl::prepare validated.
pub fn freeze_start(
    tx: &Transaction<'_>,
    agent: &str,
    attempt: &str,
    transport: TransportSnapshot,
    params: &Value,
    text: &str,
) -> StoreResult<()> {
    let rows = reserved(tx, agent, attempt, &transport)?;
    freeze_loaded(tx, agent, rows, params, text)
}

/// `freeze_start` over rows the caller already loaded and validated with `reserved`.
fn freeze_loaded(
    tx: &Transaction<'_>,
    agent: &str,
    mut rows: ClaudeRetryRecords,
    params: &Value,
    text: &str,
) -> StoreResult<()> {
    resolve(agent, &rows, params)?;
    let frozen =
        capture_input(&rows.agent, &rows.events, params, text).map_err(|_| invalid(agent))?;
    rows.attempt["claudeInputRequest"] = frozen.clone();
    rows.agent["startAttempt"]["claudeInputRequest"] = frozen;
    if let Some(record) = rows.agent.as_object_mut() {
        record.remove("claudePreInputRetry");
    }
    ClaudeRetryRepository::replace_attempt(tx, agent, &rows.attempt)?;
    AgentRepository::put(tx, &AgentRecord::from_value(rows.agent)?)?;
    Ok(())
}

/// Call after the scheduler marks Submitted, before committing that same
/// transaction. Its generic submission path initializes transport fields to null;
/// Claude rejection proofs require the exact retained connection and supervisor.
pub fn bind_submitted(
    tx: &Transaction<'_>,
    agent: &str,
    attempt: &str,
    binding: SubmittedBinding,
) -> StoreResult<()> {
    let transport = binding.transport;
    let mut rows =
        ClaudeRetryRepository::load(tx, agent, attempt)?.ok_or_else(|| invalid(agent))?;
    let current = rows
        .agent
        .get("startAttempt")
        .ok_or_else(|| invalid(agent))?;
    if transport.closed
        || rows.agent.get("provider") != Some(&json!("claude"))
        || rows
            .agent
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            != transport.account_key
        || current.get("id").and_then(Value::as_str) != Some(attempt)
        || current.get("epoch") != rows.agent.get("epoch")
        || current.get("submitted") != Some(&json!(true))
        || current.get("connectionGeneration").and_then(Value::as_u64) != Some(binding.generation)
        || rows
            .attempt
            .get("connectionGeneration")
            .and_then(Value::as_u64)
            != Some(binding.generation)
        || current.get("accountKey").and_then(Value::as_str) != Some(transport.account_key.as_str())
        || current.get("threadId") != rows.agent.get("threadId")
        || current.get("claudeInputRequest").is_none_or(Value::is_null)
        || current
            .get("nativeOperationId")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || super::RECEIPT_FIELDS
            .iter()
            .any(|key| current.get(*key) != rows.attempt.get(*key))
        || current
            .get("connectionId")
            .filter(|v| !v.is_null())
            .is_some_and(|value| value != &transport.connection_id)
        || current
            .get("supervisorIdentity")
            .filter(|v| !v.is_null())
            .is_some_and(|value| Some(value) != transport.supervisor_identity.as_ref())
    {
        return Err(invalid(agent));
    }
    for (key, value) in [
        ("connectionId", json!(transport.connection_id)),
        (
            "supervisorIdentity",
            transport.supervisor_identity.unwrap_or(Value::Null),
        ),
    ] {
        rows.attempt[key] = value.clone();
        rows.agent["startAttempt"][key] = value;
    }
    ClaudeRetryRepository::replace_attempt(tx, agent, &rows.attempt)?;
    AgentRepository::put(tx, &AgentRecord::from_value(rows.agent)?)?;
    Ok(())
}
