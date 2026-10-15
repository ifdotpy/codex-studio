#[allow(clippy::duplicate_mod)]
#[path = "support/prepared_python.rs"]
mod prepared_python_support;

use serde_json::{Value, json};
use std::sync::Arc;
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
use studio_runtime_providers::{Method, RequestId, RequestTicket};
use studio_runtime_services::{
    account_session_runtime::{
        AccountSessionError, AccountSessionPort, ConnectionScope, NotSentReason, ProviderRequest,
        ProviderResponse, RequestCertainty, SessionReceipt,
    },
    account_worker::preparation::PreparationResources,
    account_worker::retained::RequestActor,
    agent_lifecycle_runtime::{LifecycleEffect, LifecycleEffectKind, LifecyclePorts},
    agent_management::ManagementError,
    safety_buffering,
};
use studio_runtime_store::{RuntimeStore, Transaction};

fn test_resources() -> Arc<PreparationResources> {
    Arc::new(
        PreparationResources::bundled(
            String::new(),
            studio_runtime_services::model_catalog::ModelCatalogService::new(None),
        )
        .expect("bundled preparation resources"),
    )
}

fn python_corpus() -> (PathBuf, Value) {
    static CORPUS: OnceLock<(PathBuf, Value)> = OnceLock::new();
    CORPUS
        .get_or_init(|| {
            let root =
                std::env::temp_dir().join(format!("w9-safety-buffer-{}", std::process::id()));
            let source = Path::new(env!("CARGO_MANIFEST_DIR"));
            let output = Command::new(prepared_python_support::prepared_python(
                &source.join("../../apps/server/src"),
            ))
            .arg(source.join("tests/wave9_safety_buffering_oracle.py"))
            .arg(&root)
            .output()
            .expect("Python safety corpus");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let corpus: Value = serde_json::from_slice(&output.stdout).expect("Python corpus JSON");
            (root, corpus)
        })
        .clone()
}

struct Session<'a> {
    store: &'a RuntimeStore,
    scope: ConnectionScope,
    calls: Mutex<Vec<Value>>,
    unknown_at: Option<usize>,
    unknown_receipt_at: Option<usize>,
    requires_at: Option<usize>,
    not_sent_at: Option<usize>,
    not_sent_used: Mutex<bool>,
    reject_start: bool,
}
impl AccountSessionPort for Session<'_> {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        (account == self.scope.account).then(|| self.scope.clone())
    }

    fn provider_request(
        &self,
        request: ProviderRequest,
    ) -> Result<SessionReceipt, AccountSessionError> {
        let connection = self
            .store
            .read()
            .map_err(|_| AccountSessionError::Persistence)?;
        let raw = connection
            .query_row(
                "SELECT record FROM runtime_safety_retries WHERE id='agent:turn'",
                [],
                |row| row.get::<_, String>(0),
            )
            .map_err(|_| AccountSessionError::Persistence)?;
        let intent: Value =
            serde_json::from_str(&raw).map_err(|_| AccountSessionError::Persistence)?;
        assert_eq!(intent["dispatch"], "intent");
        assert_eq!(intent["rpcMethod"], request.method.as_str());
        assert_eq!(intent["operationId"], request.operation);
        assert_eq!(intent["requestParams"], request.params);
        assert_eq!(request.expected_scope, Some(self.scope.clone()));
        let mut calls = self.calls.lock().expect("calls");
        let call_index = calls.len();
        calls.push(json!({
            "stage":intent["stage"],
            "method":request.method.as_str(),
            "params":request.params,
            "operation":request.operation,
            "scope":request.expected_scope,
        }));
        drop(calls);
        if self.unknown_at == Some(call_index) {
            return Err(AccountSessionError::Unknown);
        }
        if self.unknown_receipt_at == Some(call_index) {
            return Ok(SessionReceipt {
                scope: self.scope.clone(),
                operation: request.operation,
                ticket: None,
                certainty: RequestCertainty::Unknown,
                result: None,
                not_sent_reason: None,

                replayed: false,
            });
        }
        if self.requires_at == Some(call_index) {
            return Err(AccountSessionError::Requires(
                "fixture/temporarily-unavailable",
            ));
        }
        if self.not_sent_at == Some(call_index) {
            let mut used = self.not_sent_used.lock().expect("not-sent state");
            if !*used {
                *used = true;
                return Ok(SessionReceipt {
                    scope: self.scope.clone(),
                    operation: request.operation.clone(),
                    ticket: None,
                    certainty: RequestCertainty::NotSent,
                    result: None,
                    not_sent_reason: Some(NotSentReason::WorkerNotSent),

                    replayed: false,
                });
            }
        }
        let stage = intent["stage"].as_str().expect("intent stage");
        let result = match request.method {
            Method::ThreadTurnsList => {
                let verify = stage == "verify_turns";
                json!({"data":[{"id":"turn","status":if verify {"interrupted"} else {"inProgress"}}]})
            }
            Method::ThreadItemsList => {
                json!({"data":[{"turnId":"turn","item":{"type":"userMessage","content":[{"type":"text","text":"hello"}]}}]})
            }
            Method::TurnInterrupt => json!({}),
            Method::ThreadFork => json!({"thread":{"id":"new-thread"}}),
            Method::TurnStart => json!({"turn":{"id":"new-turn"}}),
            _ => return Err(AccountSessionError::Refused("Unexpected safety method")),
        };
        Ok(SessionReceipt {
            scope: self.scope.clone(),
            operation: request.operation.clone(),
            ticket: Some(RequestTicket {
                generation: self.scope.generation,
                id: RequestId::String(request.operation),
                method: request.method,
            }),
            certainty: RequestCertainty::Written,
            result: Some(
                if self.reject_start && request.method == Method::TurnStart {
                    ProviderResponse::Rejected(json!({"message":"start rejected"}))
                } else {
                    ProviderResponse::Reply(result)
                },
            ),
            not_sent_reason: None,

            replayed: false,
        })
    }
}

struct BlockingSession {
    scope: ConnectionScope,
    entered: Mutex<Option<mpsc::Sender<()>>>,
    release: Mutex<mpsc::Receiver<()>>,
    calls: Mutex<Vec<String>>,
}

struct IntentBlockingSession {
    scope: ConnectionScope,
    scope_calls: AtomicUsize,
    entered: Mutex<Option<mpsc::Sender<()>>>,
    release: Mutex<mpsc::Receiver<()>>,
    requests: AtomicUsize,
}

impl AccountSessionPort for IntentBlockingSession {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        if self.scope_calls.fetch_add(1, Ordering::SeqCst) == 1 {
            if let Some(entered) = self.entered.lock().expect("entered").take() {
                entered.send(()).expect("notify before intent");
            }
            self.release
                .lock()
                .expect("release")
                .recv()
                .expect("release before intent");
        }
        (account == self.scope.account).then(|| self.scope.clone())
    }

    fn provider_request(
        &self,
        _request: ProviderRequest,
    ) -> Result<SessionReceipt, AccountSessionError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        Err(AccountSessionError::Refused("unexpected dispatch"))
    }
}

struct StartBlockingSession {
    store: Arc<RuntimeStore>,
    scope: ConnectionScope,
    entered: Mutex<Option<mpsc::Sender<()>>>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl AccountSessionPort for StartBlockingSession {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        (account == self.scope.account).then(|| self.scope.clone())
    }

    fn provider_request(
        &self,
        request: ProviderRequest,
    ) -> Result<SessionReceipt, AccountSessionError> {
        let raw = self
            .store
            .read()
            .map_err(|_| AccountSessionError::Persistence)?
            .query_row(
                "SELECT record FROM runtime_safety_retries WHERE id='agent:turn'",
                [],
                |row| row.get::<_, String>(0),
            )
            .map_err(|_| AccountSessionError::Persistence)?;
        let intent: Value =
            serde_json::from_str(&raw).map_err(|_| AccountSessionError::Persistence)?;
        assert_eq!(intent["dispatch"], "intent");
        assert_eq!(intent["rpcMethod"], request.method.as_str());
        let stage = intent["stage"].as_str().unwrap_or_default();
        if request.method == Method::TurnStart {
            if let Some(entered) = self.entered.lock().expect("entered").take() {
                entered.send(()).expect("notify at accepted start");
            }
            self.release
                .lock()
                .expect("release")
                .recv()
                .expect("release accepted start");
        }
        let result = match request.method {
            Method::ThreadTurnsList => {
                json!({"data":[{"id":"turn","status":if stage == "verify_turns" {"interrupted"} else {"inProgress"}}]})
            }
            Method::ThreadItemsList => {
                json!({"data":[{"turnId":"turn","item":{"type":"userMessage","content":[{"type":"text","text":"hello"}]}}]})
            }
            Method::TurnInterrupt => json!({}),
            Method::ThreadFork => json!({"thread":{"id":"new-thread"}}),
            Method::TurnStart => json!({"turn":{"id":"new-turn"}}),
            _ => return Err(AccountSessionError::Refused("Unexpected safety method")),
        };
        Ok(SessionReceipt {
            scope: self.scope.clone(),
            operation: request.operation.clone(),
            ticket: Some(RequestTicket {
                generation: self.scope.generation,
                id: RequestId::String(request.operation),
                method: request.method,
            }),
            certainty: RequestCertainty::Written,
            result: Some(ProviderResponse::Reply(result)),
            not_sent_reason: None,

            replayed: false,
        })
    }
}

#[derive(Default)]
struct LifecycleProbe {
    store: Option<Arc<RuntimeStore>>,
    staged: Mutex<Vec<Value>>,
    committed: AtomicUsize,
}

impl LifecycleProbe {
    fn new(store: Arc<RuntimeStore>) -> Self {
        Self {
            store: Some(store),
            staged: Mutex::default(),
            committed: AtomicUsize::new(0),
        }
    }
}

impl LifecyclePorts for LifecycleProbe {
    fn stage(&self, tx: &Transaction<'_>, effect: &LifecycleEffect) -> Result<(), ManagementError> {
        assert_eq!(effect.kind(), LifecycleEffectKind::Interrupt);
        let stage: String = tx.query_row(
            "SELECT json_extract(record,'$.stage') FROM runtime_safety_retries WHERE id='agent:turn'",
            [],
            |row| row.get(0),
        ).map_err(studio_runtime_store::StoreError::from)?;
        assert_eq!(stage, "start", "effect is staged in acceptance transaction");
        self.staged.lock().expect("staged").push(json!({
            "before":effect.before(),
            "stopped":effect.stopped(),
        }));
        Ok(())
    }

    fn committed(&self) -> Result<(), ManagementError> {
        if let Some(store) = &self.store {
            let stage: String = store
                .read()?
                .query_row(
                    "SELECT json_extract(record,'$.stage') FROM runtime_safety_retries WHERE id='agent:turn'",
                    [],
                    |row| row.get(0),
                )
                .map_err(studio_runtime_store::StoreError::from)?;
            assert_eq!(
                stage, "running",
                "commit hook runs after acceptance transaction"
            );
        }
        self.committed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl AccountSessionPort for BlockingSession {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        (account == self.scope.account).then(|| self.scope.clone())
    }

    fn provider_request(
        &self,
        request: ProviderRequest,
    ) -> Result<SessionReceipt, AccountSessionError> {
        assert_eq!(request.method, Method::ThreadTurnsList);
        assert_eq!(request.expected_scope, Some(self.scope.clone()));
        self.calls
            .lock()
            .expect("calls")
            .push(request.operation.clone());
        if let Some(entered) = self.entered.lock().expect("entered").take() {
            entered.send(()).expect("notify blocked provider");
        }
        self.release
            .lock()
            .expect("release")
            .recv()
            .expect("release blocked provider");
        Ok(SessionReceipt {
            scope: self.scope.clone(),
            operation: request.operation,
            ticket: None,
            certainty: RequestCertainty::Written,
            result: Some(ProviderResponse::Reply(
                json!({"data":[{"id":"turn","status":"inProgress"}]}),
            )),
            not_sent_reason: None,

            replayed: false,
        })
    }
}

#[test]
fn action_runs_python_corpus_stage_path_and_commits_each_intent_before_dispatch() {
    let (template, corpus) = python_corpus();
    let expected = corpus["path"]["calls"].as_array().expect("Python requests");
    let root = tempfile::tempdir().expect("state copy");
    for file in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(template.join(file), root.path().join(file)).expect("Python-written DB");
    }
    let store = RuntimeStore::open(Some(root.path())).expect("open Python-written state");
    let session = Session {
        store: &store,
        scope: ConnectionScope {
            account: "acct".into(),
            connection: "connection".into(),
            generation: 1,
        },
        calls: Mutex::default(),
        unknown_at: None,
        unknown_receipt_at: None,
        requires_at: None,
        not_sent_at: None,
        not_sent_used: Mutex::new(false),
        reject_start: false,
    };
    let result = safety_buffering::execute_action(
        &store,
        &session,
        &test_resources(),
        None,
        "agent",
        RequestActor {
            agent: "agent".into(),
            epoch: 7,
        },
        "turn",
        "retry",
    )
    .expect("safety retry completes");
    assert_eq!(result["stage"], "running");
    assert_eq!(result["acceptedTurnId"], "new-turn");
    assert_eq!(session.calls.lock().expect("calls").len(), expected.len());
    for (index, (actual, expected)) in session
        .calls
        .lock()
        .expect("calls")
        .iter()
        .zip(expected)
        .enumerate()
    {
        assert_eq!(actual["method"], expected["method"], "stage {index}");
        if actual["method"] == "thread/fork" {
            for key in ["config", "developerInstructions"] {
                assert!(
                    expected["params"].get(key).is_some(),
                    "Python fork params include {key}"
                );
                assert!(
                    actual["params"].get(key).is_some(),
                    "Rust fork params include {key}"
                );
            }
        }
        if matches!(
            actual["method"].as_str(),
            Some("thread/fork" | "turn/start")
        ) {
            assert_eq!(
                without_generated_client_id(actual["params"].clone()),
                without_generated_client_id(expected["params"].clone()),
                "complete normalized params at stage {index}"
            );
        } else {
            assert_eq!(actual["params"], expected["params"], "stage {index}");
        }
        assert_eq!(actual["stage"], expected["stage"], "stage {index}");
        assert_eq!(actual["scope"]["account"], "acct");
        assert_eq!(actual["scope"]["connection"], "connection");
    }
    let connection = store.read().expect("read final state");
    let raw = connection
        .query_row(
            "SELECT record FROM runtime_safety_retries WHERE id='agent:turn'",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("retry receipt");
    let operation: Value = serde_json::from_str(&raw).expect("stored receipt");
    assert_eq!(
        operation["responses"]
            .as_object()
            .expect("stage receipts")
            .len(),
        7
    );
    assert_eq!(
        operation["receipts"]
            .as_object()
            .expect("provider receipts")
            .len(),
        7
    );
    assert_eq!(operation["input"][0]["text"], "hello");
    assert_eq!(operation["newThreadId"], "new-thread");
    assert_eq!(operation["acceptedTurnId"], "new-turn");
    assert_eq!(
        session.calls.lock().expect("calls")[6]["params"]["clientUserMessageId"],
        operation["attemptId"]
    );
}

fn without_generated_client_id(mut params: Value) -> Value {
    params
        .as_object_mut()
        .expect("provider params object")
        .remove("clientUserMessageId");
    params
}

#[test]
fn late_provider_receipt_preserves_concurrent_cancel_and_does_not_advance() {
    let (template, _) = python_corpus();
    let root = tempfile::tempdir().expect("state copy");
    for file in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(template.join(file), root.path().join(file)).expect("Python-written DB");
    }
    let store = Arc::new(RuntimeStore::open(Some(root.path())).expect("open state"));
    let scope = ConnectionScope {
        account: "acct".into(),
        connection: "connection".into(),
        generation: 1,
    };
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let session = Arc::new(BlockingSession {
        scope,
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(release_rx),
        calls: Mutex::default(),
    });
    let retry_store = Arc::clone(&store);
    let retry_session = Arc::clone(&session);
    let worker = thread::spawn(move || {
        safety_buffering::execute_action(
            &retry_store,
            retry_session.as_ref(),
            &test_resources(),
            None,
            "agent",
            RequestActor {
                agent: "agent".into(),
                epoch: 7,
            },
            "turn",
            "retry",
        )
    });
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("provider request reached blocked boundary");
    let cancelled = safety_buffering::execute_action(
        &store,
        session.as_ref(),
        &test_resources(),
        None,
        "agent",
        RequestActor {
            agent: "agent".into(),
            epoch: 7,
        },
        "turn",
        "cancel",
    )
    .expect("concurrent cancellation commits");
    assert_eq!(cancelled["stage"], "cancelled");
    release_tx.send(()).expect("release provider");
    let late = worker
        .join()
        .expect("retry worker")
        .expect("late receipt handling");
    assert_eq!(late["stage"], "cancelled");
    assert_eq!(session.calls.lock().expect("calls").len(), 1);
    let connection = store.read().expect("read state");
    let raw: String = connection
        .query_row(
            "SELECT record FROM runtime_safety_retries WHERE id='agent:turn'",
            [],
            |row| row.get(0),
        )
        .expect("cancelled retry record");
    let operation: Value = serde_json::from_str(&raw).expect("retry record");
    assert_eq!(operation["stage"], "cancelled");
    assert_eq!(operation["receipts"]["turns"][0]["certainty"], "written");
    assert_eq!(
        operation["receipts"]["turns"][0]["result"]["data"][0]["id"],
        "turn"
    );
    assert!(
        operation
            .get("responses")
            .and_then(|value| value.get("turns"))
            .is_none()
    );
}

#[test]
fn cancellation_before_intent_persistence_prevents_provider_dispatch() {
    let (template, _) = python_corpus();
    let root = tempfile::tempdir().expect("state copy");
    for file in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(template.join(file), root.path().join(file)).expect("Python-written DB");
    }
    let store = Arc::new(RuntimeStore::open(Some(root.path())).expect("open state"));
    let scope = ConnectionScope {
        account: "acct".into(),
        connection: "connection".into(),
        generation: 1,
    };
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let session = Arc::new(IntentBlockingSession {
        scope,
        scope_calls: AtomicUsize::new(0),
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(release_rx),
        requests: AtomicUsize::new(0),
    });
    let retry_store = Arc::clone(&store);
    let retry_session = Arc::clone(&session);
    let worker = thread::spawn(move || {
        safety_buffering::execute_action(
            &retry_store,
            retry_session.as_ref(),
            &test_resources(),
            None,
            "agent",
            RequestActor {
                agent: "agent".into(),
                epoch: 7,
            },
            "turn",
            "retry",
        )
    });
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("pause before intent");
    let cancelled = safety_buffering::execute_action(
        &store,
        session.as_ref(),
        &test_resources(),
        None,
        "agent",
        RequestActor {
            agent: "agent".into(),
            epoch: 7,
        },
        "turn",
        "cancel",
    )
    .expect("cancel commits");
    assert_eq!(cancelled["stage"], "cancelled");
    release_tx.send(()).expect("continue retry");
    let resumed = worker
        .join()
        .expect("retry worker")
        .expect("cancel respected");
    assert_eq!(resumed["stage"], "cancelled");
    assert_eq!(session.requests.load(Ordering::SeqCst), 0);
}

#[test]
fn accepted_start_after_concurrent_stop_stages_interrupt_then_commits_lifecycle() {
    let (template, _) = python_corpus();
    let root = tempfile::tempdir().expect("state copy");
    for file in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(template.join(file), root.path().join(file)).expect("Python-written DB");
    }
    let store = Arc::new(RuntimeStore::open(Some(root.path())).expect("open state"));
    let scope = ConnectionScope {
        account: "acct".into(),
        connection: "connection".into(),
        generation: 1,
    };
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let session = Arc::new(StartBlockingSession {
        store: Arc::clone(&store),
        scope,
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(release_rx),
    });
    let lifecycle = Arc::new(LifecycleProbe::new(Arc::clone(&store)));
    let resources = test_resources();
    let retry_store = Arc::clone(&store);
    let retry_session = Arc::clone(&session);
    let retry_lifecycle = Arc::clone(&lifecycle);
    let worker = thread::spawn(move || {
        let result = safety_buffering::execute_action(
            &retry_store,
            retry_session.as_ref(),
            &resources,
            Some(retry_lifecycle.as_ref()),
            "agent",
            RequestActor {
                agent: "agent".into(),
                epoch: 7,
            },
            "turn",
            "retry",
        );
        let state = result
            .as_ref()
            .map_or_else(|error| error.to_string(), |_| "success".into());
        let _ = done_tx.send(state);
        result
    });
    if entered_rx.recv_timeout(Duration::from_secs(5)).is_err() {
        let state = done_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap_or_else(|_| "still running".into());
        panic!("turn/start was not reached: {state}");
    }

    store
        .transaction_with(|tx| {
            let raw: String = tx.query_row(
                "SELECT record FROM runtime_agents WHERE id='agent'",
                [],
                |row| row.get(0),
            )?;
            let mut agent: Value = serde_json::from_str(&raw).expect("agent record");
            agent["epoch"] = json!(8);
            agent["status"] = json!("paused");
            agent["autoWake"] = json!(false);
            agent["turnEpoch"] = json!(7);
            agent["inFlight"] = json!(false);
            agent["error"] = json!("Stopped by user");
            tx.execute(
                "UPDATE runtime_agents SET record=?1 WHERE id='agent'",
                [agent.to_string()],
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .expect("concurrent stop commits before late start receipt");
    release_tx.send(()).expect("release start receipt");
    let receipt = worker
        .join()
        .expect("retry worker")
        .expect("lifecycle settlement");
    assert_eq!(receipt["stage"], "running");
    assert_eq!(lifecycle.committed.load(Ordering::SeqCst), 1);
    let effects = lifecycle.staged.lock().expect("staged effects");
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0]["before"]["turnId"], "new-turn");
    assert_eq!(effects[0]["before"]["epoch"], 7);
    assert_eq!(effects[0]["stopped"]["epoch"], 8);
}

#[test]
fn rejected_safety_start_settles_the_empty_event_attempt() {
    let (template, _) = python_corpus();
    let root = tempfile::tempdir().expect("state copy");
    for file in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(template.join(file), root.path().join(file)).expect("Python-written DB");
    }
    let store = RuntimeStore::open(Some(root.path())).expect("open state");
    let session = Session {
        store: &store,
        scope: ConnectionScope {
            account: "acct".into(),
            connection: "connection".into(),
            generation: 1,
        },
        calls: Mutex::default(),
        unknown_at: None,
        unknown_receipt_at: None,
        requires_at: None,
        not_sent_at: None,
        not_sent_used: Mutex::new(false),
        reject_start: true,
    };
    assert!(
        safety_buffering::execute_action(
            &store,
            &session,
            &test_resources(),
            None,
            "agent",
            RequestActor {
                agent: "agent".into(),
                epoch: 7,
            },
            "turn",
            "retry",
        )
        .is_err()
    );
    let db = store.read().expect("state read");
    let raw: String = db
        .query_row(
            "SELECT record FROM runtime_agents WHERE id='agent'",
            [],
            |row| row.get(0),
        )
        .expect("settled agent");
    let agent: Value = serde_json::from_str(&raw).expect("agent JSON");
    assert_eq!(agent["status"], "failed");
    assert_eq!(agent["inFlight"], false);
    assert_eq!(agent["startAttempt"]["executionOutcome"], "rejected");
    let retry: String = db
        .query_row(
            "SELECT record FROM runtime_safety_retries WHERE id='agent:turn'",
            [],
            |row| row.get(0),
        )
        .expect("failed retry");
    let retry: Value = serde_json::from_str(&retry).expect("retry JSON");
    assert_eq!(retry["stage"], "failed");
    assert_eq!(retry["dispatch"], "rejected");
}

#[test]
fn unknown_at_every_stage_is_durable_and_never_resent_after_restart_or_action_replay() {
    let (template, _) = python_corpus();
    for failed_stage in 0..7 {
        let root = tempfile::tempdir().expect("state copy");
        for file in ["canvas.sqlite3", "analytics.sqlite3"] {
            std::fs::copy(template.join(file), root.path().join(file)).expect("Python-written DB");
        }
        let store = RuntimeStore::open(Some(root.path())).expect("open state");
        let session = Session {
            store: &store,
            scope: ConnectionScope {
                account: "acct".into(),
                connection: "connection".into(),
                generation: 1,
            },
            calls: Mutex::default(),
            unknown_at: Some(failed_stage),
            unknown_receipt_at: None,
            requires_at: None,
            not_sent_at: None,
            not_sent_used: Mutex::new(false),
            reject_start: false,
        };
        let result = safety_buffering::execute_action(
            &store,
            &session,
            &test_resources(),
            None,
            "agent",
            RequestActor {
                agent: "agent".into(),
                epoch: 7,
            },
            "turn",
            "retry",
        )
        .expect("unknown outcome is surfaced as receipt");
        assert_eq!(result["stage"], "unknown", "stage {failed_stage}");
        let before = session.calls.lock().expect("calls").len();
        drop(session);
        drop(store);
        let reopened = RuntimeStore::open(Some(root.path())).expect("restart store");
        let restarted_session = Session {
            store: &reopened,
            scope: ConnectionScope {
                account: "acct".into(),
                connection: "connection".into(),
                generation: 1,
            },
            calls: Mutex::default(),
            unknown_at: None,
            unknown_receipt_at: None,
            requires_at: None,
            not_sent_at: None,
            not_sent_used: Mutex::new(false),
            reject_start: false,
        };
        let replay = safety_buffering::execute_action(
            &reopened,
            &restarted_session,
            &test_resources(),
            None,
            "agent",
            RequestActor {
                agent: "agent".into(),
                epoch: 7,
            },
            "turn",
            "retry",
        )
        .expect("unknown receipt replays after restart");
        assert_eq!(replay["stage"], "unknown", "stage {failed_stage}");
        assert_eq!(
            restarted_session.calls.lock().expect("calls").len(),
            0,
            "resent stage {failed_stage}; prior calls={before}"
        );
    }
}

#[test]
fn crash_after_unknown_receipt_before_terminal_stage_never_dispatches_after_restart() {
    let (template, _) = python_corpus();
    let root = tempfile::tempdir().expect("state copy");
    for file in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(template.join(file), root.path().join(file)).expect("Python-written DB");
    }
    let store = RuntimeStore::open(Some(root.path())).expect("open state");
    let interrupted = Session {
        store: &store,
        scope: ConnectionScope {
            account: "acct".into(),
            connection: "connection".into(),
            generation: 1,
        },
        calls: Mutex::default(),
        unknown_at: None,
        unknown_receipt_at: None,
        requires_at: Some(0),
        not_sent_at: None,
        not_sent_used: Mutex::new(false),
        reject_start: false,
    };
    assert!(
        safety_buffering::execute_action(
            &store,
            &interrupted,
            &test_resources(),
            None,
            "agent",
            RequestActor {
                agent: "agent".into(),
                epoch: 7,
            },
            "turn",
            "retry",
        )
        .is_err()
    );
    assert_eq!(interrupted.calls.lock().expect("calls").len(), 1);
    store
        .transaction(|tx| {
            let raw: String = tx.query_row(
                "SELECT record FROM runtime_safety_retries WHERE id='agent:turn'",
                [],
                |row| row.get(0),
            )?;
            let mut operation: Value = serde_json::from_str(&raw).expect("retry JSON");
            assert_eq!(operation["stage"], "turns");
            assert_eq!(operation["dispatch"], "intent");
            operation["receipts"]["turns"] = json!([{
                "operation":"agent:turn:turns",
                "certainty":"unknown"
            }]);
            tx.execute(
                "UPDATE runtime_safety_retries SET record=?1 WHERE id='agent:turn'",
                [operation.to_string()],
            )?;
            Ok(())
        })
        .expect("commit simulated crash-window receipt");
    drop(interrupted);
    drop(store);

    let reopened = RuntimeStore::open(Some(root.path())).expect("restart store");
    let restarted = Session {
        store: &reopened,
        scope: ConnectionScope {
            account: "acct".into(),
            connection: "connection".into(),
            generation: 1,
        },
        calls: Mutex::default(),
        unknown_at: None,
        unknown_receipt_at: None,
        requires_at: None,
        not_sent_at: None,
        not_sent_used: Mutex::new(false),
        reject_start: false,
    };
    let result = safety_buffering::execute_action(
        &reopened,
        &restarted,
        &test_resources(),
        None,
        "agent",
        RequestActor {
            agent: "agent".into(),
            epoch: 7,
        },
        "turn",
        "retry",
    )
    .expect("unknown recovery is held");
    assert_eq!(result["stage"], "unknown");
    assert_eq!(restarted.calls.lock().expect("calls").len(), 0);
    let raw: String = reopened
        .read()
        .expect("read recovered state")
        .query_row(
            "SELECT record FROM runtime_safety_retries WHERE id='agent:turn'",
            [],
            |row| row.get(0),
        )
        .expect("recovered retry");
    let operation: Value = serde_json::from_str(&raw).expect("retry JSON");
    assert_eq!(operation["stage"], "unknown");
    assert_eq!(operation["dispatch"], "unknown");
}

#[test]
fn unknown_receipt_commits_terminal_stage_atomically_and_is_never_resent() {
    let (template, _) = python_corpus();
    let root = tempfile::tempdir().expect("state copy");
    for file in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(template.join(file), root.path().join(file)).expect("Python-written DB");
    }
    let store = RuntimeStore::open(Some(root.path())).expect("open state");
    let session = Session {
        store: &store,
        scope: ConnectionScope {
            account: "acct".into(),
            connection: "connection".into(),
            generation: 1,
        },
        calls: Mutex::default(),
        unknown_at: None,
        unknown_receipt_at: Some(0),
        requires_at: None,
        not_sent_at: None,
        not_sent_used: Mutex::new(false),
        reject_start: false,
    };
    let result = safety_buffering::execute_action(
        &store,
        &session,
        &test_resources(),
        None,
        "agent",
        RequestActor {
            agent: "agent".into(),
            epoch: 7,
        },
        "turn",
        "retry",
    )
    .expect("Unknown provider receipt is retained");
    assert_eq!(result["stage"], "unknown");
    let raw: String = store
        .read()
        .expect("read state")
        .query_row(
            "SELECT record FROM runtime_safety_retries WHERE id='agent:turn'",
            [],
            |row| row.get(0),
        )
        .expect("terminal unknown operation");
    let operation: Value = serde_json::from_str(&raw).expect("operation JSON");
    assert_eq!(operation["stage"], "unknown");
    assert_eq!(operation["dispatch"], "unknown");
    assert_eq!(operation["receipts"]["turns"][0]["certainty"], "unknown");
    drop(session);
    drop(store);

    let reopened = RuntimeStore::open(Some(root.path())).expect("restart store");
    let restarted = Session {
        store: &reopened,
        scope: ConnectionScope {
            account: "acct".into(),
            connection: "connection".into(),
            generation: 1,
        },
        calls: Mutex::default(),
        unknown_at: None,
        unknown_receipt_at: None,
        requires_at: None,
        not_sent_at: None,
        not_sent_used: Mutex::new(false),
        reject_start: false,
    };
    let replay = safety_buffering::execute_action(
        &reopened,
        &restarted,
        &test_resources(),
        None,
        "agent",
        RequestActor {
            agent: "agent".into(),
            epoch: 7,
        },
        "turn",
        "retry",
    )
    .expect("terminal Unknown result replays");
    assert_eq!(replay["stage"], "unknown");
    assert_eq!(restarted.calls.lock().expect("calls").len(), 0);
}

#[test]
fn not_sent_retries_same_operation_id_and_records_each_receipt() {
    let (template, _) = python_corpus();
    for not_sent_at in 0..7 {
        let root = tempfile::tempdir().expect("state copy");
        for file in ["canvas.sqlite3", "analytics.sqlite3"] {
            std::fs::copy(template.join(file), root.path().join(file)).expect("Python-written DB");
        }
        let store = RuntimeStore::open(Some(root.path())).expect("open state");
        let session = Session {
            store: &store,
            scope: ConnectionScope {
                account: "acct".into(),
                connection: "connection".into(),
                generation: 1,
            },
            calls: Mutex::default(),
            unknown_at: None,
            unknown_receipt_at: None,
            requires_at: None,
            not_sent_at: Some(not_sent_at),
            not_sent_used: Mutex::new(false),
            reject_start: false,
        };
        let result = safety_buffering::execute_action(
            &store,
            &session,
            &test_resources(),
            None,
            "agent",
            RequestActor {
                agent: "agent".into(),
                epoch: 7,
            },
            "turn",
            "retry",
        )
        .expect("NotSent retry completes");
        assert_eq!(result["stage"], "running", "failed stage {not_sent_at}");
        let calls = session.calls.lock().expect("calls");
        assert_eq!(
            calls[not_sent_at]["operation"],
            calls[not_sent_at + 1]["operation"]
        );
        assert_eq!(calls[not_sent_at]["stage"], calls[not_sent_at + 1]["stage"]);
        let stage = calls[not_sent_at]["stage"]
            .as_str()
            .expect("stage")
            .to_owned();
        drop(calls);
        let db = store.read().expect("state read");
        let raw: String = db
            .query_row(
                "SELECT record FROM runtime_safety_retries WHERE id='agent:turn'",
                [],
                |row| row.get(0),
            )
            .expect("retry receipt");
        let operation: Value = serde_json::from_str(&raw).expect("stored operation");
        assert_eq!(
            operation["receipts"][stage]
                .as_array()
                .expect("stage receipts")
                .len(),
            2
        );
    }
}

#[test]
fn restart_resumes_recorded_intent_with_the_same_operation_id() {
    let (template, _) = python_corpus();
    for interrupted_stage in 0..7 {
        let root = tempfile::tempdir().expect("state copy");
        for file in ["canvas.sqlite3", "analytics.sqlite3"] {
            std::fs::copy(template.join(file), root.path().join(file)).expect("Python-written DB");
        }
        let store = RuntimeStore::open(Some(root.path())).expect("open state");
        let first = Session {
            store: &store,
            scope: ConnectionScope {
                account: "acct".into(),
                connection: "connection".into(),
                generation: 1,
            },
            calls: Mutex::default(),
            unknown_at: None,
            unknown_receipt_at: None,
            requires_at: Some(interrupted_stage),
            not_sent_at: None,
            not_sent_used: Mutex::new(false),
            reject_start: false,
        };
        assert!(
            safety_buffering::execute_action(
                &store,
                &first,
                &test_resources(),
                None,
                "agent",
                RequestActor {
                    agent: "agent".into(),
                    epoch: 7
                },
                "turn",
                "retry",
            )
            .is_err(),
            "interrupted stage {interrupted_stage}"
        );
        let calls = first.calls.lock().expect("calls");
        let operation_id = calls[interrupted_stage]["operation"].clone();
        let stage = calls[interrupted_stage]["stage"].clone();
        drop(calls);
        drop(first);
        drop(store);

        let restarted = RuntimeStore::open(Some(root.path())).expect("restart store");
        let second = Session {
            store: &restarted,
            scope: ConnectionScope {
                account: "acct".into(),
                connection: "connection".into(),
                generation: 1,
            },
            calls: Mutex::default(),
            unknown_at: None,
            unknown_receipt_at: None,
            requires_at: None,
            not_sent_at: None,
            not_sent_used: Mutex::new(false),
            reject_start: false,
        };
        let result = safety_buffering::execute_action(
            &restarted,
            &second,
            &test_resources(),
            None,
            "agent",
            RequestActor {
                agent: "agent".into(),
                epoch: 7,
            },
            "turn",
            "retry",
        )
        .expect("resume from recorded intent");
        assert_eq!(result["stage"], "running", "stage {interrupted_stage}");
        assert_eq!(
            second.calls.lock().expect("calls")[0]["operation"],
            operation_id
        );
        assert_eq!(second.calls.lock().expect("calls")[0]["stage"], stage);
    }
}

#[test]
fn restart_applies_persisted_written_receipt_without_redispatch() {
    let (template, _) = python_corpus();
    for interrupted_stage in 0..7 {
        let root = tempfile::tempdir().expect("state copy");
        for file in ["canvas.sqlite3", "analytics.sqlite3"] {
            std::fs::copy(template.join(file), root.path().join(file)).expect("Python-written DB");
        }
        let store = RuntimeStore::open(Some(root.path())).expect("open state");
        let first = Session {
            store: &store,
            scope: ConnectionScope {
                account: "acct".into(),
                connection: "connection".into(),
                generation: 1,
            },
            calls: Mutex::default(),
            unknown_at: None,
            unknown_receipt_at: None,
            requires_at: Some(interrupted_stage),
            not_sent_at: None,
            not_sent_used: Mutex::new(false),
            reject_start: false,
        };
        assert!(
            safety_buffering::execute_action(
                &store,
                &first,
                &test_resources(),
                None,
                "agent",
                RequestActor {
                    agent: "agent".into(),
                    epoch: 7,
                },
                "turn",
                "retry",
            )
            .is_err(),
            "interrupted stage {interrupted_stage}"
        );
        let interrupted = first.calls.lock().expect("calls")[interrupted_stage].clone();
        let stage = interrupted["stage"].as_str().expect("stage").to_owned();
        let operation_id = interrupted["operation"].clone();
        let response = successful_response(&stage);
        store
            .transaction_with(|tx| {
                let raw: String = tx.query_row(
                    "SELECT record FROM runtime_safety_retries WHERE id='agent:turn'",
                    [],
                    |row| row.get(0),
                )?;
                let mut operation: Value = serde_json::from_str(&raw).expect("operation");
                operation["responses"][&stage] = response.clone();
                operation["receipts"][&stage] = json!([{
                    "operation":operation_id,
                    "scope":interrupted["scope"],
                    "certainty":"written",
                    "result":response,
                }]);
                operation["dispatch"] = json!("received");
                if stage == "start" {
                    let request_id = operation_id.as_str().expect("start operation id");
                    operation["requestId"] = json!(request_id);
                    operation["requestGeneration"] = json!(1);
                    let raw: String = tx.query_row(
                        "SELECT record FROM runtime_agents WHERE id='agent'",
                        [],
                        |row| row.get(0),
                    )?;
                    let mut agent: Value = serde_json::from_str(&raw).expect("agent record");
                    agent["startAttempt"]["requestTicket"] = json!(request_id);
                    agent["startAttempt"]["connectionGeneration"] = json!(1);
                    tx.execute(
                        "UPDATE runtime_agents SET record=?1 WHERE id='agent'",
                        [agent.to_string()],
                    )?;
                }
                tx.execute(
                    "UPDATE runtime_safety_retries SET record=?1 WHERE id='agent:turn'",
                    [operation.to_string()],
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("simulate process stop after durable receipt");
        drop(first);
        drop(store);

        let restarted = RuntimeStore::open(Some(root.path())).expect("restart store");
        let second = Session {
            store: &restarted,
            scope: ConnectionScope {
                account: "acct".into(),
                connection: "connection".into(),
                generation: 1,
            },
            calls: Mutex::default(),
            unknown_at: None,
            unknown_receipt_at: None,
            requires_at: None,
            not_sent_at: None,
            not_sent_used: Mutex::new(false),
            reject_start: false,
        };
        let result = safety_buffering::execute_action(
            &restarted,
            &second,
            &test_resources(),
            None,
            "agent",
            RequestActor {
                agent: "agent".into(),
                epoch: 7,
            },
            "turn",
            "retry",
        )
        .expect("apply the saved receipt and continue");
        assert_eq!(result["stage"], "running", "stage {interrupted_stage}");
        assert!(
            second
                .calls
                .lock()
                .expect("calls")
                .iter()
                .all(|call| call["operation"] != operation_id),
            "redispatched stage {interrupted_stage}"
        );
    }
}

fn successful_response(stage: &str) -> Value {
    match stage {
        "turns" => json!({"data":[{"id":"turn","status":"inProgress"}]}),
        "items" | "verify_items" => {
            json!({"data":[{"turnId":"turn","item":{"type":"userMessage","content":[{"type":"text","text":"hello"}]}}]})
        }
        "interrupt" => json!({}),
        "verify_turns" => json!({"data":[{"id":"turn","status":"interrupted"}]}),
        "fork" => json!({"thread":{"id":"new-thread"}}),
        "start" => json!({"turn":{"id":"new-turn"}}),
        _ => panic!("unknown stage {stage}"),
    }
}
