//! Durable mutations of pending user messages in an agent queue.

use std::collections::BTreeSet;

use crate::event_write;
use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, EventWriteRepository, QueueWriteRepository, StoreError,
    Transaction,
};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq)]
pub struct QueueSnapshot {
    pub items: Vec<Value>,
    pub revision: String,
    pub capabilities: Value,
}

#[derive(Debug, Error)]
pub enum QueueWriteError {
    #[error("invalid request: {0}")]
    InvalidRequest(&'static str),
    #[error("{0}")]
    Conflict(&'static str),
    #[error("invalid stored object {object}/{id}")]
    InvalidStoredObject { object: &'static str, id: String },
    #[error("Not implemented: requires store-bootstrap/operation-receipts")]
    ReceiptsUnavailable,
    #[error("Not implemented: requires store-bootstrap/event-metadata")]
    MetadataUnavailable,
    #[error("Not implemented: requires cross-server/remote-events")]
    RemoteEventsUnavailable,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Applies one validated queue request atomically. The snapshot callback is
/// supplied by the HTTP edge so reads and writes share the same stored-data
/// validation and projection.
pub fn action(
    transaction: &Transaction<'_>,
    agent_id: &str,
    body: &Value,
    signature: &str,
    snapshot: impl Fn(&Transaction<'_>, &AgentRecord) -> Result<QueueSnapshot, QueueWriteError>,
) -> Result<Value, QueueWriteError> {
    let agent = AgentRepository::get(transaction, &AgentId::from(agent_id))
        .map_err(|_| invalid_stored("runtime_agents", agent_id))?
        .ok_or(QueueWriteError::InvalidRequest("Invalid request"))?;
    if agent.id.0 != agent_id {
        return Err(invalid_stored("runtime_agents", agent_id));
    }
    if truthy(agent.extra.get("deletedAt")) {
        return Err(QueueWriteError::InvalidRequest("This agent was deleted"));
    }
    if truthy(agent.extra.get("remoteWorker")) || truthy(agent.extra.get("remoteAnchor")) {
        return Err(QueueWriteError::RemoteEventsUnavailable);
    }

    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let request_id = body
        .get("request_id")
        .and_then(Value::as_str)
        .map(event_write::python_strip)
        .filter(|id| !id.is_empty());
    if body.get("request_id").is_some_and(|value| !value.is_null()) && request_id.is_none() {
        return Err(QueueWriteError::InvalidRequest(
            "Supply a request ID with 1 to 200 characters",
        ));
    }
    if matches!(action, "reorder" | "send_now") && request_id.is_none() {
        return Err(QueueWriteError::InvalidRequest(
            "Supply a request ID to change queue delivery",
        ));
    }
    let receipt_id = request_id.map(|id| format!("queue:{agent_id}:{id}"));
    if let Some(receipt_id) = receipt_id.as_deref() {
        if !QueueWriteRepository::table_exists(transaction, "runtime_operation_receipts")? {
            return Err(QueueWriteError::ReceiptsUnavailable);
        }
        let receipt = EventWriteRepository::receipt(transaction, receipt_id)
            .map_err(|_| invalid_stored("runtime_operation_receipts", receipt_id))?;
        if let Some(receipt) = receipt {
            if receipt.signature != signature {
                return Err(QueueWriteError::Conflict(
                    "This request id has different content",
                ));
            }
            let saved: Value = serde_json::from_str(&receipt.result)
                .map_err(|_| invalid_stored("runtime_operation_receipts", receipt_id))?;
            if !valid_receipt(&saved) {
                return Err(invalid_stored("runtime_operation_receipts", receipt_id));
            }
            return Ok(saved);
        }
    }

    let current = snapshot(transaction, &agent)?;
    if (body.get("expected_revision").is_some() || action == "reorder")
        && body.get("expected_revision").and_then(Value::as_str) != Some(current.revision.as_str())
    {
        return Err(QueueWriteError::Conflict(
            "This queue changed. Reload before editing or reordering",
        ));
    }
    let rows = &current.items;
    if action == "reorder" {
        reorder(transaction, &agent, body, rows)?;
    } else {
        let target_id = body
            .get("message_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .or_else(|| body.get("id").and_then(Value::as_str))
            .unwrap_or_default();
        let row = rows
            .iter()
            .find(|row| row.get("id").and_then(Value::as_str) == Some(target_id))
            .ok_or(QueueWriteError::Conflict(
                "This message already left the queue",
            ))?;
        if body
            .get("expectedText")
            .is_some_and(|value| !value.is_null())
            && body.get("expectedText") != row.get("text")
        {
            return Err(QueueWriteError::Conflict("This queued message changed"));
        }
        match action {
            "cancel" => QueueWriteRepository::update_status(transaction, target_id, "cancelled")?,
            "edit" => {
                let text = body.get("text").and_then(Value::as_str).unwrap_or_default();
                let text = event_write::python_strip(text);
                if text.is_empty()
                    && row
                        .get("assets")
                        .and_then(Value::as_array)
                        .is_none_or(Vec::is_empty)
                {
                    return Err(QueueWriteError::InvalidRequest(
                        "Supply a message or attachment",
                    ));
                }
                QueueWriteRepository::update_text(transaction, target_id, text)?;
                if row.get("text").and_then(Value::as_str) != Some(text) {
                    let mut metadata = read_metadata(transaction, target_id)?;
                    metadata["acceptedAt"] = json!(unix_time()?);
                    QueueWriteRepository::put_metadata(transaction, target_id, &metadata)?;
                }
            }
            "first" | "send_now" => {
                if action == "send_now" && row.get("kind").and_then(Value::as_str) != Some("user") {
                    return Err(QueueWriteError::Conflict(
                        "Only a user message can be sent now",
                    ));
                }
                let first = rows
                    .iter()
                    .filter_map(|item| item.get("created").and_then(Value::as_f64))
                    .reduce(f64::min)
                    .ok_or(QueueWriteError::Conflict(
                        "This message already left the queue",
                    ))?;
                QueueWriteRepository::update_created(transaction, target_id, first - 0.001)?;
                if action == "send_now" {
                    let mut metadata = read_metadata(transaction, target_id)?;
                    metadata["delivery"] = json!("steer");
                    metadata["sendNow"] = json!(true);
                    QueueWriteRepository::put_metadata(transaction, target_id, &metadata)?;
                }
            }
            _ => {
                return Err(QueueWriteError::InvalidRequest(
                    "Choose edit, cancel, first, reorder, or send_now",
                ));
            }
        }
    }

    let updated = snapshot(transaction, &agent)?;
    let mut agent = agent;
    if updated.items != current.items {
        let revision = match agent.extra.get("queueMutationRevision") {
            None => 0,
            Some(value) => value
                .as_i64()
                .filter(|revision| *revision >= 0)
                .ok_or_else(|| invalid_stored("runtime_agents", agent_id))?,
        };
        agent
            .extra
            .insert("queueMutationRevision".to_owned(), json!(revision + 1));
        AgentRepository::put(transaction, &agent)?;
    }
    let final_snapshot = if updated.items != current.items {
        snapshot(transaction, &agent)?
    } else {
        updated
    };
    let result = json!({
        "status": "updated",
        "revision": final_snapshot.revision,
        "capabilities": final_snapshot.capabilities,
    });
    if !valid_receipt(&result) {
        return Err(QueueWriteError::InvalidRequest("Invalid queue receipt"));
    }
    if let Some(receipt_id) = receipt_id {
        EventWriteRepository::put_receipt(transaction, &receipt_id, signature, &result.to_string())
            .map_err(|error| match error {
                StoreError::Sqlite(_) => QueueWriteError::ReceiptsUnavailable,
                other => QueueWriteError::Store(other),
            })?;
    }
    Ok(result)
}

fn reorder(
    transaction: &Transaction<'_>,
    agent: &AgentRecord,
    body: &Value,
    rows: &[Value],
) -> Result<(), QueueWriteError> {
    let ordered = body
        .get("ordered_ids")
        .and_then(Value::as_array)
        .filter(|ids| ids.iter().all(Value::is_string))
        .ok_or(QueueWriteError::InvalidRequest(
            "Supply every current queued message ID exactly once",
        ))?;
    let ids = ordered.iter().filter_map(Value::as_str).collect::<Vec<_>>();
    let expected = rows
        .iter()
        .filter_map(|row| row.get("id").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    if ids.len() != rows.len() || ids.iter().copied().collect::<BTreeSet<_>>() != expected {
        return Err(QueueWriteError::InvalidRequest(
            "Supply every current queued message ID exactly once",
        ));
    }
    let pending = QueueWriteRepository::pending_events(transaction, &agent.id.0, agent.epoch)?;
    if let Some(event) = pending.iter().find(|event| !event.valid) {
        return Err(invalid_stored("runtime_events", &event.id));
    }
    let mut slots = Vec::new();
    let mut run = Vec::new();
    let mut lower = f64::NEG_INFINITY;
    for event in pending.iter().map(Some).chain(std::iter::once(None)) {
        if let Some(event) =
            event.filter(|event| matches!(event.kind.as_str(), "user" | "followup"))
        {
            run.push(event.created);
            continue;
        }
        let upper = event.map_or(f64::INFINITY, |event| event.created);
        let mut values = Vec::new();
        let mut last = lower;
        for original in &run {
            last = (*original).max(last.next_up());
            values.push(last);
        }
        if values.last().is_some_and(|value| *value >= upper) {
            values.clear();
            last = upper;
            for original in run.iter().rev() {
                last = (*original).min(last.next_down());
                values.push(last);
            }
            values.reverse();
        }
        if !values.is_empty()
            && (values[0] <= lower
                || *values.last().unwrap_or(&upper) >= upper
                || values.iter().any(|value| !value.is_finite()))
        {
            return Err(QueueWriteError::Conflict(
                "Queue timestamps overlap. The queue cannot be reordered safely",
            ));
        }
        slots.extend(values);
        run.clear();
        lower = upper;
    }
    if slots.len() != ids.len() {
        return Err(QueueWriteError::Conflict(
            "Queue timestamps overlap. The queue cannot be reordered safely",
        ));
    }
    for (id, created) in ids.into_iter().zip(slots) {
        QueueWriteRepository::update_created(transaction, id, created)?;
    }
    Ok(())
}

fn read_metadata(transaction: &Transaction<'_>, id: &str) -> Result<Value, QueueWriteError> {
    if !QueueWriteRepository::table_exists(transaction, "runtime_event_meta")? {
        return Err(QueueWriteError::MetadataUnavailable);
    }
    let record = QueueWriteRepository::metadata(transaction, id)
        .map_err(|_| invalid_stored("runtime_event_meta", id))?;
    match record {
        None => Ok(json!({})),
        Some(record) => {
            let value: Value = serde_json::from_str(&record)
                .map_err(|_| invalid_stored("runtime_event_meta", id))?;
            if value.is_object() {
                Ok(value)
            } else {
                Err(invalid_stored("runtime_event_meta", id))
            }
        }
    }
}

fn valid_receipt(value: &Value) -> bool {
    value.get("status").and_then(Value::as_str) == Some("updated")
        && value.get("revision").and_then(Value::as_str).is_some()
        && value
            .get("capabilities")
            .and_then(Value::as_object)
            .is_some()
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
        Some(Value::Bool(true)) => true,
    }
}

fn unix_time() -> Result<f64, QueueWriteError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .map_err(|_| QueueWriteError::InvalidRequest("System clock is before the Unix epoch"))
}

fn invalid_stored(object: &'static str, id: &str) -> QueueWriteError {
    QueueWriteError::InvalidStoredObject {
        object,
        id: id.to_owned(),
    }
}
