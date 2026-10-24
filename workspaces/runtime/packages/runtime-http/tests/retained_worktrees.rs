//! Retained worktrees use the closed cleanup projection, also for deleted workers.
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
use studio_runtime_services::sync_views;
use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore};
use tower::ServiceExt;

async fn call(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Value,
    token: &str,
) -> (u16, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("host", "127.0.0.1:49171")
                .header("content-type", "application/json")
                .header("x-canvas-token", token)
                .header(
                    studio_runtime_http::API_SCHEMA_HASH_HEADER,
                    studio_runtime_http::api_schema_hash(),
                )
                .extension(ConnectInfo(
                    "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
                ))
                .body(if method == "GET" {
                    Body::empty()
                } else {
                    Body::from(body.to_string())
                })
                .unwrap(),
        )
        .await
        .unwrap();
    let code = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (code, serde_json::from_slice(&bytes).unwrap())
}

#[test]
fn the_cleanup_projection_keeps_the_folder_and_reason_without_private_receipts() {
    let record = json!({"id":"worker","rootId":"lead","parentId":"lead","status":"completed",
        "cwd":"/old/path","workspaceCleanupIntent":{"phase":"not_sent","requestId":"private-id",
        "effect":{"identity":{"cwd":"/retained/path","private":"secret"}},"receipt":{"reason":"Submodule vendor has changes","private":"secret"}}});
    let projected = sync_views::agent_entity_view(&record, &Default::default()).unwrap();
    assert_eq!(
        projected["workspaceCleanupIntent"],
        json!({"phase":"not_sent","reason":"Submodule vendor has changes","path":"/retained/path"})
    );
    assert!(!projected.to_string().contains("secret"));
    assert!(!projected.to_string().contains("private-id"));
    let mut long_record = record.clone();
    let path = format!("/work/{}", "folder/".repeat(400));
    long_record["workspaceCleanupIntent"]["effect"]["identity"]["cwd"] = json!(path);
    long_record["workspaceCleanupIntent"]["receipt"]["reason"] = json!("x".repeat(4000));
    let bounded = sync_views::project_persisted_record("agent", &long_record).unwrap();
    assert_eq!(bounded["workspaceCleanupIntent"]["path"], path);
    assert!(
        bounded["workspaceCleanupIntent"]["reason"]
            .as_str()
            .unwrap()
            .len()
            < 4000
    );

    assert!(
        studio_runtime_schema::first_error("AgentEntityDto", &projected)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        sync_views::project_persisted_record("agent", &projected).unwrap(),
        projected
    );
}

#[tokio::test]
async fn retained_worktrees_http_lists_deleted_members_without_private_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(dir.path())).await.unwrap());
    store.transaction(|tx| {
        for value in [
            json!({"id":"lead","rootId":"lead","isLead":true,"epoch":1,"status":"completed","autoWake":true,"cwd":"/work","tokensUsed":10,"tokenBudget":100}),
            json!({"id":"worker","rootId":"lead","parentId":"lead","epoch":1,"status":"completed","autoWake":false,"cwd":"/work","tokensUsed":20}),
            json!({"id":"deleted","rootId":"lead","parentId":"lead","epoch":1,"status":"completed","autoWake":false,"cwd":"/work","tokensUsed":30,"deletedAt":1,"name":"Deleted worker","workspaceCleanupIntent":{"phase":"not_sent","effect":{"kind":"remove_worktree","identity":{"cwd":"/kept/worktree"}},"receipt":{"reason":"Submodule vendor has changes","secret":"private"}}}),
        ] { AgentRepository::put(tx, &AgentRecord::from_value(value).unwrap())?; }
        Ok(())
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
    let (status, summary) = call(
        &app,
        "GET",
        "/api/retained-worktrees?agent=worker",
        Value::Null,
        "fixture-token",
    )
    .await;
    assert_eq!(status, 200, "{summary}");
    assert_eq!(summary["rootId"], "lead");
    assert!(summary.get("tokenBudget").is_none(), "{summary}");
    assert_eq!(
        summary["retainedWorktrees"],
        json!([{"id":"deleted","name":"Deleted worker","cleanup":{"phase":"not_sent","reason":"Submodule vendor has changes","path":"/kept/worktree"}}])
    );
    assert!(!summary.to_string().contains("private"));
    let validator = studio_runtime_schema::compiled_validator("RetainedWorktreesResponse")
        .expect("retained worktrees response schema compiles");
    assert!(validator.is_valid(&summary));
    let mut invalid = summary.clone();
    invalid["tokenBudget"] = json!(100);
    assert!(!validator.is_valid(&invalid), "the response has no budget");
    // Resource-triggered reads must settle without publishing another update.
    let updates = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&updates);
    store.add_resource_change_observer(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
    });
    let (status, repeated) = call(
        &app,
        "GET",
        "/api/retained-worktrees?agent=worker",
        Value::Null,
        "fixture-token",
    )
    .await;
    assert_eq!(status, 200, "{repeated}");
    assert_eq!(repeated, summary);
    assert_eq!(updates.load(Ordering::SeqCst), 0);
}
