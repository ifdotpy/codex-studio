use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use studio_runtime_http::{HttpState, router};
use studio_runtime_services::{
    account_driver::AccountDriver,
    account_session_runtime::{
        AccountSessionError, AccountSessionRuntime, ConnectionScope, RequestCertainty,
        RetainedAccountSession, SessionFuture, SessionReceipt,
    },
};
use tower::ServiceExt;

const ID: &str = "4b154e4b-80bb-4f1f-b07f-dcaa15563190";
struct Session {
    path: PathBuf,
    calls: AtomicUsize,
    certainty: RequestCertainty,
    foreign: bool,
}
impl RetainedAccountSession for Session {
    fn scope(&self, account: &str, _: bool) -> Result<ConnectionScope, AccountSessionError> {
        Ok(ConnectionScope {
            account: account.into(),
            connection: "fixture-connection".into(),
        })
    }
    fn request(
        &self,
        mut scope: ConnectionScope,
        operation: String,
        method: &'static str,
        params: Value,
        deadline: tokio::time::Instant,
    ) -> SessionFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(deadline > tokio::time::Instant::now());
        let registry: Value =
            serde_json::from_slice(&std::fs::read(&self.path).expect("durable registry"))
                .expect("JSON");
        let result = if method == "account/login/start" {
            assert_eq!(registry["logins"][ID]["status"], "starting");
            assert_eq!(params, json!({"type":"chatgptDeviceCode"}));
            json!({"type":"chatgptDeviceCode","loginId":"native","verificationUrl":"https://example.invalid/device","userCode":"fixture"})
        } else {
            assert_eq!(method, "account/login/cancel");
            assert_eq!(registry["logins"][ID]["status"], "uncertain");
            assert_eq!(params, json!({"loginId":"native"}));
            json!({"status":"canceled"})
        };
        if self.foreign {
            scope.connection = "foreign".into();
        }
        let certainty = self.certainty;
        Box::pin(async move {
            SessionReceipt {
                scope,
                operation,
                certainty,
                result: Some(result),
            }
        })
    }
}
async fn post(app: &axum::Router, path: &str, value: Value) -> (u16, Value) {
    let request = Request::builder()
        .method("POST")
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
                .expect("address"),
        ))
        .body(Body::from(value.to_string()))
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("HTTP response");
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    (status, serde_json::from_slice(&bytes).expect("JSON"))
}
fn fixture(
    certainty: RequestCertainty,
    foreign: bool,
) -> (tempfile::TempDir, axum::Router, Arc<Session>) {
    let root = tempfile::tempdir().expect("fixture state");
    let path = root.path().join("registry.json");
    std::fs::write(&path,json!({"version":1,"defaultAccountKey":"default","accounts":{"default":{"id":"default","home":root.path(),"label":"Default","source":"Fixture","status":"signedOut"}},"logins":{}}).to_string()).expect("registry");
    let driver = Arc::new(AccountDriver::open(&path).expect("driver"));
    let session = Arc::new(Session {
        path,
        calls: AtomicUsize::new(0),
        certainty,
        foreign,
    });
    let service = AccountSessionRuntime::new(
        driver,
        Some(session.clone()),
        tokio::runtime::Handle::current(),
        root.path().into(),
        root.path().join("base"),
        PathBuf::from(std::env::var_os("HOME").expect("isolated home")),
    );
    let mut state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        None,
        root.path().to_string_lossy().into(),
        49171,
        None,
        None,
        None,
    );
    state.account_session = Some(Arc::new(service));
    (root, router(state).expect("router"), session)
}
#[tokio::test]
async fn login_and_cancel_use_committed_exact_identity_through_http() {
    let (_root, app, session) = fixture(RequestCertainty::Written, false);
    let (status, first) = post(&app, "/api/accounts/login", json!({"login_id":ID})).await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["status"], "pending");
    assert!(first.get("emailHint").is_none());
    let (status, replay) = post(&app, "/api/accounts/login", json!({"login_id":ID})).await;
    assert_eq!(status, 200);
    assert_eq!(first, replay);
    let (status, _) = post(
        &app,
        "/api/accounts/login",
        json!({"login_id":ID,"label":"changed"}),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(session.calls.load(Ordering::SeqCst), 1);
    for _ in 0..2 {
        let (status, result) =
            post(&app, "/api/accounts/login/cancel", json!({"login_id":ID})).await;
        assert_eq!(status, 200, "{result}");
        assert_eq!(result["status"], "cancelled");
    }
    assert_eq!(session.calls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn unknown_foreign_and_not_sent_receipts_never_repeat_native_login() {
    for (certainty, foreign, expected) in [
        (RequestCertainty::Unknown, false, "uncertain"),
        (RequestCertainty::Written, true, "uncertain"),
        (RequestCertainty::NotSent, false, "error"),
    ] {
        let (_root, app, session) = fixture(certainty, foreign);
        for _ in 0..2 {
            let (status, result) = post(&app, "/api/accounts/login", json!({"login_id":ID})).await;
            assert_eq!(status, 200, "{result}");
            assert_eq!(result["status"], expected);
        }
        assert_eq!(session.calls.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn failed_intent_write_cannot_reach_native_session() {
    let (root, app, session) = fixture(RequestCertainty::Written, false);
    let saved = root.path().join("saved-registry.json");
    std::fs::rename(&session.path, &saved).expect("hold registry");
    std::fs::create_dir(&session.path).expect("fail atomic replace");
    let (status, _) = post(&app, "/api/accounts/login", json!({"login_id":ID})).await;
    assert_eq!(status, 500);
    assert_eq!(session.calls.load(Ordering::SeqCst), 0);
    std::fs::remove_dir(&session.path).expect("remove failure");
    std::fs::rename(saved, &session.path).expect("restore");
    let (status, result) = post(&app, "/api/accounts/login", json!({"login_id":ID})).await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(session.calls.load(Ordering::SeqCst), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn login_material_uses_private_modes_even_when_registry_modes_are_broad() {
    use std::os::unix::fs::PermissionsExt;

    let (root, app, session) = fixture(RequestCertainty::Written, false);
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755))
        .expect("broaden account directory");
    std::fs::set_permissions(&session.path, std::fs::Permissions::from_mode(0o644))
        .expect("broaden registry file");

    let (status, result) = post(&app, "/api/accounts/login", json!({"login_id":ID})).await;

    assert_eq!(status, 200, "{result}");
    let login_home = root.path().join(format!("login-{ID}"));
    assert_eq!(
        std::fs::metadata(&login_home)
            .expect("login home")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(root.path())
            .expect("accounts directory")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&session.path)
            .expect("registry file")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[tokio::test]
async fn login_refuses_preexisting_symlink_that_escapes_accounts_root() {
    let (root, app, session) = fixture(RequestCertainty::Written, false);
    let outside = tempfile::tempdir().expect("outside directory");
    let escaped_home = root.path().join(format!("login-{ID}"));
    std::os::unix::fs::symlink(outside.path(), &escaped_home).expect("login home symlink");

    let (status, _) = post(&app, "/api/accounts/login", json!({"login_id":ID})).await;

    assert_eq!(status, 500);
    assert_eq!(session.calls.load(Ordering::SeqCst), 0);
    assert!(
        std::fs::read_dir(outside.path())
            .expect("outside remains readable")
            .next()
            .is_none()
    );
    assert!(
        std::fs::symlink_metadata(&escaped_home)
            .expect("link remains")
            .file_type()
            .is_symlink()
    );
}
