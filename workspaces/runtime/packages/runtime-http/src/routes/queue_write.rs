//! HTTP adapter for durable queue mutations.

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};
use studio_runtime_services::queue_write::{self, QueueWriteError};

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
    let result = super::sync_delta::write(
        store,
        move |transaction| {
            queue_write::action(
                transaction,
                &agent_id,
                &body,
                &signature,
                |connection, agent| super::queue_read::queue_snapshot(connection, agent),
            )
        },
        write_error,
    )
    .await?;
    state.event_wake.notify_one();
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
        dependencies: &["event-driver/queue-mutations"],
    }
}
