//! System reads that can be served from local state or the local filesystem.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use studio_runtime_services::system_reads::{self, SystemReadError};

use super::extractors::QueryParams;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get};
use crate::error::ApiError;
use crate::router::HttpState;

const REMOTE_BROWSE_DEPENDENCY: [Dependency; 1] = [Dependency::new("multi-server/folder-browse")];
const UI_SUMMARY_DEPENDENCY: [Dependency; 1] = [Dependency::new("system-reads/ui-summary")];
const DIAGNOSTICS_DEPENDENCY: [Dependency; 1] = [Dependency::new("runtime-process/diagnostics")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/directories",
            OperationStatus::partial(&REMOTE_BROWSE_DEPENDENCY),
            directories,
        ),
        get(
            "/api/diagnostics",
            OperationStatus::partial(&DIAGNOSTICS_DEPENDENCY),
            diagnostics,
        ),
        get(
            "/api/ui-summary",
            OperationStatus::partial(&UI_SUMMARY_DEPENDENCY),
            ui_summary,
        ),
    ])
}

async fn directories(
    State(_state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let server = query
        .typed_value("server")
        .and_then(Value::as_str)
        .filter(|server| *server != "local");
    if server.is_some() {
        return Err(dependency_error(
            "GET",
            "/api/directories",
            &REMOTE_BROWSE_DEPENDENCY,
        ));
    }
    let path = query
        .typed_value("path")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let result = tokio::task::spawn_blocking(move || system_reads::directories(path.as_deref()))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(directory_error)?;
    super::generated_json::<crate::generated::DirectoriesResponse>(json!({
        "path": result.path,
        "parent": result.parent,
        "directories": result.directories.into_iter().map(|entry| json!({
            "name": entry.name,
            "path": entry.path,
        })).collect::<Vec<_>>(),
    }))
    .map(Json)
}

async fn diagnostics() -> Result<Json<Value>, ApiError> {
    Err(dependency_error(
        "GET",
        "/api/diagnostics",
        &DIAGNOSTICS_DEPENDENCY,
    ))
}

async fn ui_summary() -> Result<Json<Value>, ApiError> {
    Err(dependency_error(
        "GET",
        "/api/ui-summary",
        &UI_SUMMARY_DEPENDENCY,
    ))
}

fn directory_error(error: SystemReadError) -> ApiError {
    match error {
        SystemReadError::UnavailableDirectory => {
            ApiError::new(StatusCode::BAD_REQUEST, "This directory is unavailable")
        }
    }
}

fn dependency_error(method: &str, path: &str, dependencies: &[Dependency]) -> ApiError {
    dependency_unavailable(method, path, dependencies).unwrap_or_else(|_| ApiError::internal())
}
