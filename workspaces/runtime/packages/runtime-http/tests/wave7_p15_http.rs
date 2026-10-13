#[path = "../../runtime-services/tests/wave7_p15_support/fixtures.rs"]
mod fixtures;
#[path = "../../runtime-services/tests/support/prepared_python.rs"]
mod prepared_python_support;
#[path = "../../runtime-services/tests/wave7_p15_support/mod.rs"]
mod support;
use axum::{
    Extension,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_http::{HttpState, router};
use studio_runtime_services::monitor_runtime_wiring::MonitorRuntime;
use studio_runtime_store::MonitorsRepository;
use tower::ServiceExt;

async fn call(app: &axum::Router, path: &str, body: Value) -> (u16, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "127.0.0.1:49171")
        .header("content-type", "application/json")
        .header("x-canvas-token", "fixture-token")
        .header(
            studio_runtime_http::API_SCHEMA_HASH_HEADER,
            studio_runtime_http::api_schema_hash(),
        )
        .extension(ConnectInfo(
            "127.0.0.1:12345"
                .parse::<std::net::SocketAddr>()
                .expect("address"),
        ))
        .body(Body::from(body.to_string()))
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("HTTP");
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("response");
    (status, serde_json::from_slice(&bytes).expect("JSON"))
}

#[tokio::test]
async fn installed_runtime_drives_input_and_cancel_through_http() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let mut case = cases
        .iter()
        .find(|case| case["name"] == "finish-0-None-False")
        .expect("case")
        .clone();
    case["before"]["interactive"] = json!(true);
    let store = Arc::new(fixtures::store(root.path(), "http"));
    fixtures::seed(&store, &case);
    let port = Arc::new(support::Port::default());
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
    let runtime = Arc::new(MonitorRuntime::new(
        store.clone(),
        port.clone(),
        state.event_wake_handle(),
    ));
    let app = router(state).expect("router").layer(Extension(runtime));
    let body = json!({"id":case["before"]["id"],"text":"hello"});
    let (status, refusal) = call(&app, "/api/monitor/input", body.clone()).await;
    assert_eq!(status, 501);
    assert_eq!(
        refusal["error"],
        "Not implemented: requires monitor-input-request-identity"
    );
    assert!(port.submitted.lock().expect("submitted").is_empty());
    for attempt in 0..2 {
        let (status, response) = call(
            &app,
            "/api/monitor/cancel",
            json!({"id":case["before"]["id"]}),
        )
        .await;
        assert_eq!(status, 200, "{response}");
        assert_eq!(response["cancelRequested"], true);
        if attempt == 0 {
            let entities = response["_syncEntities"].as_array().expect("sync deltas");
            let entity_id = format!(
                "entity:monitor:{}",
                case["before"]["id"].as_str().expect("monitor id")
            );
            assert!(entities.iter().any(|entity| entity["id"] == entity_id));
            assert!(response["_syncEntitiesAfter"].is_number());
        }
    }
    assert_eq!(port.submitted.lock().expect("submitted").len(), 1);
    let saved = MonitorsRepository::get(
        &store.read().expect("read"),
        case["before"]["id"].as_str().expect("id"),
    )
    .expect("monitor")
    .expect("monitor")
    .value()
    .clone();
    assert_eq!(saved["cancelDispatch"]["status"], "written");
    assert_eq!(fixtures::events(&store), json!([]));
}
