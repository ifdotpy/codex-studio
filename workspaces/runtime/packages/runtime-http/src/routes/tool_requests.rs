//! Thin adapters over the synchronous receipt action service.
use super::extractors::{ApiJson, QueryParams};
use super::manifest::{Dependency, OperationStatus, RouteGroup, get, post};
use crate::{error::ApiError, router::HttpState};
use axum::{Json, extract::State, http::StatusCode};
use serde_json::Value;
use studio_runtime_services::tool_requests::{self, Action, ToolRequestError};
const DEPENDENCIES: [Dependency; 2] = [
    Dependency::new("payload-store/tool-request-recovery"),
    Dependency::new("store-bootstrap/tool-request-schema"),
];
pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/tool-requests",
            OperationStatus::partial(&DEPENDENCIES),
            read,
        ),
        post(
            "/api/tool-requests/cancel",
            OperationStatus::partial(&DEPENDENCIES),
            cancel,
        ),
    ])
}
async fn read(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<ToolRequestResponse>, ApiError> {
    let actor = query
        .typed_value("agent")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let id = query
        .typed_value("request_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    run(state, actor, id, false).await
}
async fn cancel(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::ToolRequestCancelBody>,
) -> Result<Json<ToolRequestResponse>, ApiError> {
    run(state, body.agent, Some(body.request_id), true).await
}
async fn run(
    state: HttpState,
    actor: String,
    id: Option<String>,
    cancel: bool,
) -> Result<Json<ToolRequestResponse>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let value = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/tool_requests",
        move || {
            let now = studio_runtime_services::event_write::unix_time()
                .map_err(|_| ToolRequestError::InvalidRequest("The system clock is unavailable"))?;
            let operation = |tx: &studio_runtime_store::Transaction<'_>| {
                let action = match (id.as_deref(), cancel) {
                    (Some(id), true) => Action::Cancel(id),
                    (Some(id), false) => Action::Get(id),
                    _ => Action::List,
                };
                let value = tool_requests::execute(tx, &actor, action, now)?;
                let canonical = value["id"].as_str().unwrap_or("list").to_owned();
                let response = if cancel {
                    super::generated_json::<
                        crate::generated::CancelToolRequestApiToolRequestsCancelPostResponse,
                    >(value)
                    .map(ToolRequestResponse::Cancel)
                } else {
                    super::generated_json::<
                            crate::generated::ToolRequestsApiToolRequestsGetResponse,
                        >(value)
                        .map(ToolRequestResponse::ListOrGet)
                };
                response.map_err(|_| {
                    ToolRequestError::Store(studio_runtime_store::system_reads::invalid(
                        "runtime_tool_requests",
                        &canonical,
                    ))
                })
            };
            if cancel {
                store.transaction_with_error(operation)
            } else {
                match store.read_transaction_with_error(|tx| operation(tx)) {
                    Err(ToolRequestError::Store(studio_runtime_store::StoreError::Sqlite(
                        studio_runtime_store::SqliteError::SqliteFailure(error, _),
                    ))) if error.code == studio_runtime_store::ErrorCode::ReadOnly => {
                        store.transaction_with_error(|tx| operation(tx))
                    }
                    result => result,
                }
            }
        },
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(error)?;
    Ok(Json(value))
}

#[derive(serde::Serialize)]
#[serde(untagged)]
enum ToolRequestResponse {
    Cancel(crate::generated::CancelToolRequestApiToolRequestsCancelPostResponse),
    ListOrGet(crate::generated::ToolRequestsApiToolRequestsGetResponse),
}
fn error(error: ToolRequestError) -> ApiError {
    match error {
        ToolRequestError::InvalidRequest(message) => {
            ApiError::new(StatusCode::BAD_REQUEST, message)
        }
        ToolRequestError::RequiresDependency(name) => ApiError::dependency(name),
        ToolRequestError::Store(error) => ApiError::stored(error),
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name:"tool_requests", routes:Some(routes), dependencies:&["payload-store/tool-request-recovery","store-bootstrap/tool-request-schema"],
    }
}
