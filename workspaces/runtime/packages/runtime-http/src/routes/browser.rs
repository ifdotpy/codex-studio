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
    multi_server_orchestration::{ExchangeOutcome as DeliveryOutcome, OrchestrationDriver},
};
use tokio::time::Instant;

use super::extractors::ApiJson;
use super::manifest::{OperationStatus, RouteGroup, post};
use crate::{error::ApiError, router::HttpState};

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
            let outcome = tokio::task::spawn_blocking(move || {
                driver.exchange_with_timeout(&server, &action, &payload, &request_id, now, timeout)
            })
            .await
            .map_err(|_| BrowserError::Exchange)?
            .unwrap_or(DeliveryOutcome::Unknown);
            Ok(match outcome {
                DeliveryOutcome::Applied(value) => ExchangeOutcome::Applied(value),
                DeliveryOutcome::NotApplied(error) => ExchangeOutcome::NotApplied(error),
                DeliveryOutcome::Unknown => ExchangeOutcome::Unknown,
            })
        })
    }
}

pub(crate) fn new_forwarder(driver: Arc<OrchestrationDriver>) -> Arc<BrowserForwarder> {
    Arc::new(BrowserForwarder::new(Arc::new(SignedBrowserExchange {
        driver,
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
    let forwarder = state.browser_forwarder.ok_or_else(ApiError::internal)?;
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

fn local_window(headers: &HeaderMap, peer: SocketAddr) -> bool {
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

fn map_browser_error(error: BrowserError) -> ApiError {
    let status = match &error {
        BrowserError::InvalidForward | BrowserError::Expired => StatusCode::BAD_REQUEST,
        BrowserError::PortInUse(_) | BrowserError::Conflict => StatusCode::CONFLICT,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    ApiError::new(status, error.to_string())
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration { name: "browser", routes: Some(routes), dependencies: &[] }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

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
}
