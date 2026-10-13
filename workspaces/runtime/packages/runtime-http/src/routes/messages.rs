//! HTTP adapters for persisted coordination chats and messages.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::Value;
use studio_runtime_services::coordination::{self, CoordinationError, PostMessage};

use super::extractors::{ApiJson, QueryParams};
use super::manifest::{OperationStatus, RouteGroup, get, post};
use crate::error::ApiError;
use crate::router::HttpState;

#[derive(Deserialize)]
struct ChatCreate {
    id: String,
    name: String,
    members: Vec<String>,
}

#[derive(Deserialize)]
struct Connection {
    source: String,
    target: String,
    #[serde(default = "default_connected")]
    connected: bool,
}

#[derive(Deserialize)]
struct MessagePost {
    id: String,
    room: String,
    text: String,
}

#[derive(Deserialize)]
struct RoomDelete {
    id: String,
}

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get("/api/agent-chat", OperationStatus::Implemented, agent_chat),
        get("/api/messages", OperationStatus::Implemented, messages),
        post("/api/messages", OperationStatus::Implemented, post_message),
        get(
            "/api/messages/receipts",
            OperationStatus::Implemented,
            receipts,
        ),
        post("/api/chats", OperationStatus::Implemented, create_chat),
        post(
            "/api/connections",
            OperationStatus::Implemented,
            connect_chat,
        ),
        post(
            "/api/room/delete",
            OperationStatus::Implemented,
            delete_room,
        ),
    ])
}

async fn messages(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let room = query
        .typed_value("room")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let store = state.store.ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let value = coordination::history(&connection, &room).map_err(coordination_error)?;
        super::generated_json::<Vec<crate::generated::MessageRecord>>(value)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(result))
}

async fn create_chat(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<ChatCreate>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || {
        let value = store
            .transaction_with_error(|transaction| {
                coordination::create_chat(transaction, &body.id, &body.name, &body.members)
            })
            .map_err(coordination_error)?;
        super::generated_json::<crate::generated::ChatCreated>(value)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(result))
}

async fn connect_chat(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Connection>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || {
        let value = store
            .transaction_with_error(|transaction| {
                coordination::connect_chat(transaction, &body.source, &body.target, body.connected)
            })
            .map_err(coordination_error)?;
        super::generated_json::<crate::generated::ChatReceipt>(value)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(result))
}

async fn post_message(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<MessagePost>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let wake = state.event_wake;
    let result = tokio::task::spawn_blocking(move || {
        let value = store
            .transaction_with_error(|transaction| {
                coordination::post_message(
                    transaction,
                    PostMessage {
                        room: &body.room,
                        id: &body.id,
                        text: &body.text,
                    },
                )
            })
            .map_err(coordination_error)?;
        let value =
            super::generated_json::<crate::generated::StudioApiWorkModelsMessageReceipt>(value)?;
        wake.notify_one();
        Ok::<Value, ApiError>(value)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(result))
}

async fn agent_chat(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let room = query
        .typed_value("room")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let before = query.typed_value("before").and_then(Value::as_i64);
    let after = query.typed_value("after").and_then(Value::as_i64);
    let limit = query
        .typed_value("limit")
        .and_then(Value::as_i64)
        .unwrap_or(100);
    let store = state.store.ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let value = coordination::chat_read(&connection, &room, before, after, limit)
            .map_err(coordination_error)?;
        super::generated_json::<crate::generated::ChatRead>(value)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(result))
}
async fn receipts(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let agent = query
        .typed_value("agent")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let raw = query
        .typed_value("ids")
        .and_then(Value::as_str)
        .unwrap_or("[]");
    let ids: Vec<String> = serde_json::from_str(raw)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "Invalid message receipt IDs"))?;
    let store = state.store.ok_or_else(ApiError::internal)?;
    let workspace = state.workspace_id;
    let result = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let mut value = coordination::delivery_receipts(&connection, &agent, &ids)
            .map_err(coordination_error)?;
        value["workspaceId"] = Value::String(workspace);
        super::generated_json::<crate::generated::MessageReceipts>(value)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(result))
}
async fn delete_room(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<RoomDelete>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || {
        let value = store
            .transaction_with_error(|tx| coordination::delete_room(tx, &body.id))
            .map_err(coordination_error)?;
        super::generated_json::<crate::generated::MutationReceipt>(value)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(result))
}

fn default_connected() -> bool {
    true
}

fn coordination_error(error: CoordinationError) -> ApiError {
    match error {
        CoordinationError::RequiresDependency(dependency) => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            format!("Not implemented: requires {dependency}"),
        ),
        CoordinationError::InvalidRequest(message) | CoordinationError::Conflict(message) => {
            ApiError::new(StatusCode::BAD_REQUEST, message)
        }
        CoordinationError::InvalidStored { object, id } => invalid_stored(object, &id),
        CoordinationError::Store(_) => ApiError::internal(),
    }
}

fn invalid_stored(object: &'static str, id: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Invalid stored object {object}/{id}"),
    )
}
