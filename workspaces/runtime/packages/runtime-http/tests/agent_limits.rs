//! Renderer limits use closed projections and the existing root-lead reset.
#![allow(clippy::unwrap_used)]
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_http::{HttpState, router};
use studio_runtime_services::{coordination, message_admission::PreparedAdmission, sync_views};
use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, RuntimeStore};
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
fn agent_limits_projection_keeps_effective_fields_and_excludes_private_receipts() {
    let record = json!({"id":"worker","rootId":"lead","parentId":"lead","status":"completed",
        "capabilityProfile":{"allowedTools":["Read"],"disallowedTools":["Agent"],"skills":[],"mcpServers":["docs"],"maxTurns":2,"secret":"hidden"},
        "completedWorkerTurns":2,"terminalReason":"worker_max_turns","tokensUsed":100,"tokenBudget":200,
        "tokenUsageAccounting":"provisional","budgetBlocked":"usage accounting incomplete",
        "cwd":"/old/path","workspaceCleanupIntent":{"phase":"not_sent","requestId":"private-id",
        "effect":{"identity":{"cwd":"/retained/path","private":"secret"}},"receipt":{"reason":"Submodule vendor has changes","private":"secret"}}});
    let projected = sync_views::agent_entity_view(&record, &Default::default()).unwrap();
    for field in [
        "completedWorkerTurns",
        "terminalReason",
        "tokensUsed",
        "tokenBudget",
        "tokenUsageAccounting",
        "budgetBlocked",
    ] {
        assert_eq!(projected[field], record[field], "{field}");
    }
    assert_eq!(projected["capabilityProfile"]["maxTurns"], 2);
    assert_eq!(projected["capabilityProfile"]["skills"], json!([]));
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
async fn continue_worker_http_requires_user_token_and_replays_without_resetting_again() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(dir.path())).await.unwrap());
    store.transaction(|tx| {
        for value in [
            json!({"id":"lead","rootId":"lead","isLead":true,"epoch":1,"status":"completed","autoWake":true,"cwd":"/work","concurrency":2}),
            json!({"id":"manager","rootId":"lead","parentId":"lead","epoch":1,"status":"completed","autoWake":true,"cwd":"/work"}),
            json!({"id":"worker","rootId":"lead","parentId":"manager","epoch":1,"status":"completed","autoWake":false,"cwd":"/work","capabilityProfile":{"maxTurns":2},"completedWorkerTurns":2,"terminalReason":"worker_max_turns"}),
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
    let body = json!({"id":"worker","request_id":"continue-once"});
    let (status, _) = call(
        &app,
        "POST",
        "/api/agents/continue",
        body.clone(),
        "worker-token",
    )
    .await;
    assert_eq!(status, 403);
    let denied = store
        .transaction_with_error(|tx| {
            coordination::send_followup(
                tx,
                coordination::AgentFollowup {
                    sender_id: "manager",
                    sender_epoch: 1,
                    recipient_id: "worker",
                    id: "worker-reset",
                    text: "Continue",
                    delivery: studio_runtime_schema::DeliveryMoment::NextTurn,
                    now: 10.0,
                },
                &PreparedAdmission::default(),
            )
        })
        .await;
    assert!(denied.unwrap_err().to_string().contains("maxTurns"));
    let (status, receipt) = call(
        &app,
        "POST",
        "/api/agents/continue",
        body.clone(),
        "fixture-token",
    )
    .await;
    assert_eq!(status, 200, "{receipt}");
    store
        .transaction(|tx| {
            let mut worker = AgentRepository::get(tx, &AgentId::from("worker"))?.unwrap();
            assert_eq!(worker.extra["completedWorkerTurns"], 0);
            assert_eq!(worker.extra["autoWake"], true);
            assert!(!worker.extra.contains_key("terminalReason"));
            worker.extra.insert("completedWorkerTurns".into(), json!(1));
            AgentRepository::put(tx, &worker)
        })
        .await
        .unwrap();
    let (status, replay) = call(&app, "POST", "/api/agents/continue", body, "fixture-token").await;
    assert_eq!(status, 200, "{replay}");
    assert_eq!(receipt["id"], replay["id"]);
    store
        .read_async("test/continue", |db| {
            let worker = AgentRepository::get(db, &AgentId::from("worker"))?.unwrap();
            assert_eq!(worker.extra["completedWorkerTurns"], 1);
            let events: i64 = db.query_row(
                "SELECT count(*) FROM runtime_events WHERE id='continue-once'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(events, 1);
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .unwrap();
    for body in [
        json!({"id":"lead","request_id":"invalid"}),
        json!({"id":"manager","request_id":"invalid"}),
        json!({"id":"worker","request_id":"invalid","sender_id":"lead"}),
    ] {
        let (status, result) =
            call(&app, "POST", "/api/agents/continue", body, "fixture-token").await;
        assert!((400..500).contains(&status), "{result}");
    }
}

#[tokio::test]
async fn team_budget_http_includes_deleted_members_and_reports_incomplete_usage() {
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
        Some(store),
        None,
    ))
    .unwrap();
    let (status, summary) = call(
        &app,
        "GET",
        "/api/team-budget?agent=worker",
        Value::Null,
        "fixture-token",
    )
    .await;
    assert_eq!(status, 200, "{summary}");
    assert_eq!(summary["rootId"], "lead");
    assert_eq!(summary["tokenBudget"], 100);
    assert_eq!(summary["tokensUsed"], 60);
    assert_eq!(summary["reached"], false);
    assert_eq!(
        summary["retainedWorktrees"],
        json!([{"id":"deleted","name":"Deleted worker","cleanup":{"phase":"not_sent","reason":"Submodule vendor has changes","path":"/kept/worktree"}}])
    );
    assert!(!summary.to_string().contains("private"));
    assert_eq!(summary["incomplete"].as_array().unwrap().len(), 3);
}
