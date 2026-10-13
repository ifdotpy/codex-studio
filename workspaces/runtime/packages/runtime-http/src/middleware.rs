use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr};
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures_util::FutureExt;
use subtle::ConstantTimeEq;

use crate::error::{ApiError, error_response, with_api_headers};
use crate::router::HttpState;
use crate::schema::{API_SCHEMA_HASH_HEADER, API_SCHEMA_MISMATCH_HEADER, api_schema_hash};
use crate::validation::RawRequest;

const DEFAULT_BODY_LIMIT: usize = 262_144;
const ASSET_BODY_LIMIT: usize = 28 * 1024 * 1024;
const VOICE_BODY_LIMIT: usize = 6 * 1024 * 1024;
const FEDERATION_BODY_LIMIT: usize = 256 * 1024;
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) async fn boundary(
    State(state): State<HttpState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let api = path.starts_with("/api/");
    let write = !matches!(method, Method::GET | Method::HEAD | Method::OPTIONS);
    if crate::routes::incoming_multi_server::needed(request.headers(), &path) {
        return crate::routes::incoming_multi_server::serve(state, request).await;
    }
    let headers = request.headers();
    let mut parsed_body = None;

    let peer_loopback = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|ConnectInfo(address)| address.ip().is_loopback());
    if api && method == Method::OPTIONS && is_frame_account_path(&path) && peer_loopback {
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if frame_account_origin(headers, host, state.port) {
            let allowed = [
                "content-type",
                "x-canvas-token",
                "x-studio-api-schema",
                "x-studio-request-id",
            ];
            let requested_method = headers
                .get("access-control-request-method")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");
            let requested_headers = headers
                .get("access-control-request-headers")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .split(',')
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty())
                .collect::<BTreeSet<_>>();
            if !requested_method.eq_ignore_ascii_case("POST")
                || requested_headers
                    .iter()
                    .any(|header| !allowed.contains(&header.as_str()))
            {
                return error_response(StatusCode::FORBIDDEN, "The frame request is not allowed");
            }
            let origin = headers.get(header::ORIGIN).cloned();
            let mut response = StatusCode::NO_CONTENT.into_response();
            if let Some(origin) = origin {
                response
                    .headers_mut()
                    .insert("access-control-allow-origin", origin);
            }
            response.headers_mut().insert(
                "access-control-allow-methods",
                HeaderValue::from_static("POST, OPTIONS"),
            );
            response.headers_mut().insert(
                "access-control-allow-headers",
                HeaderValue::from_static(
                    "content-type, x-canvas-token, x-studio-api-schema, x-studio-request-id",
                ),
            );
            response
                .headers_mut()
                .insert("access-control-max-age", HeaderValue::from_static("600"));
            response
                .headers_mut()
                .insert(header::VARY, HeaderValue::from_static("Origin"));
            return response;
        }
    }
    if api
        && !trusted(
            headers,
            write,
            &state.token,
            peer_loopback,
            state.port,
            &state.public_origin,
            &path,
        )
    {
        let message = if write {
            "Local origin and session token required"
        } else {
            "Local origin required"
        };
        return error_response(StatusCode::FORBIDDEN, message);
    }
    if api
        && method == Method::HEAD
        && !has_operation(Some("HEAD"), &path)
        && has_operation(None, &path)
    {
        return error_response(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed");
    }
    let renderer_hash = headers
        .get(API_SCHEMA_HASH_HEADER)
        .and_then(|value| value.to_str().ok());
    if api && write && renderer_hash.is_some_and(|value| value != api_schema_hash()) {
        let mut response = error_response(
            StatusCode::UPGRADE_REQUIRED,
            "Studio was updated. Reload this tab to continue.",
        );
        response
            .headers_mut()
            .insert(API_SCHEMA_MISMATCH_HEADER, HeaderValue::from_static("1"));
        return response;
    }
    if write && matches!(method, Method::POST | Method::PUT | Method::PATCH) {
        let has_json_schema = state
            .validators
            .has_body_schema(method.as_str(), &path)
            .unwrap_or(false);
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let limit = request_limit(&path);
        let declared = match headers.get(header::CONTENT_LENGTH) {
            None => None,
            Some(value) => match value
                .to_str()
                .ok()
                .and_then(|text| text.parse::<usize>().ok())
            {
                Some(size) => Some(size),
                None => {
                    return error_response(StatusCode::BAD_REQUEST, "Invalid request size");
                }
            },
        };
        if declared.is_some_and(|size| size > limit) {
            return error_response(StatusCode::PAYLOAD_TOO_LARGE, "Invalid request size");
        }
        let (parts, body) = request.into_parts();
        let bytes = match tokio::time::timeout(REQUEST_READ_TIMEOUT, to_bytes(body, limit)).await {
            Err(_) => {
                return error_response(StatusCode::REQUEST_TIMEOUT, "Request body timed out");
            }
            Ok(Err(_)) => {
                return error_response(StatusCode::PAYLOAD_TOO_LARGE, "Invalid request size");
            }
            Ok(Ok(bytes)) => bytes,
        };
        if declared.is_some_and(|declared| bytes.len() != declared) {
            return error_response(StatusCode::BAD_REQUEST, "Incomplete request");
        }
        if !bytes.is_empty()
            && !content_type
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/json")
        {
            return ApiError::json_transport(StatusCode::UNSUPPORTED_MEDIA_TYPE, "JSON required")
                .into_response();
        }
        if has_json_schema && !bytes.is_empty() {
            match crate::json_body::parse_bounded(&bytes, limit) {
                Err(_) => {
                    return ApiError::validation(vec![serde_json::Value::String(
                        "body".to_owned(),
                    )])
                    .into_response();
                }
                Ok(value) => {
                    parsed_body = Some(value);
                }
            }
        }
        request = Request::from_parts(parts, Body::from(bytes));
        if (path == "/api/sync/drafts" || request.headers().contains_key("x-canvas-workspace"))
            && request
                .headers()
                .get("x-canvas-workspace")
                .and_then(|value| value.to_str().ok())
                != Some(state.workspace_id.as_str())
        {
            return error_response(
                StatusCode::CONFLICT,
                "The server workspace changed. Reload before sending.",
            );
        }
    }

    if api {
        if let Some(template) = state.validators.template_for(method.as_str(), &path) {
            let query = query_values(request.uri().query().unwrap_or_default());
            let (header_values, invalid_headers) = validated_header_values(request.headers());
            let raw = RawRequest {
                body: parsed_body,
                query,
                headers: header_values,
                invalid_headers,
            };
            match state
                .validators
                .validate_route(method.as_str(), &template, raw)
            {
                Ok(validated) => {
                    request.extensions_mut().insert(validated);
                }
                Err(errors) => {
                    return ApiError::validation(errors.location).into_response();
                }
            }
        } else if method != Method::GET
            && has_operation(None, &path)
            && !has_operation(Some(method.as_str()), &path)
        {
            return error_response(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed");
        }
    }

    next.run(request).await
}

pub(crate) fn validated_header_values(
    source: &HeaderMap,
) -> (BTreeMap<String, Vec<String>>, BTreeSet<String>) {
    let mut values = BTreeMap::<String, Vec<String>>::new();
    let mut invalid = BTreeSet::new();
    for (name, value) in source {
        let name = name.as_str().to_ascii_lowercase();
        match value.to_str() {
            Ok(value) => values.entry(name).or_default().push(value.to_owned()),
            Err(_) => {
                invalid.insert(name);
            }
        }
    }
    (values, invalid)
}

fn has_operation(method: Option<&str>, path: &str) -> bool {
    crate::schema::operations().is_ok_and(|operations| {
        crate::schema::matching_template(
            operations
                .iter()
                .map(|(operation_method, template)| (operation_method.as_str(), template.as_str())),
            method,
            path,
        )
        .is_some()
    })
}

pub(crate) async fn response_safety(
    State(state): State<HttpState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let api = request.uri().path().starts_with("/api/");
    let request_id = request.headers().get("x-studio-request-id").cloned();
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    let if_none_match = request
        .headers()
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let etag_route = method == "GET"
        && crate::schema::matching_template(
            state
                .etag_operations
                .iter()
                .map(|(method, template)| (method.as_str(), template.as_str())),
            Some(&method),
            &path,
        )
        .is_some();
    let mut response = AssertUnwindSafe(next.run(request))
        .catch_unwind()
        .await
        .unwrap_or_else(|_| {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
        });
    if api {
        response = with_api_headers(response, request_id.as_ref());
    }
    if etag_route
        && response.status() == StatusCode::OK
        && response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json"))
    {
        let (parts, body) = response.into_parts();
        let body = match to_bytes(body, usize::MAX).await {
            Ok(body) => body,
            Err(_) => {
                let response =
                    error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error");
                return if api {
                    with_api_headers(response, request_id.as_ref())
                } else {
                    response
                };
            }
        };
        let etag = format!("\"{}\"", studio_runtime_store::sha256_hex(&body));
        let not_modified = if_none_match.iter().any(|value| {
            value.split(',').any(|candidate| {
                let candidate = candidate.trim();
                candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
            })
        });
        let mut response = Response::from_parts(parts, Body::from(body));
        if let Ok(value) = HeaderValue::from_str(&etag) {
            response.headers_mut().insert(header::ETAG, value);
        }
        if not_modified {
            *response.status_mut() = StatusCode::NOT_MODIFIED;
            *response.body_mut() = Body::empty();
            response
                .headers_mut()
                .insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
            response.headers_mut().remove(header::CONTENT_ENCODING);
        }
        return response;
    }
    response
}

#[cfg(test)]
mod response_safety_tests {
    use axum::Json;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use axum::middleware::from_fn_with_state;
    use axum::routing::get;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::{HttpState, middleware::response_safety};

    async fn rules() -> Json<Value> {
        Json(json!({"rules": []}))
    }

    async fn untagged() -> Json<Value> {
        Json(json!({"value": 1}))
    }

    fn state() -> HttpState {
        let mut state = HttpState::new(
            "token".to_owned(),
            "workspace".to_owned(),
            None,
            "state".to_owned(),
            1,
            None,
            None,
            None,
        );
        state
            .etag_operations
            .insert(("GET".to_owned(), "/api/rules".to_owned()));
        state
    }

    #[tokio::test]
    async fn declared_read_etag_supports_weak_if_none_match_and_304() {
        let state = state();
        let app = Router::new()
            .route("/api/rules", get(rules))
            .layer(from_fn_with_state(state, response_safety));
        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/rules")
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("response");
        assert_eq!(first.status(), StatusCode::OK);
        let etag = first.headers()[header::ETAG].clone();
        assert!(etag.to_str().is_ok_and(|value| value.starts_with('"')));

        let second = app
            .oneshot(
                Request::builder()
                    .uri("/api/rules")
                    .header(
                        header::IF_NONE_MATCH,
                        format!("W/{}", etag.to_str().expect("ETag header is ASCII")),
                    )
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("conditional response");
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(second.headers()[header::ETAG], etag);
        assert_eq!(second.headers()[header::CONTENT_LENGTH], "0");
    }

    #[tokio::test]
    async fn unregistered_etag_route_keeps_its_regular_json_response() {
        let app = Router::new()
            .route("/api/session", get(untagged))
            .layer(from_fn_with_state(state(), response_safety));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/session")
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::ETAG));
    }
}

fn query_values(query: &str) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut values = std::collections::BTreeMap::<String, Vec<String>>::new();
    for (name, value) in form_urlencoded::parse(query.as_bytes()) {
        values
            .entry(name.into_owned())
            .or_default()
            .push(value.into_owned());
    }
    values
}

pub(crate) fn trusted(
    headers: &HeaderMap,
    write: bool,
    token: &str,
    peer_loopback: bool,
    port: u16,
    public_origin: &Option<String>,
    path: &str,
) -> bool {
    if !peer_loopback {
        return false;
    }
    let host = headers.get_all(header::HOST);
    if host.iter().count() != 1
        || ["x-forwarded-host", "x-forwarded-proto", "x-forwarded-for"]
            .iter()
            .any(|name| headers.get_all(*name).iter().count() > 1)
    {
        return false;
    }
    let host = host
        .iter()
        .next()
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let local_hosts = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    let forwarded = ["x-forwarded-host", "x-forwarded-proto", "x-forwarded-for"]
        .iter()
        .any(|name| headers.contains_key(*name));
    if is_frame_account_path(path) && !forwarded && frame_account_origin(headers, host, port) {
        if write {
            let supplied = headers
                .get("x-canvas-token")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            return supplied.as_bytes().ct_eq(token.as_bytes()).into();
        }
        return true;
    }
    if headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) == Some("cross-site") {
        return false;
    }
    let request_origin = if forwarded {
        let Some(origin) = public_origin
            .as_deref()
            .filter(|origin| valid_public_origin(origin))
        else {
            return false;
        };
        let authority = origin.strip_prefix("https://").unwrap_or_default();
        let forwarded_source = headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<IpAddr>().ok());
        if headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            != Some("https")
            || headers
                .get("x-forwarded-host")
                .and_then(|v| v.to_str().ok())
                != Some(authority)
            || !local_hosts.contains(&host.to_owned()) && host != authority
            || !forwarded_source.is_some_and(is_tailnet_ip)
        {
            return false;
        }
        origin.to_owned()
    } else {
        if !local_hosts.iter().any(|local| local == host) {
            return false;
        }
        format!("http://{host}")
    };
    let origins = headers.get_all(header::ORIGIN);
    if origins.iter().count() > 1 {
        return false;
    }
    if let Some(origin) = origins.iter().next().and_then(|v| v.to_str().ok())
        && origin != request_origin
    {
        return false;
    }
    if write && !is_federation_path(path) {
        let supplied = headers
            .get("x-canvas-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        supplied.as_bytes().ct_eq(token.as_bytes()).into()
    } else {
        true
    }
}

fn valid_public_origin(origin: &str) -> bool {
    let Some(authority) = origin.strip_prefix("https://") else {
        return false;
    };
    if authority.is_empty()
        || authority
            .chars()
            .any(|char| matches!(char, '/' | '?' | '#' | '@'))
        || !authority.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b".-:".contains(&byte)
        })
    {
        return false;
    }
    let host = authority.split(':').next().unwrap_or_default();
    let valid_port = match authority.strip_prefix(host) {
        Some("") => true,
        Some(suffix) => suffix
            .strip_prefix(':')
            .and_then(|port| port.parse::<u16>().ok())
            .is_some_and(|port| port > 0),
        None => false,
    };
    valid_port && host.ends_with(".ts.net") && host.len() > ".ts.net".len()
}

fn is_tailnet_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => u32::from(address) & 0xffc0_0000 == 0x6440_0000,
        IpAddr::V6(address) => {
            u128::from(address) & (!0_u128 << 80) == 0xfd7a_115c_a1e0_0000_0000_0000_0000_0000
        }
    }
}

fn is_federation_path(path: &str) -> bool {
    matches!(
        path,
        "/api/federation/v1/pair"
            | "/api/federation/v1/status"
            | "/api/federation/v1/message"
            | "/api/federation/v1/pull"
    )
}

fn is_frame_account_path(path: &str) -> bool {
    matches!(
        path,
        "/api/accounts/login"
            | "/api/accounts/login/cancel"
            | "/api/accounts/claude/login"
            | "/api/accounts/claude/add"
            | "/api/accounts/claude/login/code"
            | "/api/accounts/claude/login/cancel"
    )
}

fn frame_account_origin(headers: &HeaderMap, host: &str, port: u16) -> bool {
    if headers.get_all(header::ORIGIN).iter().count() != 1 {
        return false;
    }
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Some(origin_host) = origin.strip_prefix("http://") else {
        return false;
    };
    let Some((name, origin_port)) = origin_host.rsplit_once(':') else {
        return false;
    };
    let suffix = format!(".localhost:{port}");
    let Some(subdomains) = name
        .strip_prefix("studio-")
        .and_then(|name| name.strip_suffix(".localhost"))
    else {
        return false;
    };
    if origin_port.parse::<u16>().ok() != Some(port)
        || !origin_host
            .to_ascii_lowercase()
            .eq(&host.to_ascii_lowercase())
        || !subdomains.split('.').all(|label| {
            (1..=50).contains(&label.len())
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || (b'2'..=b'7').contains(&byte))
        })
        || !origin_host.ends_with(&suffix)
    {
        return false;
    }
    true
}

pub(crate) fn request_limit(path: &str) -> usize {
    match path {
        "/api/assets" => ASSET_BODY_LIMIT,
        "/api/voice/audio" | "/api/projects" => VOICE_BODY_LIMIT,
        "/api/federation/v1/pair"
        | "/api/federation/v1/status"
        | "/api/federation/v1/message"
        | "/api/federation/v1/pull" => FEDERATION_BODY_LIMIT,
        _ => DEFAULT_BODY_LIMIT,
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use axum::middleware::from_fn_with_state;
    use axum::routing::get;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::response_safety;
    use crate::HttpState;

    async fn panic_handler() -> &'static str {
        panic!("test handler panic")
    }

    #[tokio::test]
    async fn panic_is_converted_to_the_common_server_error_envelope() {
        let app = Router::new()
            .route("/api/panic", get(panic_handler))
            .layer(from_fn_with_state(
                HttpState::new(
                    "token".to_owned(),
                    "workspace".to_owned(),
                    None,
                    "state".to_owned(),
                    1,
                    None,
                    None,
                    None,
                ),
                response_safety,
            ));
        let request = Request::builder()
            .uri("/api/panic")
            .header("x-studio-request-id", "request-1")
            .body(Body::empty())
            .expect("request builds");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(response.headers()["x-studio-request-id"], "request-1");
        assert!(
            response
                .headers()
                .contains_key(crate::schema::API_SCHEMA_HASH_HEADER)
        );
        let body: Value = serde_json::from_slice(
            &to_bytes(response.into_body(), 1024)
                .await
                .expect("body reads"),
        )
        .expect("JSON error envelope");
        assert_eq!(body, serde_json::json!({"error":"Internal server error"}));
    }
}
