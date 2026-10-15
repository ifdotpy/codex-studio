//! Recovery reaches the retained account-session command through real HTTP.
#![allow(clippy::unwrap_used)]
#[path = "../../runtime-services/tests/wave7_p04_callback/fixture.rs"]
mod fixture;

use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use studio_runtime_http::{HttpState, router};
use studio_runtime_services::account_session_runtime::{AccountSessionError, AccountSessionPort};
use tower::ServiceExt;

struct RecoveryPort(AtomicUsize);
impl AccountSessionPort for RecoveryPort {
    fn recover_connection(
        &self,
        account: &str,
        agent_id: &str,
    ) -> Result<Value, AccountSessionError> {
        assert_eq!(account, "default");
        assert!(!agent_id.is_empty());
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"status":"reconciled","turnId":"turn-0","outcome":"completed"}))
    }
}

struct MissingRecoveryPort;
impl AccountSessionPort for MissingRecoveryPort {
    fn recover_connection(
        &self,
        _account: &str,
        _agent_id: &str,
    ) -> Result<Value, AccountSessionError> {
        Err(AccountSessionError::Requires(
            "account-worker/recovery-history",
        ))
    }
}

struct UnknownRecoveryPort;
impl AccountSessionPort for UnknownRecoveryPort {
    fn recover_connection(
        &self,
        _account: &str,
        _agent_id: &str,
    ) -> Result<Value, AccountSessionError> {
        Err(AccountSessionError::Unknown)
    }
}

#[tokio::test]
async fn connection_recovery_http_returns_the_synchronous_worker_status() {
    let (root, store, context) = fixture::fixture();
    let port = Arc::new(RecoveryPort(AtomicUsize::new(0)));
    let mut state = HttpState::new(
        "token".into(),
        "fixture".into(),
        None,
        root.path().to_string_lossy().into(),
        49171,
        None,
        Some(store),
        None,
    );
    state.account_session = Some(port.clone());
    let app = router(state).unwrap();
    let request = Request::builder()
        .method("POST")
        .uri("/api/connection-recovery")
        .header("host", "127.0.0.1:49171")
        .header("content-type", "application/json")
        .header("x-canvas-token", "token")
        .header(
            studio_runtime_http::API_SCHEMA_HASH_HEADER,
            studio_runtime_http::schema::api_schema_hash(),
        )
        .extension(ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::from(json!({"id":context.agent}).to_string()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"status":"reconciled","turnId":"turn-0","outcome":"completed"})
    );
    assert_eq!(port.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn connection_recovery_without_history_capability_returns_not_implemented() {
    let (root, store, context) = fixture::fixture();
    let mut state = HttpState::new(
        "token".into(),
        "fixture".into(),
        None,
        root.path().to_string_lossy().into(),
        49171,
        None,
        Some(store),
        None,
    );
    state.account_session = Some(Arc::new(MissingRecoveryPort));
    let app = router(state).unwrap();
    let request = Request::builder()
        .method("POST")
        .uri("/api/connection-recovery")
        .header("host", "127.0.0.1:49171")
        .header("content-type", "application/json")
        .header("x-canvas-token", "token")
        .header(
            studio_runtime_http::API_SCHEMA_HASH_HEADER,
            studio_runtime_http::schema::api_schema_hash(),
        )
        .extension(ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::from(json!({"id":context.agent}).to_string()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn connection_recovery_unknown_outcome_is_service_unavailable() {
    let (root, store, context) = fixture::fixture();
    let mut state = HttpState::new(
        "token".into(),
        "fixture".into(),
        None,
        root.path().to_string_lossy().into(),
        49171,
        None,
        Some(store),
        None,
    );
    state.account_session = Some(Arc::new(UnknownRecoveryPort));
    let app = router(state).unwrap();
    let request = Request::builder()
        .method("POST")
        .uri("/api/connection-recovery")
        .header("host", "127.0.0.1:49171")
        .header("content-type", "application/json")
        .header("x-canvas-token", "token")
        .header(
            studio_runtime_http::API_SCHEMA_HASH_HEADER,
            studio_runtime_http::schema::api_schema_hash(),
        )
        .extension(ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::from(json!({"id":context.agent}).to_string()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
}
