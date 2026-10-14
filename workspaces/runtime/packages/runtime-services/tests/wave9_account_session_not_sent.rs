use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use studio_runtime_providers::{RequestId, RequestTicket};
use studio_runtime_services::{
    account_driver::AccountDriver,
    account_session_runtime::{
        AccountSessionPort, AccountSessionRuntime, ConnectionScope, LoginRequest, NotSentReason,
        ProviderRequest, ProviderResponse, RequestCertainty, RetainedAccountSession, SessionFuture,
        SessionReceipt,
    },
};

struct FakeSession {
    certainty: RequestCertainty,
    result: Option<ProviderResponse>,
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
            calls: AtomicUsize::new(0),
        }
    }
}

impl RetainedAccountSession for FakeSession {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        (self.certainty != RequestCertainty::NotSent).then(|| ConnectionScope {
            account: account.to_owned(),
            connection: "fixture-session".to_owned(),
            generation: 1,
        })
    }

    fn request(&self, request: ProviderRequest) -> SessionFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let certainty = self.certainty;
        let result = self.result.clone();
        let scope = request.expected_scope.unwrap_or_else(|| ConnectionScope {
            account: request.account,
            connection: "fixture-session".to_owned(),
            generation: 1,
        });
        let operation = request.operation;
        let ticket = (certainty != RequestCertainty::NotSent).then(|| RequestTicket {
            generation: scope.generation,
            id: RequestId::String(operation.clone()),
            method: request.method,
        });
        Box::pin(async move {
            Ok(SessionReceipt {
                scope,
                operation,
                certainty,
                ticket,
                result,
                not_sent_reason: (certainty == RequestCertainty::NotSent)
                    .then_some(NotSentReason::NoLiveAccountWorker),
            })
        })
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    driver: Arc<AccountDriver>,
    runtime: tokio::runtime::Runtime,
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
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime"),
            accounts_root,
            base_home,
            home,
        }
    }

    fn service(&self, session: Arc<FakeSession>) -> AccountSessionRuntime {
        AccountSessionRuntime::new(
            Arc::clone(&self.driver),
            Some(session),
            self.runtime.handle().clone(),
            self.accounts_root.clone(),
            self.base_home.clone(),
            self.home.clone(),
        )
    }

    fn login(&self, certainty: RequestCertainty, result: Option<Value>) -> Value {
        let session = Arc::new(FakeSession::new(certainty, result));
        self.service(session)
            .login(LoginRequest {
                login_id: "9b06d4c4-0b71-49b5-b984-927d6fdcde01".into(),
                account_key: None,
                email: None,
                label: None,
            })
            .expect("login receipt")
    }

    fn login_with_session(&self, session: Arc<FakeSession>) -> Value {
        self.service(session)
            .login(LoginRequest {
                login_id: "9b06d4c4-0b71-49b5-b984-927d6fdcde01".into(),
                account_key: None,
                email: None,
                label: None,
            })
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

#[test]
fn login_not_sent_returns_python_error_receipt() {
    let fixture = Fixture::new();
    let receipt = fixture.login(RequestCertainty::NotSent, None);
    assert_eq!(receipt["status"], "error");
    assert_eq!(receipt["error"], "Could not start sign-in. Try again.");
}

#[test]
fn unknown_login_remains_uncertain_and_is_never_replayed() {
    let fixture = Fixture::new();
    let session = Arc::new(FakeSession::new(RequestCertainty::Unknown, None));
    for _ in 0..2 {
        let receipt = fixture.login_with_session(Arc::clone(&session));
        assert_eq!(receipt["status"], "uncertain");
        assert_eq!(
            receipt["error"],
            "Sign-in response is unconfirmed. Check its status before starting again."
        );
    }
    assert_eq!(session.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn cancel_not_sent_restores_python_pending_receipt_and_allows_safe_retry() {
    let fixture = Fixture::new();
    fixture.login(RequestCertainty::Written, Some(login_response()));
    let session = Arc::new(FakeSession::new(RequestCertainty::NotSent, None));
    let service = fixture.service(Arc::clone(&session));
    let first = service
        .cancel_login("9b06d4c4-0b71-49b5-b984-927d6fdcde01")
        .expect("not-sent cancellation receipt");
    assert_eq!(first["status"], "pending");
    assert_eq!(
        first["error"],
        "Cancellation is unconfirmed. Check status or retry cancellation."
    );
    let second = service
        .cancel_login("9b06d4c4-0b71-49b5-b984-927d6fdcde01")
        .expect("safe retry receipt");
    assert_eq!(second["status"], "pending");
    assert_eq!(session.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn unknown_cancel_remains_uncertain_and_is_never_replayed() {
    let fixture = Fixture::new();
    fixture.login(RequestCertainty::Written, Some(login_response()));
    let session = Arc::new(FakeSession::new(RequestCertainty::Unknown, None));
    let service = fixture.service(Arc::clone(&session));
    for _ in 0..2 {
        let receipt = service
            .cancel_login("9b06d4c4-0b71-49b5-b984-927d6fdcde01")
            .expect("uncertain cancellation receipt");
        assert_eq!(receipt["status"], "uncertain");
    }
    assert_eq!(session.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn rejected_cancel_response_is_not_treated_as_canceled_or_replayed() {
    let fixture = Fixture::new();
    fixture.login(RequestCertainty::Written, Some(login_response()));
    let session = Arc::new(FakeSession::with_response(
        RequestCertainty::Written,
        Some(ProviderResponse::Rejected(json!({"status":"canceled"}))),
    ));
    let service = fixture.service(Arc::clone(&session));
    let first = service
        .cancel_login("9b06d4c4-0b71-49b5-b984-927d6fdcde01")
        .expect("rejected cancellation receipt");
    assert_eq!(first["status"], "uncertain");
    let second = service
        .cancel_login("9b06d4c4-0b71-49b5-b984-927d6fdcde01")
        .expect("replayed uncertain receipt");
    assert_eq!(second["status"], "uncertain");
    assert_eq!(session.calls.load(Ordering::SeqCst), 1);
}
