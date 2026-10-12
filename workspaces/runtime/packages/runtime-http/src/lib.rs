//! Frozen HTTP contract and its Axum compatibility boundary.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

#[allow(clippy::all)]
#[rustfmt::skip]
pub mod generated;

const OPENAPI_DOCUMENT: &[u8] = include_bytes!("../openapi.json");
pub const API_SCHEMA_HASH_HEADER: &str = "X-Studio-API-Schema";
pub const API_SCHEMA_MISMATCH_HEADER: &str = "X-Studio-API-Schema-Mismatch";
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self' https://api.openai.com https://*.ts.net:*; img-src 'self' data: blob: https: http:; media-src 'self' blob: data:; frame-src 'self' blob: http://*.localhost:*; frame-ancestors 'none'; base-uri 'none'";
const DEFAULT_BODY_LIMIT: usize = 262_144;
const ASSET_BODY_LIMIT: usize = 28 * 1024 * 1024;
const VOICE_BODY_LIMIT: usize = 6 * 1024 * 1024;
const FEDERATION_BODY_LIMIT: usize = 256 * 1024;
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);

static SCHEMA_HASH: OnceLock<String> = OnceLock::new();
static OPERATIONS: OnceLock<Result<BTreeSet<(String, String)>, serde_json::Error>> =
    OnceLock::new();

/// Hash the committed canonicalizer-v3 artifact. The artifact is itself the canonical byte sequence.
pub fn api_schema_hash() -> &'static str {
    SCHEMA_HASH.get_or_init(|| format!("{:x}", Sha256::digest(OPENAPI_DOCUMENT)))
}

/// Expose the operation set used by registration and conformance reporting.
pub fn operations() -> Result<&'static BTreeSet<(String, String)>, &'static serde_json::Error> {
    match OPERATIONS.get_or_init(|| {
        let document: Value = serde_json::from_slice(OPENAPI_DOCUMENT)?;
        let mut operations = BTreeSet::new();
        if let Some(paths) = document.get("paths").and_then(Value::as_object) {
            for (path, item) in paths {
                if let Some(methods) = item.as_object() {
                    for method in [
                        "get", "put", "post", "delete", "options", "head", "patch", "trace",
                    ] {
                        if methods.contains_key(method) {
                            operations.insert((method.to_uppercase(), path.clone()));
                        }
                    }
                }
            }
        }
        Ok(operations)
    }) {
        Ok(operations) => Ok(operations),
        Err(error) => Err(error),
    }
}

#[derive(Clone)]
pub struct HttpState {
    token: String,
    workspace_id: String,
    native_path: Option<String>,
    state_dir: String,
    port: u16,
    public_origin: Option<String>,
}

impl HttpState {
    pub fn new(
        token: String,
        workspace_id: String,
        native_path: Option<String>,
        state_dir: String,
        port: u16,
        public_origin: Option<String>,
    ) -> Self {
        Self {
            token,
            workspace_id,
            native_path,
            state_dir,
            port,
            public_origin,
        }
    }
}

pub fn router(state: HttpState) -> Router {
    Router::new()
        .fallback(any(dispatch))
        .layer(middleware::from_fn_with_state(state.clone(), boundary))
        .with_state(state)
}

async fn boundary(
    State(state): State<HttpState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let api = path.starts_with("/api/");
    let write = !matches!(method, Method::GET | Method::HEAD | Method::OPTIONS);
    let headers = request.headers();
    let request_id = headers.get("x-studio-request-id").cloned();

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
        return with_api_headers(response, headers.get("x-studio-request-id"));
    }
    if write && matches!(method, Method::POST | Method::PUT | Method::PATCH) {
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("application/json")
        {
            return with_api_headers(
                error_response(StatusCode::UNSUPPORTED_MEDIA_TYPE, "JSON required"),
                headers.get("x-studio-request-id"),
            );
        }
        let limit = request_limit(&path);
        let declared = match headers.get(header::CONTENT_LENGTH) {
            None => {
                return with_api_headers(
                    error_response(StatusCode::PAYLOAD_TOO_LARGE, "Invalid request size"),
                    headers.get("x-studio-request-id"),
                );
            }
            Some(value) => match value
                .to_str()
                .ok()
                .and_then(|text| text.parse::<usize>().ok())
            {
                Some(size) => size,
                None => {
                    return with_api_headers(
                        error_response(StatusCode::BAD_REQUEST, "Invalid request size"),
                        headers.get("x-studio-request-id"),
                    );
                }
            },
        };
        if declared == 0 || declared > limit {
            return with_api_headers(
                error_response(StatusCode::PAYLOAD_TOO_LARGE, "Invalid request size"),
                headers.get("x-studio-request-id"),
            );
        }
        let (parts, body) = request.into_parts();
        let bytes = match tokio::time::timeout(REQUEST_READ_TIMEOUT, to_bytes(body, limit)).await {
            Err(_) => {
                return with_api_headers(
                    error_response(StatusCode::REQUEST_TIMEOUT, "Request body timed out"),
                    parts.headers.get("x-studio-request-id"),
                );
            }
            Ok(Err(_)) => {
                return with_api_headers(
                    error_response(StatusCode::PAYLOAD_TOO_LARGE, "Invalid request size"),
                    parts.headers.get("x-studio-request-id"),
                );
            }
            Ok(Ok(bytes)) => bytes,
        };
        if bytes.is_empty() {
            return with_api_headers(
                error_response(StatusCode::PAYLOAD_TOO_LARGE, "Invalid request size"),
                parts.headers.get("x-studio-request-id"),
            );
        }
        if bytes.len() != declared {
            return with_api_headers(
                error_response(StatusCode::BAD_REQUEST, "Incomplete request"),
                parts.headers.get("x-studio-request-id"),
            );
        }
        let federation = is_federation_path(&path);
        if !federation {
            match serde_json::from_slice::<Value>(&bytes) {
                Err(_) => {
                    return with_api_headers(
                        error_response(StatusCode::BAD_REQUEST, "Invalid JSON"),
                        parts.headers.get("x-studio-request-id"),
                    );
                }
                Ok(value) if !value.is_object() => {
                    return with_api_headers(
                        error_response(StatusCode::BAD_REQUEST, "JSON object required"),
                        parts.headers.get("x-studio-request-id"),
                    );
                }
                _ => {}
            }
        }
        request = Request::from_parts(parts, Body::from(bytes));
        if !federation
            && (path == "/api/sync/drafts" || request.headers().contains_key("x-canvas-workspace"))
            && request
                .headers()
                .get("x-canvas-workspace")
                .and_then(|value| value.to_str().ok())
                != Some(state.workspace_id.as_str())
        {
            return with_api_headers(
                error_response(
                    StatusCode::CONFLICT,
                    "The server workspace changed. Reload before sending.",
                ),
                request.headers().get("x-studio-request-id"),
            );
        }
    }

    let mut response = next.run(request).await;
    if api {
        response = with_api_headers(response, request_id.as_ref());
    }
    response
}

fn trusted(
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

fn request_limit(path: &str) -> usize {
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

fn with_api_headers(mut response: Response, request_id: Option<&HeaderValue>) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        API_SCHEMA_HASH_HEADER,
        HeaderValue::from_static(api_schema_hash()),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert("content-security-policy", HeaderValue::from_static(CSP));
    headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    if let Some(value) = request_id {
        headers.insert("x-studio-request-id", value.clone());
    }
    response
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": message}))).into_response()
}

async fn dispatch(State(state): State<HttpState>, request: Request<Body>) -> Response {
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    if path == "/api/session" && method == "GET" {
        return Json(json!({"token": state.token})).into_response();
    }
    if path == "/api/sync/identity" && method == "GET" {
        return Json(json!({"workspaceId": state.workspace_id, "syncProtocol": 2})).into_response();
    }
    if path == "/api/desktop" && method == "GET" {
        return Json(desktop_status(&state)).into_response();
    }
    if operations().is_ok_and(|operations| {
        operations.iter().any(|(registered_method, route)| {
            registered_method == &method && route_matches(route, &path)
        })
    }) {
        return error_response(StatusCode::NOT_IMPLEMENTED, "Not implemented");
    }
    error_response(StatusCode::NOT_FOUND, "Not found")
}

fn route_matches(route: &str, path: &str) -> bool {
    let route_parts = route.split('/').collect::<Vec<_>>();
    let path_parts = path.split('/').collect::<Vec<_>>();
    route_parts.len() == path_parts.len()
        && route_parts
            .iter()
            .zip(path_parts)
            .all(|(expected, actual)| expected.starts_with('{') || *expected == actual)
}

fn desktop_status(state: &HttpState) -> Value {
    let native = state.native_path.as_ref().map(|path| {
        json!({
            "path": path, "sourcePath": path, "version": "unknown"
        })
    });
    let native_runtime = json!({
        "status": if native.is_some() { "ready" } else { "failed" },
        "checkedAt": null,
        "selected": native,
        "candidates": [], "accounts": {},
        "error": if state.native_path.is_none() { Some("No native provider configured") } else { None }
    });
    json!({
        "application": "codex-agents", "protocol": 1, "mobileProtocol": 1,
        "backendBuild": "rust-runtime-http-skeleton", "nativeRuntime": native_runtime,
        "providerVersions": {"checkedAt": null, "providers": [], "warnings": []},
        "browser": {"enabled": false, "reason": "Browser diagnostics are not implemented"},
        "liveUpdate": null, "restartEnvironment": {}, "publicOrigin": null,
        "pid": std::process::id(), "supervisorMode": false, "supervisorRequired": false,
        "supervisorError": null, "supervisor": null, "supervisorFallback": false,
        "supervisorNotice": null, "stateDir": state.state_dir, "linuxVm": null
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_hash_matches_python_generated_identity() {
        let client_schema =
            include_str!("../../../../../workspaces/client/apps/web/src/generated/apiSchema.ts");
        let client_hash = client_schema
            .split('"')
            .nth(1)
            .expect("generated TypeScript schema hash is present");
        assert_eq!(api_schema_hash(), client_hash);
    }

    #[test]
    fn route_template_matching_is_segment_exact() {
        assert!(route_matches("/api/agents/{id}", "/api/agents/a"));
        assert!(!route_matches("/api/agents/{id}", "/api/agents/a/b"));
    }

    #[test]
    fn trust_requires_loopback_origin_and_constant_time_token_match() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:4312"));
        assert!(trusted(
            &headers,
            false,
            "token",
            true,
            4312,
            &None,
            "/api/probe"
        ));
        assert!(!trusted(
            &headers,
            false,
            "token",
            false,
            4312,
            &None,
            "/api/probe"
        ));
        assert!(!trusted(
            &headers,
            true,
            "token",
            true,
            4312,
            &None,
            "/api/probe"
        ));
        headers.insert("x-canvas-token", HeaderValue::from_static("token"));
        assert!(trusted(
            &headers,
            true,
            "token",
            true,
            4312,
            &None,
            "/api/probe"
        ));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://attacker.example"),
        );
        assert!(!trusted(
            &headers,
            false,
            "token",
            true,
            4312,
            &None,
            "/api/probe"
        ));
    }

    #[test]
    fn forwarded_origins_require_the_configured_tailnet_proxy() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:4312"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://studio.ts.net"),
        );
        headers.insert(
            "x-forwarded-host",
            HeaderValue::from_static("studio.ts.net"),
        );
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        headers.insert("x-forwarded-for", HeaderValue::from_static("100.64.0.9"));
        let public_origin = Some("https://studio.ts.net".to_owned());
        assert!(trusted(
            &headers,
            false,
            "token",
            true,
            4312,
            &public_origin,
            "/api/probe"
        ));
        headers.insert("x-forwarded-for", HeaderValue::from_static("192.0.2.9"));
        assert!(!trusted(
            &headers,
            false,
            "token",
            true,
            4312,
            &public_origin,
            "/api/probe"
        ));
    }

    #[test]
    fn signed_federation_writes_do_not_require_the_local_session_token() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:4312"));
        assert!(trusted(
            &headers,
            true,
            "token",
            true,
            4312,
            &None,
            "/api/federation/v1/message"
        ));
    }

    #[test]
    fn api_responses_include_schema_and_security_headers() {
        let response = with_api_headers(
            error_response(StatusCode::NOT_IMPLEMENTED, "Not implemented"),
            None,
        );
        assert_eq!(
            response.headers()[API_SCHEMA_HASH_HEADER],
            api_schema_hash()
        );
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        assert_eq!(response.headers()["content-security-policy"], CSP);
    }

    #[test]
    fn schema_contains_159_api_operations() {
        assert_eq!(
            operations()
                .expect("committed schema parses")
                .iter()
                .filter(|(_, path)| path.starts_with("/api/"))
                .count(),
            159
        );
    }
}
