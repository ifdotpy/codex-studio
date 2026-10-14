//! Browser forwarding route; its paired orchestration consumer is not on this base.

use axum::Json;
use serde_json::Value;

use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, post};
use crate::error::ApiError;

const ORCHESTRATION: [Dependency; 1] = [Dependency::new("multi-server-runtime/orchestration")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![post(
        "/api/browser/forward",
        OperationStatus::partial(&ORCHESTRATION),
        forward,
    )])
}

async fn forward(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(
        dependency_unavailable("POST", "/api/browser/forward", &ORCHESTRATION)
            .unwrap_or_else(|_| ApiError::internal()),
    )
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "browser",
        routes: Some(routes),
        dependencies: &["multi-server-runtime/orchestration"],
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tower::ServiceExt;

    #[tokio::test]
    async fn forward_refuses_without_the_orchestration_consumer() {
        let state = crate::router::HttpState::new(
            "token".into(),
            "workspace".into(),
            None,
            "isolated-test-state".into(),
            12345,
            None,
            None,
            None,
        );
        let app = crate::router::router(state).expect("HTTP router composes the route");
        let body = json!({
            "server":"peer",
            "bridgeId":"0123456789abcdef0123456789abcdef",
            "host":"127.0.0.1",
            "port":1024,
            "path":"/callback",
            "expiresAt":2000000000,
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
        assert_eq!(response.status(), axum::http::StatusCode::NOT_IMPLEMENTED);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("error body reads");
        let body: Value = serde_json::from_slice(&bytes).expect("error envelope parses");
        assert_eq!(
            body,
            json!({"error":"Not implemented: requires multi-server-runtime/orchestration"})
        );
    }
}
