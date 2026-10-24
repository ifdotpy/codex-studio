//! Rust-only witness of the server contract used by the web RxDB ordering tests.
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::{net::SocketAddr, sync::Arc};
use studio_runtime_http::{HttpState, router};
use studio_runtime_store::RuntimeStore;
use tower::ServiceExt;

async fn request(app: &Router, method: &str, uri: &str, body: Value) -> Value {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1:4321")
        .header("origin", "http://127.0.0.1:4321")
        .header("x-canvas-token", "token")
        .header("x-canvas-workspace", "workspace")
        .header("content-type", "application/json")
        .body(if method == "GET" {
            Body::empty()
        } else {
            Body::from(body.to_string())
        })
        .expect("request");
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:4321".parse::<SocketAddr>().expect("peer"),
    ));
    let response = app.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body"),
    )
    .expect("JSON")
}

#[tokio::test]
async fn draft_push_ack_and_echo_follow_shared_contract() {
    let directory = tempfile::tempdir().expect("isolated draft store");
    let store = Arc::new(
        RuntimeStore::open(Some(directory.path()))
            .await
            .expect("store"),
    );
    let app = router(HttpState::new(
        "token".into(),
        "workspace".into(),
        directory.path().display().to_string(),
        4321,
        None,
        Some(store),
        None,
    ))
    .expect("router");
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../client/contracts/draft-replication.json"
    ))
    .expect("shared fixture");
    let document = json!({"id": fixture["document"]["id"], "payload": fixture["document"].to_string(), "seq": 0, "deleted": false});
    let push = json!({"rows": [{"newDocumentState": document}]});
    assert_eq!(
        request(&app, "POST", "/api/sync/drafts", push.clone()).await,
        json!([])
    );
    let first = request(&app, "GET", "/api/sync/pull?scope=drafts", Value::Null).await;
    assert_eq!(first["documents"][0]["id"], document["id"]);
    assert_eq!(first["documents"][0]["payload"], document["payload"]);
    // An accepted write whose ACK was lost retries without allocating a version.
    assert_eq!(
        request(&app, "POST", "/api/sync/drafts", push).await,
        json!([])
    );
    let retried = request(&app, "GET", "/api/sync/pull?scope=drafts", Value::Null).await;
    assert_eq!(retried["documents"], first["documents"]);
    let mut changed = fixture["document"].clone();
    changed["text"] = json!("stale writer");
    let mut stale = document;
    stale["payload"] = json!(changed.to_string());
    let conflicts = request(
        &app,
        "POST",
        "/api/sync/drafts",
        json!({"rows": [{"newDocumentState": stale}]}),
    )
    .await;
    assert_eq!(conflicts[0]["payload"], first["documents"][0]["payload"]);
    assert_eq!(conflicts[0]["seq"], first["documents"][0]["seq"]);
}
