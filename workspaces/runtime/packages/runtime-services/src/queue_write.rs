//! Durable mutations of pending user messages in an agent queue.

use std::collections::BTreeSet;

use crate::event_write;
use serde_json::{Value, json};
use studio_runtime_schema::queue_command::QueueCommand;
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, Connection, EventWriteRepository, QueueWriteRepository,
    StoreError, Transaction, WorkItemsRepository,
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
    body: &QueueCommand,
    signature: &str,
    snapshot: impl Fn(&Transaction<'_>, &AgentRecord) -> Result<QueueSnapshot, QueueWriteError>,
) -> Result<Value, QueueWriteError> {
    let agent = AgentRepository::get(transaction, &AgentId::from(agent_id))?
        .ok_or(QueueWriteError::InvalidRequest("Invalid request"))?;
    if agent.id.0 != agent_id {
        return Err(invalid_stored("runtime_agents", agent_id));
    }
    if event_write::truthy(agent.extra.get("deletedAt")) {
        return Err(QueueWriteError::InvalidRequest("This agent was deleted"));
    }
    if event_write::truthy(agent.extra.get("remoteWorker"))
        || event_write::truthy(agent.extra.get("remoteAnchor"))
    {
        return Err(QueueWriteError::RemoteEventsUnavailable);
    }

    let request_id = body
        .request_id()
        .map(event_write::python_strip)
        .filter(|id| !id.is_empty());
    if body.request_id().is_some() && request_id.is_none() {
        return Err(QueueWriteError::InvalidRequest(
            "Supply a request ID with 1 to 200 characters",
        ));
    }
    if matches!(body, QueueCommand::Reorder(_) | QueueCommand::SendNow(_)) && request_id.is_none() {
        return Err(QueueWriteError::InvalidRequest(
            "Supply a request ID to change queue delivery",
        ));
    }
    let receipt_id = request_id.map(|id| format!("queue:{agent_id}:{id}"));
    if let Some(receipt_id) = receipt_id.as_deref() {
        let receipt = EventWriteRepository::receipt(transaction, receipt_id)?;
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
    if !body.accepts_revision(&current.revision) {
        return Err(QueueWriteError::Conflict(
            "This queue changed. Reload before editing or reordering",
        ));
    }
    let rows = &current.items;
    match body {
        QueueCommand::Reorder(reorder_body) => {
            reorder(transaction, &agent, &reorder_body.ordered_ids, rows)?
        }
        QueueCommand::Cancel(_) => {
            let (target_id, _) = target_row(body, rows)?;
            QueueWriteRepository::update_status(transaction, target_id, "cancelled")?;
        }
        QueueCommand::Edit(edit) => {
            let (target_id, row) = target_row(body, rows)?;
            let text = event_write::python_strip(&edit.text);
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
        QueueCommand::First(_) | QueueCommand::SendNow(_) => {
            let (target_id, row) = target_row(body, rows)?;
            if matches!(body, QueueCommand::SendNow(_))
                && row.get("kind").and_then(Value::as_str) != Some("user")
            {
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
            if matches!(body, QueueCommand::SendNow(_)) {
                let mut metadata = read_metadata(transaction, target_id)?;
                if metadata.get("requestedDelivery").is_none()
                    && !event_write::truthy(metadata.get("sendNow"))
                {
                    let original = studio_runtime_schema::DeliveryMoment::for_event(
                        "user",
                        "",
                        Some(&metadata),
                    )
                    .map_err(|_| QueueWriteError::InvalidStoredObject {
                        object: "runtime_event_meta",
                        id: target_id.to_owned(),
                    })?;
                    metadata["requestedDelivery"] = json!(original);
                }
                metadata["delivery"] =
                    json!(studio_runtime_schema::delivery::DeliveryMoment::CurrentTurn);
                metadata["sendNow"] = json!(true);
                QueueWriteRepository::put_metadata(transaction, target_id, &metadata)?;
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
            .map_err(QueueWriteError::Store)?;
    }
    Ok(result)
}

fn target_row<'a>(
    command: &'a QueueCommand,
    rows: &'a [Value],
) -> Result<(&'a str, &'a Value), QueueWriteError> {
    let id = command.target_id().unwrap_or_default();
    let row = rows
        .iter()
        .find(|row| row.get("id").and_then(Value::as_str) == Some(id))
        .ok_or(QueueWriteError::Conflict(
            "This message already left the queue",
        ))?;
    if command
        .expected_text()
        .is_some_and(|text| Some(text) != row.get("text").and_then(Value::as_str))
    {
        return Err(QueueWriteError::Conflict("This queued message changed"));
    }
    Ok((id, row))
}

fn reorder(
    transaction: &Transaction<'_>,
    agent: &AgentRecord,
    ordered: &[String],
    rows: &[Value],
) -> Result<(), QueueWriteError> {
    let ids = ordered.iter().map(String::as_str).collect::<Vec<_>>();
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
    let record = QueueWriteRepository::metadata(transaction, id)?;
    match record {
        None => Ok(json!({})),
        Some(value) => Ok(value),
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

pub fn queue_snapshot(
    connection: &Connection,
    agent: &AgentRecord,
) -> Result<QueueSnapshot, QueueWriteError> {
    let prepared = crate::work_policy::prepare_queue(WorkItemsRepository::queue(
        connection,
        &agent.id.0,
        agent.epoch,
    )?)
    .map_err(|error| match error {
        crate::work_policy::WorkPolicyError::InvalidStoredObject { object, id } => {
            invalid_stored(object, &id)
        }
    })?;
    let assets = WorkItemsRepository::assets(connection, &prepared.asset_ids)?;
    let assets = assets
        .into_iter()
        .map(|mut value| {
            value
                .as_object_mut()
                .expect("stored asset is an object")
                .remove("path");
            Ok(value)
        })
        .collect::<Result<Vec<_>, QueueWriteError>>()?;
    let owners = assets
        .iter()
        .map(|asset| {
            asset
                .get("agent")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_stored("runtime_assets", "<asset>"))
        })
        .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
    for owner in owners {
        let record = AgentRepository::get(connection, &AgentId::from(owner))
            .map_err(QueueWriteError::Store)?;
        let Some(record) = record else {
            return Err(invalid_stored("runtime_agents", owner));
        };
        if !is_live(&record) {
            return Err(invalid_stored("runtime_agents", owner));
        }
    }
    let mut items =
        crate::work_policy::queue_items(prepared, &assets).map_err(|error| match error {
            crate::work_policy::WorkPolicyError::InvalidStoredObject { object, id } => {
                invalid_stored(object, &id)
            }
        })?;
    for item in &mut items {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("<event>")
            .to_owned();
        for field in ["delivery", "requestedDelivery"] {
            let raw = item[field]
                .as_str()
                .ok_or_else(|| invalid_stored("runtime_events", &id))?;
            let moment = studio_runtime_schema::DeliveryMoment::parse_legacy(raw)
                .map_err(|_| invalid_stored("runtime_events", &id))?;
            item[field] = json!(moment.as_str());
        }
        if !matches!(
            studio_runtime_schema::first_error("QueueItem", item),
            Ok(None)
        ) {
            return Err(invalid_stored("runtime_events", &id));
        }
    }
    let revision_number = match agent.extra.get("queueMutationRevision") {
        None => 0,
        Some(value) => value
            .as_i64()
            .filter(|value| *value >= 0)
            .ok_or_else(|| invalid_stored("runtime_agents", &agent.id.0))?,
    };
    let bytes = serde_json::to_vec(&json!([agent.epoch, revision_number, items]))?;
    let revision = studio_runtime_store::sha256_hex(&bytes);
    Ok(QueueSnapshot {
        items,
        revision,
        capabilities: json!({"reorder":true,"receipts":true}),
    })
}

pub fn is_live(agent: &AgentRecord) -> bool {
    !agent
        .extra
        .get("deletedAt")
        .is_some_and(|value| match value {
            Value::Null | Value::Bool(false) => false,
            Value::Bool(true) => true,
            Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
            Value::String(value) => !value.is_empty(),
            Value::Array(_) | Value::Object(_) => true,
        })
}

/// Queue collection owner. Every relevant source write calls this before commit.
pub fn publish_queue(
    transaction: &Transaction<'_>,
    id: &str,
) -> studio_runtime_store::StoreResult<()> {
    let agent = AgentRepository::get(transaction, &AgentId::from(id))?;
    let (value, deleted) = match agent.filter(is_live) {
        Some(agent) => {
            let snapshot = queue_snapshot(transaction, &agent).map_err(|error| match error {
                QueueWriteError::Store(error) => error,
                QueueWriteError::Json(error) => StoreError::Json(error),
                QueueWriteError::InvalidRequest(_)
                | QueueWriteError::Conflict(_)
                | QueueWriteError::InvalidStoredObject { .. }
                | QueueWriteError::RemoteEventsUnavailable => StoreError::InvalidSyncObject {
                    scope: "queue".into(),
                    id: id.into(),
                },
            })?;
            (
                json!({"items":snapshot.items,"revision":snapshot.revision,"capabilities":snapshot.capabilities}),
                false,
            )
        }
        None => (json!({}), true),
    };
    studio_runtime_store::SyncDocumentsRepository::put_entity(
        transaction,
        "queue",
        id,
        value,
        deleted,
    )?;
    Ok(())
}
