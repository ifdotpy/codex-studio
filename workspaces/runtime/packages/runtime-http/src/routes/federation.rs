//! Federation database projections and signed peer HTTP routes.

use super::extractors::ApiJson;
use super::manifest::{OperationStatus, RouteGroup, post};
use axum::Json;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use serde_json::Value;
use studio_runtime_services::federation::{self, FederationError};
use studio_runtime_services::federation_effects;

use crate::error::ApiError;
use crate::router::HttpState;

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post("/api/federation", OperationStatus::Implemented, manage),
        post(
            "/api/federation/v1/message",
            OperationStatus::Implemented,
            message,
        ),
        post(
            "/api/federation/v1/pair",
            OperationStatus::Implemented,
            pair,
        ),
        post(
            "/api/federation/v1/pull",
            OperationStatus::Implemented,
            pull,
        ),
        post(
            "/api/federation/v1/status",
            OperationStatus::Implemented,
            signed_status,
        ),
    ])
}

async fn message(
    State(state): State<HttpState>,
    request: Request<Body>,
) -> Result<Json<Value>, ApiError> {
    signed_post(
        state,
        request,
        federation_effects::FederationAction::Message,
    )
    .await
}

async fn pair(
    State(state): State<HttpState>,
    request: Request<Body>,
) -> Result<Json<Value>, ApiError> {
    signed_post(state, request, federation_effects::FederationAction::Pair).await
}

async fn pull(
    State(state): State<HttpState>,
    request: Request<Body>,
) -> Result<Json<Value>, ApiError> {
    signed_post(state, request, federation_effects::FederationAction::Pull).await
}

async fn signed_status(
    State(state): State<HttpState>,
    request: Request<Body>,
) -> Result<Json<Value>, ApiError> {
    signed_post(state, request, federation_effects::FederationAction::Status).await
}

async fn signed_post(
    state: HttpState,
    request: Request<Body>,
    action: federation_effects::FederationAction,
) -> Result<Json<Value>, ApiError> {
    let (parts, body) = request.into_parts();
    let raw = to_bytes(
        body,
        studio_runtime_services::federation_limits::FEDERATION_BODY_LIMIT,
    )
    .await
    .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "Invalid federation request body"))?;
    let store = state.store.ok_or_else(ApiError::internal)?;
    federation_effects::ensure_enabled(&store)
        .await
        .map_err(map_signed_error)?;

    let peer_id = single_header(&parts.headers, "x-studio-federation-state")?;
    let timestamp = single_header(&parts.headers, "x-studio-federation-time")?
        .parse::<i64>()
        .map_err(|_| ApiError::new(StatusCode::FORBIDDEN, "Invalid federation timestamp"))?;
    let nonce = single_header(&parts.headers, "x-studio-federation-nonce")?;
    let signature = single_header(&parts.headers, "x-studio-federation-signature")?;
    let observed_user = optional_single_header(&parts.headers, "tailscale-user-login")?;
    let origin = federation_effects::OriginObservation::from_trusted_serve(observed_user);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| ApiError::internal())?
        .as_secs_f64();
    let result = federation_effects::handle(
        store,
        federation_effects::SignedRequest {
            action,
            peer_id: &peer_id,
            timestamp,
            nonce: &nonce,
            signature: &signature,
            raw: &raw,
        },
        origin,
        now,
    )
    .await
    .map_err(map_signed_error)?;
    Ok(Json(result))
}

fn single_header(headers: &HeaderMap, name: &'static str) -> Result<String, ApiError> {
    let values = headers.get_all(name);
    if values.iter().count() != 1 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Invalid federation headers",
        ));
    }
    values
        .iter()
        .next()
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid federation headers"))
}

fn optional_single_header(
    headers: &HeaderMap,
    name: &'static str,
) -> Result<Option<String>, ApiError> {
    let values = headers.get_all(name);
    if values.iter().count() > 1 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Invalid federation headers",
        ));
    }
    values
        .iter()
        .next()
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "Invalid federation headers"))
        })
        .transpose()
}

fn map_signed_error(
    error: studio_runtime_services::federation_messaging::FederationMessageError,
) -> ApiError {
    use studio_runtime_services::federation_messaging::FederationMessageError as Error;
    match error {
        Error::Invalid(_) => ApiError::new(StatusCode::BAD_REQUEST, "Invalid federation request"),
        Error::Forbidden(_) => ApiError::new(StatusCode::FORBIDDEN, "Federation request refused"),
        Error::Conflict(_) => ApiError::new(
            StatusCode::CONFLICT,
            "Federation request conflicts with stored state",
        ),
        Error::Unavailable(dependency) => ApiError::dependency(dependency),
        Error::Store(_) | Error::Event(_) => ApiError::internal(),
        Error::Transport(_) => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Federation transport input is invalid",
        ),
    }
}

async fn manage(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let origin = state.public_origin.clone();
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    match action.as_str() {
        "create_invite" => {
            let store = state.store.clone().ok_or_else(ApiError::internal)?;
            let whoami = state.tailscale_whoami.clone();
            let observed = match (origin.as_ref(), whoami) {
                (Some(_), Some(probe)) => probe().await,
                _ => None,
            };
            let result = federation::manage_with_origin_and_whoami(
                store,
                &body,
                origin.as_deref(),
                observed.as_ref(),
            )
            .await
            .map_err(map_service_error)?;
            Ok(Json(result))
        }
        "status" => {
            let store = state.store.clone().ok_or_else(ApiError::internal)?;
            let snapshot = federation::snapshot(&store)
                .await
                .map_err(map_service_error)?;
            let snapshot = super::generated_json::<crate::generated::FederationSnapshot>(snapshot)?;
            Ok(Json(
                serde_json::to_value(snapshot).map_err(|_| ApiError::internal())?,
            ))
        }
        "set_enabled" | "approve_peer" | "revoke_peer" | "room_options" | "create_room"
        | "approve_room" | "accept_peer" => {
            let store = state.store.clone().ok_or_else(ApiError::internal)?;
            let queue_signal = matches!(action.as_str(), "create_room" | "approve_room");
            let wake = state.event_wake.clone();
            let result = federation::manage_with_origin(store, &body, origin.as_deref())
                .await
                .map_err(map_service_error)?;
            if queue_signal {
                wake.notify_one();
            }
            if action == "create_invite" {
                return Ok(Json(result));
            }
            let snapshot = super::generated_json::<crate::generated::FederationSnapshot>(result)?;
            Ok(Json(
                serde_json::to_value(snapshot).map_err(|_| ApiError::internal())?,
            ))
        }
        _ => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Unknown federation action",
        )),
    }
}

fn map_service_error(error: FederationError) -> ApiError {
    match error {
        FederationError::Store(studio_runtime_store::StoreError::SchemaMismatch(_)) => {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Federation data is unavailable",
            )
        }
        FederationError::Store(error) => ApiError::stored(error),
        FederationError::Refused(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
        FederationError::NeedsIdentity => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Local federation identity is missing",
        ),
        FederationError::Credentials(_) => ApiError::internal(),
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "federation",
        routes: Some(routes),
        dependencies: &[],
    }
}
