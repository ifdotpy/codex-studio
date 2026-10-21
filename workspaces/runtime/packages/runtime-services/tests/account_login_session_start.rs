//! A Codex sign-in on an account without a live provider session starts that
//! account's session before it sends `account/login/start`. The fake session
//! models the account session broker: no scope and a NotSent receipt until
//! `ensure_session` starts the account's worker.
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
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

const LOGIN_ID: &str = "6f1d2a3b-4c5d-4e6f-8a9b-0c1d2e3f4a5b";

struct BrokerSession {
    /// Whether `ensure_session` can start a worker for the account.
    can_start: bool,
    live: Mutex<Vec<String>>,
    ensured: Mutex<Vec<String>>,
    written: AtomicUsize,
}

impl BrokerSession {
    fn new(can_start: bool) -> Arc<Self> {
        Arc::new(Self {
            can_start,
            live: Mutex::new(Vec::new()),
            ensured: Mutex::new(Vec::new()),
            written: AtomicUsize::new(0),
        })
    }
}

fn scope(account: &str) -> ConnectionScope {
    ConnectionScope {
        account: account.to_owned(),
        connection: "fixture-session".to_owned(),
        generation: 1,
    }
}

impl RetainedAccountSession for BrokerSession {
    fn ensure_session(&self, account: String) -> SessionFutureScope {
        self.ensured.lock().expect("ensured").push(account.clone());
        let result = if self.can_start {
            self.live.lock().expect("live").push(account.clone());
            Ok(scope(&account))
        } else {
            Err(AccountSessionError::Requires(
                "provider-session/account-session",
            ))
        };
        Box::pin(async move { result })
    }

    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        self.live
            .lock()
            .expect("live")
            .iter()
            .any(|live| live == account)
            .then(|| scope(account))
    }

    fn request(&self, request: ProviderRequest) -> SessionFuture {
        let live = self.current_scope(&request.account);
        let operation = request.operation;
        let Some(live) = live.filter(|live| request.expected_scope.as_ref() == Some(live)) else {
            let receipt = SessionReceipt {
                scope: scope(&request.account),
                operation,
                ticket: None,
                certainty: RequestCertainty::NotSent,
                result: None,
                not_sent_reason: Some(NotSentReason::NoLiveAccountWorker),
                replayed: false,
            };
            return Box::pin(async move { Ok(receipt) });
        };
        assert_eq!(request.method, Method::AccountLoginStart);
        self.written.fetch_add(1, Ordering::SeqCst);
        let receipt = SessionReceipt {
            ticket: Some(RequestTicket {
                generation: live.generation,
                id: RequestId::String(operation.clone()),
                method: request.method,
            }),
            scope: live,
            operation,
            certainty: RequestCertainty::Written,
            result: Some(ProviderResponse::Reply(json!({
                "type":"chatgptDeviceCode",
                "loginId":"fixture-native-login",
                "verificationUrl":"https://example.invalid/device",
                "userCode":"fixture-code"
            }))),
            not_sent_reason: None,
            replayed: false,
        };
        Box::pin(async move { Ok(receipt) })
    }
}

struct Fixture {
    root: tempfile::TempDir,
    driver: Arc<AccountDriver>,
}

impl Fixture {
    /// `signed-out` is a saved Codex subscription account without credentials.
    fn new() -> Self {
        let root = tempfile::tempdir().expect("fixture root");
        let accounts = root.path().join("accounts");
        for directory in ["accounts", "base-home", "home", "signed-out-home"] {
            std::fs::create_dir_all(root.path().join(directory)).expect("fixture directory");
        }
        let registry = accounts.join("registry.json");
        std::fs::write(
            &registry,
            json!({
                "version":1,
                "defaultAccountKey":"default",
                "accounts":{
                    "default":{"id":"default","home":root.path().join("base-home"),"source":"Fixture","label":"Default","status":"signedOut"},
                    "signed-out":{"id":"signed-out","home":root.path().join("signed-out-home"),"source":"Codex Agents","label":"Signed out","accountId":"fixture-subscription","email":"fixture@example.invalid","plan":"pro","status":"signedOut"}
                },
                "logins":{}
            })
            .to_string(),
        )
        .expect("registry");
        Self {
            driver: Arc::new(AccountDriver::open(registry).expect("validated registry")),
            root,
        }
    }

    fn service(&self, session: Arc<BrokerSession>) -> AccountSessionRuntime {
        AccountSessionRuntime::new(
            Arc::clone(&self.driver),
            Some(session),
            tokio::runtime::Handle::current(),
            self.root.path().join("accounts"),
            self.root.path().join("base-home"),
            self.root.path().join("home"),
        )
    }

    async fn login(&self, session: Arc<BrokerSession>, account_key: Option<&str>) -> Value {
        self.service(session)
            .login(LoginRequest {
                login_id: LOGIN_ID.into(),
                account_key: account_key.map(str::to_owned),
                email: None,
                label: Some("Fixture".into()),
            })
            .await
            .expect("login receipt")
    }
}

#[tokio::test]
async fn new_account_sign_in_starts_its_session_and_returns_the_device_code() {
    let fixture = Fixture::new();
    let session = BrokerSession::new(true);
    let receipt = fixture.login(Arc::clone(&session), None).await;
    assert_eq!(receipt["status"], "pending", "{receipt}");
    assert_eq!(receipt["userCode"], "fixture-code");
    assert_eq!(
        *session.ensured.lock().expect("ensured"),
        vec![format!("login-{LOGIN_ID}")]
    );
    assert_eq!(session.written.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn signed_out_account_sign_in_starts_its_session_and_returns_the_device_code() {
    let fixture = Fixture::new();
    let listed = fixture.driver.listing_snapshot().expect("listing");
    assert!(
        listed
            .accounts
            .iter()
            .any(|row| row["id"] == "signed-out" && row["status"] == "signedOut")
    );
    let session = BrokerSession::new(true);
    let receipt = fixture
        .login(Arc::clone(&session), Some("signed-out"))
        .await;
    assert_eq!(receipt["status"], "pending", "{receipt}");
    assert_eq!(receipt["accountKey"], "signed-out");
    assert_eq!(
        *session.ensured.lock().expect("ensured"),
        vec!["signed-out".to_owned()]
    );
    assert_eq!(session.written.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn session_start_failure_is_not_sent_and_names_its_cause() {
    let fixture = Fixture::new();
    let session = BrokerSession::new(false);
    let receipt = fixture.login(Arc::clone(&session), None).await;
    assert_eq!(receipt["status"], "error");
    assert_eq!(
        receipt["error"],
        "Could not start sign-in: the Codex app server for this account did not start. Try again."
    );
    assert_eq!(session.written.load(Ordering::SeqCst), 0);
    let stored = fixture.driver.snapshot().expect("registry");
    assert_eq!(stored.logins[0]["error"], receipt["error"]);
}
