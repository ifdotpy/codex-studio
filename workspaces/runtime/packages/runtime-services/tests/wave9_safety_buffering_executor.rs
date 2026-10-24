use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::{
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};
use studio_runtime_providers::{Method, RequestId, RequestTicket};
use studio_runtime_services::{
    account_session_runtime::{
        AccountSessionError, AccountSessionFuture, AccountSessionPort, ConnectionScope,
        NotSentReason, ProviderRequest, ProviderResponse, RequestCertainty, SessionReceipt,
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
            studio_runtime_services::model_catalog::ModelCatalogService::new(),
        )
        .expect("bundled preparation resources"),
    )
}

fn python_corpus() -> ([Vec<u8>; 2], Value) {
    static CORPUS: OnceLock<([Vec<u8>; 2], Value)> = OnceLock::new();
    CORPUS
        .get_or_init(|| {
            (
                crate::media_auth_fixtures::state_bytes(include_str!("wave9_safety_state.json")),
                serde_json::from_str(include_str!("wave9_safety_expected.json"))
                    .expect("frozen safety corpus"),
            )
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

    fn provider_request<'a>(
        &'a self,
        request: ProviderRequest,
    ) -> AccountSessionFuture<'a, SessionReceipt> {
        Box::pin(async move {
            let connection = self
                .store
                .test_reader()
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

    fn provider_request<'a>(
        &'a self,
        _request: ProviderRequest,
    ) -> AccountSessionFuture<'a, SessionReceipt> {
        Box::pin(async move {
            self.requests.fetch_add(1, Ordering::SeqCst);
            Err(AccountSessionError::Refused("unexpected dispatch"))
        })
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

    fn provider_request<'a>(
        &'a self,
        request: ProviderRequest,
    ) -> AccountSessionFuture<'a, SessionReceipt> {
        Box::pin(async move {
            let raw = self
                .store
                .test_reader()
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

    fn committed(
        &self,
    ) -> studio_runtime_services::agent_lifecycle_runtime::LifecycleCommitFuture<'_> {
        Box::pin(async move {
            if let Some(store) = &self.store {
                let stage: String = store
                .test_reader()?
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
        })
    }
}

impl AccountSessionPort for BlockingSession {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        (account == self.scope.account).then(|| self.scope.clone())
    }

    fn provider_request<'a>(
        &'a self,
        request: ProviderRequest,
    ) -> AccountSessionFuture<'a, SessionReceipt> {
        Box::pin(async move {
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
        })
    }
}

#[tokio::test]
async fn action_runs_python_corpus_stage_path_and_commits_each_intent_before_dispatch() {
    let (template, corpus) = python_corpus();
    let expected = corpus["path"]["calls"].as_array().expect("Python requests");
    let root = tempfile::tempdir().expect("state copy");
    for (file, bytes) in ["canvas.sqlite3", "analytics.sqlite3"]
        .into_iter()
        .zip(&template)
    {
        std::fs::write(root.path().join(file), bytes).expect("Python-written DB");
    }
    let store = Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("open Python-written state"),
    );
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
        Arc::clone(&store),
        &session,
        &test_resources(),
        None,
        safety_buffering::SafetyAction {
            agent_id: "agent",
            actor: RequestActor {
                agent: "agent".into(),
                epoch: 7,
            },
            turn_id: "turn",
            choice: "retry",
        },
    )
    .await
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
                normalized_provider_params(actual["params"].clone()),
                normalized_provider_params(expected["params"].clone()),
                "complete normalized params at stage {index}"
            );
        } else {
            assert_eq!(actual["params"], expected["params"], "stage {index}");
        }
        assert_eq!(actual["stage"], expected["stage"], "stage {index}");
        assert_eq!(actual["scope"]["account"], "acct");
        assert_eq!(actual["scope"]["connection"], "connection");
    }
    let connection = store.test_reader().expect("read final state");
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

// Decode volatile envelope fields once; retain every method-specific parameter
// in the complete equality check below.
#[derive(Deserialize, Serialize)]
struct ComparedProviderParams {
    #[serde(rename = "clientUserMessageId", skip_serializing)]
    _client_user_message_id: Option<String>,
    #[serde(
        rename = "developerInstructions",
        default,
        deserialize_with = "present_instructions",
        skip_serializing_if = "Option::is_none"
    )]
    developer_instructions: Option<String>,
    #[serde(flatten)]
    method_params: BTreeMap<String, Value>,
}

fn present_instructions<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

fn normalized_provider_params(params: Value) -> Value {
    let mut params: ComparedProviderParams =
        serde_json::from_value(params).expect("provider comparison params");
    if let Some(instructions) = &mut params.developer_instructions {
        // Source headers carry the checkout's absolute path. Their skill identity
        // and the complete guidance body remain part of the assertion.
        *instructions = instructions
            .split_inclusive('\n')
            .map(|line| {
                for (prefix, skill) in [
                    ("Source: ", "codex-subagent"),
                    ("Shared tool guidance: ", "codex-workspace"),
                ] {
                    if let Some(source) = line.trim_end_matches('\n').strip_prefix(prefix) {
                        let relative = std::path::Path::new(".agents")
                            .join("skills")
                            .join(skill)
                            .join("SKILL.md");
                        if std::path::Path::new(source).ends_with(relative) {
                            let newline = if line.ends_with('\n') { "\n" } else { "" };
                            return format!(
                                "{prefix}$REPOSITORY_ROOT/.agents/skills/{skill}/SKILL.md{newline}"
                            );
                        }
                    }
                }
                line.to_owned()
            })
            .collect();
    }
    serde_json::to_value(params).expect("normalized provider comparison params")
}

#[test]
fn comparison_normalizes_only_generated_identity_and_role_source_roots() {
    let params = |root: &str, client: &str, skill: &str, body: &str| {
        json!({
            "threadId": "thread",
            "clientUserMessageId": client,
            "config": {"model_reasoning_effort": "low"},
            "developerInstructions": format!(
                "Source: {root}/.agents/skills/{skill}/SKILL.md\nShared tool guidance: {root}/.agents/skills/codex-workspace/SKILL.md\n{body}\n"
            ),
        })
    };
    let baseline = normalized_provider_params(params(
        "/first-checkout",
        "first",
        "codex-subagent",
        "guidance",
    ));
    assert_eq!(
        baseline,
        normalized_provider_params(params(
            "/other-checkout",
            "second",
            "codex-subagent",
            "guidance"
        ))
    );
    assert_ne!(
        baseline,
        normalized_provider_params(params(
            "/other-checkout",
            "second",
            "other-skill",
            "guidance"
        ))
    );
    assert_ne!(
        baseline,
        normalized_provider_params(params(
            "/other-checkout",
            "second",
            "codex-subagent",
            "changed guidance"
        ))
    );
    let mut changed_method_params =
        params("/other-checkout", "second", "codex-subagent", "guidance");
    changed_method_params["threadId"] = json!("different-thread");
    assert_ne!(baseline, normalized_provider_params(changed_method_params));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_provider_receipt_preserves_concurrent_cancel_and_does_not_advance() {
    let (template, _) = python_corpus();
    let root = tempfile::tempdir().expect("state copy");
    for (file, bytes) in ["canvas.sqlite3", "analytics.sqlite3"]
        .into_iter()
        .zip(&template)
    {
        std::fs::write(root.path().join(file), bytes).expect("Python-written DB");
    }
    let store = Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("open state"),
    );
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
    let worker = tokio::spawn(async move {
        safety_buffering::execute_action(
            retry_store,
            retry_session.as_ref(),
            &test_resources(),
            None,
            safety_buffering::SafetyAction {
                agent_id: "agent",
                actor: RequestActor {
                    agent: "agent".into(),
                    epoch: 7,
                },
                turn_id: "turn",
                choice: "retry",
            },
        )
        .await
    });
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("provider request reached blocked boundary");
    let cancelled = safety_buffering::execute_action(
        Arc::clone(&store),
        session.as_ref(),
        &test_resources(),
        None,
        safety_buffering::SafetyAction {
            agent_id: "agent",
            actor: RequestActor {
                agent: "agent".into(),
                epoch: 7,
            },
            turn_id: "turn",
            choice: "cancel",
        },
    )
    .await
    .expect("concurrent cancellation commits");
    assert_eq!(cancelled["stage"], "cancelled");
    release_tx.send(()).expect("release provider");
    let late = worker
        .await
        .expect("retry worker")
        .expect("late receipt handling");
    assert_eq!(late["stage"], "cancelled");
    assert_eq!(session.calls.lock().expect("calls").len(), 1);
    let connection = store.test_reader().expect("read state");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_before_intent_persistence_prevents_provider_dispatch() {
    let (template, _) = python_corpus();
    let root = tempfile::tempdir().expect("state copy");
    for (file, bytes) in ["canvas.sqlite3", "analytics.sqlite3"]
        .into_iter()
        .zip(&template)
    {
        std::fs::write(root.path().join(file), bytes).expect("Python-written DB");
    }
    let store = Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("open state"),
    );
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
    let worker = tokio::spawn(async move {
        safety_buffering::execute_action(
            retry_store,
            retry_session.as_ref(),
            &test_resources(),
            None,
            safety_buffering::SafetyAction {
                agent_id: "agent",
                actor: RequestActor {
                    agent: "agent".into(),
                    epoch: 7,
                },
                turn_id: "turn",
                choice: "retry",
            },
        )
        .await
    });
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("pause before intent");
    let cancelled = safety_buffering::execute_action(
        Arc::clone(&store),
        session.as_ref(),
        &test_resources(),
        None,
        safety_buffering::SafetyAction {
            agent_id: "agent",
            actor: RequestActor {
                agent: "agent".into(),
                epoch: 7,
            },
            turn_id: "turn",
            choice: "cancel",
        },
    )
    .await
    .expect("cancel commits");
    assert_eq!(cancelled["stage"], "cancelled");
    release_tx.send(()).expect("continue retry");
    let resumed = worker
        .await
        .expect("retry worker")
        .expect("cancel respected");
    assert_eq!(resumed["stage"], "cancelled");
    assert_eq!(session.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_start_after_concurrent_stop_stages_interrupt_then_commits_lifecycle() {
    let (template, _) = python_corpus();
    let root = tempfile::tempdir().expect("state copy");
    for (file, bytes) in ["canvas.sqlite3", "analytics.sqlite3"]
        .into_iter()
        .zip(&template)
    {
        std::fs::write(root.path().join(file), bytes).expect("Python-written DB");
    }
    let store = Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("open state"),
    );
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
    let worker = tokio::spawn(async move {
        let result = safety_buffering::execute_action(
            retry_store,
            retry_session.as_ref(),
            &resources,
            Some(retry_lifecycle),
            safety_buffering::SafetyAction {
                agent_id: "agent",
                actor: RequestActor {
                    agent: "agent".into(),
                    epoch: 7,
                },
                turn_id: "turn",
                choice: "retry",
            },
        )
        .await;
        let state = result
            .as_ref()
            .map_or_else(|error| error.to_string(), |_| "success".into());
        done_tx.send(state).unwrap_or(());
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
        .await
        .expect("concurrent stop commits before late start receipt");
    release_tx.send(()).expect("release start receipt");
    let receipt = worker
        .await
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

#[tokio::test]
async fn rejected_safety_start_settles_the_empty_event_attempt() {
    let (template, _) = python_corpus();
    let root = tempfile::tempdir().expect("state copy");
    for (file, bytes) in ["canvas.sqlite3", "analytics.sqlite3"]
        .into_iter()
        .zip(&template)
    {
        std::fs::write(root.path().join(file), bytes).expect("Python-written DB");
    }
    let store = Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("open state"),
    );
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
            Arc::clone(&store),
            &session,
            &test_resources(),
            None,
            safety_buffering::SafetyAction {
                agent_id: "agent",
                actor: RequestActor {
                    agent: "agent".into(),
                    epoch: 7,
                },
                turn_id: "turn",
                choice: "retry",
            },
        )
        .await
        .is_err()
    );
    let db = store.test_reader().expect("state read");
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

#[tokio::test]
async fn unknown_at_every_stage_is_durable_and_never_resent_after_restart_or_action_replay() {
    let (template, _) = python_corpus();
    for failed_stage in 0..7 {
        let root = tempfile::tempdir().expect("state copy");
        for (file, bytes) in ["canvas.sqlite3", "analytics.sqlite3"]
            .into_iter()
            .zip(&template)
        {
            std::fs::write(root.path().join(file), bytes).expect("Python-written DB");
        }
        let store = Arc::new(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("open state"),
        );
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
            Arc::clone(&store),
            &session,
            &test_resources(),
            None,
            safety_buffering::SafetyAction {
                agent_id: "agent",
                actor: RequestActor {
                    agent: "agent".into(),
                    epoch: 7,
                },
                turn_id: "turn",
                choice: "retry",
            },
        )
        .await
        .expect("unknown outcome is surfaced as receipt");
        assert_eq!(result["stage"], "unknown", "stage {failed_stage}");
        let before = session.calls.lock().expect("calls").len();
        drop(session);
        drop(store);
        let reopened = Arc::new(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("restart store"),
        );
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
            Arc::clone(&reopened),
            &restarted_session,
            &test_resources(),
            None,
            safety_buffering::SafetyAction {
                agent_id: "agent",
                actor: RequestActor {
                    agent: "agent".into(),
                    epoch: 7,
                },
                turn_id: "turn",
                choice: "retry",
            },
        )
        .await
        .expect("unknown receipt replays after restart");
        assert_eq!(replay["stage"], "unknown", "stage {failed_stage}");
        assert_eq!(
            restarted_session.calls.lock().expect("calls").len(),
            0,
            "resent stage {failed_stage}; prior calls={before}"
        );
    }
}

#[tokio::test]
async fn crash_after_unknown_receipt_before_terminal_stage_never_dispatches_after_restart() {
    let (template, _) = python_corpus();
    let root = tempfile::tempdir().expect("state copy");
    for (file, bytes) in ["canvas.sqlite3", "analytics.sqlite3"]
        .into_iter()
        .zip(&template)
    {
        std::fs::write(root.path().join(file), bytes).expect("Python-written DB");
    }
    let store = Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("open state"),
    );
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
            Arc::clone(&store),
            &interrupted,
            &test_resources(),
            None,
            safety_buffering::SafetyAction {
                agent_id: "agent",
                actor: RequestActor {
                    agent: "agent".into(),
                    epoch: 7,
                },
                turn_id: "turn",
                choice: "retry",
            },
        )
        .await
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
        .await
        .expect("commit simulated crash-window receipt");
    drop(interrupted);
    drop(store);

    let reopened = Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("restart store"),
    );
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
        Arc::clone(&reopened),
        &restarted,
        &test_resources(),
        None,
        safety_buffering::SafetyAction {
            agent_id: "agent",
            actor: RequestActor {
                agent: "agent".into(),
                epoch: 7,
            },
            turn_id: "turn",
            choice: "retry",
        },
    )
    .await
    .expect("unknown recovery is held");
    assert_eq!(result["stage"], "unknown");
    assert_eq!(restarted.calls.lock().expect("calls").len(), 0);
    let raw: String = reopened
        .test_reader()
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

#[tokio::test]
async fn unknown_receipt_commits_terminal_stage_atomically_and_is_never_resent() {
    let (template, _) = python_corpus();
    let root = tempfile::tempdir().expect("state copy");
    for (file, bytes) in ["canvas.sqlite3", "analytics.sqlite3"]
        .into_iter()
        .zip(&template)
    {
        std::fs::write(root.path().join(file), bytes).expect("Python-written DB");
    }
    let store = Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("open state"),
    );
    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&snapshots);
    let weak_store = Arc::downgrade(&store);
    store.add_commit_observer(move || {
        use rusqlite::OptionalExtension;
        let reader = weak_store
            .upgrade()
            .expect("store alive")
            .test_reader()
            .expect("commit reader");
        let raw: Option<String> = reader
            .query_row(
                "SELECT record FROM runtime_safety_retries WHERE id='agent:turn'",
                [],
                |row| row.get(0),
            )
            .optional()
            .expect("committed retry reads");
        if let Some(raw) = raw {
            let operation: Value = serde_json::from_str(&raw).expect("committed retry JSON");
            if operation["receipts"]["turns"][0]["certainty"] == "unknown" {
                observed
                    .lock()
                    .expect("snapshots")
                    .push((operation["stage"].clone(), operation["dispatch"].clone()));
            }
        }
    });
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
        Arc::clone(&store),
        &session,
        &test_resources(),
        None,
        safety_buffering::SafetyAction {
            agent_id: "agent",
            actor: RequestActor {
                agent: "agent".into(),
                epoch: 7,
            },
            turn_id: "turn",
            choice: "retry",
        },
    )
    .await
    .expect("Unknown provider receipt is retained");
    assert_eq!(result["stage"], "unknown");
    {
        let snapshots = snapshots.lock().expect("snapshots");
        assert!(
            !snapshots.is_empty(),
            "observed a committed Unknown receipt"
        );
        assert!(
            snapshots
                .iter()
                .all(|snapshot| snapshot == &(json!("unknown"), json!("unknown"))),
            "nonterminal Unknown receipt committed: {snapshots:?}"
        );
    }
    drop(session);
    drop(store);

    let reopened = Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("restart store"),
    );
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
        Arc::clone(&reopened),
        &restarted,
        &test_resources(),
        None,
        safety_buffering::SafetyAction {
            agent_id: "agent",
            actor: RequestActor {
                agent: "agent".into(),
                epoch: 7,
            },
            turn_id: "turn",
            choice: "retry",
        },
    )
    .await
    .expect("terminal Unknown result replays");
    assert_eq!(replay["stage"], "unknown");
    assert_eq!(restarted.calls.lock().expect("calls").len(), 0);
}

#[tokio::test]
async fn not_sent_retries_same_operation_id_and_records_each_receipt() {
    let (template, _) = python_corpus();
    for not_sent_at in 0..7 {
        let root = tempfile::tempdir().expect("state copy");
        for (file, bytes) in ["canvas.sqlite3", "analytics.sqlite3"]
            .into_iter()
            .zip(&template)
        {
            std::fs::write(root.path().join(file), bytes).expect("Python-written DB");
        }
        let store = Arc::new(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("open state"),
        );
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
            Arc::clone(&store),
            &session,
            &test_resources(),
            None,
            safety_buffering::SafetyAction {
                agent_id: "agent",
                actor: RequestActor {
                    agent: "agent".into(),
                    epoch: 7,
                },
                turn_id: "turn",
                choice: "retry",
            },
        )
        .await
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
        let db = store.test_reader().expect("state read");
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

#[tokio::test]
async fn restart_resumes_recorded_intent_with_the_same_operation_id() {
    let (template, _) = python_corpus();
    for interrupted_stage in 0..7 {
        let root = tempfile::tempdir().expect("state copy");
        for (file, bytes) in ["canvas.sqlite3", "analytics.sqlite3"]
            .into_iter()
            .zip(&template)
        {
            std::fs::write(root.path().join(file), bytes).expect("Python-written DB");
        }
        let store = Arc::new(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("open state"),
        );
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
                Arc::clone(&store),
                &first,
                &test_resources(),
                None,
                safety_buffering::SafetyAction {
                    agent_id: "agent",
                    actor: RequestActor {
                        agent: "agent".into(),
                        epoch: 7,
                    },
                    turn_id: "turn",
                    choice: "retry",
                },
            )
            .await
            .is_err(),
            "interrupted stage {interrupted_stage}"
        );
        let (operation_id, stage) = {
            let calls = first.calls.lock().expect("calls");
            (
                calls[interrupted_stage]["operation"].clone(),
                calls[interrupted_stage]["stage"].clone(),
            )
        };
        drop(first);
        drop(store);

        let restarted = Arc::new(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("restart store"),
        );
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
            Arc::clone(&restarted),
            &second,
            &test_resources(),
            None,
            safety_buffering::SafetyAction {
                agent_id: "agent",
                actor: RequestActor {
                    agent: "agent".into(),
                    epoch: 7,
                },
                turn_id: "turn",
                choice: "retry",
            },
        )
        .await
        .expect("resume from recorded intent");
        assert_eq!(result["stage"], "running", "stage {interrupted_stage}");
        assert_eq!(
            second.calls.lock().expect("calls")[0]["operation"],
            operation_id
        );
        assert_eq!(second.calls.lock().expect("calls")[0]["stage"], stage);
    }
}

#[tokio::test]
async fn restart_applies_persisted_written_receipt_without_redispatch() {
    let (template, _) = python_corpus();
    for interrupted_stage in 0..7 {
        let root = tempfile::tempdir().expect("state copy");
        for (file, bytes) in ["canvas.sqlite3", "analytics.sqlite3"]
            .into_iter()
            .zip(&template)
        {
            std::fs::write(root.path().join(file), bytes).expect("Python-written DB");
        }
        let store = Arc::new(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("open state"),
        );
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
                Arc::clone(&store),
                &first,
                &test_resources(),
                None,
                safety_buffering::SafetyAction {
                    agent_id: "agent",
                    actor: RequestActor {
                        agent: "agent".into(),
                        epoch: 7,
                    },
                    turn_id: "turn",
                    choice: "retry",
                },
            )
            .await
            .is_err(),
            "interrupted stage {interrupted_stage}"
        );
        let interrupted = first.calls.lock().expect("calls")[interrupted_stage].clone();
        let stage = interrupted["stage"].as_str().expect("stage").to_owned();
        let operation_id = interrupted["operation"].clone();
        let operation_id_for_transaction = operation_id.clone();
        let response = successful_response(&stage);
        store
            .transaction_with(move |tx| {
                let raw: String = tx.query_row(
                    "SELECT record FROM runtime_safety_retries WHERE id='agent:turn'",
                    [],
                    |row| row.get(0),
                )?;
                let mut operation: Value = serde_json::from_str(&raw).expect("operation");
                operation["responses"][&stage] = response.clone();
                operation["receipts"][&stage] = json!([{
                    "operation":operation_id_for_transaction.clone(),
                    "scope":interrupted["scope"],
                    "certainty":"written",
                    "result":response,
                }]);
                operation["dispatch"] = json!("received");
                if stage == "start" {
                    let request_id = operation_id_for_transaction
                        .as_str()
                        .expect("start operation id");
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
            .await
            .expect("simulate process stop after durable receipt");
        drop(first);
        drop(store);

        let restarted = Arc::new(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("restart store"),
        );
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
            Arc::clone(&restarted),
            &second,
            &test_resources(),
            None,
            safety_buffering::SafetyAction {
                agent_id: "agent",
                actor: RequestActor {
                    agent: "agent".into(),
                    epoch: 7,
                },
                turn_id: "turn",
                choice: "retry",
            },
        )
        .await
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
