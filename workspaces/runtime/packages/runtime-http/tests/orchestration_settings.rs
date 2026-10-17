//! The child-result hold setting round-trips through HTTP and is validated.
#![allow(clippy::unwrap_used)]
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_http::{HttpState, router};
use studio_runtime_store::{
    CHILD_RESULT_COALESCE_DEFAULT, CHILD_RESULT_COALESCE_MAX, OrchestrationSettingsRepository,
    RuntimeStore,
};
use tower::ServiceExt;

const PATH: &str = "/api/orchestration/settings";

async fn call(app: &axum::Router, method: &str, body: Option<Value>) -> (u16, Value) {
    let request = Request::builder()
        .method(method)
        .uri(PATH)
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

fn seconds(body: &Value) -> Value {
    body["settings"]["childResultCoalesceSeconds"].clone()
}

#[tokio::test]
async fn hold_setting_defaults_persists_and_refuses_out_of_range_values() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    let state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        None,
        store.root().to_string_lossy().into(),
        49171,
        None,
        Some(store.clone()),
        None,
    );
    let app = router(state).unwrap();

    let (status, body) = call(&app, "GET", None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        seconds(&body),
        json!(CHILD_RESULT_COALESCE_DEFAULT.as_secs())
    );

    for value in [0, CHILD_RESULT_COALESCE_MAX.as_secs()] {
        let (status, body) = call(
            &app,
            "POST",
            Some(json!({"childResultCoalesceSeconds": value})),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(seconds(&body), json!(value));
        let (_, body) = call(&app, "GET", None).await;
        assert_eq!(seconds(&body), json!(value));
    }

    for value in [
        json!(CHILD_RESULT_COALESCE_MAX.as_secs() + 1),
        json!(-1),
        json!("10"),
    ] {
        let (status, body) = call(
            &app,
            "POST",
            Some(json!({"childResultCoalesceSeconds": value})),
        )
        .await;
        assert!((400..500).contains(&status), "{value}: {status} {body}");
    }
    assert_eq!(
        OrchestrationSettingsRepository::child_result_coalesce(&store.read().unwrap()).unwrap(),
        CHILD_RESULT_COALESCE_MAX,
        "a refused write keeps the stored hold"
    );
}
