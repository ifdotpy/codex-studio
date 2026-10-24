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

async fn batch_fixture() -> (tempfile::TempDir, Arc<RuntimeStore>, axum::Router) {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    let path = root.path().to_path_buf();
    let now = studio_runtime_services::event_write::unix_time().unwrap();
    store
        .transaction(move |tx| {
            AgentRepository::put(
                tx,
                &AgentRecord::from_value(json!({"id":"owner","rootId":"owner","epoch":0,
            "status":"waiting","autoWake":true,"cwd":path.to_string_lossy(),
            "accountKey":"default","provider":"codex","name":"Fixture worker"}))?,
            )?;
            for id in ["one", "two", "three"] {
                Table::new(tx).put(&MonitorRecord::from_value(monitor(
                    &path,
                    id,
                    "lost",
                    Value::Null,
                    "Command outcome unknown after losing the provider result; not rerun.",
                    now,
                ))?)?;
            }
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .unwrap();
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
    (root, store, app)
}

#[tokio::test]
async fn studio_errors_batch_exact_ids_unknown_and_idempotent_repeat() {
    let (_root, _store, app) = batch_fixture().await;
    let request = json!({"ids":["monitor:one", "missing", "monitor:three", "monitor:one"]});
    let (status, body) = call(
        &app,
        "POST",
        "/api/studio-errors/dismiss",
        Some(request.clone()),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["dismissedIds"],
        json!(["monitor:one", "monitor:three"])
    );
    assert_eq!(body["unknownIds"], json!(["missing"]));
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["id"], "monitor:two");
    let (status, repeat) = call(&app, "POST", "/api/studio-errors/dismiss", Some(request)).await;
    assert_eq!(status, 200, "{repeat}");
    assert_eq!(repeat["dismissedIds"], json!([]));
    assert_eq!(
        repeat["unknownIds"],
        json!(["monitor:one", "missing", "monitor:three"])
    );
    assert_eq!(repeat["items"], body["items"]);
}

#[tokio::test]
async fn studio_errors_batch_rejects_oversize_without_dismissing_anything() {
    let (_root, _store, app) = batch_fixture().await;
    let ids = vec![
        "monitor:one";
        studio_runtime_schema::shared_values::STUDIO_ERRORS_DISMISS_MAX_IDS + 1
    ];
    let (status, body) = call(
        &app,
        "POST",
        "/api/studio-errors/dismiss",
        Some(json!({"ids":ids})),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let (_, body) = call(&app, "GET", "/api/studio-errors", None).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn studio_errors_batch_rolls_back_all_receipts_if_one_write_fails() {
    let (_root, store, app) = batch_fixture().await;
    store
        .transaction(|tx| {
            tx.execute_batch(
                "CREATE TRIGGER fail_second_dismissal BEFORE INSERT ON runtime_operation_receipts
            WHEN NEW.id='studio-errors:dismissed:monitor:two'
            BEGIN SELECT RAISE(ABORT, 'fixture dismissal failure'); END;",
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .unwrap();
    let (status, body) = call(
        &app,
        "POST",
        "/api/studio-errors/dismiss",
        Some(json!({"ids":["monitor:one", "monitor:two"]})),
    )
    .await;
    assert_eq!(status, 500, "{body}");
    let (_, body) = call(&app, "GET", "/api/studio-errors", None).await;
    assert_eq!(
        body["items"].as_array().unwrap().len(),
        3,
        "the first dismissal must roll back"
    );
}

#[tokio::test]
async fn startup_models_http_reports_pending_and_refuses_an_unknown_explicit_account() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    let registry = json!({"version":1,"defaultAccountKey":"default","accounts":{},"logins":{}});
    let policy = studio_runtime_services::account_policy::AccountPolicyState::from_startup(
        Some(&serde_json::to_vec(&registry).unwrap()),
        [],
    ).unwrap();
    let state = HttpState::new(
        "fixture-token".into(), "fixture".into(), store.root().to_string_lossy().into(),
        49171, None, Some(store), Some(Arc::new(policy)),
    );
    let app = router(state).unwrap();
    let (status, value) = call(&app, "GET", "/api/models", None).await;
    assert_eq!(status, 503, "startup is pending, not an invalid request: {value}");
    assert_eq!(value["catalogPending"], true);
    let (status, value) = call(&app, "GET", "/api/models?account_key=unknown", None).await;
    assert_eq!(status, 400, "explicit unknown accounts remain invalid: {value}");
    assert_ne!(value["catalogPending"], true);
}
