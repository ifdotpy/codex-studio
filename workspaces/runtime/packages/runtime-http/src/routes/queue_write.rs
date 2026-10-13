//! HTTP adapter for durable queue mutations.

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};
use studio_runtime_services::queue_write::{self, QueueWriteError};
use studio_runtime_store::SyncDocumentsRepository;

use super::extractors::ApiJson;
use crate::error::ApiError;
use crate::router::HttpState;

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
    let signature =
        super::work_items::python_operation_signature(&json!({"agent": agent_id, "body": body}))?;
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
                    |connection, agent| {
                        super::queue_read::queue_snapshot(connection, agent, &validators)
                    },
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

fn write_error(error: QueueWriteError) -> ApiError {
    super::queue_read::api_error(error)
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "queue_write",
        routes: None,
    }
}

inventory::submit! {
    crate::routes::manifest::DependencyRegistration("event-driver/queue-mutations")
}
