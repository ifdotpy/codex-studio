//! Thin adapters over the synchronous receipt action service.
use super::extractors::{ApiJson, QueryParams};
use super::manifest::{OperationStatus, RouteGroup, get, post};
use crate::{error::ApiError, router::HttpState};
use axum::{Json, extract::State, http::StatusCode};
use serde_json::Value;
use studio_runtime_services::tool_requests::{self, Action, ToolRequestError};
pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get("/api/tool-requests", OperationStatus::Implemented, read),
        post(
            "/api/tool-requests/cancel",
            OperationStatus::Implemented,
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
    let state_root = store.root().to_owned();
    let payload_root = state_root.clone();
    let now =
        studio_runtime_services::event_write::unix_time().map_err(|_| ApiError::internal())?;
    let value = if cancel {
        store
            .transaction_with_error(move |tx| {
                let action = match (id.as_deref(), cancel) {
                    (Some(id), true) => Action::Cancel(id),
                    (Some(id), false) => Action::Get(id),
                    _ => Action::List,
                };
                tool_requests::execute_http(tx, &actor, action, now, &payload_root)
            })
            .await
            .map_err(error)?
    } else {
        let read_actor = actor.clone();
        let read_id = id.clone();
        let read_root = payload_root.clone();
        let read_result = store
            .read_async("runtime-http/routes/tool_requests", move |tx| {
                let action = match (read_id.as_deref(), cancel) {
                    (Some(id), true) => Action::Cancel(id),
                    (Some(id), false) => Action::Get(id),
                    _ => Action::List,
                };
                tool_requests::execute_http(tx, &read_actor, action, now, &read_root)
            })
            .await;
        match read_result {
            Err(ToolRequestError::Store(studio_runtime_store::StoreError::Sqlite(
                studio_runtime_store::SqliteError::SqliteFailure(sqlite_error, _),
            ))) if sqlite_error.code == studio_runtime_store::ErrorCode::ReadOnly => store
                .transaction_with_error(move |tx| {
                    let action = match (id.as_deref(), cancel) {
                        (Some(id), true) => Action::Cancel(id),
                        (Some(id), false) => Action::Get(id),
                        _ => Action::List,
                    };
                    tool_requests::execute_http(tx, &actor, action, now, &payload_root)
                })
                .await
                .map_err(error)?,
            result => result.map_err(error)?,
        }
    };
    let mut value = resolve_payload_response(&state_root, value)?;
    if let Some(result) = value.get_mut("result") {
        tool_requests::expire(result, now);
    }
    let canonical = value["id"].as_str().unwrap_or("list").to_owned();
    let value = if cancel {
        let response = super::generated_json::<
            crate::generated::CancelToolRequestApiToolRequestsCancelPostResponse,
        >(value)
        .map_err(|_| {
            ApiError::stored(studio_runtime_store::system_reads::invalid(
                "runtime_tool_requests",
                &canonical,
            ))
        })?;
        serde_json::to_value(response).map_err(|_| ApiError::internal())?
    } else {
        let response = super::generated_json::<
            crate::generated::ToolRequestsApiToolRequestsGetResponse,
        >(value)
        .map_err(|_| {
            ApiError::stored(studio_runtime_store::system_reads::invalid(
                "runtime_tool_requests",
                &canonical,
            ))
        })?;
        serde_json::to_value(response).map_err(|_| ApiError::internal())?
    };
    Ok(Json(value))
}

fn resolve_payload_response(state_root: &std::path::Path, value: Value) -> Result<Value, ApiError> {
    fn resolve(state_root: &std::path::Path, value: Value) -> Result<Value, ApiError> {
        let value = if value
            .as_object()
            .is_some_and(|object| object.contains_key("_payloadBlobs"))
        {
            studio_runtime_services::payload_blobs::resolve_record(state_root, &value)
                .map_err(|_| ApiError::internal())?
        } else {
            value
        };
        match value {
            Value::Array(values) => values
                .into_iter()
                .map(|value| resolve(state_root, value))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            Value::Object(values) => values
                .into_iter()
                .map(|(key, value)| Ok((key, resolve(state_root, value)?)))
                .collect::<Result<serde_json::Map<_, _>, ApiError>>()
                .map(Value::Object),
            value => Ok(value),
        }
    }
    resolve(state_root, value)
}
fn error(error: ToolRequestError) -> ApiError {
    match error {
        ToolRequestError::InvalidRequest(message) => {
            ApiError::new(StatusCode::BAD_REQUEST, message)
        }
        ToolRequestError::RequiresDependency(name) => ApiError::dependency(name),
        ToolRequestError::Payload(_) => ApiError::internal(),
        ToolRequestError::Store(error) => ApiError::stored(error),
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name:"tool_requests", routes:Some(routes), dependencies:&[],
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_payload_response;
    use serde_json::json;

    #[test]
    fn tool_request_response_resolves_nested_blob_payloads() {
        let root = tempfile::tempdir().expect("isolated state directory");
        let content = "recovered output ".repeat(5_000);
        let externalized =
            studio_runtime_services::payload_blobs::decide_externalize_result(&json!({
                "success": true,
                "contentItems": [{"type":"inputText", "text": content}],
            }))
            .expect("large result externalizes");
        studio_runtime_services::payload_blobs::write_blobs(root.path(), &externalized.writes)
            .expect("payload writes");
        let response = json!({"id":"request","result":externalized.record});

        let recovered = resolve_payload_response(root.path(), response)
            .expect("payload is restored after the transaction");

        assert_eq!(recovered["result"]["contentItems"][0]["text"], content);
        assert!(recovered["result"].get("_payloadBlobs").is_none());
    }
}
