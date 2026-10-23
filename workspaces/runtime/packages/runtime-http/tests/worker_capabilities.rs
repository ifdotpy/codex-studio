//! Profile limits pass through the real editor HTTP boundary and persist.
#![allow(clippy::unwrap_used)]
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_http::{HttpState, router};
use studio_runtime_store::RuntimeStore;
use tower::ServiceExt;

async fn call(app: &axum::Router, method: &str, body: Option<Value>) -> (u16, Value) {
    let request = Request::builder()
        .method(method)
        .uri("/api/profiles")
        .header("host", "127.0.0.1:49171")
        .header("content-type", "application/json")
        .header("x-canvas-token", "fixture-token")
        .header(
            studio_runtime_http::API_SCHEMA_HASH_HEADER,
            studio_runtime_http::api_schema_hash(),
        )
        .extension(ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn capability_profile_editor_http_roundtrip_and_validation() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    let app = router(HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        store.root().to_string_lossy().into(),
        49171,
        None,
        Some(store.clone()),
        None,
    ))
    .unwrap();
    let profile = json!({"action":"save","id":"limited","name":"Limited","role":"implementer","model":"claude-haiku-4-5","effort":null,"instructions":"Read only",
        "allowedTools":["Read*"],"disallowedTools":["Bash"],"mcpServers":["docs"],"skills":["research"],"maxTurns":2});
    let (status, saved) = call(&app, "POST", Some(profile.clone())).await;
    assert_eq!(status, 200, "{saved}");
    for field in [
        "allowedTools",
        "disallowedTools",
        "mcpServers",
        "skills",
        "maxTurns",
    ] {
        assert_eq!(saved[field], profile[field]);
    }
    let (status, list) = call(&app, "GET", None).await;
    assert_eq!(status, 200, "{list}");
    for field in [
        "allowedTools",
        "disallowedTools",
        "mcpServers",
        "skills",
        "maxTurns",
    ] {
        assert_eq!(list["profiles"][0][field], profile[field]);
    }
    for (field, bad) in [
        ("allowedTools", json!([""])),
        ("disallowedTools", json!(["Bash(foo)"])),
        ("mcpServers", json!(["unknown*"])),
        ("skills", json!(["../private"])),
        ("maxTurns", json!(0)),
        ("maxTurns", json!(1.5)),
    ] {
        let mut invalid = profile.clone();
        invalid[field] = bad;
        let (status, body) = call(&app, "POST", Some(invalid)).await;
        assert!((400..500).contains(&status), "{field}: {status} {body}");
    }
    let mut empty = profile;
    for field in ["allowedTools", "disallowedTools", "mcpServers", "skills"] {
        empty[field] = json!([]);
    }
    empty["maxTurns"] = Value::Null;
    let (status, saved) = call(&app, "POST", Some(empty)).await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(saved["allowedTools"], json!([]));
    assert_eq!(saved["maxTurns"], Value::Null);
}
