//! HTTP adapter for durable queue mutations.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use studio_runtime_services::queue_write::{self, QueueSnapshot, QueueWriteError};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, Connection, StoredRecord, SyncDocumentsRepository,
    WorkItemsRepository,
};

use super::extractors::ApiJson;
use crate::error::ApiError;
use crate::router::HttpState;
use crate::validation::Registry;

pub(crate) async fn queue_action(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    validate_queue_action(&body)?;
    let agent_id = body
        .get("agent")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| state.workspace_id.clone());
    let signature = operation_signature(&agent_id, &body)?;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let before_connection = store.read().map_err(|_| ApiError::internal())?;
    let before_sequence = SyncDocumentsRepository::entity_sequence(&before_connection)
        .map_err(|_| ApiError::internal())?;
    drop(before_connection);
    let validators = state.validators.clone();
    let result = tokio::task::spawn_blocking(move || {
        let result = store
            .transaction_with_error(|transaction| {
                queue_write::action(
                    transaction,
                    &agent_id,
                    &body,
                    &signature,
                    |connection, agent| queue_snapshot(connection, agent, &validators),
                )
            })
            .map_err(write_error)?;
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let entities = SyncDocumentsRepository::entities_after(&connection, before_sequence, 501)
            .map_err(|_| ApiError::internal())?;
        let floor = SyncDocumentsRepository::entity_tombstone_floor(&connection)
            .map_err(|_| ApiError::internal())?;
        let sync_after =
            (entities.len() <= 500 && floor <= before_sequence).then_some(before_sequence);
        Ok::<_, ApiError>((result, entities, sync_after))
    })
    .await
    .map_err(|_| ApiError::internal())??;
    let (mut result, entities, sync_after) = result;
    state.event_wake.notify_one();
    if !entities.is_empty() {
        result["_syncEntities"] = Value::Array(
            entities
                .iter()
                .take(500)
                .map(|row| {
                    json!({
                        "id": format!("entity:{}:{}", row.collection, row.id),
                        "seq": row.sequence,
                        "payload": row.payload,
                        "_deleted": row.deleted,
                    })
                })
                .collect(),
        );
        if let Some(sync_after) = sync_after {
            result["_syncEntitiesAfter"] = json!(sync_after);
        }
    }
    Ok(Json(result))
}

fn queue_snapshot(
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
        } => QueueWriteError::InvalidStoredObject { object, id },
    })?;
    let assets = WorkItemsRepository::assets(connection, &prepared.asset_ids)?;
    let assets_present = assets.is_some();
    let assets = assets
        .unwrap_or_default()
        .into_iter()
        .map(|row| {
            let value = stored_record("runtime_assets", &row)?;
            if !validators.validate_stored("TranscriptAsset", &value) {
                return Err(invalid_stored("runtime_assets", &row.id));
            }
            Ok(value)
        })
        .collect::<Result<Vec<_>, QueueWriteError>>()?;
    for asset in &assets {
        let owner = asset
            .get("agent")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_stored("runtime_assets", "<asset>"))?;
        let record = AgentRepository::get(connection, &AgentId::from(owner))?
            .ok_or_else(|| invalid_stored("runtime_agents", owner))?;
        if record.id.0 != owner || !is_live(&record) {
            return Err(invalid_stored("runtime_agents", owner));
        }
    }
    let items =
        studio_runtime_services::work_policy::queue_items(prepared, &assets, assets_present)
            .map_err(|error| match error {
                studio_runtime_services::work_policy::WorkPolicyError::InvalidStoredObject {
                    object,
                    id,
                } => QueueWriteError::InvalidStoredObject { object, id },
            })?;
    for item in &items {
        if !validators.validate_stored("QueueItem", item) {
            return Err(invalid_stored(
                "runtime_events",
                item.get("id").and_then(Value::as_str).unwrap_or("<event>"),
            ));
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

fn is_live(agent: &AgentRecord) -> bool {
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

fn validate_queue_action(body: &Value) -> Result<(), ApiError> {
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let has_id = ["id", "message_id"].iter().any(|field| {
        body.get(*field)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    });
    if matches!(action, "cancel" | "edit" | "first" | "send_now") && !has_id {
        return Err(ApiError::validation(vec![Value::String("body".into())]));
    }
    if action == "send_now"
        && !body
            .get("request_id")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    {
        return Err(ApiError::validation(vec![Value::String("body".into())]));
    }
    Ok(())
}

fn operation_signature(agent_id: &str, body: &Value) -> Result<String, ApiError> {
    let value = json!({"agent": agent_id, "body": body});
    let canonical = python_json(&value)?;
    Ok(Sha256::digest(canonical.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn python_json(value: &Value) -> Result<String, ApiError> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => Ok(value.to_string()),
        Value::String(value) => Ok(python_quote(value)),
        Value::Array(values) => Ok(format!(
            "[{}]",
            values
                .iter()
                .map(python_json)
                .collect::<Result<Vec<_>, _>>()?
                .join(", ")
        )),
        Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort();
            let fields = keys
                .into_iter()
                .map(|key| {
                    Ok(format!(
                        "{}: {}",
                        python_quote(key),
                        python_json(&values[key])?
                    ))
                })
                .collect::<Result<Vec<_>, ApiError>>()?;
            Ok(format!("{{{}}}", fields.join(", ")))
        }
    }
}

fn python_quote(value: &str) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{8}' => output.push_str("\\b"),
            '\u{c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            c if c < ' ' => output.push_str(&format!("\\u{:04x}", c as u32)),
            c if c as u32 > 0x7f => {
                let n = c as u32;
                if n <= 0xffff {
                    output.push_str(&format!("\\u{n:04x}"));
                } else {
                    let n = n - 0x10000;
                    output.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xd800 + (n >> 10),
                        0xdc00 + (n & 0x3ff)
                    ));
                }
            }
            c => output.push(c),
        }
    }
    output.push('"');
    output
}

fn write_error(error: QueueWriteError) -> ApiError {
    match error {
        QueueWriteError::InvalidRequest(message) | QueueWriteError::Conflict(message) => {
            ApiError::new(StatusCode::BAD_REQUEST, message)
        }
        QueueWriteError::InvalidStoredObject { object, id } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("invalid stored object {object}/{id}"),
        ),
        QueueWriteError::ReceiptsUnavailable => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires store-bootstrap/operation-receipts",
        ),
        QueueWriteError::MetadataUnavailable => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires store-bootstrap/event-metadata",
        ),
        QueueWriteError::RemoteEventsUnavailable => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires cross-server/remote-events",
        ),
        QueueWriteError::Store(_) | QueueWriteError::Json(_) => ApiError::internal(),
    }
}

fn invalid_stored(object: &'static str, id: &str) -> QueueWriteError {
    QueueWriteError::InvalidStoredObject {
        object,
        id: id.to_owned(),
    }
}
