//! The safe report read crosses HTTP and the retained Claude adapter without repair.
#![allow(clippy::unwrap_used)]
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn get(app: &axum::Router, path: &str, origin: Option<&str>) -> (u16, Value) {
    let mut request = Request::builder();
    if let Some(origin) = origin {
        request = request.header("origin", origin);
    }
    let response = app
        .clone()
        .oneshot(
            request
                .uri(path)
                .header("host", "127.0.0.1:49171")
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
    let (status, report) = get(&app, "/api/claude/repair-report?id=agent", None).await;
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
    // Read routes require a trusted local origin, without a write token.
    assert_eq!(
        get(
            &app,
            "/api/claude/repair-report?id=agent",
            Some("https://attacker.example")
        )
        .await
        .0,
        403
    );
    assert_ne!(
        get(&app, "/api/claude/repair-report?id=foreign", None)
            .await
            .0,
        200
    );
    assert_ne!(get(&app, "/api/claude/repair-report", None).await.0, 200);
    assert_eq!(native.calls.lock().unwrap().len(), 1);
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn repair_report_refuses_invalid_evidence_without_exposing_provider_values() {
    let (_root, _store, app, mut worker, native) = super::claude_file_backups::fixture().await;
    let report = json!({"status":"needs_repair","skippedRows":3,"invalidSequence":true,"scopeConfirmed":true,"nativeHistoryConfirmed":false});
    for (field, value) in [
        ("status", json!("private-provider-text")),
        ("skippedRows", json!(-1)),
        ("skippedRows", json!(9007199254740992_u64)),
        ("invalidSequence", json!("private-provider-text")),
        ("scopeConfirmed", json!("private-provider-text")),
    ] {
        let mut invalid = report.clone();
        invalid[field] = value;
        *native.repair_report.lock().unwrap() = Some(invalid);
        let (status, body) = get(&app, "/api/claude/repair-report?id=agent", None).await;
        assert_eq!(status, 500, "{field}: {body}");
        assert!(!body.to_string().contains("private-provider-text"));
    }
    worker.shutdown().await.unwrap();
}
