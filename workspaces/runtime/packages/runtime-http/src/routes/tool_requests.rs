//! Thin adapters over the synchronous receipt action service.
use super::extractors::{ApiJson, QueryParams};
use super::manifest::{Dependency, OperationStatus, RouteGroup, get, post};
use crate::{error::ApiError, router::HttpState};
use axum::{Json, extract::State, http::StatusCode};
use serde_json::Value;
use studio_runtime_services::tool_requests::{self, Action, ToolRequestError};
const DEPENDENCIES: [Dependency; 3] = [
    Dependency::new("multi-server/move-receipt"),
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
async fn read(State(state): State<HttpState>, query: QueryParams) -> Result<Json<Value>, ApiError> {
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
) -> Result<Json<Value>, ApiError> {
    run(state, body.agent, Some(body.request_id), true).await
}
async fn run(
    state: HttpState,
    actor: String,
    id: Option<String>,
    cancel: bool,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let value = tokio::task::spawn_blocking(move || {
        store.transaction_with_error(|tx| {
            let action = match (id.as_deref(), cancel) {
                (Some(id), true) => Action::Cancel(id),
                (Some(id), false) => Action::Get(id),
                _ => Action::List,
            };
            let now = studio_runtime_services::event_write::unix_time()
                .map_err(|_| ToolRequestError::InvalidRequest("The system clock is unavailable"))?;
            let value = tool_requests::execute(tx, &actor, action, now)?;
            let canonical = value["id"].as_str().unwrap_or("list").to_owned();
            let response = if cancel {
                super::generated_json::<
                    crate::generated::CancelToolRequestApiToolRequestsCancelPostResponse,
                >(value)
            } else {
                super::generated_json::<crate::generated::ToolRequestsApiToolRequestsGetResponse>(
                    value,
                )
            };
            response.map_err(|_| {
                ToolRequestError::Store(studio_runtime_store::system_reads::invalid(
                    "runtime_tool_requests",
                    &canonical,
                ))
            })
        })
    })
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(error)?;
    Ok(Json(value))
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
        name:"tool_requests", routes:Some(routes), dependencies:&["multi-server/move-receipt","payload-store/tool-request-recovery","store-bootstrap/tool-request-schema"],
    }
}
