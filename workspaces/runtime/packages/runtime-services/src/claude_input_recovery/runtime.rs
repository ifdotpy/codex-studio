//! Submission helpers for the account worker's existing transaction. Preparation
//! calls `start_input`; the Submitted transaction calls `freeze_start` again with
//! the exact prepared wire params. No provider call or journal ACK belongs here.
use super::{TransportSnapshot, capture_input, store};
use serde_json::{Value, json};
use studio_runtime_store::{
    AgentRecord, AgentRepository, ClaudeRetryRecords, ClaudeRetryRepository, StoreError,
    StoreResult, Transaction,
};

pub struct StartInput {
    pub params: Value,
    pub text: String,
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

/// Preserve the exact input and configuration of a positively rejected attempt.
/// Dynamic tools alone may refresh. Newer queued events cannot join this batch.
pub fn start_input(
    tx: &Transaction<'_>,
    agent: &str,
    attempt: &str,
    transport: TransportSnapshot,
    params: Value,
    text: String,
) -> StoreResult<StartInput> {
    let rows = reserved(tx, agent, attempt, &transport)?;
    resolve(tx, agent, &rows, transport, params, text)
}
fn resolve(
    tx: &Transaction<'_>,
    agent: &str,
    rows: &ClaudeRetryRecords,
    transport: TransportSnapshot,
    mut params: Value,
    mut text: String,
) -> StoreResult<StartInput> {
    let object = params.as_object_mut().ok_or_else(|| invalid(agent))?;
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
    let retry = rows
        .agent
        .pointer("/startAttempt/claudeRetryOf")
        .and_then(Value::as_str);
    if let Some(retry) = retry {
        let frozen = store::retry_input(tx, agent, retry, transport, false)?
            .ok_or_else(|| invalid(agent))?;
        let configuration: serde_json::Map<String, Value> = object
            .iter()
            .filter(|(key, _)| {
                !["input", "dynamicTools", "clientUserMessageId"].contains(&key.as_str())
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if frozen.get("configuration") != Some(&Value::Object(configuration)) {
            return Err(invalid(agent));
        }
        object.insert(
            "input".into(),
            frozen.get("input").cloned().ok_or_else(|| invalid(agent))?,
        );
        text = frozen
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid(agent))?
            .into();
    } else if rows
        .agent
        .get("claudePreInputRetry")
        .is_some_and(|value| !value.is_null())
    {
        return Err(invalid(agent));
    }
    Ok(StartInput { params, text })
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
    let mut rows = reserved(tx, agent, attempt, &transport)?;
    let input = resolve(tx, agent, &rows, transport, params.clone(), text.into())?;
    if &input.params != params || input.text != text {
        return Err(invalid(agent));
    }
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
    transport: TransportSnapshot,
) -> StoreResult<()> {
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
