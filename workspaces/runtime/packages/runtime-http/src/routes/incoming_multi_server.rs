//! Authenticated incoming owner-server edge. The retained host boundary is
//! optional; a missing adapter is an explicit no-write refusal.
use axum::Json;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{net::SocketAddr, sync::Arc};
use studio_runtime_services::multi_server_auth::{self, AccessError, SignedRequest};

use crate::error::error_response;
use crate::router::HttpState;

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "incoming_multi_server",
        routes: None,
        dependencies: &[],
    }
}

pub(crate) fn needed(headers: &HeaderMap, path: &str) -> bool {
    // Ordinary authenticated browser writes carry request IDs too; signed
    // identity headers select this boundary.
    !path.starts_with("/api/federation/")
        && ((path == "/api/multi-server/v1/identity" && headers.contains_key("x-forwarded-for"))
            || (path.starts_with("/api/multi-server")
                && headers.contains_key("access-control-request-headers"))
            || [
                "x-studio-client",
                "x-studio-server",
                "x-studio-timestamp",
                "x-studio-nonce",
                "x-studio-signature",
            ]
            .iter()
            .any(|key| headers.contains_key(*key)))
}
pub(crate) async fn serve(state: HttpState, request: Request<Body>) -> Response {
    let mut response = serve_inner(state, request).await;
    response.headers_mut().insert(
        "access-control-allow-origin",
        axum::http::HeaderValue::from_static("*"),
    );
    response.headers_mut().insert("access-control-expose-headers",axum::http::HeaderValue::from_static("X-Studio-Server, X-Studio-Api-Schema, X-Studio-Api-Mismatch, ETag, Content-Disposition, X-Log-Truncated"));
    response
}
async fn serve_inner(state: HttpState, request: Request<Body>) -> Response {
    let peer_link_advert = state
        .peer_link_advert
        .read()
        .ok()
        .and_then(|advert| advert.clone());
    let peer_loopback = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|ConnectInfo(a)| a.ip().is_loopback());
    let mut proxy_headers = request.headers().clone();
    if proxy_headers.get_all("origin").iter().count() > 1 {
        return error_response(
            StatusCode::FORBIDDEN,
            "The Tailscale Serve origin is invalid",
        );
    }
    proxy_headers.remove("origin");
    proxy_headers.remove("sec-fetch-site");
    if !request.headers().contains_key("x-forwarded-for")
        || !crate::middleware::trusted(
            &proxy_headers,
            false,
            &state.token,
            peer_loopback,
            state.port,
            &state.public_origin,
            request.uri().path(),
        )
    {
        return error_response(
            StatusCode::FORBIDDEN,
            "Signed requests require Tailscale Serve HTTPS",
        );
    }
    if request
        .headers()
        .get_all("tailscale-user-login")
        .iter()
        .any(|v| v.to_str().is_err())
    {
        return error_response(StatusCode::FORBIDDEN, "The Tailscale identity is invalid");
    }
    let Some(boundary) = state.multi_server_boundary.clone() else {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires remote-transport/owner-lookup",
        );
    };
    let credentials = state.credentials.clone();
    let public_origin = state.public_origin.clone();
    let whoami = state.tailscale_whoami.clone();
    if request.method() == "OPTIONS" {
        const ALLOWED: &str = "content-type, accept, x-canvas-workspace, x-studio-api-schema, x-codex-sync-protocol, last-event-id, if-none-match, cache-control, range, x-studio-client, x-studio-server, x-studio-timestamp, x-studio-nonce, x-studio-request-id, x-studio-signature";
        let requested = request
            .headers()
            .get("access-control-request-headers")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if requested.split(',').any(|v| {
            !v.trim().is_empty()
                && !ALLOWED
                    .split(", ")
                    .any(|a| a.eq_ignore_ascii_case(v.trim()))
        }) {
            return error_response(
                StatusCode::FORBIDDEN,
                "The preflight requests an unsupported header",
            );
        }
        let mut response = StatusCode::NO_CONTENT.into_response();
        response.headers_mut().insert(
            "access-control-allow-headers",
            axum::http::HeaderValue::from_static(ALLOWED),
        );
        response.headers_mut().insert(
            "access-control-allow-methods",
            axum::http::HeaderValue::from_static("GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS"),
        );
        return response;
    }
    if request.method() == "GET" && request.uri().path() == "/api/multi-server/v1/identity" {
        let Some(store) = state.store.clone() else {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error");
        };
        let address = request
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let logins = request
            .headers()
            .get_all("tailscale-user-login")
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_owned))
            .collect::<Vec<_>>();
        return match boundary.discovery_identity(store, &address, &logins).await {
            Ok(v) => match super::generated_json::<crate::generated::DiscoveryIdentity>(v) {
                Ok(v) => Json(v).into_response(),
                Err(e) => e.into_response(),
            },
            Err(e) => failure(e),
        };
    }
    let supported = matches!(
        (request.method().as_str(), request.uri().path()),
        ("GET", multi_server_auth::STATUS_PATH)
            | ("POST", multi_server_auth::PAIR_PATH)
            | ("POST", multi_server_auth::AUTO_PAIR_PATH)
            | ("POST", multi_server_auth::NAME_PATH)
            | ("POST", "/api/multi-server")
            | ("POST", "/api/servers/orchestration")
    );
    if !supported {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires multi-server-runtime/signed-dispatch",
        );
    }
    let Some(store) = state.store.clone() else {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error");
    };
    let lengths = request.headers().get_all("content-length");
    let declared = lengths
        .iter()
        .next()
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if lengths.iter().count() > 1 || request.headers().contains_key("transfer-encoding") {
        return error_response(
            StatusCode::BAD_REQUEST,
            "Use a single Content-Length for a signed request",
        );
    }
    if request.method() == "POST"
        && declared.is_none_or(|v| v > studio_runtime_services::remote_transport::MAX_REQUEST_BYTES)
    {
        return error_response(StatusCode::PAYLOAD_TOO_LARGE, "Invalid request size");
    }
    if request.method() == "GET" && declared.is_some_and(|v| v != 0) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "A read request must have an empty body",
        );
    }
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    let target = request
        .uri()
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let headers = request
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|v| (k.as_str().to_owned(), v.to_owned()))
        })
        .collect::<Vec<_>>();
    let address = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let logins = request
        .headers()
        .get_all("tailscale-user-login")
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_owned))
        .collect::<Vec<_>>();
    let bytes = match tokio::time::timeout(
        Duration::from_secs(10),
        to_bytes(
            request.into_body(),
            studio_runtime_services::remote_transport::MAX_REQUEST_BYTES,
        ),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => return error_response(StatusCode::PAYLOAD_TOO_LARGE, "Invalid request size"),
        Err(_) => return error_response(StatusCode::REQUEST_TIMEOUT, "Request body timed out"),
    };
    if declared.is_some_and(|v| v != bytes.len()) || (method == "GET" && !bytes.is_empty()) {
        return error_response(StatusCode::BAD_REQUEST, "Incomplete request");
    }
    let request = match SignedRequest::from_headers(&method, &target, &headers, &bytes) {
        Ok(v) => v,
        Err(e) => return failure(e),
    };
    let now = match SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs_f64())
    {
        Some(v) => v,
        None => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
    };
    let is_orchestration = request.is_orchestration();
    let is_name = request.is_name();
    let is_management_invite = request.is_management_invite();
    let updates_snapshot = !is_orchestration
        && (request.is_pairing() || request.is_auto_pairing() || is_management_invite);
    let advertise_link = is_orchestration
        || request.is_status()
        || request.is_pairing()
        || request.is_auto_pairing()
        || is_name
        || is_management_invite;
    let request_id = request.request_id().to_owned();
    let is_mutation = method == "POST";
    let notifications = state.resource_notifications.clone();
    let driver = state.orchestration_driver.clone();
    let prepared_server = if method == "POST"
        && (is_orchestration || is_name || is_management_invite)
    {
        match boundary
            .prepare_server_request_incoming(Arc::clone(&store), &request, &address, &logins, now)
            .await
        {
            Ok(prepared) => Some(prepared),
            Err(error) => return failure(error),
        }
    } else {
        None
    };
    let prepared_access =
        if method == "POST" && !is_orchestration && !is_name && !is_management_invite {
            match boundary
                .prepare_incoming(Arc::clone(&store), &request, &address, &logins, now)
                .await
            {
                Ok(prepared) => Some(prepared),
                Err(error) => return failure(error),
            }
        } else {
            None
        };
    let publish_notifications = Arc::clone(&notifications);
    let effect_request_id = request_id.clone();
    let tracked_route = format!("{method} {path}");
    let tracked_request_id = Some(request_id.clone());
    let effect = async move {
        if is_orchestration {
            let prepared = prepared_server.as_ref().ok_or_else(|| {
                multi_server_auth::refused(400, "invalid_request", "Invalid signed request")
            })?;
            let principal = boundary
                .commit_prepared_server_request(&store, prepared)
                .await?;
            let driver = driver.ok_or_else(|| {
                multi_server_auth::refused(
                    501,
                    "orchestration_unavailable",
                    "The cross-server orchestration receiver is unavailable",
                )
            })?;
            let outcome = driver
                .receive(&principal, request.body(), now as i64)
                .await?;
            Ok(outcome.to_value(&request_id))
        } else if is_name {
            let principal = boundary
                .commit_prepared_server_request(
                    &store,
                    prepared_server.as_ref().ok_or_else(|| {
                        multi_server_auth::refused(400, "invalid_request", "Invalid signed request")
                    })?,
                )
                .await?;
            let label = request
                .body()
                .get("label")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let notification_sink = Arc::clone(&notifications);
            credentials
                .rename_with_notification(
                    Arc::clone(&store),
                    "local",
                    &label,
                    &request_id,
                    &principal,
                    move |request_id| {
                        super::multi_server::publish_resource_change(&notification_sink, request_id)
                    },
                )
                .await
                .map_err(credential_error)?;
            credentials
                .identity(Arc::clone(&store), public_origin.as_deref())
                .await
                .map_err(credential_error)
        } else if is_management_invite {
            let actor = boundary
                .commit_prepared_server_request(
                    &store,
                    prepared_server.as_ref().ok_or_else(|| {
                        multi_server_auth::refused(400, "invalid_request", "Invalid signed request")
                    })?,
                )
                .await?;
            let identity = credentials
                .identity(Arc::clone(&store), public_origin.as_deref())
                .await
                .map_err(credential_error)?;
            let owner_value = match whoami {
                Some(probe) => probe().await,
                None => None,
            };
            let owner = owner_value
                .as_ref()
                .and_then(super::multi_server::tailscale_login)
                .ok_or_else(|| {
                    multi_server_auth::refused(
                        503,
                        "identity_unavailable",
                        "The Tailscale owner identity is unavailable",
                    )
                })?;
            studio_runtime_services::multi_server::create_invite(
                Arc::clone(&store),
                &identity,
                request.body(),
                &actor,
                Some(&owner),
            )
            .await
            .map_err(map_invite_error)
        } else {
            match prepared_access.as_ref() {
                Some(prepared) => boundary.apply_prepared_incoming(&store, prepared).await,
                None => {
                    boundary
                        .handle_incoming(Arc::clone(&store), &request, &address, &logins, now)
                        .await
                }
            }
        }
    };
    let peer_link_advert = peer_link_advert.clone();
    let finish = move |result: Result<serde_json::Value, AccessError>| {
        if updates_snapshot && result.is_ok() {
            // Publish after the durable write succeeds. Stable request IDs deduplicate retries.
            super::multi_server::publish_resource_change(
                &publish_notifications,
                &effect_request_id,
            );
        }
        match result {
            Ok(value) if is_orchestration => {
                let mut response = Json(value).into_response();
                if advertise_link {
                    add_link_advert(&mut response, peer_link_advert.as_deref());
                }
                response
            }
            Ok(value) => match validate_response(&method, is_name, is_management_invite, value) {
                Ok(v) => {
                    let mut response = Json(v).into_response();
                    if advertise_link {
                        add_link_advert(&mut response, peer_link_advert.as_deref());
                    }
                    response
                }
                Err(e) => e.into_response(),
            },
            Err(e) => failure(e),
        }
    };
    if is_mutation {
        state
            .run_tracked_mutation(tracked_route, tracked_request_id, async move {
                finish(effect.await)
            })
            .await
    } else {
        finish(effect.await)
    }
}

#[cfg(test)]
mod classification_tests {
    use axum::http::HeaderMap;
    use studio_runtime_services::multi_server_auth::SignedRequest;

    use super::needed;

    #[test]
    fn request_identity_alone_keeps_forwarded_browser_writes_on_the_local_api_route() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "100.64.0.1".parse().expect("header value"),
        );
        headers.insert(
            "x-studio-request-id",
            "request-1".parse().expect("header value"),
        );

        assert!(!needed(&headers, "/api/sync/preferences"));
    }

    #[test]
    fn partial_signed_credentials_still_use_the_fail_closed_signed_boundary() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "100.64.0.1".parse().expect("header value"),
        );
        headers.insert(
            "x-studio-request-id",
            "request-1".parse().expect("header value"),
        );
        headers.insert(
            "x-studio-signature",
            "partial".parse().expect("header value"),
        );

        assert!(needed(&headers, "/api/sync/preferences"));
        let signed_headers = headers
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            })
            .collect::<Vec<_>>();
        let error = SignedRequest::from_headers(
            "POST",
            "/api/sync/preferences",
            &signed_headers,
            br#"{"scope":"user","fields":{}}"#,
        )
        .err()
        .expect("partial signed headers are rejected");
        assert_eq!(error.status(), 401);
    }

    #[test]
    fn remote_discovery_identity_still_uses_the_incoming_boundary() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "100.64.0.1".parse().expect("header value"),
        );

        assert!(needed(&headers, "/api/multi-server/v1/identity"));
    }

    #[test]
    fn federation_paths_do_not_enter_the_multi_server_boundary() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-studio-signature",
            "partial".parse().expect("header value"),
        );

        assert!(!needed(&headers, "/api/federation/v1/message"));
    }
}

fn map_invite_error(error: studio_runtime_services::multi_server::MultiServerError) -> AccessError {
    use studio_runtime_services::multi_server::MultiServerError;
    match error {
        MultiServerError::Conflict(_) => multi_server_auth::refused(
            409,
            "request_conflict",
            "The request ID has different content",
        ),
        MultiServerError::Refused(_) => multi_server_auth::refused(
            400,
            "invalid_invite_request",
            "The invitation request is invalid",
        ),
        MultiServerError::NeedsSignedRequest => {
            multi_server_auth::refused(403, "server_required", "A paired server is required")
        }
        MultiServerError::Store(_) | MultiServerError::Sqlite(_) => {
            multi_server_auth::refused(503, "invite_unavailable", "The invitation is unavailable")
        }
    }
}

fn add_link_advert(response: &mut Response, advert: Option<&str>) {
    if response.status().is_success()
        && let Some(advert) = advert
        && let Ok(value) = axum::http::HeaderValue::from_str(advert)
    {
        response.headers_mut().insert("x-studio-link", value);
    }
}

#[derive(serde::Serialize)]
#[serde(untagged)]
enum IncomingMultiServerResponse {
    Identity(crate::generated::ServerIdentityResponse),
    Pair(crate::generated::DevicePairResponse),
    Invite(crate::generated::InvitationResponse),
}

fn validate_response(
    method: &str,
    is_name: bool,
    is_management_invite: bool,
    value: serde_json::Value,
) -> Result<IncomingMultiServerResponse, crate::error::ApiError> {
    if is_management_invite {
        super::generated_json::<crate::generated::InvitationResponse>(value)
            .map(IncomingMultiServerResponse::Invite)
    } else if method == "GET" || is_name {
        super::generated_json::<crate::generated::ServerIdentityResponse>(value)
            .map(IncomingMultiServerResponse::Identity)
    } else {
        super::generated_json::<crate::generated::DevicePairResponse>(value)
            .map(IncomingMultiServerResponse::Pair)
    }
}

pub(super) fn failure(error: AccessError) -> Response {
    let status = StatusCode::from_u16(error.status()).unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
    // Stored-object locations are operator diagnostics, not remote response data.
    match error {
        AccessError::Refused { message, code, .. } => (
            status,
            Json(serde_json::json!({"error":message,"code":code})),
        )
            .into_response(),
        _ => error_response(status, "The server access operation failed"),
    }
}

fn credential_error(
    error: studio_runtime_services::federation_credentials::CredentialError,
) -> AccessError {
    use studio_runtime_services::federation_credentials::CredentialError;
    match error {
        CredentialError::Conflict => multi_server_auth::refused(
            409,
            "request_conflict",
            "The request ID has different content",
        ),
        CredentialError::RemoteRequired => {
            multi_server_auth::refused(403, "server_required", "A paired server is required")
        }
        _ => multi_server_auth::refused(
            503,
            "identity_unavailable",
            "The server identity is unavailable",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{IncomingMultiServerResponse, validate_response};
    use serde_json::json;

    #[test]
    fn signed_name_post_validates_identity_response() {
        let response = json!({
            "serverId": "remote-server",
            "label": "Renamed",
            "origin": "https://remote.example",
            "publicKey": "key",
            "tailscaleUser": null
        });

        assert!(matches!(
            validate_response("POST", true, false, response),
            Ok(IncomingMultiServerResponse::Identity(_))
        ));
    }

    #[test]
    fn signed_pair_post_still_validates_pair_response() {
        let response = json!({
            "protocol": 1,
            "serverId": "remote-server",
            "clientId": "client",
            "label": "Remote",
            "origin": "https://remote.example",
            "publicKey": "key",
            "tailscaleUser": "owner@example.test",
            "paired": true
        });

        assert!(matches!(
            validate_response("POST", false, false, response),
            Ok(IncomingMultiServerResponse::Pair(_))
        ));
    }
}
