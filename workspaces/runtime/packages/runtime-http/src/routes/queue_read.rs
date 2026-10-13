//! Shared queue projection used by both queue reads and writes.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use studio_runtime_services::queue_write::{QueueSnapshot, QueueWriteError};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, Connection, WorkItemsRepository,
};

use crate::error::ApiError;

pub(super) fn queue_snapshot(
    connection: &Connection,
    agent: &AgentRecord,
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
            .map_err(|_| invalid_stored("runtime_agents", owner))?;
        let Some(record) = record else {
            return Err(invalid_stored("runtime_agents", owner));
        };
        if !is_live(&record) {
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
        if super::generated_json::<crate::generated::QueueItem>(item.clone()).is_err() {
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
    let revision = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
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
        QueueWriteError::Store(studio_runtime_store::StoreError::InvalidStoredObject {
            table,
            id,
        }) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("invalid stored object {table}/{id}"),
        ),
        QueueWriteError::Store(_) | QueueWriteError::Json(_) => ApiError::internal(),
    }
}
