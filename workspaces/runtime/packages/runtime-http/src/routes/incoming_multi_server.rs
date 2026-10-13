//! Authenticated incoming owner-server edge. The retained host boundary is
//! optional; a missing adapter is an explicit no-write refusal.
use axum::Json;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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
    !path.starts_with("/api/federation/")
        && ((path == "/api/multi-server/v1/identity" && headers.contains_key("x-forwarded-for"))
            || (path.starts_with("/api/multi-server")
                && headers.contains_key("access-control-request-headers"))
            || (headers.contains_key("x-forwarded-for")
                && headers.contains_key("x-studio-request-id"))
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
    let Some(boundary) = state.multi_server_boundary else {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires remote-transport/owner-lookup",
        );
    };
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
        let Some(store) = state.store else {
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
        return match tokio::task::spawn_blocking(move || {
            boundary.discovery_identity(&store, &address, &logins)
        })
        .await
        {
            Ok(Ok(v)) => match super::generated_json::<crate::generated::DiscoveryIdentity>(v) {
                Ok(v) => Json(v).into_response(),
                Err(e) => e.into_response(),
            },
            Ok(Err(e)) => failure(e),
            Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
        };
    }
    let supported = matches!(
        (request.method().as_str(), request.uri().path()),
        ("GET", multi_server_auth::STATUS_PATH)
            | ("POST", multi_server_auth::PAIR_PATH)
            | ("POST", multi_server_auth::AUTO_PAIR_PATH)
    );
    if !supported {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires multi-server-runtime/signed-dispatch",
        );
    }
    let Some(store) = state.store else {
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
    let result = tokio::task::spawn_blocking(move || {
        boundary.handle_incoming(&store, &request, &address, &logins, now)
    })
    .await;
    match result {
        Ok(Ok(value)) => {
            let checked = if method == "GET" {
                super::generated_json::<crate::generated::ServerIdentityResponse>(value)
            } else {
                super::generated_json::<crate::generated::DevicePairResponse>(value)
            };
            match checked {
                Ok(v) => Json(v).into_response(),
                Err(e) => e.into_response(),
            }
        }
        Ok(Err(e)) => failure(e),
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
    }
}
fn failure(error: AccessError) -> Response {
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
