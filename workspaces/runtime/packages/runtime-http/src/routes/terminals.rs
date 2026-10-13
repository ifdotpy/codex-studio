//! Persisted terminal reads and session-dependent terminal operations.

use super::extractors::{ApiJson, QueryParams};
use super::manifest::{Dependency, OperationStatus, RouteGroup, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;
use studio_runtime_services::terminal_driver::{self, TerminalError};

use crate::error::ApiError;
use crate::router::HttpState;

const SESSION_DEPENDENCIES: [Dependency; 1] = [Dependency::new("provider-session/app-server")];

pub(crate) fn routes() -> RouteGroup {
    let mut routes = vec![
        get("/api/terminals", OperationStatus::Implemented, listing),
        get(
            "/api/terminals/output",
            OperationStatus::Implemented,
            output,
        ),
    ];
    routes.extend(
        [
            "/api/terminals/create",
            "/api/terminals/input",
            "/api/terminals/resize",
            "/api/terminals/rename",
            "/api/terminals/close",
        ]
        .into_iter()
        .map(|path| {
            post(
                path,
                OperationStatus::partial(&SESSION_DEPENDENCIES),
                refuse,
            )
        }),
    );
    RouteGroup::new(routes)
}

async fn listing(State(state): State<HttpState>) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || terminal_driver::listing(&store))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(terminal_error)?;
    Ok(Json(
        super::generated_json::<crate::generated::TerminalList>(result)?,
    ))
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
    Ok(Json(super::generated_json::<
        crate::generated::TerminalOutput,
    >(result)?))
}

async fn refuse(
    path: axum::extract::MatchedPath,
    ApiJson(_body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    Err(
        super::manifest::dependency_unavailable("POST", path.as_str(), &SESSION_DEPENDENCIES)
            .unwrap_or_else(|_| ApiError::internal()),
    )
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
        TerminalError::Store(studio_runtime_store::StoreError::InvalidStoredObject {
            table,
            id,
        }) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {table}/{id}"),
        ),
        _ => ApiError::internal(),
    }
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "terminals",
        routes: Some(routes),
        dependencies: &["provider-session/app-server"],
    }
}
