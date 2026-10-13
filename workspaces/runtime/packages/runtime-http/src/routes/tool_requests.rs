//! Tool request endpoints whose durable and live recovery drivers are not yet available.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;
use studio_runtime_services::tool_requests::{self, ToolRequestError};

use super::extractors::{ApiJson, QueryParams};
use super::manifest::{Dependency, OperationStatus, RouteGroup, get, post};
use crate::error::ApiError;
use crate::router::HttpState;

const LIST_DEPENDENCIES: [Dependency; 1] = [Dependency::new("payload-store/tool-request-list")];
const CANCEL_DEPENDENCIES: [Dependency; 1] = [Dependency::new("tool-driver/request-lifecycle")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/tool-requests",
            OperationStatus::partial(&LIST_DEPENDENCIES),
            list,
        ),
        post(
            "/api/tool-requests/cancel",
            OperationStatus::partial(&CANCEL_DEPENDENCIES),
            cancel,
        ),
    ])
}

async fn list(State(state): State<HttpState>, query: QueryParams) -> Result<Json<Value>, ApiError> {
    let agent = query
        .typed_value("agent")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let request_id = query
        .typed_value("request_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let store = state.store.ok_or_else(ApiError::internal)?;
    let value = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let value =
            tool_requests::read(&connection, &agent, request_id.as_deref()).map_err(tool_error)?;
        super::generated_json::<crate::generated::ToolRequestsApiToolRequestsGetResponse>(value)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(value))
}

async fn cancel(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let agent = body
        .get("agent")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let request_id = body
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let store = state.store.ok_or_else(ApiError::internal)?;
    let value =
        tokio::task::spawn_blocking(move || {
            let value = store
                .transaction_with_error(|transaction| {
                    tool_requests::cancel(transaction, &agent, &request_id)
                })
                .map_err(tool_error)?;
            super::generated_json::<
                crate::generated::CancelToolRequestApiToolRequestsCancelPostResponse,
            >(value)
        })
        .await
        .map_err(|_| ApiError::internal())??;
    Ok(Json(value))
}

fn tool_error(error: ToolRequestError) -> ApiError {
    match error {
        ToolRequestError::InvalidRequest(message) => {
            ApiError::new(StatusCode::BAD_REQUEST, message)
        }
        ToolRequestError::RequiresDependency(dependency) => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            format!("Not implemented: requires {dependency}"),
        ),
        ToolRequestError::InvalidStored { object, id } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {object}/{id}"),
        ),
        ToolRequestError::Store(_) => ApiError::internal(),
    }
}
