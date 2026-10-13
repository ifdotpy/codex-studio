//! Multi-server local audit reads and explicit transport-dependent operations.

use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;
use studio_runtime_services::federation_credentials::CredentialError;
use studio_runtime_services::multi_server::{self, MultiServerError};

use crate::error::ApiError;
use crate::router::HttpState;

const SIGNED_DEPENDENCIES: [Dependency; 1] =
    [Dependency::new("remote-transport/signed-server-requests")];
const AUTO_PAIR_DEPENDENCIES: [Dependency; 2] = [
    Dependency::new("remote-transport/tailscale-discovery"),
    Dependency::new("remote-transport/owner-lookup"),
];
const OWNER_DEPENDENCIES: [Dependency; 1] = [Dependency::new("remote-transport/owner-lookup")];
const SIGNED_ORCHESTRATION_DEPENDENCIES: [Dependency; 2] = [
    Dependency::new("remote-transport/signed-server-requests"),
    Dependency::new("multi-server-runtime/orchestration"),
];
const MANAGEMENT_DEPENDENCIES: [Dependency; 5] = [
    Dependency::new("multi-server-runtime/orchestration"),
    Dependency::new("multi-server-runtime/pairing"),
    Dependency::new("remote-transport/owner-lookup"),
    Dependency::new("remote-transport/signed-server-requests"),
    Dependency::new("remote-transport/tailscale-discovery"),
];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get("/api/multi-server", OperationStatus::Implemented, snapshot),
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
            OperationStatus::partial(&SIGNED_DEPENDENCIES),
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
    let credentials = state.credentials;
    let origin = state.public_origin;
    let store = state.store.ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || {
        let identity = credentials
            .identity(&store, origin.as_deref())
            .map_err(map_credential_error)?;
        multi_server::snapshot(&store, &identity).map_err(map_service_error)
    })
    .await
    .map_err(|_| ApiError::internal())?;
    let result = result?;
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
    if matches!(
        action,
        "settings" | "alias" | "revoke" | "unrevoke" | "name"
    ) || (action == "discover" && state.public_origin.is_none())
    {
        let store = state.store.ok_or_else(ApiError::internal)?;
        let credentials = state.credentials;
        let origin = state.public_origin;
        let result = tokio::task::spawn_blocking(move || {
            let action = body["action"].as_str().unwrap_or_default();
            if action == "alias"
                && !credentials
                    .is_local_target(&store, body["serverId"].as_str().unwrap_or_default())
                    .map_err(map_credential_error)?
            {
                return Err(unavailable_for(
                    "POST",
                    "/api/multi-server",
                    &MANAGEMENT_DEPENDENCIES,
                ));
            }
            if action == "name" {
                credentials
                    .rename(
                        &store,
                        body["serverId"].as_str().unwrap_or_default(),
                        body["label"].as_str().unwrap_or_default(),
                        body["requestId"].as_str().unwrap_or_default(),
                        "local",
                    )
                    .map_err(map_credential_error)?;
            }
            let identity = credentials
                .identity(&store, origin.as_deref())
                .map_err(map_credential_error)?;
            if matches!(action, "name" | "discover") {
                multi_server::snapshot(&store, &identity).map_err(map_service_error)
            } else {
                multi_server::manage(&store, &identity, &body, "local").map_err(map_service_error)
            }
        })
        .await
        .map_err(|_| ApiError::internal())??;
        return Ok(Json(super::generated_json::<
            crate::generated::AccessSnapshot,
        >(result)?));
    }
    Err(unavailable_for(
        "POST",
        "/api/multi-server",
        &MANAGEMENT_DEPENDENCIES,
    ))
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
        &SIGNED_DEPENDENCIES,
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

fn map_credential_error(error: CredentialError) -> ApiError {
    match error {
        CredentialError::Conflict => ApiError::new(StatusCode::CONFLICT, error.to_string()),
        CredentialError::Identity(_) => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Invalid stored object multi-server/identity.json",
        ),
        CredentialError::InvalidJournal => {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
        }
        CredentialError::RemoteRequired => {
            unavailable_for("POST", "/api/multi-server", &MANAGEMENT_DEPENDENCIES)
        }
        _ => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "The local server credentials are unavailable",
        ),
    }
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
        MultiServerError::NeedsSignedRequest => {
            unavailable_for("POST", "/api/multi-server", &MANAGEMENT_DEPENDENCIES)
        }
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "multi_server",
        routes: Some(routes),
        dependencies: &[
            "multi-server-runtime/orchestration",
            "multi-server-runtime/pairing",
            "remote-transport/owner-lookup",
            "remote-transport/signed-server-requests",
            "remote-transport/tailscale-discovery",
        ],
    }
}
