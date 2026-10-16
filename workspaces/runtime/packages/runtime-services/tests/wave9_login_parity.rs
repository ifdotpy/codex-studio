#[path = "claude_runtime/login_fixture.rs"]
mod claude_fixture;
#[path = "support/prepared_python.rs"]
mod python_test_support;

use claude_fixture::{Fixture as ClaudeFixture, Script};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use studio_runtime_providers::{Method, RequestId, RequestTicket};
use studio_runtime_services::{
    account_driver::{AccountDriver, AuthMetadata, AuthStatus},
    account_session_runtime::{
        AccountSessionPort, AccountSessionRuntime, ConnectionScope, LoginRequest, ProviderRequest,
        ProviderResponse, RequestCertainty, RetainedAccountSession, SessionFuture, SessionReceipt,
    },
    claude_login::ports::{LoginEvent, LoginReceipts},
};

const CODEX_ID: &str = "4b154e4b-80bb-4f1f-b07f-dcaa15563190";
const CLAUDE_ADD_ID: &str = "e0166868-592a-4dca-8994-04d34a730af5";
const CLAUDE_START_ID: &str = "987edcdc-0e86-4fdf-a1c6-1513517d3ad0";
const CLAUDE_URL: &str = "https://claude.com/cai/oauth/authorize?fixture=1";

struct CodexSession(AtomicUsize);
impl RetainedAccountSession for CodexSession {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        Some(ConnectionScope {
            account: account.to_owned(),
            connection: "scripted-provider".into(),
            generation: 1,
        })
    }

    fn request(&self, request: ProviderRequest) -> SessionFuture {
        self.0.fetch_add(1, Ordering::SeqCst);
        let result = match request.method {
            Method::AccountLoginStart => json!({
                "type":"chatgptDeviceCode", "loginId":"fixture-login",
                "verificationUrl":"https://example.invalid/device", "userCode":"fixture-code"
            }),
            Method::AccountLoginCancel => json!({"status":"canceled"}),
            other => panic!("unexpected method {other:?}"),
        };
        let scope = ConnectionScope {
            account: request.account,
            connection: "scripted-provider".into(),
            generation: 1,
        };
        let operation = request.operation;
        let method = request.method;
        Box::pin(async move {
            Ok(SessionReceipt {
                scope: scope.clone(),
                operation: operation.clone(),
                ticket: Some(RequestTicket {
                    generation: scope.generation,
                    id: RequestId::String(operation),
                    method,
                }),
                certainty: RequestCertainty::Written,
                result: Some(ProviderResponse::Reply(result)),
                not_sent_reason: None,

                replayed: false,
            })
        })
    }
}

fn python_corpus() -> Value {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
    let python = python_test_support::prepared_python(&source);
    let output = std::process::Command::new(python)
        .args([
            "-B",
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/wave9_login_parity_oracle.py"
            ),
        ])
        .output()
        .expect("launch isolated Python parity corpus");
    assert!(
        output.status.success(),
        "Python login oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Python parity corpus JSON")
}

fn normalize_paths(value: &mut Value, roots: &[(&str, &str)], normalize_account: bool) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if key == "createdAt" {
                    *value = json!(0);
                } else if normalize_account && key == "accountKey" {
                    *value = json!("$ACCOUNT");
                } else {
                    normalize_paths(value, roots, normalize_account);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize_paths(item, roots, normalize_account);
            }
        }
        Value::String(string) => {
            for (root, replacement) in roots {
                if !root.is_empty() {
                    *string = string.replace(root, replacement);
                }
            }
        }
        _ => {}
    }
}

#[test]
fn codex_login_and_cancel_match_python_registry_and_replays() {
    let oracle = python_corpus();
    compare_codex(&oracle["codex"]);
}

#[tokio::test]
async fn claude_login_service_matches_python_add_profile_code_and_cancel_replays() {
    let oracle = python_corpus();
    compare_claude(&oracle["claudeAdd"], true).await;
    compare_claude(&oracle["claudeStart"], false).await;
}

fn compare_codex(expected: &Value) {
    let root = tempfile::tempdir().expect("isolated Rust account root");
    let accounts_root = root.path().join("codex-runtime/accounts");
    let base_home = root.path().join("codex-home");
    let home = root.path().join("home");
    std::fs::create_dir_all(&accounts_root).expect("accounts root");
    std::fs::create_dir_all(&base_home).expect("base home");
    std::fs::create_dir_all(&home).expect("home");
    let registry = accounts_root.join("registry.json");
    let mut initial = expected["initial"].clone();
    let py_root = expected["root"].as_str().expect("Python fixture root");
    // Retain Python's complete initial rows while redirecting their disposable paths.
    replace_paths(
        &mut initial,
        py_root,
        root.path().to_string_lossy().as_ref(),
    );
    std::fs::write(&registry, initial.to_string()).expect("initial registry");
    let driver = Arc::new(AccountDriver::open(&registry).expect("account driver"));
    let session = Arc::new(CodexSession(AtomicUsize::new(0)));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("account runtime");
    let service = AccountSessionRuntime::new(
        driver.clone(),
        Some(session.clone()),
        runtime.handle().clone(),
        accounts_root.clone(),
        base_home,
        home,
    );
    let request = || LoginRequest {
        login_id: CODEX_ID.into(),
        account_key: None,
        email: None,
        label: None,
    };
    let started = service.login(request()).expect("Codex login");
    let replay = service.login(request()).expect("Codex login replay");
    let cancelled = service.cancel_login(CODEX_ID).expect("Codex cancellation");
    let cancel_replay = service
        .cancel_login(CODEX_ID)
        .expect("Codex cancellation replay");
    let mut actual_responses = json!([started, replay, cancelled, cancel_replay]);
    let mut expected_responses = expected["responses"].clone();
    normalize_paths(&mut actual_responses, &[], false);
    for response in expected_responses
        .as_array_mut()
        .expect("Python login response sequence")
    {
        if let Some(object) = response.as_object_mut() {
            object.remove("emailHint");
            object.remove("requestedLabel");
        }
    }
    normalize_paths(&mut expected_responses, &[], false);
    assert_eq!(actual_responses, expected_responses, "Codex login receipts");
    assert_eq!(
        session.0.load(Ordering::SeqCst),
        2,
        "one start and one cancel"
    );
    let mut actual_registry: Value =
        serde_json::from_slice(&std::fs::read(&registry).expect("written Rust registry"))
            .expect("Rust registry JSON");
    let mut expected_registry = expected["stored"].clone();
    // Rust persists this notification marker for its account-list consumer;
    // Python emits the change through its runtime and has no registry field for it.
    actual_registry
        .as_object_mut()
        .expect("Rust registry object")
        .remove("pendingAccountsChange");
    let rust_root = root.path().to_string_lossy().into_owned();
    let python_root = expected["root"].as_str().expect("Python fixture root");
    let roots = [(python_root, "$ROOT"), (rust_root.as_str(), "$ROOT")];
    normalize_paths(&mut actual_registry, &roots, false);
    normalize_paths(&mut expected_registry, &roots, false);
    assert_eq!(
        actual_registry, expected_registry,
        "complete Codex account and login rows"
    );
}

fn replace_paths(value: &mut Value, from: &str, to: &str) {
    match value {
        Value::Object(object) => object
            .values_mut()
            .for_each(|value| replace_paths(value, from, to)),
        Value::Array(items) => items
            .iter_mut()
            .for_each(|value| replace_paths(value, from, to)),
        Value::String(string) => *string = string.replace(from, to),
        _ => {}
    }
}

async fn compare_claude(expected: &Value, adding: bool) {
    let f = ClaudeFixture::new();
    let service = f.open().await;
    let id = if adding {
        CLAUDE_ADD_ID
    } else {
        CLAUDE_START_ID
    };
    let account = if adding {
        None
    } else {
        let config = f.directory.join("profile-config");
        std::fs::create_dir_all(&config).expect("profile config");
        let metadata = AuthMetadata {
            status: AuthStatus::Ready,
            account_id: Some("claude:fixture@example.test".into()),
            email: Some("fixture@example.test".into()),
            plan: Some("max".into()),
            credential_identity: Some("claude:fixture@example.test".into()),
            error: None,
            error_kind: None,
        };
        Some(
            f.accounts
                .register_claude_verified(
                    json!({"binaryPath":"/fixture/scripted","configDir":config,"customModels":[]})
                        .as_object()
                        .expect("profile options")
                        .clone(),
                    "Fixture Claude",
                    &f.directory,
                    &metadata,
                )
                .expect("Claude profile"),
        )
    };
    let account_key = account.clone();
    let started = service
        .start(
            id.into(),
            account.clone(),
            adding.then(|| "fixture@example.test".into()),
            adding.then(|| "Fixture Claude".into()),
        )
        .await
        .expect("Claude start");
    let replay = service
        .start(
            id.into(),
            account,
            adding.then(|| "fixture@example.test".into()),
            adding.then(|| "Fixture Claude".into()),
        )
        .await
        .expect("Claude start replay");
    let child: Script = f.launcher.next().await;
    child
        .output
        .send(LoginEvent::Output(format!("{CLAUDE_URL}\n").into_bytes()))
        .await
        .expect("scripted link");
    let pending = f.status(&service, id, "pending").await;
    let code = if adding {
        Some(
            service
                .code(id, "fixture-code".into())
                .await
                .expect("Claude code"),
        )
    } else {
        None
    };
    let cancelled = service.cancel(id).await.expect("Claude cancellation");
    let final_replay = service
        .start(
            id.into(),
            if adding { None } else { account_key },
            adding.then(|| "fixture@example.test".into()),
            adding.then(|| "Fixture Claude".into()),
        )
        .await;
    let final_replay = final_replay.expect("Claude replay with same request content");
    let expected_writes = if adding {
        vec![b"fixture-code\n".to_vec()]
    } else {
        Vec::new()
    };
    assert_eq!(
        *child.writes.lock().expect("scripted stdin writes"),
        expected_writes,
        "same scripted Claude process boundary"
    );
    service.shutdown().await.expect("Claude shutdown");
    let stored = f
        .receipts
        .load()
        .expect("saved receipts")
        .into_iter()
        .find(|receipt| receipt.request_id() == id)
        .expect("saved login record")
        .record()
        .clone();
    let mut actual = json!([started, replay, pending, code, cancelled, final_replay]);
    let mut expected_responses = expected["responses"].clone();
    let mut rust_record = stored;
    let mut python_record = expected["stored"].clone();
    // Rust's receipt journal keeps restart lease metadata; Python keeps it in the
    // live job for profile reauth. Compare all persisted application fields.
    if !adding {
        for record in [&mut rust_record, &mut python_record] {
            if let Some(metadata) = record.get_mut("metadata").and_then(Value::as_object_mut) {
                metadata.remove("configDir");
                metadata.remove("inheritedLease");
            }
        }
    } else if let Some(metadata) = rust_record
        .get_mut("metadata")
        .and_then(Value::as_object_mut)
    {
        metadata.remove("inheritedLease");
    }
    for record in [&mut rust_record, &mut python_record] {
        if record
            .pointer("/metadata/child")
            .is_some_and(Value::is_null)
        {
            record["metadata"]
                .as_object_mut()
                .expect("record metadata")
                .remove("child");
        }
    }
    let python_root = expected["root"].as_str().expect("Python fixture root");
    let rust_root = f.directory.to_string_lossy().into_owned();
    let roots = [(python_root, "$ROOT"), (rust_root.as_str(), "$ROOT")];
    normalize_paths(&mut actual, &roots, true);
    normalize_paths(&mut expected_responses, &roots, true);
    normalize_paths(&mut rust_record, &roots, true);
    normalize_paths(&mut python_record, &roots, true);
    assert_eq!(
        actual, expected_responses,
        "Claude route response sequence add={adding}"
    );
    assert_eq!(
        rust_record, python_record,
        "complete Claude login row add={adding}"
    );
}
