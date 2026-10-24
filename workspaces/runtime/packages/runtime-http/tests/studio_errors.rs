//! The owner's Studio error list reads Studio-caused endings and hides a
//! dismissed row.
#![allow(clippy::unwrap_used)]
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_http::{HttpState, router};
use studio_runtime_store::{AgentRecord, AgentRepository, MonitorRecord, RuntimeStore, Table};
use tower::ServiceExt;

async fn call(app: &axum::Router, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
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

fn monitor(
    root: &std::path::Path,
    id: &str,
    status: &str,
    exit: Value,
    error: &str,
    now: f64,
) -> Value {
    json!({"id":id,"agent":"owner","epoch":0,"turnId":null,"requestId":id,
        "command":"cargo test","cwd":root.to_string_lossy(),
        "timeout_ms":60000,"stallTimeoutSeconds":1800,"livenessCommand":"",
        "activityAt":now,"activityGeneration":0,"stallWakeGeneration":-1,"wakeOn":"exit",
        "successExitCodes":[0],"outputPatterns":[],"outputEventSequence":0,
        "outputEventCount":0,"outputEventIds":[],"status":status,"created":now,
        "finished":now,"exitCode":exit,"error":error,"tail":"","bytes":0,"interactive":false,
        "ruleId":null,"log":root.join(format!("{id}.log")).to_string_lossy(),"cancelRequested":false})
}

#[tokio::test]
async fn studio_errors_list_studio_caused_endings_and_hide_a_dismissed_row() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    let now = studio_runtime_services::event_write::unix_time().unwrap();
    let path = root.path().to_path_buf();
    store
        .transaction(move |tx| {
            AgentRepository::put(
                tx,
                &AgentRecord::from_value(json!({"id":"owner","rootId":"owner","epoch":0,
                    "status":"waiting","autoWake":true,"cwd":path.to_string_lossy(),
                    "accountKey":"default","provider":"codex","name":"Fixture worker"}))?,
            )?;
            for record in [
                monitor(
                    &path,
                    "lost-watch",
                    "lost",
                    Value::Null,
                    "Command outcome unknown after losing the provider result; not rerun.",
                    now,
                ),
                monitor(&path, "own-exit", "failed", json!(1), "", now),
                monitor(
                    &path,
                    "stopped",
                    "failed",
                    Value::Null,
                    "The agent turn was stopped",
                    now,
                ),
            ] {
                Table::new(tx).put(&MonitorRecord::from_value(record)?)?;
            }
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .unwrap();
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

    let (status, body) = call(&app, "GET", "/api/studio-errors", None).await;
    assert_eq!(status, 200, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "only the Studio-caused ending: {body}");
    assert_eq!(items[0]["id"], "monitor:lost-watch");
    assert_eq!(items[0]["kind"], "monitor_outcome_lost");
    assert_eq!(items[0]["action"], "check");
    assert_eq!(items[0]["agentName"], "Fixture worker");

    for _ in 0..2 {
        let (status, body) = call(
            &app,
            "POST",
            "/api/studio-errors/dismiss",
            Some(json!({"id":"monitor:lost-watch"})),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["items"], json!([]), "a dismissed row stays hidden");
    }
    let (_, body) = call(&app, "GET", "/api/studio-errors", None).await;
    assert_eq!(body["items"], json!([]));
}
