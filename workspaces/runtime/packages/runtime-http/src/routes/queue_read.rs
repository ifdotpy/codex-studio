//! Shared queue projection used by both queue reads and writes.

use axum::http::StatusCode;
use serde_json::{Value, json};
use studio_runtime_services::queue_write::{QueueSnapshot, QueueWriteError};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, Connection, StoredRecord, WorkItemsRepository,
};

use crate::error::ApiError;
use crate::validation::Registry;

pub(super) fn queue_snapshot(
    connection: &Connection,
    agent: &AgentRecord,
    validators: &Registry,
) -> Result<QueueSnapshot, QueueWriteError> {
    let prepared = studio_runtime_services::work_policy::prepare_queue(WorkItemsRepository::queue(
        connection,
        &agent.id.0,
        agent.epoch,
    )?)
    .map_err(|error| match error {
        studio_runtime_services::work_policy::WorkPolicyError::InvalidStoredObject {
            object,
            id,
        } => invalid_stored(object, &id),
    })?;
    let assets = WorkItemsRepository::assets(connection, &prepared.asset_ids)?;
    let assets = assets
        .into_iter()
        .map(|row| {
            let mut value = stored_record("runtime_assets", &row)?;
            value
                .as_object_mut()
                .expect("stored asset is an object")
                .remove("path");
            if !validators.validate_stored("TranscriptAsset", &value) {
                return Err(invalid_stored("runtime_assets", &row.id));
            }
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
            .map_err(|_| invalid_stored("runtime_agents", owner))?;
        let Some(record) = record else {
            return Err(invalid_stored("runtime_agents", owner));
        };
        if record.id.0 != owner || !is_live(&record) {
            return Err(invalid_stored("runtime_agents", owner));
        }
    }
    let items =
        studio_runtime_services::work_policy::queue_items(prepared, &assets).map_err(|error| {
            match error {
                studio_runtime_services::work_policy::WorkPolicyError::InvalidStoredObject {
                    object,
                    id,
                } => invalid_stored(object, &id),
            }
        })?;
    for item in &items {
        let id = item.get("id").and_then(Value::as_str).unwrap_or("<event>");
        if !validators.validate_stored("QueueItem", item) {
            return Err(invalid_stored("runtime_events", id));
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

pub(super) fn is_live(agent: &AgentRecord) -> bool {
    !agent
        .extra
        .get("deletedAt")
        .is_some_and(|value| match value {
            Value::Null | Value::Bool(false) => false,
            Value::Bool(true) => true,
            Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
            Value::String(value) => !value.is_empty(),
            _ => true,
        })
}

fn stored_record(object: &'static str, row: &StoredRecord) -> Result<Value, QueueWriteError> {
    let value: Value = serde_json::from_str(
        row.record
            .as_deref()
            .ok_or_else(|| invalid_stored(object, &row.id))?,
    )
    .map_err(|_| invalid_stored(object, &row.id))?;
    if value.get("id").and_then(Value::as_str) != Some(row.id.as_str()) {
        return Err(invalid_stored(object, &row.id));
    }
    Ok(value)
}

fn invalid_stored(object: &'static str, id: &str) -> QueueWriteError {
    QueueWriteError::InvalidStoredObject {
        object,
        id: id.to_owned(),
    }
}

pub(super) fn api_error(error: QueueWriteError) -> ApiError {
    match error {
        QueueWriteError::InvalidRequest(message) | QueueWriteError::Conflict(message) => {
            ApiError::new(StatusCode::BAD_REQUEST, message)
        }
        QueueWriteError::InvalidStoredObject { object, id } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("invalid stored object {object}/{id}"),
        ),
        QueueWriteError::RemoteEventsUnavailable => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires cross-server/remote-events",
        ),
        QueueWriteError::Store(_) | QueueWriteError::Json(_) => ApiError::internal(),
    }
}
