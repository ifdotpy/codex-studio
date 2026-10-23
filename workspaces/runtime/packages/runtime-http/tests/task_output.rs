//! Task output uses the trusted-origin HTTP boundary and the stored owner path.
#![allow(clippy::unwrap_used)]
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_http::{HttpState, router};
use studio_runtime_store::{RuntimeStore, StoreError};
use tower::ServiceExt;

const LOCAL_ORIGIN: &str = "http://127.0.0.1:49171";

async fn get(app: &axum::Router, path: &str, origin: &str) -> (u16, Value) {
    let request = Request::builder()
        .uri(path)
        .header("host", "127.0.0.1:49171")
        .header("origin", origin)
        .header(
            studio_runtime_http::API_SCHEMA_HASH_HEADER,
            studio_runtime_http::api_schema_hash(),
        )
        .extension(ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
async fn authorized_task_output_read_and_projection() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    tokio::fs::write(workspace.path().join("out.txt"), "Claude task output")
        .await
        .unwrap();
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    let cwd = workspace.path().to_string_lossy().into_owned();
    store.transaction_with(move |tx| {
        let owner = json!({"id":"owner","rootId":"owner","epoch":1,"status":"idle","provider":"claude","cwd":cwd});
        let task = json!({"id":"task","agent":"owner","itemId":"task:native","kind":"tool","status":"running","created":1,"outputFile":"out.txt"});
        tx.execute("INSERT INTO runtime_agents(id,record) VALUES('owner',?)",[owner.to_string()])?;
        tx.execute("INSERT INTO runtime_tasks(id,record) VALUES('task',?)",[task.to_string()])?;
        Ok::<_, StoreError>(())
    }).await.unwrap();
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
    let (status, value) = get(&app, "/api/task/output?id=task&agent=owner", LOCAL_ORIGIN).await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["text"], "Claude task output");
    let (status, detail) = get(&app, "/api/task?id=task", LOCAL_ORIGIN).await;
    assert_eq!(status, 200, "{detail}");
    assert_eq!(detail["outputFile"], "out.txt");
    assert!(
        get(&app, "/api/task/output?id=task&agent=another", LOCAL_ORIGIN)
            .await
            .0
            >= 400
    );
    let (status, refusal) = get(
        &app,
        "/api/task/output?id=task&agent=owner",
        "https://untrusted.invalid",
    )
    .await;
    assert_eq!(status, 403, "{refusal}");
    store.transaction(move |tx| {
        tx.execute("UPDATE runtime_tasks SET record=json_set(record,'$.outputFile','../secret') WHERE id='task'",[])?;
        Ok(())
    }).await.unwrap();
    assert!(
        get(&app, "/api/task/output?id=task&agent=owner", LOCAL_ORIGIN)
            .await
            .0
            >= 400
    );
}

#[tokio::test]
async fn monitor_restart_http_preserves_identity_and_waits_for_old_exit() {
    use axum::Extension;
    use studio_runtime_services::monitor_runtime_wiring::MonitorRuntime;
    let root = tempfile::tempdir().unwrap();
    let mut case = crate::monitor_fixtures::corpus(root.path()).remove(0);
    case["actor"]["yoloMode"] = json!(true);
    case["before"]["outputPatterns"] = json!(["marker"]);
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    crate::monitor_fixtures::seed(&store, &case).await;
    let port = Arc::new(crate::monitor_runtime_support::Port::default());
    let state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
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
    let app = router(state).unwrap().layer(Extension(runtime.clone()));
    let body = json!({"id":case["before"]["id"],"agent":"agent","epoch":2,"accountKey":"default","requestId":"ui-restart","outputPatterns":["marker complete"]});
    let mut first = None;
    for _ in 0..2 {
        let request = Request::builder()
            .method("POST")
            .uri("/api/monitor/restart")
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
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let response: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(status, 200, "{response}");
        if let Some(id) = &first {
            assert_eq!(&response["id"], id);
        }
        first = Some(response["id"].clone());
    }
    let db = store.test_reader().unwrap();
    let count: i64 = db
        .query_row("SELECT count(*) FROM runtime_monitors", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 2);
    assert!(
        studio_runtime_services::monitor_driver::pending_start_ids(&store)
            .await
            .unwrap()
            .is_empty()
    );
    let old =
        studio_runtime_store::MonitorsRepository::get(&db, case["before"]["id"].as_str().unwrap())
            .unwrap()
            .unwrap();
    assert_eq!(old.value()["cancelRequested"], true);
    assert!(port.starts.lock().unwrap().is_empty());
}
