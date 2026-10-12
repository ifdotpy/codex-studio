//! HTTP adapters for persisted coordination chats and messages.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::Value;
use studio_runtime_services::coordination::{self, CoordinationError, PostMessage};

use super::extractors::{ApiJson, QueryParams};
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get, post};
use crate::error::ApiError;
use crate::router::HttpState;

const CHAT_HISTORY_DEP: [Dependency; 1] = [Dependency::new("coordination-policy/chat-history")];
const MESSAGE_RECEIPTS_DEP: [Dependency; 1] = [Dependency::new("event-driver/message-receipts")];
const ROOM_VISIBILITY_DEP: [Dependency; 1] =
    [Dependency::new("coordination-policy/room-visibility")];

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

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/agent-chat",
            OperationStatus::partial(&CHAT_HISTORY_DEP),
            agent_chat,
        ),
        get("/api/messages", OperationStatus::Implemented, messages),
        post("/api/messages", OperationStatus::Implemented, post_message),
        get(
            "/api/messages/receipts",
            OperationStatus::partial(&MESSAGE_RECEIPTS_DEP),
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
            OperationStatus::partial(&ROOM_VISIBILITY_DEP),
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

async fn agent_chat() -> Result<Json<Value>, ApiError> {
    Err(dependency_error(
        "GET",
        "/api/agent-chat",
        &CHAT_HISTORY_DEP,
    ))
}
async fn receipts() -> Result<Json<Value>, ApiError> {
    Err(dependency_error(
        "GET",
        "/api/messages/receipts",
        &MESSAGE_RECEIPTS_DEP,
    ))
}
async fn delete_room() -> Result<Json<Value>, ApiError> {
    Err(dependency_error(
        "POST",
        "/api/room/delete",
        &ROOM_VISIBILITY_DEP,
    ))
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

fn dependency_error(method: &str, path: &str, dependencies: &[Dependency]) -> ApiError {
    dependency_unavailable(method, path, dependencies).unwrap_or_else(|_| ApiError::internal())
}
