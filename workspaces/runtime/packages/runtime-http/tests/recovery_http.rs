//! Recovery reaches the retained account-session command through real HTTP.
#![allow(clippy::unwrap_used)]

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
use studio_runtime_services::account_session_runtime::{
    AccountSessionError, AccountSessionFuture, AccountSessionPort,
};
use studio_runtime_store::RuntimeStore;
use tower::ServiceExt;

struct RecoveryPort(AtomicUsize);
impl AccountSessionPort for RecoveryPort {
    fn recover_connection<'a>(
        &'a self,
        account: &'a str,
        agent_id: &'a str,
    ) -> AccountSessionFuture<'a, Value> {
        Box::pin(async move {
            assert_eq!(account, "default");
            assert_eq!(agent_id, "recovery-agent");
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"status":"reconciled","turnId":"turn-0","outcome":"completed"}))
        })
    }
}

struct MissingRecoveryPort;
impl AccountSessionPort for MissingRecoveryPort {
    fn recover_connection<'a>(
        &'a self,
        _account: &'a str,
        _agent_id: &'a str,
    ) -> AccountSessionFuture<'a, Value> {
        Box::pin(async {
            Err(AccountSessionError::Requires(
                "account-worker/recovery-history",
            ))
        })
    }
}

struct UnknownRecoveryPort;
impl AccountSessionPort for UnknownRecoveryPort {
    fn recover_connection<'a>(
        &'a self,
        _account: &'a str,
        _agent_id: &'a str,
    ) -> AccountSessionFuture<'a, Value> {
        Box::pin(async { Err(AccountSessionError::Unknown) })
    }
}

struct SupersededRecoveryPort;
impl AccountSessionPort for SupersededRecoveryPort {
    fn recover_connection<'a>(
        &'a self,
        _account: &'a str,
        _agent_id: &'a str,
    ) -> AccountSessionFuture<'a, Value> {
        Box::pin(async { Ok(json!({"status":"superseded"})) })
    }
}

async fn fixture(port: Arc<dyn AccountSessionPort>) -> (tempfile::TempDir, axum::Router) {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    store.transaction(|tx| {
        tx.execute("INSERT INTO runtime_agents(id,record) VALUES('recovery-agent',?1)",
            [json!({"id":"recovery-agent","rootId":"recovery-agent","epoch":1,"status":"idle","accountKey":"default"}).to_string()])?;
        Ok(())
    }).await.unwrap();
    let mut state = HttpState::new(
        "token".into(),
        "fixture".into(),
        root.path().display().to_string(),
        49171,
        None,
        Some(store),
        None,
    );
    state.account_session = Some(port);
    (root, router(state).unwrap())
}

fn request() -> Request<Body> {
    Request::builder()
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
        .body(Body::from(json!({"id":"recovery-agent"}).to_string()))
        .unwrap()
}

#[tokio::test]
async fn connection_recovery_http_returns_the_synchronous_worker_status() {
    let port = Arc::new(RecoveryPort(AtomicUsize::new(0)));
    let (_root, app) = fixture(port.clone()).await;
    let response = app.oneshot(request()).await.unwrap();
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
    let (_root, app) = fixture(Arc::new(MissingRecoveryPort)).await;
    let response = app.oneshot(request()).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn connection_recovery_unknown_outcome_is_service_unavailable() {
    let (_root, app) = fixture(Arc::new(UnknownRecoveryPort)).await;
    let response = app.oneshot(request()).await.unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn connection_recovery_superseded_outcome_is_successful_http_result() {
    let (_root, app) = fixture(Arc::new(SupersededRecoveryPort)).await;
    let response = app.oneshot(request()).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"status":"superseded"})
    );
}
