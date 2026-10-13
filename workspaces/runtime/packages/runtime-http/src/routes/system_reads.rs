//! System reads that can be served from local state or the local filesystem.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;
use studio_runtime_services::system_reads::{self, SystemReadError};

use super::extractors::QueryParams;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get};
use crate::error::ApiError;
use crate::router::HttpState;

const REMOTE_BROWSE_DEPENDENCY: [Dependency; 2] = [
    Dependency::new("multi-server/discovery-pairing"),
    Dependency::new("multi-server/folder-browse"),
];
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
            let mut connection = store.read().map_err(|_| ApiError::internal())?;
            let tx = connection.transaction().map_err(|_| ApiError::internal())?;
            use studio_runtime_store::system_reads as records;
            let client =
                records::get(&tx, "runtime_access_clients", &server).map_err(ApiError::stored)?;
            if let Some(client) = client {
                // The table schema has checked the complete access record;
                // directories consumes only this typed pairing projection.
                #[derive(serde::Deserialize)]
                struct AccessSelection {
                    kind: String,
                    status: String,
                    revoked: Option<f64>,
                }
                let client: AccessSelection = serde_json::from_value(client).map_err(|_| {
                    ApiError::stored(records::invalid("runtime_access_clients", &server))
                })?;
                return Ok::<bool, ApiError>(
                    client.kind == "server"
                        && client.status == "paired"
                        && client.revoked.unwrap_or(0.0) == 0.0,
                );
            }
            if records::get(&tx, "runtime_access_discovered", &server)
                .map_err(ApiError::stored)?
                .is_some()
            {
                return Err(ApiError::new(
                    StatusCode::NOT_IMPLEMENTED,
                    "Not implemented: requires multi-server/discovery-pairing",
                ));
            }
            Ok(false)
        })
        .await
        .map_err(|_| ApiError::internal())??;
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
    super::generated_json::<crate::generated::DirectoriesResponse>(
        serde_json::to_value(result).map_err(|_| ApiError::internal())?,
    )
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
            Some(
                driver
                    .listing_snapshot()
                    .map_err(|_| ApiError::internal())?,
            )
        } else {
            account_policy.map(|policy| policy.accounts())
        };
        let mut connection = store.read().map_err(|_| ApiError::internal())?;
        let transaction = connection.transaction().map_err(|_| ApiError::internal())?;
        let value = system_reads::ui_summary(&transaction, accounts).map_err(ApiError::stored)?;
        super::generated_json::<crate::generated::UiSummaryResponse>(value)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(value))
}

fn directory_error(error: SystemReadError) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
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

fn dependency_error(method: &str, path: &str, deps: &[Dependency]) -> ApiError {
    dependency_unavailable(method, path, deps).unwrap_or_else(|_| ApiError::internal())
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name:"system_reads", routes:Some(routes), dependencies:&[
            "multi-server/discovery-pairing","multi-server/folder-browse",
            "runtime-process/process-tree","runtime-process/host-resources","runtime-process/resource-attribution","provider-session/native-accounts","runtime-process/studio-loaded-threads","runtime-process/live-queues","runtime-process/lock-samples","runtime-store/sqlite-contention","runtime-http/request-metrics","runtime-process/analytics-capture","runtime-process/supervisor-status",
        ],
    }
}
