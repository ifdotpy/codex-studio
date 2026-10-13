//! Federation database projections and the documented 404 counterparts.

use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;
use studio_runtime_services::federation::{self, FederationError};

use crate::error::ApiError;
use crate::router::HttpState;

const SIGNING_DEPENDENCIES: [Dependency; 1] =
    [Dependency::new("federation-credentials/private-key")];
const INVITE_DEPENDENCIES: [Dependency; 2] = [
    Dependency::new("remote-transport/tailscale-identity"),
    Dependency::new("federation-credentials/key-generation"),
];
const ACCEPT_DEPENDENCIES: [Dependency; 2] = [
    Dependency::new("federation-credentials/invitation"),
    Dependency::new("remote-transport/signed-federation-exchange"),
];
const MANAGEMENT_DEPENDENCIES: [Dependency; 5] = [
    Dependency::new("federation-credentials/private-key"),
    Dependency::new("remote-transport/tailscale-identity"),
    Dependency::new("federation-credentials/key-generation"),
    Dependency::new("federation-credentials/invitation"),
    Dependency::new("remote-transport/signed-federation-exchange"),
];
const IDENTITY_DEPENDENCIES: [Dependency; 1] =
    [Dependency::new("federation-credentials/key-generation")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/federation",
            OperationStatus::Implemented,
            no_get_counterpart,
        ),
        post(
            "/api/federation",
            OperationStatus::partial(&MANAGEMENT_DEPENDENCIES),
            manage,
        ),
        get(
            "/api/federation/v1/message",
            OperationStatus::Implemented,
            no_get_counterpart,
        ),
        post(
            "/api/federation/v1/message",
            OperationStatus::partial(&SIGNING_DEPENDENCIES),
            message,
        ),
        get(
            "/api/federation/v1/pair",
            OperationStatus::Implemented,
            no_get_counterpart,
        ),
        post(
            "/api/federation/v1/pair",
            OperationStatus::partial(&SIGNING_DEPENDENCIES),
            pair,
        ),
        get(
            "/api/federation/v1/pull",
            OperationStatus::Implemented,
            no_get_counterpart,
        ),
        post(
            "/api/federation/v1/pull",
            OperationStatus::partial(&SIGNING_DEPENDENCIES),
            pull,
        ),
        get(
            "/api/federation/v1/status",
            OperationStatus::Implemented,
            no_get_counterpart,
        ),
        post(
            "/api/federation/v1/status",
            OperationStatus::partial(&SIGNING_DEPENDENCIES),
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
        "set_enabled" | "approve_peer" | "revoke_peer" | "room_options" | "create_room"
        | "approve_room" => {
            let store = state.store.clone().ok_or_else(ApiError::internal)?;
            let queue_signal = matches!(action, "create_room" | "approve_room");
            let wake = state.event_wake.clone();
            let result = tokio::task::spawn_blocking(move || federation::manage(&store, &body))
                .await
                .map_err(|_| ApiError::internal())?
                .map_err(map_service_error)?;
            if queue_signal {
                wake.notify_one();
            }
            Ok(Json(super::generated_json::<
                crate::generated::FederationSnapshot,
            >(result)?))
        }
        // The route is declared with MANAGEMENT_DEPENDENCIES; report missing
        // operation-specific services without violating that manifest contract.
        "create_invite" => Err(unavailable_for(&MANAGEMENT_DEPENDENCIES)),
        "accept_peer" => Err(unavailable_for(&ACCEPT_DEPENDENCIES)),
        _ => Err(unavailable_for(&SIGNING_DEPENDENCIES)),
    }
}

fn dependency_error(method: &str, path: &str) -> ApiError {
    dependency_unavailable(method, path, &SIGNING_DEPENDENCIES)
        .unwrap_or_else(|_| ApiError::internal())
}

fn unavailable_for(dependencies: &'static [Dependency]) -> ApiError {
    dependency_unavailable("POST", "/api/federation", dependencies)
        .unwrap_or_else(|_| ApiError::internal())
}

fn map_service_error(error: FederationError) -> ApiError {
    match error {
        FederationError::Store(error) => {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
        }
        FederationError::Refused(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
        FederationError::NeedsIdentity => unavailable_for(&IDENTITY_DEPENDENCIES),
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "federation",
        routes: Some(routes),
        dependencies: &[
            "federation-credentials/invitation",
            "federation-credentials/key-generation",
            "federation-credentials/private-key",
            "remote-transport/signed-federation-exchange",
            "remote-transport/tailscale-identity",
        ],
    }
}
