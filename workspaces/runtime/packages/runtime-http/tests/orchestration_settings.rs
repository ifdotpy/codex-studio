//! The orchestration settings round-trip through HTTP and are validated.
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
    CHILD_RESULT_COALESCE_DEFAULT, CHILD_RESULT_COALESCE_MAX, IDLE_SESSION_UNLOAD_SETTING,
    OrchestrationSettingsRepository, RuntimeStore,
};
use tower::ServiceExt;

const PATH: &str = "/api/orchestration/settings";
const HOLD: &str = "childResultCoalesceSeconds";
const UNLOAD: &str = "idleSessionUnloadMinutes";

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

fn settings(hold: Value, unload: Value) -> Value {
    json!({ HOLD: hold, UNLOAD: unload })
}

#[tokio::test]
async fn settings_default_persist_and_refuse_out_of_range_values() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    let state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
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
        body["settings"],
        settings(
            json!(CHILD_RESULT_COALESCE_DEFAULT.as_secs()),
            json!(IDLE_SESSION_UNLOAD_SETTING.default)
        )
    );

    for (hold, unload) in [
        (0, IDLE_SESSION_UNLOAD_SETTING.max),
        (CHILD_RESULT_COALESCE_MAX.as_secs(), 0),
    ] {
        let written = settings(json!(hold), json!(unload));
        let (status, body) = call(&app, "POST", Some(written.clone())).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["settings"], written);
        let (_, body) = call(&app, "GET", None).await;
        assert_eq!(body["settings"], written);
    }

    for body in [
        settings(json!(CHILD_RESULT_COALESCE_MAX.as_secs() + 1), json!(0)),
        settings(json!(-1), json!(0)),
        settings(json!("10"), json!(0)),
        settings(json!(0), json!(IDLE_SESSION_UNLOAD_SETTING.max + 1)),
        settings(json!(0), json!(-1)),
        json!({ HOLD: 0 }),
    ] {
        let (status, response) = call(&app, "POST", Some(body.clone())).await;
        assert!((400..500).contains(&status), "{body}: {status} {response}");
    }
    let connection = store.read().unwrap();
    assert_eq!(
        OrchestrationSettingsRepository::child_result_coalesce(&connection).unwrap(),
        CHILD_RESULT_COALESCE_MAX,
        "a refused write keeps the stored hold"
    );
    assert_eq!(
        OrchestrationSettingsRepository::idle_session_unload(&connection).unwrap(),
        None,
        "a refused write keeps the stored unload delay"
    );
}
