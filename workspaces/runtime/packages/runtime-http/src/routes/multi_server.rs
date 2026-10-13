//! Multi-server local audit reads and explicit transport-dependent operations.

use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;
use studio_runtime_services::multi_server::{self, MultiServerError};

use crate::error::ApiError;
use crate::router::HttpState;

const REMOTE_DEPENDENCIES: [Dependency; 1] = [Dependency::new("remote-transport/multi-server")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/multi-server",
            OperationStatus::partial(&REMOTE_DEPENDENCIES),
            snapshot,
        ),
        get(
            "/api/multi-server/audit",
            OperationStatus::Implemented,
            audit,
        ),
        post(
            "/api/multi-server",
            OperationStatus::partial(&REMOTE_DEPENDENCIES),
            manage,
        ),
        post(
            "/api/multi-server/v1/auto-pair",
            OperationStatus::partial(&REMOTE_DEPENDENCIES),
            auto_pair,
        ),
        get(
            "/api/multi-server/v1/identity",
            OperationStatus::partial(&REMOTE_DEPENDENCIES),
            identity,
        ),
        post(
            "/api/multi-server/v1/name",
            OperationStatus::partial(&REMOTE_DEPENDENCIES),
            rename,
        ),
        get(
            "/api/multi-server/v1/pair",
            OperationStatus::Implemented,
            no_pair_get,
        ),
        post(
            "/api/multi-server/v1/pair",
            OperationStatus::partial(&REMOTE_DEPENDENCIES),
            pair,
        ),
        get(
            "/api/multi-server/v1/status",
            OperationStatus::partial(&REMOTE_DEPENDENCIES),
            status,
        ),
        post(
            "/api/servers/orchestration",
            OperationStatus::partial(&REMOTE_DEPENDENCIES),
            orchestration,
        ),
    ])
}

async fn snapshot() -> Result<Json<Value>, ApiError> {
    Err(unavailable("GET", "/api/multi-server"))
}

async fn audit(State(state): State<HttpState>) -> Result<Json<Value>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || multi_server::audit(&store))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(map_service_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::AccessAuditResponse,
    >(result)?))
}

async fn manage(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(unavailable("POST", "/api/multi-server"))
}

async fn auto_pair(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(unavailable("POST", "/api/multi-server/v1/auto-pair"))
}

async fn identity() -> Result<Json<Value>, ApiError> {
    Err(unavailable("GET", "/api/multi-server/v1/identity"))
}

async fn rename(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(unavailable("POST", "/api/multi-server/v1/name"))
}

async fn no_pair_get() -> Result<Json<Value>, ApiError> {
    Err(ApiError::new(StatusCode::NOT_FOUND, "Not found"))
}

async fn pair(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(unavailable("POST", "/api/multi-server/v1/pair"))
}

async fn status() -> Result<Json<Value>, ApiError> {
    Err(unavailable("GET", "/api/multi-server/v1/status"))
}

async fn orchestration(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(unavailable("POST", "/api/servers/orchestration"))
}

fn unavailable(method: &str, path: &str) -> ApiError {
    dependency_unavailable(method, path, &REMOTE_DEPENDENCIES)
        .unwrap_or_else(|_| ApiError::internal())
}

fn map_service_error(error: MultiServerError) -> ApiError {
    match error {
        MultiServerError::Store(error) => {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
        }
    }
}
