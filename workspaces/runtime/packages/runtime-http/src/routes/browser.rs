//! Local-window browser forwarding backed by an idempotent paired callback.

use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
};
use serde_json::{Value, json};
use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use studio_runtime_services::{
    browser::{BrowserError, BrowserForwarder, ExchangeOutcome, ForwardInput, PairedExchange},
    linux_vm_guest::GuestRpc,
    multi_server_orchestration::{ExchangeOutcome as DeliveryOutcome, OrchestrationDriver},
};
use tokio::time::Instant;

use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, post};
use crate::{error::ApiError, router::HttpState};

const FORWARD_DEPENDENCIES: [Dependency; 1] =
    [Dependency::new("multi-server-runtime/orchestration")];

struct SignedBrowserExchange {
    driver: Arc<OrchestrationDriver>,
}
impl PairedExchange for SignedBrowserExchange {
    fn exchange<'a>(
        &'a self,
        server: &'a str,
        action: &'a str,
        payload: &'a Value,
        request_id: &'a str,
        now: i64,
        deadline: Instant,
    ) -> Pin<Box<dyn Future<Output = Result<ExchangeOutcome, BrowserError>> + Send + 'a>> {
        let driver = Arc::clone(&self.driver);
        let server = server.to_owned();
        let action = action.to_owned();
        let payload = payload.clone();
        let request_id = request_id.to_owned();
        Box::pin(async move {
            let timeout = deadline
                .checked_duration_since(Instant::now())
                .filter(|v| !v.is_zero())
                .ok_or(BrowserError::Unknown)?;
            let outcome = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-http/routes/browser",
                move || {
                    driver.exchange_with_timeout(
                        &server,
                        &action,
                        &payload,
                        &request_id,
                        now,
                        timeout,
                    )
                },
            )
            .await
            .map_err(|_| BrowserError::Exchange)?
            .unwrap_or(DeliveryOutcome::Unknown);
            Ok(match outcome {
                DeliveryOutcome::Applied(value) => ExchangeOutcome::Applied(value),
                DeliveryOutcome::NotApplied(error) => ExchangeOutcome::NotApplied(error),
                DeliveryOutcome::Unknown => ExchangeOutcome::Unknown,
                DeliveryOutcome::PreEffectWaiting => ExchangeOutcome::Unknown,
            })
        })
    }
}

pub(crate) fn new_forwarder(driver: Arc<OrchestrationDriver>) -> Arc<BrowserForwarder> {
    Arc::new(BrowserForwarder::new(Arc::new(SignedBrowserExchange {
        driver,
    })))
}

struct GuestCallbackExchange {
    guest: Arc<dyn GuestRpc>,
}

impl PairedExchange for GuestCallbackExchange {
    fn exchange<'a>(
        &'a self,
        _server: &'a str,
        action: &'a str,
        payload: &'a Value,
        request_id: &'a str,
        _now: i64,
        deadline: Instant,
    ) -> Pin<Box<dyn Future<Output = Result<ExchangeOutcome, BrowserError>> + Send + 'a>> {
        let guest = Arc::clone(&self.guest);
        let action = action.to_owned();
        let mut payload = payload.clone();
        let request_id = request_id.to_owned();
        Box::pin(async move {
            if action != "browser_callback" {
                return Err(BrowserError::Exchange);
            }
            if let Some(bridge_id) = payload.get("bridgeId").cloned() {
                payload["id"] = bridge_id;
            }
            let timeout = deadline
                .checked_duration_since(Instant::now())
                .filter(|value| !value.is_zero())
                .ok_or(BrowserError::Unknown)?;
            let result = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-http/routes/browser",
                move || guest.call("browser.callback", &payload, Some(&request_id), timeout),
            )
            .await
            .map_err(|_| BrowserError::Exchange)?;
            match result {
                Ok(value) => Ok(ExchangeOutcome::Applied(value)),
                Err(error) if error.uncertain() => Ok(ExchangeOutcome::Unknown),
                Err(error) => Ok(ExchangeOutcome::NotApplied(error.to_string())),
            }
        })
    }
}

pub(crate) fn new_local_forwarder(guest: Arc<dyn GuestRpc>) -> Arc<BrowserForwarder> {
    Arc::new(BrowserForwarder::new(Arc::new(GuestCallbackExchange {
        guest,
    })))
}

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![post(
        "/api/browser/forward",
        OperationStatus::Implemented,
        forward,
    )])
}

async fn forward(
    State(state): State<HttpState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    if !local_window(&headers, peer) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "Only a window on this Mac can hold a sign-in port here.",
        ));
    }
    let input = ForwardInput {
        server: string(&body, "server")?,
        bridge_id: string(&body, "bridgeId")?,
        token: string(&body, "token")?,
        host: string(&body, "host")?,
        port: body
            .get("port")
            .and_then(Value::as_u64)
            .and_then(|v| u16::try_from(v).ok())
            .ok_or_else(|| {
                ApiError::new(StatusCode::BAD_REQUEST, "The sign-in forward is invalid")
            })?,
        path: string(&body, "path")?,
        expires_at: body
            .get("expiresAt")
            .and_then(Value::as_f64)
            .ok_or_else(|| {
                ApiError::new(StatusCode::BAD_REQUEST, "The sign-in forward is invalid")
            })?,
    };
    let forwarder = state.browser_forwarder.ok_or_else(|| {
        dependency_unavailable("POST", "/api/browser/forward", &FORWARD_DEPENDENCIES)
            .unwrap_or_else(|_| ApiError::internal())
    })?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ApiError::internal())?
        .as_secs_f64();
    forwarder
        .open(input, now, None)
        .await
        .map_err(map_browser_error)?;
    Ok(Json(json!({"status":"forwarding"})))
}

fn string(body: &Value, key: &str) -> Result<String, ApiError> {
    body.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "The sign-in forward is invalid"))
}

pub(crate) fn local_window(headers: &HeaderMap, peer: SocketAddr) -> bool {
    if !peer.ip().is_loopback()
        || [
            "x-forwarded-for",
            "x-forwarded-host",
            "x-forwarded-proto",
            "x-studio-client",
            "x-studio-signature",
        ]
        .iter()
        .any(|name| headers.contains_key(*name))
    {
        return false;
    }
    let host = headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let host = host
        .strip_prefix('[')
        .and_then(|value| value.split(']').next())
        .unwrap_or_else(|| host.split(':').next().unwrap_or(host));
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

pub(crate) fn map_browser_error(error: BrowserError) -> ApiError {
    let status = match &error {
        BrowserError::InvalidForward | BrowserError::Expired => StatusCode::BAD_REQUEST,
        BrowserError::PortInUse(_) | BrowserError::Conflict => StatusCode::CONFLICT,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    ApiError::new(status, error.to_string())
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration { name: "browser", routes: Some(routes), dependencies: &["multi-server-runtime/orchestration"] }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use serde_json::{Value, json};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use tower::ServiceExt;

    #[test]
    fn forwarded_and_non_loopback_callers_are_not_local_windows() {
        let headers = HeaderMap::from_iter([(
            axum::http::HeaderName::from_static("host"),
            "127.0.0.1:12345".parse().expect("host header"),
        )]);
        assert!(!local_window(
            &headers,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 1)
        ));
        let forwarded = HeaderMap::from_iter([
            (
                axum::http::HeaderName::from_static("host"),
                "127.0.0.1:12345".parse().expect("host header"),
            ),
            (
                axum::http::HeaderName::from_static("x-forwarded-for"),
                "127.0.0.1".parse().expect("forwarded address"),
            ),
        ]);
        assert!(!local_window(
            &forwarded,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1)
        ));
    }

    #[tokio::test]
    async fn forward_starts_for_a_local_window_with_the_runtime_available() {
        let state_dir = tempfile::tempdir().expect("isolated state directory");
        let store = Arc::new(
            studio_runtime_store::RuntimeStore::open(Some(state_dir.path()))
                .expect("isolated runtime store"),
        );
        let state = crate::router::HttpState::new(
            "token".into(),
            "workspace".into(),
            None,
            state_dir.path().to_string_lossy().into_owned(),
            12345,
            None,
            Some(store),
            None,
        );
        let forwarder = Arc::clone(
            state
                .browser_forwarder
                .as_ref()
                .expect("runtime installs its browser forwarder"),
        );
        let callback =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral callback port");
        let callback_port = callback.local_addr().expect("callback address").port();
        drop(callback);
        let expires_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock follows the Unix epoch")
            .as_secs_f64()
            + 30.0;
        let app = crate::router::router(state).expect("HTTP router composes the route");
        let body = json!({
            "server":"peer",
            "bridgeId":"0123456789abcdef0123456789abcdef",
            "host":"127.0.0.1",
            "port":callback_port,
            "path":"/callback",
            "expiresAt":expires_at,
            "token":"abcdefghijklmnop"
        });
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/browser/forward")
            .header("host", "127.0.0.1:12345")
            .header("content-type", "application/json")
            .header("x-canvas-token", "token")
            .header(
                crate::schema::API_SCHEMA_HASH_HEADER,
                crate::schema::api_schema_hash(),
            )
            .body(axum::body::Body::from(body.to_string()))
            .expect("HTTP request builds");
        let mut request = request;
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                54321,
            ))));
        let response = app.oneshot(request).await.expect("HTTP response");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("response body reads");
        let body: Value = serde_json::from_slice(&bytes).expect("error envelope parses");
        assert_eq!(body, json!({"status":"forwarding"}));
        assert!(
            forwarder
                .close("0123456789abcdef0123456789abcdef")
                .await
                .expect("forwarder closes")
        );
    }
}
