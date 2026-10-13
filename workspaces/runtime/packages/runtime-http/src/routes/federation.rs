//! Federation database projections and the documented 404 counterparts.

use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use studio_runtime_services::federation::{self, FederationError};

use crate::error::ApiError;
use crate::router::HttpState;

const EXCHANGE_DEPENDENCIES: [Dependency; 2] = [
    Dependency::new("federation-driver/rooms"),
    Dependency::new("remote-transport/federation-exchange"),
];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/federation",
            OperationStatus::Implemented,
            no_get_counterpart,
        ),
        post(
            "/api/federation",
            OperationStatus::partial(&EXCHANGE_DEPENDENCIES),
            manage,
        ),
        get(
            "/api/federation/v1/message",
            OperationStatus::Implemented,
            no_get_counterpart,
        ),
        post(
            "/api/federation/v1/message",
            OperationStatus::partial(&EXCHANGE_DEPENDENCIES),
            message,
        ),
        get(
            "/api/federation/v1/pair",
            OperationStatus::Implemented,
            no_get_counterpart,
        ),
        post(
            "/api/federation/v1/pair",
            OperationStatus::partial(&EXCHANGE_DEPENDENCIES),
            pair,
        ),
        get(
            "/api/federation/v1/pull",
            OperationStatus::Implemented,
            no_get_counterpart,
        ),
        post(
            "/api/federation/v1/pull",
            OperationStatus::partial(&EXCHANGE_DEPENDENCIES),
            pull,
        ),
        get(
            "/api/federation/v1/status",
            OperationStatus::Implemented,
            no_get_counterpart,
        ),
        post(
            "/api/federation/v1/status",
            OperationStatus::partial(&EXCHANGE_DEPENDENCIES),
            signed_status,
        ),
    ])
}

async fn no_get_counterpart() -> Result<Json<Value>, ApiError> {
    Err(ApiError::new(StatusCode::NOT_FOUND, "Not found"))
}

async fn message() -> Result<Json<Value>, ApiError> {
    Err(dependency_error("POST", "/api/federation/v1/message"))
}

async fn pair() -> Result<Json<Value>, ApiError> {
    Err(dependency_error("POST", "/api/federation/v1/pair"))
}

async fn pull() -> Result<Json<Value>, ApiError> {
    Err(dependency_error("POST", "/api/federation/v1/pull"))
}

async fn signed_status() -> Result<Json<Value>, ApiError> {
    Err(dependency_error("POST", "/api/federation/v1/status"))
}

async fn manage(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match action {
        "status" => {
            let store = state.store.clone().ok_or_else(ApiError::internal)?;
            let snapshot = tokio::task::spawn_blocking(move || federation::snapshot(&store))
                .await
                .map_err(|_| ApiError::internal())?
                .map_err(map_service_error)?;
            Ok(Json(super::generated_json::<
                crate::generated::FederationSnapshot,
            >(snapshot)?))
        }
        "set_enabled" => {
            let enabled = body
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or_else(|| ApiError::validation(vec![json!("body"), json!("enabled")]))?;
            if enabled {
                return Err(dependency_error("POST", "/api/federation"));
            }
            let store = state.store.clone().ok_or_else(ApiError::internal)?;
            tokio::task::spawn_blocking(move || federation::set_enabled(&store, false))
                .await
                .map_err(|_| ApiError::internal())?
                .map_err(map_service_error)?;
            let store = state.store.clone().ok_or_else(ApiError::internal)?;
            let snapshot = tokio::task::spawn_blocking(move || federation::snapshot(&store))
                .await
                .map_err(|_| ApiError::internal())?
                .map_err(map_service_error)?;
            Ok(Json(super::generated_json::<
                crate::generated::FederationSnapshot,
            >(snapshot)?))
        }
        "revoke_peer" => {
            let _ = body
                .get("state_id")
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::validation(vec![json!("body"), json!("state_id")]))?;
            Err(dependency_error("POST", "/api/federation"))
        }
        _ => Err(dependency_error("POST", "/api/federation")),
    }
}

fn dependency_error(method: &str, path: &str) -> ApiError {
    dependency_unavailable(method, path, &EXCHANGE_DEPENDENCIES)
        .unwrap_or_else(|_| ApiError::internal())
}

fn map_service_error(error: FederationError) -> ApiError {
    match error {
        FederationError::Store(error) => {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
        }
    }
}
