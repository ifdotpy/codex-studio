#[path = "../../runtime-services/tests/claude_runtime/login_fixture.rs"]
mod fixture;
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use fixture::*;
use serde_json::{Value, json};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use studio_runtime_http::{HttpState, router};
use tower::ServiceExt;

struct ReadyNotificationGate {
    hold_next: AtomicBool,
    entered: tokio::sync::Notify,
    release: (Mutex<bool>, Condvar),
}
impl ReadyNotificationGate {
    fn new() -> Self {
        Self {
            hold_next: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: (Mutex::new(false), Condvar::new()),
        }
    }
    fn hold_next(&self) {
        self.hold_next.store(true, Ordering::SeqCst);
    }
    fn pause_if_held(&self) {
        if !self.hold_next.swap(false, Ordering::SeqCst) {
            return;
        }
        self.entered.notify_one();
        let (released, changed) = &self.release;
        let mut released = released.lock().expect("valid fixture");
        while !*released {
            released = changed.wait(released).expect("valid fixture");
        }
        *released = false;
    }
    fn release(&self) {
        let (released, changed) = &self.release;
        *released
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        changed.notify_one();
    }
}
struct ReleaseReadyGateOnDrop(Arc<ReadyNotificationGate>);
impl Drop for ReleaseReadyGateOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn five_login_routes_drive_retained_actor_and_reauth_cancel() {
    let f = Fixture::new();
    let ready_gate = Arc::new(ReadyNotificationGate::new());
    let callback_gate = Arc::clone(&ready_gate);
    let changes = Arc::clone(&f.changes);
    let service = f
        .open_with_change(Arc::new(move || {
            callback_gate.pause_if_held();
            changes.notify_one();
        }))
        .await;
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
    child
        .output
        .send(LoginEvent::Exited(true))
        .await
        .expect("valid fixture");
    let probe = f.launcher.next().await;
    assert_eq!(probe.command.args, vec!["auth", "status", "--json"]);
    probe
        .output
        .send(LoginEvent::Diagnostic(b"ignored diagnostic".to_vec()))
        .await
        .expect("valid fixture");
    probe
        .output
        .send(LoginEvent::Output(br#"{"loggedIn":true,"authMethod":"claude.ai","email":"fixture@example.test","subscriptionType":"max"}"#.to_vec()))
        .await
        .expect("valid fixture");
    let release_gate_guard = ReleaseReadyGateOnDrop(Arc::clone(&ready_gate));
    ready_gate.hold_next();
    probe
        .output
        .send(LoginEvent::Exited(true))
        .await
        .expect("valid fixture");
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        ready_gate.entered.notified(),
    )
    .await
    .expect("ready publication reached its notification");
    let second = "a4166868-592a-4dca-8994-04d34a730af5";
    let responses = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let ready = call(
            &app,
            "GET",
            &format!("/api/accounts/claude/login?request_id={ID}"),
            None,
        )
        .await;
        let reauth = call(
            &app,
            "POST",
            "/api/accounts/claude/login",
            Some(json!({"login_id":second,"account_key":ready.1["accountKey"]})),
        )
        .await;
        (ready, reauth)
    })
    .await;
    drop(release_gate_guard);
    let ((ready_status, ready), (reauth_status, receipt)) =
        responses.expect("ready observation and immediate reauthentication complete");
    assert_eq!(ready_status, 200, "{ready}");
    assert_eq!(ready["status"], "ready");
    assert_eq!(reauth_status, 200, "{receipt}");
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
