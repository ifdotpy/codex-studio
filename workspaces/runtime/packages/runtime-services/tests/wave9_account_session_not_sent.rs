use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use studio_runtime_providers::{Method, RequestId, RequestTicket};
use studio_runtime_services::{
    account_driver::AccountDriver,
    account_session_runtime::{
        AccountSessionError, AccountSessionPort, AccountSessionRuntime, ConnectionScope,
        LoginRequest, NotSentReason, ProviderRequest, ProviderResponse, RequestCertainty,
        RetainedAccountSession, SessionFuture, SessionFutureScope, SessionReceipt,
    },
};

struct FakeSession {
    certainty: RequestCertainty,
    result: Option<ProviderResponse>,
    unknown_error: bool,
    foreign_receipt: bool,
    no_scope: bool,
    calls: AtomicUsize,
}

impl FakeSession {
    fn new(certainty: RequestCertainty, result: Option<Value>) -> Self {
        Self::with_response(certainty, result.map(ProviderResponse::Reply))
    }

    fn with_response(certainty: RequestCertainty, result: Option<ProviderResponse>) -> Self {
        Self {
            certainty,
            result,
            unknown_error: false,
            foreign_receipt: false,
            no_scope: false,
            calls: AtomicUsize::new(0),
        }
    }

    fn unknown_error() -> Self {
        Self {
            certainty: RequestCertainty::Unknown,
            result: None,
            unknown_error: true,
            foreign_receipt: false,
            no_scope: false,
            calls: AtomicUsize::new(0),
        }
    }

    fn foreign_receipt() -> Self {
        Self {
            certainty: RequestCertainty::Written,
            result: Some(ProviderResponse::Reply(login_response())),
            unknown_error: false,
            foreign_receipt: true,
            no_scope: false,
            calls: AtomicUsize::new(0),
        }
    }

    fn no_live_scope() -> Self {
        Self {
            certainty: RequestCertainty::NotSent,
            result: None,
            unknown_error: false,
            foreign_receipt: false,
            no_scope: true,
            calls: AtomicUsize::new(0),
        }
    }
}

impl RetainedAccountSession for FakeSession {
    fn ensure_session(&self, account: String) -> SessionFutureScope {
        let scope = self.current_scope(&account);
        Box::pin(async move {
            scope.ok_or(AccountSessionError::Requires(
                "provider-session/account-session",
            ))
        })
    }

    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        if self.no_scope {
            return None;
        }
        Some(ConnectionScope {
            account: account.to_owned(),
            connection: "fixture-session".to_owned(),
            generation: 1,
        })
    }

    fn request(&self, request: ProviderRequest) -> SessionFuture {
        // A missing scope models the broker's known NotSent response. Count
        // only requests that would reach a retained provider connection.
        if request.expected_scope.is_some() {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
        let certainty = self.certainty;
        let result = self.result.clone();
        let unknown_error = self.unknown_error;
        let foreign_receipt = self.foreign_receipt;
        let scope = request.expected_scope.clone().unwrap_or(ConnectionScope {
            account: request.account.clone(),
            connection: "fixture-session".to_owned(),
            generation: 1,
        });
        let scope = if foreign_receipt {
            ConnectionScope {
                connection: "foreign-session".to_owned(),
                ..scope
            }
        } else {
            scope
        };
        let operation = request.operation;
        let ticket = (certainty != RequestCertainty::NotSent).then(|| RequestTicket {
            generation: scope.generation,
            id: RequestId::String(operation.clone()),
            method: request.method,
        });
        Box::pin(async move {
            if unknown_error {
                return Err(AccountSessionError::Unknown);
            }
            Ok(SessionReceipt {
                scope,
                operation,
                certainty,
                ticket,
                result,
                not_sent_reason: (certainty == RequestCertainty::NotSent)
                    .then_some(NotSentReason::NoLiveAccountWorker),
                replayed: false,
            })
        })
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    driver: Arc<AccountDriver>,
    accounts_root: std::path::PathBuf,
    base_home: std::path::PathBuf,
    home: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("fixture root");
        let accounts_root = root.path().join("accounts");
        let base_home = root.path().join("base-home");
        let home = root.path().join("home");
        std::fs::create_dir_all(&accounts_root).expect("accounts root");
        std::fs::create_dir_all(&base_home).expect("base home");
        std::fs::create_dir_all(&home).expect("home");
        let registry = accounts_root.join("registry.json");
        std::fs::write(
            &registry,
            json!({
                "version":1,
                "defaultAccountKey":"default",
                "accounts":{"default":{"id":"default","home":base_home,"source":"Fixture","label":"Default","status":"signedOut"}},
                "logins":{}
            })
            .to_string(),
        )
        .expect("registry");
        Self {
            _root: root,
            driver: Arc::new(AccountDriver::open(registry).expect("validated registry")),
            accounts_root,
            base_home,
            home,
        }
    }

    fn service(&self, session: Arc<FakeSession>) -> AccountSessionRuntime {
        AccountSessionRuntime::new(
            Arc::clone(&self.driver),
            Some(session),
            tokio::runtime::Handle::current(),
            self.accounts_root.clone(),
            self.base_home.clone(),
            self.home.clone(),
        )
    }

    async fn login(&self, certainty: RequestCertainty, result: Option<Value>) -> Value {
        let session = Arc::new(FakeSession::new(certainty, result));
        self.service(session)
            .login(LoginRequest {
                login_id: "9b06d4c4-0b71-49b5-b984-927d6fdcde01".into(),
                account_key: None,
                email: None,
                label: None,
            })
            .await
            .expect("login receipt")
    }

    async fn login_with_session(&self, session: Arc<FakeSession>) -> Value {
        self.service(session)
            .login(LoginRequest {
                login_id: "9b06d4c4-0b71-49b5-b984-927d6fdcde01".into(),
                account_key: None,
                email: None,
                label: None,
            })
            .await
            .expect("login receipt")
    }
}

fn login_response() -> Value {
    json!({
        "type":"chatgptDeviceCode",
        "loginId":"fixture-native-login",
        "verificationUrl":"https://example.invalid",
        "userCode":"fixture-code"
    })
}

#[tokio::test]
async fn login_not_sent_returns_python_error_receipt() {
    let fixture = Fixture::new();
    let receipt = fixture.login(RequestCertainty::NotSent, None).await;
    assert_eq!(receipt["status"], "error");
    assert_eq!(
        receipt["error"],
        "Could not start sign-in: the Codex app server for this account is not running. Try again."
    );
}

#[tokio::test]
async fn absent_live_scope_maps_login_and_cancel_to_python_not_sent_receipts() {
    let fixture = Fixture::new();
    let session = Arc::new(FakeSession::no_live_scope());
    let runtime = fixture.service(Arc::clone(&session));
    let login_id = "9b06d4c4-0b71-49b5-b984-927d6fdcde01";
    let login = runtime
        .login(LoginRequest {
            login_id: login_id.into(),
            account_key: None,
            email: None,
            label: None,
        })
        .await
        .expect("not-sent login receipt");
    assert_eq!(login["status"], "error");
    assert_eq!(
        login["error"],
        "Could not start sign-in: the Codex app server for this account did not start. Try again."
    );
    assert_eq!(session.calls.load(Ordering::SeqCst), 0);

    let cancel_fixture = Fixture::new();
    let started = cancel_fixture
        .login(RequestCertainty::Written, Some(login_response()))
        .await;
    assert_eq!(started["status"], "pending");
    let no_live_session = Arc::new(FakeSession::no_live_scope());
    let cancel_runtime = cancel_fixture.service(Arc::clone(&no_live_session));
    let cancel = cancel_runtime
        .cancel_login(login_id)
        .await
        .expect("not-sent cancellation receipt");
    assert_eq!(cancel["status"], "pending");
    assert_eq!(
        cancel["error"],
        "Cancellation is unconfirmed. Check status or retry cancellation."
    );
    assert_eq!(no_live_session.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn absent_recovery_scope_returns_superseded_without_provider_replay() {
    // Python owner: codex_connection_recovery.recover returns superseded when
    // the agent's connection context is no longer current (lines 227-264).
    let fixture = Fixture::new();
    let session = Arc::new(FakeSession::no_live_scope());
    let result = fixture
        .service(Arc::clone(&session))
        .recover_connection("default", "fixture-agent")
        .await
        .expect("stale recovery is a completed superseded outcome");
    assert_eq!(result, json!({"status":"superseded"}));
    assert_eq!(session.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unknown_login_remains_uncertain_and_is_never_replayed() {
    let fixture = Fixture::new();
    let session = Arc::new(FakeSession::new(RequestCertainty::Unknown, None));
    for _ in 0..2 {
        let receipt = fixture.login_with_session(Arc::clone(&session)).await;
        assert_eq!(receipt["status"], "uncertain");
        assert_eq!(
            receipt["error"],
            "Sign-in response is unconfirmed. Check its status before starting again."
        );
    }
    assert_eq!(session.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unknown_login_error_remains_uncertain_and_is_never_replayed() {
    let fixture = Fixture::new();
    let session = Arc::new(FakeSession::unknown_error());
    for _ in 0..2 {
        let receipt = fixture.login_with_session(Arc::clone(&session)).await;
        assert_eq!(receipt["status"], "uncertain");
        assert_eq!(
            receipt["error"],
            "Sign-in response is unconfirmed. Check its status before starting again."
        );
    }
    assert_eq!(session.calls.load(Ordering::SeqCst), 1);
    let snapshot = fixture.driver.snapshot().expect("persisted registry");
    assert_eq!(snapshot.logins[0]["status"], "uncertain");
    let restarted_driver = Arc::new(
        AccountDriver::open(fixture.accounts_root.join("registry.json"))
            .expect("restart from durable registry"),
    );
    let restarted = AccountSessionRuntime::new(
        restarted_driver,
        Some(session.clone()),
        tokio::runtime::Handle::current(),
        fixture.accounts_root.clone(),
        fixture.base_home.clone(),
        fixture.home.clone(),
    );
    let after_restart = restarted
        .login(LoginRequest {
            login_id: "9b06d4c4-0b71-49b5-b984-927d6fdcde01".into(),
            account_key: None,
            email: None,
            label: None,
        })
        .await
        .expect("durable uncertain replay");
    assert_eq!(after_restart["status"], "uncertain");
    assert_eq!(session.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn foreign_login_receipt_scope_is_uncertain_and_never_replayed() {
    let fixture = Fixture::new();
    let session = Arc::new(FakeSession::foreign_receipt());
    for _ in 0..2 {
        let receipt = fixture.login_with_session(Arc::clone(&session)).await;
        assert_eq!(receipt["status"], "uncertain");
        assert_eq!(
            receipt["error"],
            "Sign-in response is unconfirmed. Check its status before starting again."
        );
    }
    assert_eq!(session.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn generic_provider_requests_reject_foreign_and_unknown_receipts() {
    for session in [
        Arc::new(FakeSession::foreign_receipt()),
        Arc::new(FakeSession::unknown_error()),
    ] {
        let fixture = Fixture::new();
        let service = fixture.service(Arc::clone(&session));
        let expected_scope = service
            .current_scope("default")
            .expect("live provider scope");
        let result = service
            .provider_request(ProviderRequest {
                account: "default".into(),
                operation: "fixture:generic-request".into(),
                actor: None,
                expected_scope: Some(expected_scope),
                deleted_actor_session: None,
                method: Method::AccountLoginStart,
                params: json!({"type":"chatgptDeviceCode"}),
                deadline: std::time::Instant::now() + std::time::Duration::from_secs(1),
            })
            .await;
        assert!(matches!(result, Err(AccountSessionError::Unknown)));
        assert_eq!(session.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn cancel_not_sent_restores_python_pending_receipt_and_allows_safe_retry() {
    let fixture = Fixture::new();
    fixture
        .login(RequestCertainty::Written, Some(login_response()))
        .await;
    let session = Arc::new(FakeSession::new(RequestCertainty::NotSent, None));
    let service = fixture.service(Arc::clone(&session));
    let first = service
        .cancel_login("9b06d4c4-0b71-49b5-b984-927d6fdcde01")
        .await
        .expect("not-sent cancellation receipt");
    assert_eq!(first["status"], "pending");
    assert_eq!(
        first["error"],
        "Cancellation is unconfirmed. Check status or retry cancellation."
    );
    let second = service
        .cancel_login("9b06d4c4-0b71-49b5-b984-927d6fdcde01")
        .await
        .expect("safe retry receipt");
    assert_eq!(second["status"], "pending");
    assert_eq!(session.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn unknown_cancel_remains_uncertain_and_is_never_replayed() {
    let fixture = Fixture::new();
    fixture
        .login(RequestCertainty::Written, Some(login_response()))
        .await;
    let session = Arc::new(FakeSession::new(RequestCertainty::Unknown, None));
    let service = fixture.service(Arc::clone(&session));
    for _ in 0..2 {
        let receipt = service
            .cancel_login("9b06d4c4-0b71-49b5-b984-927d6fdcde01")
            .await
            .expect("uncertain cancellation receipt");
        assert_eq!(receipt["status"], "uncertain");
    }
    assert_eq!(session.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rejected_cancel_response_is_not_treated_as_canceled_or_replayed() {
    let fixture = Fixture::new();
    fixture
        .login(RequestCertainty::Written, Some(login_response()))
        .await;
    let session = Arc::new(FakeSession::with_response(
        RequestCertainty::Written,
        Some(ProviderResponse::Rejected(json!({"status":"canceled"}))),
    ));
    let service = fixture.service(Arc::clone(&session));
    let first = service
        .cancel_login("9b06d4c4-0b71-49b5-b984-927d6fdcde01")
        .await
        .expect("rejected cancellation receipt");
    assert_eq!(first["status"], "uncertain");
    let second = service
        .cancel_login("9b06d4c4-0b71-49b5-b984-927d6fdcde01")
        .await
        .expect("replayed uncertain receipt");
    assert_eq!(second["status"], "uncertain");
    assert_eq!(session.calls.load(Ordering::SeqCst), 1);
}
