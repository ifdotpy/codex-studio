//! System reads that can be served from local state or the local filesystem.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use studio_runtime_services::system_reads::{self, SystemReadError};

use super::extractors::QueryParams;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_error, get};
use crate::error::ApiError;
use crate::router::HttpState;

const REMOTE_BROWSE_DEPENDENCY: [Dependency; 1] = [Dependency::new("multi-server/folder-browse")];
const DIAGNOSTICS_DEPENDENCIES: [Dependency; 11] = [
    Dependency::new("runtime-process/process-tree"),
    Dependency::new("runtime-process/host-resources"),
    Dependency::new("runtime-process/resource-attribution"),
    Dependency::new("provider-session/native-accounts"),
    Dependency::new("runtime-process/studio-loaded-threads"),
    Dependency::new("runtime-process/live-queues"),
    Dependency::new("runtime-process/lock-samples"),
    Dependency::new("runtime-store/sqlite-contention"),
    Dependency::new("runtime-http/request-metrics"),
    Dependency::new("runtime-process/analytics-capture"),
    Dependency::new("runtime-process/supervisor-status"),
];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/directories",
            OperationStatus::partial(&REMOTE_BROWSE_DEPENDENCY),
            directories,
        ),
        get(
            "/api/diagnostics",
            OperationStatus::partial(&DIAGNOSTICS_DEPENDENCIES),
            diagnostics,
        ),
        get("/api/ui-summary", OperationStatus::Implemented, ui_summary),
    ])
}

async fn directories(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let server = query
        .typed_value("server")
        .and_then(Value::as_str)
        .unwrap_or("");
    let local = server.is_empty()
        || server == "local"
        || local_server_id(&state.state_dir).as_deref() == Some(server);
    if !local {
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let server = server.to_owned();
        let paired = tokio::task::spawn_blocking(move || {
            let connection = store.read().map_err(|_| ApiError::internal())?;
            let exists: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_access_clients')",
                [], |row| row.get(0),
            ).map_err(|_| ApiError::internal())?;
            if !exists { return Ok::<bool, ApiError>(false); }
            let mut statement = connection.prepare("SELECT record FROM runtime_access_clients WHERE id=?1")
                .map_err(|_| ApiError::internal())?;
            let mut rows = statement.query([&server]).map_err(|_| ApiError::internal())?;
            let Some(row) = rows.next().map_err(|_| ApiError::internal())? else { return Ok(false); };
            let record: String = row.get(0).map_err(|_| ApiError::internal())?;
            let value: Value = serde_json::from_str(&record).map_err(|_| ApiError::internal())?;
            Ok(value.get("kind").and_then(Value::as_str) == Some("server")
                && value.get("revokedAt").is_none_or(Value::is_null))
        }).await.map_err(|_| ApiError::internal())??;
        if !paired {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "The server is not paired or is revoked",
            ));
        }
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
        &DIAGNOSTICS_DEPENDENCIES,
    ))
}

async fn ui_summary(State(state): State<HttpState>) -> Result<Json<Value>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let account_driver = state.account_driver;
    let account_policy = state.account_policy;
    let value = tokio::task::spawn_blocking(move || {
        let accounts = if let Some(driver) = account_driver {
            let bytes = driver
                .registry_snapshot_bytes()
                .map_err(|_| ApiError::internal())?;
            Some(
                studio_runtime_services::account_policy::AccountPolicyState::accounts_snapshot(
                    Some(&bytes),
                )
                .map_err(|_| ApiError::internal())?,
            )
        } else {
            account_policy.map(|policy| policy.accounts())
        };
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let value =
            system_reads::ui_summary(&connection, accounts).map_err(|_| ApiError::internal())?;
        super::generated_json::<crate::generated::UiSummaryResponse>(value)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(value))
}

fn directory_error(error: SystemReadError) -> ApiError {
    match error {
        SystemReadError::UnavailableDirectory => {
            ApiError::new(StatusCode::BAD_REQUEST, "This directory is unavailable")
        }
        SystemReadError::PermissionDenied => {
            ApiError::new(StatusCode::BAD_REQUEST, "Permission denied")
        }
    }
}

fn local_server_id(state_dir: &str) -> Option<String> {
    let raw =
        std::fs::read_to_string(std::path::Path::new(state_dir).join("multi-server/identity.json"))
            .ok()?;
    serde_json::from_str::<Value>(&raw)
        .ok()?
        .get("serverId")?
        .as_str()
        .map(str::to_owned)
}
