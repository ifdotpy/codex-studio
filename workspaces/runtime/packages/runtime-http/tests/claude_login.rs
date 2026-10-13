#[path = "../../runtime-services/tests/claude_runtime/login_fixture.rs"]
mod fixture;
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use fixture::*;
use serde_json::{Value, json};
use studio_runtime_http::{HttpState, router};
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
            studio_runtime_http::schema::api_schema_hash(),
        )
        .extension(ConnectInfo(
            "127.0.0.1:12345"
                .parse::<std::net::SocketAddr>()
                .expect("valid fixture"),
        ))
        .body(body.map_or_else(Body::empty, |v| Body::from(v.to_string())))
        .expect("valid fixture");
    let response = app.clone().oneshot(request).await.expect("valid fixture");
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("valid fixture");
    (
        status,
        serde_json::from_slice(&bytes).expect("valid fixture"),
    )
}
#[tokio::test]
async fn five_login_routes_drive_retained_actor_and_reauth_cancel() {
    let f = Fixture::new();
    let service = f.open().await;
    let mut state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        None,
        f.directory.to_string_lossy().into(),
        49171,
        None,
        None,
        None,
    );
    state.claude_login = Some(service.clone());
    let app = router(state).expect("valid fixture");
    let (status, started) = call(
        &app,
        "POST",
        "/api/accounts/claude/add",
        Some(json!({"login_id":ID})),
    )
    .await;
    assert_eq!(status, 200, "{started}");
    let child = f.launcher.next().await;
    link(&f, &service, &child).await;
    let (status, receipt) = call(
        &app,
        "GET",
        &format!("/api/accounts/claude/login?request_id={ID}"),
        None,
    )
    .await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["verificationUrl"], URL);
    let (status, receipt) = call(
        &app,
        "POST",
        "/api/accounts/claude/login/code",
        Some(json!({"login_id":ID,"code":"fixture-code"})),
    )
    .await;
    assert_eq!(status, 200, "{receipt}");
    let ready = complete(&f, &service, &child).await;
    let second = "a4166868-592a-4dca-8994-04d34a730af5";
    let (status, receipt) = call(
        &app,
        "POST",
        "/api/accounts/claude/login",
        Some(json!({"login_id":second,"account_key":ready["accountKey"]})),
    )
    .await;
    assert_eq!(status, 200, "{receipt}");
    let reauth = f.launcher.next().await;
    let (status, receipt) = call(
        &app,
        "POST",
        "/api/accounts/claude/login/cancel",
        Some(json!({"login_id":second})),
    )
    .await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["status"], "cancelled");
    service.shutdown().await.expect("valid fixture");
    assert!(reauth.closed.load(std::sync::atomic::Ordering::SeqCst));
}
