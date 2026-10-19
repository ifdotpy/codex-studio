#![allow(clippy::unwrap_used)]

//! The Serve origin comes from `remote-access.json`, and disabling that file
//! refuses remote requests without a restart (`codex_remote.py` parity).

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use studio_runtime_http::{HttpState, router};
use studio_runtime_services::multi_server_protocol_generated::REMOTE_ACCESS_FILE;
use studio_runtime_services::remote_access::RemoteAccess;
use tower::ServiceExt;

const ORIGIN: &str = "https://studio.example.ts.net";

fn configure(root: &Path, enabled: bool) {
    std::fs::write(
        root.join(REMOTE_ACCESS_FILE),
        serde_json::json!({"enabled": enabled, "origin": ORIGIN}).to_string(),
    )
    .unwrap();
}

fn forwarded(path: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method("GET")
        .uri(path)
        .header("host", "127.0.0.1:4000")
        .header("origin", ORIGIN)
        .header("x-forwarded-host", "studio.example.ts.net")
        .header("x-forwarded-proto", "https")
        .header("x-forwarded-for", "100.64.0.9")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:4321".parse::<std::net::SocketAddr>().unwrap(),
    ));
    request
}

async fn send(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let response = app.clone().oneshot(forwarded(path)).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn the_state_file_origin_admits_serve_requests_until_it_is_disabled() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(
        studio_runtime_store::RuntimeStore::open(Some(root.path()))
            .await
            .unwrap(),
    );
    // A fresh state directory accepts no configuration file; the owner
    // enables Serve access in an existing one.
    configure(root.path(), true);
    // The server resolves its startup origin the same way.
    let access = Arc::new(RemoteAccess::new(root.path(), None).unwrap());
    let startup_origin = access.origin();
    assert_eq!(startup_origin.as_deref(), Some(ORIGIN));
    let state = HttpState::new(
        "token".into(),
        "workspace".into(),
        root.path().display().to_string(),
        4000,
        startup_origin,
        Some(store),
        None,
    )
    .with_remote_access(access);
    let app = router(state).unwrap();
    let signed_refusal = "Signed requests require Tailscale Serve HTTPS";

    let (status, body) = send(&app, "/api/session").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = send(&app, "/api/multi-server/v1/identity").await;
    assert_ne!(body["error"], signed_refusal);

    configure(root.path(), false);
    let (status, body) = send(&app, "/api/session").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = send(&app, "/api/multi-server/v1/identity").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], signed_refusal);

    configure(root.path(), true);
    let (status, body) = send(&app, "/api/session").await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
