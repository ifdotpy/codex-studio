//! The safe report read crosses HTTP and the retained Claude adapter without repair.
#![allow(clippy::unwrap_used)]
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn get(app: &axum::Router, path: &str, token: &str) -> (u16, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("host", "127.0.0.1:49171")
                .header("x-canvas-token", token)
                .header(
                    studio_runtime_http::API_SCHEMA_HASH_HEADER,
                    studio_runtime_http::schema::api_schema_hash(),
                )
                .extension(ConnectInfo(
                    "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn repair_report_read_is_scoped_bounded_and_does_not_expose_provider_text() {
    let (_root, store, app, mut worker, native) = super::claude_file_backups::fixture().await;
    let before: String = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT record FROM runtime_agents WHERE id='agent'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let (status, report) = get(&app, "/api/claude/repair-report?id=agent", "fixture-token").await;
    assert_eq!(status, 200, "{report}");
    assert_eq!(
        report,
        json!({"status":"needs_repair","skippedRows":3,"invalidSequence":true,"scopeConfirmed":true,"nativeHistoryConfirmed":false})
    );
    let calls = native.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["method"], "claude/repairReport");
    assert_eq!(calls[0]["params"], json!({"threadId":"thread"}));
    let after: String = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT record FROM runtime_agents WHERE id='agent'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(before, after);
    assert_ne!(
        get(&app, "/api/claude/repair-report?id=agent", "wrong")
            .await
            .0,
        200
    );
    assert_ne!(
        get(
            &app,
            "/api/claude/repair-report?id=foreign",
            "fixture-token"
        )
        .await
        .0,
        200
    );
    assert_ne!(
        get(&app, "/api/claude/repair-report", "fixture-token")
            .await
            .0,
        200
    );
    assert_eq!(native.calls.lock().unwrap().len(), 1);
    worker.shutdown().await.unwrap();
}
