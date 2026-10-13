#[path = "../support/prepared_python.rs"]
mod python;
pub(super) mod support;

#[tokio::test]
async fn http_accepts_unrelated_operation_started_after_preparation() {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;
    let fixture = support::Fixture::new("http", python::prepared_python);
    let state = crate::router::HttpState::new(
        "token".into(),
        "workspace".into(),
        None,
        "isolated".into(),
        4620,
        None,
        Some(fixture.store.clone()),
        None,
    );
    let app = crate::router::router(state).expect("router");
    let (ready, release) = support::arm();
    let store = fixture.store.clone();
    let operation =
        std::thread::spawn(move || support::start_late_operation(&store, ready, release));
    let request = Request::builder().method("POST").uri("/api/messages")
        .header("x-canvas-token", "token").header("host", "127.0.0.1:4620").header("content-type", "application/json")
        .header(crate::schema::API_SCHEMA_HASH_HEADER, crate::schema::api_schema_hash())
        .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 12345))))
        .body(Body::from(serde_json::json!({"room":"late-operation-recipient", "id":"00000000-0000-4000-8000-000000000001", "text":"hello"}).to_string())).expect("request");
    let response = app.oneshot(request).await.expect("HTTP response");
    operation.join().expect("operation writer joins");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let receipt: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.expect("body"))
            .expect("receipt");
    assert_eq!(receipt["status"], "queued");
    assert!(
        receipt.get("waitingFor").is_none(),
        "unrelated operation is not a blocker"
    );
}
