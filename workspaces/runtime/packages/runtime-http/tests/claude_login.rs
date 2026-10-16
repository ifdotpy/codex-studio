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
use studio_runtime_services::claude_login::{
    AUTHORIZATION_NOT_CONFIRMED, FAILED, ports::LoginEvent,
};
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
    let (status, rejected) = call(
        &app,
        "POST",
        "/api/accounts/claude/login/code",
        Some(json!({"login_id":ID,"code":"fixture-code"})),
    )
    .await;
    assert_eq!(status, 400, "{rejected}");
    assert_eq!(rejected["outcome"], "not_applied");
    assert!(
        rejected["error"]
            .as_str()
            .expect("safe validation message")
            .contains("code and state separated by #")
    );
    assert!(
        !rejected["error"]
            .as_str()
            .expect("safe validation message")
            .contains("fixture-code")
    );
    let (status, before_code) = call(
        &app,
        "GET",
        &format!("/api/accounts/claude/login?request_id={ID}"),
        None,
    )
    .await;
    assert_eq!(status, 200, "{before_code}");
    assert_eq!(before_code["codeSubmitted"], false);
    assert!(child.writes.lock().expect("valid fixture").is_empty());
    let (status, receipt) = call(
        &app,
        "POST",
        "/api/accounts/claude/login/code",
        Some(json!({"login_id":ID,"code":"fixture-code#fixture-state"})),
    )
    .await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["codeSubmitted"], true);
    assert_eq!(
        child.writes.lock().expect("valid fixture").as_slice(),
        &[b"fixture-code#fixture-state\n".to_vec()]
    );
    let (status, replayed) = call(
        &app,
        "POST",
        "/api/accounts/claude/login/code",
        Some(json!({"login_id":ID,"code":"different-code#different-state"})),
    )
    .await;
    assert_eq!(status, 200, "{replayed}");
    assert_eq!(
        child.writes.lock().expect("valid fixture").len(),
        1,
        "a later code POST must not repeat the consumed provider input"
    );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambiguous_code_write_stays_latched_without_resending() {
    let f = Fixture::new();
    let service = f.open().await;
    service
        .start(ID.into(), None, None, None)
        .await
        .expect("start sign-in");
    let child = f.launcher.next().await;
    link(&f, &service, &child).await;

    // The fake child records receipt of the bytes, then reports that the pipe
    // acknowledgement was lost. The request must remain one-shot.
    child.write_unknown.store(true, Ordering::SeqCst);
    let receipt = service
        .code(ID, "fixture-code#fixture-state".into())
        .await
        .expect("code write outcome is returned as a receipt");
    assert_eq!(receipt["status"], "error");
    assert_eq!(receipt["codeSubmitted"], true);
    assert_eq!(
        child.writes.lock().expect("valid fixture").as_slice(),
        &[b"fixture-code#fixture-state\n".to_vec()]
    );

    let replayed = service
        .code(ID, "fixture-code#fixture-state".into())
        .await
        .expect("retry returns current receipt without writing again");
    assert_eq!(replayed["codeSubmitted"], true);
    assert_eq!(
        child.writes.lock().expect("valid fixture").len(),
        1,
        "unknown delivery must never submit provider input twice"
    );
    service
        .shutdown()
        .await
        .expect("fixture service shuts down");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inconclusive_status_probe_keeps_the_unknown_outcome() {
    let f = Fixture::new();
    let service = f.open().await;
    service
        .start(ID.into(), None, None, None)
        .await
        .expect("start sign-in");
    let child = f.launcher.next().await;
    link(&f, &service, &child).await;
    service
        .code(ID, "fixture-code#fixture-state".into())
        .await
        .expect("code is accepted once");
    child
        .output
        .send(LoginEvent::Exited(true))
        .await
        .expect("valid fixture");
    let probe = f.launcher.next().await;
    probe
        .output
        .send(LoginEvent::Output(b"not-json".to_vec()))
        .await
        .expect("valid fixture");
    probe
        .output
        .send(LoginEvent::Exited(true))
        .await
        .expect("valid fixture");

    service
        .shutdown()
        .await
        .expect("fixture service shuts down after probe");
    let persisted =
        studio_runtime_services::claude_login::ports::LoginReceipts::load(f.receipts.as_ref())
            .expect("load durable login receipts");
    let saved = persisted
        .iter()
        .find(|snapshot| snapshot.request_id() == ID)
        .expect("request receipt exists");
    assert_eq!(saved.receipt()["codeSubmitted"], true);
    assert_eq!(saved.receipt()["error"], FAILED);
    assert_eq!(saved.record()["metadata"]["outcomeUnknown"], true);
    assert_eq!(
        child.writes.lock().expect("valid fixture").as_slice(),
        &[b"fixture-code#fixture-state\n".to_vec()]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirmed_provider_rejection_has_safe_terminal_receipt() {
    let f = Fixture::new();
    let service = f.open().await;
    service
        .start(
            ID.into(),
            None,
            Some("fixture@example.test".into()),
            Some("Fixture".into()),
        )
        .await
        .expect("start sign-in");
    let child = f.launcher.next().await;
    link(&f, &service, &child).await;
    service
        .code(ID, "fixture-code#fixture-state".into())
        .await
        .expect("code is accepted once");
    child
        .output
        .send(LoginEvent::Diagnostic(
            b"sensitive provider diagnostic fixture".to_vec(),
        ))
        .await
        .expect("valid fixture");
    child
        .output
        .send(LoginEvent::Exited(true))
        .await
        .expect("valid fixture");
    let probe = f.launcher.next().await;
    probe
        .output
        .send(LoginEvent::Output(br#"{"loggedIn":true,"authMethod":"claude.ai","email":"other@example.test","subscriptionType":"max"}"#.to_vec()))
        .await
        .expect("valid fixture");
    probe
        .output
        .send(LoginEvent::Exited(true))
        .await
        .expect("valid fixture");

    let receipt = f.status(&service, ID, "error").await;
    assert_eq!(receipt["codeSubmitted"], true);
    assert_eq!(receipt["error"], AUTHORIZATION_NOT_CONFIRMED);
    assert!(
        !receipt["error"]
            .as_str()
            .expect("safe terminal message")
            .contains("sensitive provider diagnostic fixture")
    );
    assert_eq!(
        child.writes.lock().expect("valid fixture").as_slice(),
        &[b"fixture-code#fixture-state\n".to_vec()]
    );
    service
        .shutdown()
        .await
        .expect("fixture service shuts down");
}
