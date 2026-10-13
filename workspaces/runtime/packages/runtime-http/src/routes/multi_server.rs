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

const IDENTITY_DEPENDENCIES: [Dependency; 1] =
    [Dependency::new("server-credentials/local-identity")];
const SIGNED_DEPENDENCIES: [Dependency; 1] =
    [Dependency::new("remote-transport/signed-server-requests")];
const IDENTITY_WRITE_DEPENDENCIES: [Dependency; 1] =
    [Dependency::new("remote-transport/identity-write")];
const AUTO_PAIR_DEPENDENCIES: [Dependency; 2] = [
    Dependency::new("remote-transport/tailscale-discovery"),
    Dependency::new("remote-transport/owner-lookup"),
];
const OWNER_DEPENDENCIES: [Dependency; 1] = [Dependency::new("remote-transport/owner-lookup")];
const SIGNED_ORCHESTRATION_DEPENDENCIES: [Dependency; 2] = [
    Dependency::new("remote-transport/signed-server-requests"),
    Dependency::new("multi-server-runtime/orchestration"),
];
const TAILSCALE_DEPENDENCIES: [Dependency; 1] =
    [Dependency::new("remote-transport/tailscale-discovery")];
const INVITE_DEPENDENCIES: [Dependency; 2] = [
    Dependency::new("remote-transport/owner-lookup"),
    Dependency::new("federation-credentials/private-key"),
];
const MANAGEMENT_DEPENDENCIES: [Dependency; 7] = [
    Dependency::new("federation-credentials/private-key"),
    Dependency::new("multi-server-runtime/orchestration"),
    Dependency::new("remote-transport/identity-write"),
    Dependency::new("remote-transport/owner-lookup"),
    Dependency::new("remote-transport/signed-server-requests"),
    Dependency::new("remote-transport/tailscale-discovery"),
    Dependency::new("server-credentials/local-identity"),
];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/multi-server",
            OperationStatus::partial(&IDENTITY_DEPENDENCIES),
            snapshot,
        ),
        get(
            "/api/multi-server/audit",
            OperationStatus::Implemented,
            audit,
        ),
        post(
            "/api/multi-server",
            OperationStatus::partial(&MANAGEMENT_DEPENDENCIES),
            manage,
        ),
        post(
            "/api/multi-server/v1/auto-pair",
            OperationStatus::partial(&AUTO_PAIR_DEPENDENCIES),
            auto_pair,
        ),
        get(
            "/api/multi-server/v1/identity",
            OperationStatus::partial(&OWNER_DEPENDENCIES),
            identity,
        ),
        post(
            "/api/multi-server/v1/name",
            OperationStatus::partial(&IDENTITY_WRITE_DEPENDENCIES),
            rename,
        ),
        get(
            "/api/multi-server/v1/pair",
            OperationStatus::Implemented,
            no_pair_get,
        ),
        post(
            "/api/multi-server/v1/pair",
            OperationStatus::partial(&SIGNED_DEPENDENCIES),
            pair,
        ),
        get(
            "/api/multi-server/v1/status",
            OperationStatus::partial(&SIGNED_DEPENDENCIES),
            status,
        ),
        post(
            "/api/servers/orchestration",
            OperationStatus::partial(&SIGNED_ORCHESTRATION_DEPENDENCIES),
            orchestration,
        ),
    ])
}

async fn snapshot(State(state): State<HttpState>) -> Result<Json<Value>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || multi_server::snapshot(&store))
        .await
        .map_err(|_| ApiError::internal())?;
    let result = result.map_err(|e| match e {
        MultiServerError::MissingIdentity => {
            unavailable_for("GET", "/api/multi-server", &IDENTITY_DEPENDENCIES)
        }
        other => map_service_error(other),
    })?;
    Ok(Json(super::generated_json::<
        crate::generated::AccessSnapshot,
    >(result)?))
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

async fn manage(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if action == "discover" && state.public_origin.is_none() {
        let store = state.store.ok_or_else(ApiError::internal)?;
        let result = tokio::task::spawn_blocking(move || multi_server::snapshot(&store))
            .await
            .map_err(|_| ApiError::internal())?
            .map_err(map_service_error)?;
        return Ok(Json(super::generated_json::<
            crate::generated::AccessSnapshot,
        >(result)?));
    }
    if action == "name" {
        return Err(unavailable_for(
            "POST",
            "/api/multi-server",
            &IDENTITY_WRITE_DEPENDENCIES,
        ));
    }
    if matches!(action, "settings" | "alias" | "revoke" | "unrevoke") {
        let store = state.store.ok_or_else(ApiError::internal)?;
        let result =
            tokio::task::spawn_blocking(move || multi_server::manage(&store, &body, "local"))
                .await
                .map_err(|_| ApiError::internal())?;
        let result = result.map_err(map_service_error)?;
        return Ok(Json(super::generated_json::<
            crate::generated::AccessSnapshot,
        >(result)?));
    }
    let deps: &'static [Dependency] = match action {
        "create_invite" => &INVITE_DEPENDENCIES,
        "discover" => &TAILSCALE_DEPENDENCIES,
        "accept_invite" | "ui_invite" => &SIGNED_DEPENDENCIES,
        _ => &MANAGEMENT_DEPENDENCIES,
    };
    Err(unavailable_for("POST", "/api/multi-server", deps))
}

async fn auto_pair(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(unavailable_for(
        "POST",
        "/api/multi-server/v1/auto-pair",
        &AUTO_PAIR_DEPENDENCIES,
    ))
}

async fn identity() -> Result<Json<Value>, ApiError> {
    Err(unavailable_for(
        "GET",
        "/api/multi-server/v1/identity",
        &OWNER_DEPENDENCIES,
    ))
}

async fn rename(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(unavailable_for(
        "POST",
        "/api/multi-server/v1/name",
        &IDENTITY_WRITE_DEPENDENCIES,
    ))
}

async fn no_pair_get() -> Result<Json<Value>, ApiError> {
    Err(ApiError::new(StatusCode::NOT_FOUND, "Not found"))
}

async fn pair(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(unavailable("POST", "/api/multi-server/v1/pair"))
}

async fn status() -> Result<Json<Value>, ApiError> {
    Err(unavailable_for(
        "GET",
        "/api/multi-server/v1/status",
        &SIGNED_DEPENDENCIES,
    ))
}

async fn orchestration(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(unavailable_for(
        "POST",
        "/api/servers/orchestration",
        &SIGNED_ORCHESTRATION_DEPENDENCIES,
    ))
}

fn unavailable(method: &str, path: &str) -> ApiError {
    dependency_unavailable(method, path, &SIGNED_DEPENDENCIES)
        .unwrap_or_else(|_| ApiError::internal())
}
fn unavailable_for(method: &str, path: &str, dependencies: &'static [Dependency]) -> ApiError {
    dependency_unavailable(method, path, dependencies).unwrap_or_else(|_| ApiError::internal())
}

fn map_service_error(error: MultiServerError) -> ApiError {
    match error {
        MultiServerError::Store(error) => {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
        }
        MultiServerError::Sqlite(error) => {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
        }
        MultiServerError::Refused(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
        MultiServerError::Conflict(message) => ApiError::new(StatusCode::CONFLICT, message),
        MultiServerError::MissingIdentity => {
            unavailable_for("GET", "/api/multi-server", &IDENTITY_DEPENDENCIES)
        }
        MultiServerError::NeedsSignedRequest => {
            unavailable_for("POST", "/api/multi-server", &SIGNED_DEPENDENCIES)
        }
    }
}
