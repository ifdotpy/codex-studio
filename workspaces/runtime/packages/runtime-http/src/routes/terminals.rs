//! Persisted terminal reads and session-dependent terminal operations.

use super::extractors::{ApiJson, QueryParams};
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use studio_runtime_services::terminal_driver::{self, TerminalError};

use crate::error::ApiError;
use crate::router::HttpState;

const SESSION_DEPENDENCIES: [Dependency; 1] = [Dependency::new("provider-session/app-server")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get("/api/terminals", OperationStatus::Implemented, listing),
        get(
            "/api/terminals/output",
            OperationStatus::Implemented,
            output,
        ),
        post(
            "/api/terminals/create",
            OperationStatus::partial(&SESSION_DEPENDENCIES),
            create,
        ),
        post(
            "/api/terminals/input",
            OperationStatus::partial(&SESSION_DEPENDENCIES),
            input,
        ),
        post(
            "/api/terminals/resize",
            OperationStatus::partial(&SESSION_DEPENDENCIES),
            resize,
        ),
        post(
            "/api/terminals/rename",
            OperationStatus::partial(&SESSION_DEPENDENCIES),
            rename,
        ),
        post(
            "/api/terminals/close",
            OperationStatus::partial(&SESSION_DEPENDENCIES),
            close,
        ),
    ])
}

async fn listing(State(state): State<HttpState>) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || terminal_driver::listing(&store))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(terminal_error)?;
    Ok(Json(generated_json::<crate::generated::TerminalList>(
        result,
    )?))
}

async fn output(
    query: QueryParams,
    State(state): State<HttpState>,
) -> Result<Json<Value>, ApiError> {
    let id = query
        .typed_value("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let offset = query
        .typed_value("offset")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let history = query.typed_value("history").and_then(Value::as_str) == Some("1");
    let limit = query
        .typed_value("limit")
        .and_then(Value::as_i64)
        .unwrap_or(65_536);
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || {
        if history {
            terminal_driver::history_output(&store, &id, offset, limit)
        } else {
            terminal_driver::output(&store, &id, offset)
        }
    })
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(terminal_error)?;
    Ok(Json(generated_json::<crate::generated::TerminalOutput>(
        result,
    )?))
}

async fn create(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(dependency_error("POST", "/api/terminals/create"))
}
async fn input(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(dependency_error("POST", "/api/terminals/input"))
}
async fn resize(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(dependency_error("POST", "/api/terminals/resize"))
}
async fn rename(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(dependency_error("POST", "/api/terminals/rename"))
}
async fn close(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(dependency_error("POST", "/api/terminals/close"))
}

fn dependency_error(method: &str, path: &str) -> ApiError {
    dependency_unavailable(method, path, &SESSION_DEPENDENCIES)
        .unwrap_or_else(|_| ApiError::internal())
}

fn terminal_error(error: TerminalError) -> ApiError {
    match error {
        TerminalError::InvalidRequest
        | TerminalError::InvalidCursor
        | TerminalError::InvalidHistoryRange => {
            ApiError::validation(vec![Value::String("query".to_owned())])
        }
        TerminalError::UnknownTerminal => ApiError::validation(vec![
            Value::String("query".to_owned()),
            Value::String("id".to_owned()),
        ]),
        TerminalError::InvalidStoredObject(id) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object terminals/{id}"),
        ),
        _ => ApiError::internal(),
    }
}

fn generated_json<T>(value: Value) -> Result<Value, ApiError>
where
    T: DeserializeOwned + Serialize,
{
    let dto = serde_json::from_value::<T>(value).map_err(|_| ApiError::internal())?;
    serde_json::to_value(dto).map_err(|_| ApiError::internal())
}
