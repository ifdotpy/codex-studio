use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use studio_runtime_services::claude_controls::{
    ControlError, ControlRequest, service::*, store::StoredControls,
};
use studio_runtime_store::{AgentId, AgentRepository, RuntimeStore};
/// Create a store in `state` holding the paused Claude agent `agent` with three
/// completed turns, then close it so the test can reopen the directory.
async fn seed_state(state: &std::path::Path, cwd: &std::path::Path) {
    let store = RuntimeStore::open(Some(state))
        .await
        .expect("valid fixture");
    let agent = json!({"id":"agent","rootId":"agent","epoch":2,"cwd":cwd,"provider":"claude",
        "accountKey":"account","threadId":"thread","status":"paused","autoWake":false})
    .to_string();
    store
        .transaction_with(move |tx| {
            tx.execute(
                "INSERT INTO runtime_agents(id,record) VALUES ('agent',?1)",
                [&agent],
            )
            .map_err(studio_runtime_store::StoreError::from)?;
            for turn in ["one", "two", "three"] {
                let record =
                    json!({"id":turn,"agent":"agent","turnId":turn,"role":"assistant","text":turn})
                        .to_string();
                tx.execute(
                    "INSERT INTO runtime_items(id,agent,created,record) VALUES (?1,'agent',100,?2)",
                    [turn, record.as_str()],
                )
                .map_err(studio_runtime_store::StoreError::from)?;
            }
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("valid fixture");
}
struct Native {
    calls: Mutex<Vec<String>>,
    unknown: bool,
    generation: std::sync::atomic::AtomicU64,
}

struct MalformedState(Mutex<Vec<String>>);
struct UnknownState(std::sync::atomic::AtomicUsize);
impl ControlSession for UnknownState {
    fn binding(&self) -> ControlBinding {
        ControlBinding::new("account".into(), "connection".into(), 1).expect("binding")
    }
    fn call_scoped(
        &self,
        _: ControlScope,
        invocation: ControlInvocation,
    ) -> ControlFuture<'_, ControlReply> {
        assert!(matches!(invocation, ControlInvocation::State { .. }));
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Err(ControlError::Unknown) })
    }
}

#[tokio::test]
async fn early_unknown_does_not_finalize_the_control_request() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("cwd");
    let state = root.path().join("state");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir(&state).unwrap();
    seed_state(&state, &cwd).await;
    let store = Arc::new(RuntimeStore::open(Some(&state)).await.unwrap());
    let native = Arc::new(UnknownState(std::sync::atomic::AtomicUsize::new(0)));
    let controls = Controls::new(
        Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {}))),
        native.clone(),
    );
    let request = || {
        serde_json::from_str(r#"{"action":"settings","id":"agent","request_id":"early-unknown","settings":{"thinking":false}}"#).unwrap()
    };
    assert!(matches!(
        controls.execute(request()).await,
        Err(ControlError::Unknown)
    ));
    let db = store.test_reader().unwrap();
    let final_response: Option<String> = db.query_row("SELECT json_extract(record,'$.finalResponse') FROM runtime_workspace_operations WHERE json_extract(record,'$.requestId')='early-unknown'", [], |row| row.get(0)).unwrap();
    assert!(final_response.is_none());
    drop(db);
    assert!(matches!(
        controls.execute(request()).await,
        Err(ControlError::Unknown)
    ));
    assert_eq!(native.0.load(std::sync::atomic::Ordering::SeqCst), 2);
}
impl ControlSession for MalformedState {
    fn binding(&self) -> ControlBinding {
        ControlBinding::new("account".into(), "connection".into(), 1).expect("binding")
    }
    fn call_scoped(
        &self,
        _scope: ControlScope,
        invocation: ControlInvocation,
    ) -> ControlFuture<'_, ControlReply> {
        let result = self.call(invocation.method().as_str(), invocation.params());
        Box::pin(async move { ControlReply::from_wire(&invocation, result.await?) })
    }
}
impl MalformedState {
    fn call(&self, method: &str, _: Value) -> ControlFuture<'_, Value> {
        self.0.lock().expect("calls").push(method.to_owned());
        let state = method == "claude/state";
        Box::pin(async move {
            if state {
                Ok(json!({"tasks": [], "turns": null}))
            } else {
                Ok(json!({"settings": {}}))
            }
        })
    }
}

#[tokio::test]
async fn malformed_state_never_authorizes_a_settings_mutation() {
    let root = tempfile::tempdir().expect("scratch");
    let cwd = root.path().join("cwd");
    let state = root.path().join("state");
    std::fs::create_dir(&cwd).expect("cwd");
    std::fs::create_dir(&state).expect("state");
    seed_state(&state, &cwd).await;
    let store = Arc::new(RuntimeStore::open(Some(&state)).await.expect("store"));
    let native = Arc::new(MalformedState(Mutex::new(vec![])));
    let controls = Controls::new(
        Arc::new(StoredControls::new(store, Arc::new(|_| {}))),
        native.clone(),
    );
    let request = serde_json::from_value(json!({"action":"settings","id":"agent","request_id":"malformed","settings":{"thinking":false}})).expect("request");
    assert!(controls.execute(request).await.is_err());
    assert_eq!(*native.0.lock().expect("calls"), ["claude/state"]);
}

#[test]
fn control_wire_roundtrips_presence_numbers_extensions_and_original_ids() {
    let state = json!({"turns":[{"id":"turn","status":null,"extra":1.5}],"tasks":null,"extra":{"count":9007199254740991u64}});
    let parsed: NativeState = serde_json::from_value(state.clone()).expect("state");
    assert_eq!(
        serde_json::to_value(parsed).expect("state serialization"),
        state
    );
    let settings = json!({"autoCompactWindow":null,"thinking":false});
    let call = ControlInvocation::Settings {
        thread_id: "thread".into(),
        settings: serde_json::from_value(settings.clone()).expect("settings"),
        request_id: "transport".into(),
    };
    assert_eq!(
        call.params(),
        json!({"threadId":"thread","settings":settings})
    );
    let reply = json!({"settings":{"thinking":false,"vendor":{"count":1.5}},"extra":null});
    let parsed: SettingsReply = serde_json::from_value(reply.clone()).expect("reply");
    assert_eq!(
        serde_json::to_value(parsed).expect("reply serialization"),
        reply
    );
    let call = ControlInvocation::Rollback {
        thread_id: "thread".into(),
        request_id: serde_json::from_value(json!("original")).expect("request"),
        turn_id: serde_json::from_value(json!("turn")).expect("turn"),
        restore_files: false,
        transport_request_id: "retry-transport".into(),
    };
    assert_eq!(
        call.params(),
        json!({"threadId":"thread","requestId":"original","turnId":"turn","restoreFiles":false})
    );
    assert_eq!(call.request_id(), Some("retry-transport"));
}
impl ControlSession for Native {
    fn binding(&self) -> ControlBinding {
        ControlBinding::new(
            "account".into(),
            "connection".into(),
            self.generation.load(std::sync::atomic::Ordering::SeqCst),
        )
        .expect("valid fixture")
    }
    fn call_scoped(
        &self,
        _scope: ControlScope,
        invocation: ControlInvocation,
    ) -> ControlFuture<'_, ControlReply> {
        let result = self.call(invocation.method().as_str(), invocation.params());
        Box::pin(async move { ControlReply::from_wire(&invocation, result.await?) })
    }
}
impl Native {
    fn call(&self, method: &str, _params: Value) -> ControlFuture<'_, Value> {
        let method = method.to_owned();
        Box::pin(async move {
            self.calls
                .lock()
                .expect("valid fixture")
                .push(method.clone());
            Ok(match method.as_str() {
                "claude/state" => {
                    json!({"tasks":[],"turns":[{"id":"one","status":"completed"},{"id":"two","status":"completed"},{"id":"three","status":"completed"}]})
                }
                "thread/queue/list" => json!({"data":[]}),
                "thread/rollback" => {
                    if self.unknown {
                        return Err(ControlError::Unknown);
                    }
                    json!({"threadId":"thread","nativeId":"restored"})
                }
                "claude/commands" => json!({"commands":["/fixture"]}),
                "claude/stopTask" => json!({"stopped":true}),
                _ => unreachable!(),
            })
        })
    }
}

struct MismatchedSettings {
    inner: Native,
    attempts: std::sync::atomic::AtomicUsize,
    response: Value,
}
impl ControlSession for MismatchedSettings {
    fn binding(&self) -> ControlBinding {
        self.inner.binding()
    }
    fn call_scoped(
        &self,
        scope: ControlScope,
        invocation: ControlInvocation,
    ) -> ControlFuture<'_, ControlReply> {
        if matches!(invocation, ControlInvocation::Settings { .. }) {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let response = self.response.clone();
            return Box::pin(async move {
                ControlReply::from_wire(&invocation, json!({"settings":response,"extension":1}))
            });
        }
        self.inner.call_scoped(scope, invocation)
    }
}
#[tokio::test]
async fn mismatched_settings_reply_is_unknown_and_never_replayed() {
    let root = tempfile::tempdir().expect("scratch");
    let cwd = root.path().join("cwd");
    let state = root.path().join("state");
    std::fs::create_dir(&cwd).expect("cwd");
    std::fs::create_dir(&state).expect("state");
    seed_state(&state, &cwd).await;
    let store = Arc::new(RuntimeStore::open(Some(&state)).await.expect("store"));
    let native = Arc::new(MismatchedSettings {
        inner: Native {
            calls: Mutex::new(vec![]),
            unknown: false,
            generation: 1.into(),
        },
        attempts: 0.into(),
        response: json!({"permissionMode":"default"}),
    });
    let controls = Controls::new(
        Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {}))),
        native.clone(),
    );
    let request: ControlRequest = serde_json::from_value(json!({"action":"settings","id":"agent","request_id":"mismatch","settings":{"permissionMode":"plan"}})).expect("request");
    assert!(matches!(
        controls.execute(request.clone()).await,
        Err(ControlError::Unknown)
    ));
    assert!(matches!(
        controls.execute(request).await,
        Err(ControlError::Unknown)
    ));
    assert_eq!(native.attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    let agent = AgentRepository::get(
        &store.test_reader().expect("reader"),
        &AgentId("agent".into()),
    )
    .expect("agent")
    .expect("present")
    .value();
    assert_eq!(agent["workspaceOperation"], "branch_recovery");
}

#[tokio::test]
async fn extra_known_settings_in_reply_remain_unknown_and_are_not_replayed() {
    for response in [
        json!({"thinking":false}),
        json!({"permissionMode":"bypassPermissions"}),
        json!({"mcpAutoBackground":false}),
        json!({"autoCompactWindow":null}),
    ] {
        let root = tempfile::tempdir().expect("scratch");
        let cwd = root.path().join("cwd");
        let state = root.path().join("state");
        std::fs::create_dir(&cwd).expect("cwd");
        std::fs::create_dir(&state).expect("state");
        seed_state(&state, &cwd).await;
        let store = Arc::new(RuntimeStore::open(Some(&state)).await.expect("store"));
        let native = Arc::new(MismatchedSettings {
            inner: Native {
                calls: Mutex::new(vec![]),
                unknown: false,
                generation: 1.into(),
            },
            attempts: 0.into(),
            response,
        });
        let controls = Controls::new(
            Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {}))),
            native.clone(),
        );
        let request: ControlRequest = serde_json::from_value(
            json!({"action":"settings","id":"agent","request_id":"extra-known","settings":{}}),
        )
        .expect("request");
        assert!(matches!(
            controls.execute(request.clone()).await,
            Err(ControlError::Unknown)
        ));
        assert!(matches!(
            controls.execute(request).await,
            Err(ControlError::Unknown)
        ));
        assert_eq!(native.attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
        let agent = AgentRepository::get(
            &store.test_reader().expect("reader"),
            &AgentId("agent".into()),
        )
        .expect("agent")
        .expect("present")
        .value();
        assert_eq!(agent["workspaceOperation"], "branch_recovery");
    }
}

struct MalformedRollback {
    inner: Native,
    attempts: std::sync::atomic::AtomicUsize,
}
impl ControlSession for MalformedRollback {
    fn binding(&self) -> ControlBinding {
        self.inner.binding()
    }
    fn call_scoped(
        &self,
        scope: ControlScope,
        invocation: ControlInvocation,
    ) -> ControlFuture<'_, ControlReply> {
        if matches!(invocation, ControlInvocation::Rollback { .. }) {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Box::pin(async move {
                ControlReply::from_wire(&invocation, json!({"nativeId":"restored"}))
            });
        }
        self.inner.call_scoped(scope, invocation)
    }
}

#[tokio::test]
async fn malformed_mutation_reply_is_unknown_and_never_replayed() {
    let root = tempfile::tempdir().expect("scratch");
    let cwd = root.path().join("cwd");
    let state = root.path().join("state");
    std::fs::create_dir(&cwd).expect("cwd");
    std::fs::create_dir(&state).expect("state");
    seed_state(&state, &cwd).await;
    let store = Arc::new(RuntimeStore::open(Some(&state)).await.expect("store"));
    let native = Arc::new(MalformedRollback {
        inner: Native {
            calls: Mutex::new(vec![]),
            unknown: false,
            generation: 1.into(),
        },
        attempts: 0.into(),
    });
    let controls = Controls::new(
        Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {}))),
        native.clone(),
    );
    let request: ControlRequest = serde_json::from_value(
        json!({"action":"rollback","id":"agent","request_id":"malformed-reply","turn_id":"two"}),
    )
    .expect("request");
    assert!(matches!(
        controls.execute(request.clone()).await,
        Err(ControlError::Unknown)
    ));
    assert!(matches!(
        controls.execute(request).await,
        Err(ControlError::Unknown)
    ));
    assert_eq!(native.attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    let agent = AgentRepository::get(
        &store.test_reader().expect("reader"),
        &AgentId("agent".into()),
    )
    .expect("agent")
    .expect("present")
    .value();
    assert_eq!(agent["workspaceOperation"], "branch_recovery");
}
#[tokio::test]
async fn stored_control_replay_and_unknown_never_repeat_native_write() {
    let root = tempfile::tempdir().expect("valid fixture");
    let cwd = root.path().join("cwd");
    std::fs::create_dir(&cwd).expect("valid fixture");
    for unknown in [false, true] {
        let state = root.path().join(format!("state-{unknown}"));
        std::fs::create_dir(&state).expect("valid fixture");
        seed_state(&state, &cwd).await;
        let store = Arc::new(
            RuntimeStore::open(Some(&state))
                .await
                .expect("valid fixture"),
        );
        let native = Arc::new(Native {
            calls: Mutex::new(vec![]),
            unknown,
            generation: std::sync::atomic::AtomicU64::new(1),
        });
        let adapter = Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {})));
        let service = Controls::new(adapter.clone(), native.clone());
        for (action, thread) in [
            ("state", Value::Null),
            ("state", json!("thread")),
            ("commands", json!("thread")),
            ("stop_task", json!("thread")),
        ] {
            let agent = json!({"id":"agent","rootId":"agent","epoch":2,"cwd":"/fixture","provider":"claude","accountKey":"account","threadId":thread,"status":"paused"});
            let body = json!({"id":"agent","action":action,"task_id":"task","request_id":studio_runtime_schema::persistence::RequestIdentity::issue_root()});
            service
                .read_or_stop(&agent, &body, None)
                .await
                .expect("read action");
        }

        let request_id = studio_runtime_schema::persistence::RequestIdentity::issue_root();
        let request = || {
            serde_json::from_value::<ControlRequest>(json!({"id":"agent","action":"rollback","request_id":request_id,"turn_id":"two"})).expect("valid fixture")
        };
        let first = service.execute(request()).await;
        assert_eq!(
            first.is_err(),
            unknown,
            "{first:?}: {:?}",
            native.calls.lock().expect("fixture calls")
        );
        let replay = Controls::new(adapter, native.clone())
            .execute(request())
            .await;
        assert_eq!(replay.is_err(), unknown);
        assert_eq!(
            native
                .calls
                .lock()
                .expect("valid fixture")
                .iter()
                .filter(|m| m.as_str() == "thread/rollback")
                .count(),
            1
        );
        let agent = AgentRepository::get(
            &store.test_reader().expect("valid fixture"),
            &AgentId("agent".into()),
        )
        .expect("valid fixture")
        .expect("valid fixture")
        .value();
        assert_eq!(agent["status"], "paused");
        assert_eq!(
            agent["workspaceOperation"],
            if unknown {
                json!("branch_recovery")
            } else {
                Value::Null
            }
        );
        let item: String = store
            .test_reader()
            .expect("valid fixture")
            .query_row("SELECT record FROM runtime_items WHERE id='two'", [], |r| {
                r.get(0)
            })
            .expect("valid fixture");
        assert_eq!(
            serde_json::from_str::<Value>(&item)
                .expect("valid fixture")
                .get("afterRestore")
                .is_some(),
            !unknown
        );
    }
}

struct Cut {
    inner: Arc<StoredControls>,
    step: &'static str,
    after: bool,
    reached: tokio::sync::Notify,
}
impl Cut {
    async fn at(&self, step: &str, after: bool) {
        if self.step == step && self.after == after {
            self.reached.notify_one();
            std::future::pending::<()>().await;
        }
    }
}
impl ControlStorage for Cut {
    fn replay(
        &self,
        request: ControlRequest,
    ) -> ControlFuture<'_, Option<Result<Value, ControlError>>> {
        self.inner.replay(request)
    }
    fn reserve(
        &self,
        request: ControlRequest,
        binding: ControlBinding,
    ) -> ControlFuture<'_, studio_runtime_services::claude_controls::ControlSnapshot> {
        Box::pin(async move {
            self.at("reserve", false).await;
            let result = self.inner.reserve(request, binding).await?;
            self.at("reserve", true).await;
            Ok(result)
        })
    }
    fn submitted(
        &self,
        expected: studio_runtime_services::claude_controls::ControlSnapshot,
        request: ControlRequest,
        proof: NativeProof,
    ) -> ControlFuture<'_, studio_runtime_services::claude_controls::ControlSnapshot> {
        Box::pin(async move {
            self.at("submitted", false).await;
            let result = self.inner.submitted(expected, request, proof).await?;
            self.at("submitted", true).await;
            Ok(result)
        })
    }
    fn provider(
        &self,
        expected: studio_runtime_services::claude_controls::ControlSnapshot,
        request: ControlRequest,
        result: MutationReply,
    ) -> ControlFuture<'_, studio_runtime_services::claude_controls::ControlSnapshot> {
        Box::pin(async move {
            self.at("provider", false).await;
            let result = self.inner.provider(expected, request, result).await?;
            self.at("provider", true).await;
            Ok(result)
        })
    }
    fn complete(
        &self,
        expected: studio_runtime_services::claude_controls::ControlSnapshot,
        request: ControlRequest,
    ) -> ControlFuture<'_, Value> {
        Box::pin(async move {
            self.at("complete", false).await;
            let result = self.inner.complete(expected, request).await?;
            self.at("complete", true).await;
            Ok(result)
        })
    }
    fn failed(
        &self,
        expected: studio_runtime_services::claude_controls::ControlSnapshot,
        evidence: FailureEvidence,
        error: ControlError,
    ) -> ControlFuture<'_, ()> {
        Box::pin(async move {
            self.at("failed", false).await;
            self.inner.failed(expected, evidence, error).await?;
            self.at("failed", true).await;
            Ok(())
        })
    }
}
#[tokio::test]
async fn durable_control_store_boundaries_resume_or_hold_without_duplicate_write() {
    let root = tempfile::tempdir().expect("valid fixture");
    let cwd = root.path().join("cwd");
    std::fs::create_dir(&cwd).expect("valid fixture");
    let full = std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some();
    for step in ["reserve", "submitted", "provider", "complete", "failed"] {
        if !full && !["submitted", "provider"].contains(&step) {
            continue;
        }
        for after in [false, true] {
            let state = root.path().join(format!("{step}-{after}"));
            std::fs::create_dir(&state).expect("valid fixture");
            seed_state(&state, &cwd).await;
            let store = Arc::new(
                RuntimeStore::open(Some(&state))
                    .await
                    .expect("valid fixture"),
            );
            let native = Arc::new(Native {
                calls: Mutex::new(vec![]),
                unknown: step == "failed",
                generation: std::sync::atomic::AtomicU64::new(1),
            });
            let adapter = Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {})));
            let cut = Arc::new(Cut {
                inner: adapter.clone(),
                step,
                after,
                reached: tokio::sync::Notify::new(),
            });
            let service = Controls::new(cut.clone(), native.clone());
            let request = || {
                serde_json::from_value::<ControlRequest>(json!({"id":"agent","action":"rollback","request_id":"control-one","turn_id":"two"})).expect("valid fixture")
            };
            tokio::select! { _=cut.reached.notified()=>{}, result=service.execute(request())=>panic!("{step}/{after}: unexpected completion {}",result.is_ok()) }
            drop(service);
            drop(cut);
            drop(adapter);
            drop(store);
            let store = Arc::new(
                RuntimeStore::open(Some(&state))
                    .await
                    .expect("valid fixture"),
            );
            let service = Controls::new(
                Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {}))),
                native.clone(),
            );
            if step == "provider" && after {
                native
                    .generation
                    .store(2, std::sync::atomic::Ordering::SeqCst);
            }
            let outcome = service.execute(request()).await;
            let held = (step == "submitted" && after)
                || (step == "provider" && !after)
                || step == "failed";
            assert_eq!(outcome.is_err(), held, "{step}/{after}: {outcome:?}");
            assert!(
                native
                    .calls
                    .lock()
                    .expect("valid fixture")
                    .iter()
                    .filter(|m| m.as_str() == "thread/rollback")
                    .count()
                    <= 1,
                "{step}/{after}"
            );
            let agent = AgentRepository::get(
                &store.test_reader().expect("valid fixture"),
                &AgentId("agent".into()),
            )
            .expect("valid fixture")
            .expect("valid fixture")
            .value();
            assert_eq!(
                agent["workspaceOperation"],
                if held {
                    json!("branch_recovery")
                } else {
                    Value::Null
                },
                "{step}/{after}"
            );
        }
    }
}

struct RewindNative(Mutex<Vec<Value>>, bool);
impl ControlSession for RewindNative {
    fn binding(&self) -> ControlBinding {
        ControlBinding::new("account".into(), "connection".into(), 1)
            .expect("control binding validates")
    }
    fn call_scoped(
        &self,
        _scope: ControlScope,
        invocation: ControlInvocation,
    ) -> ControlFuture<'_, ControlReply> {
        let result = self.call(invocation.method().as_str(), invocation.params());
        Box::pin(async move { ControlReply::from_wire(&invocation, result.await?) })
    }
}
impl RewindNative {
    fn call(&self, method: &str, params: Value) -> ControlFuture<'_, Value> {
        if method == "thread/rollback" && self.1 {
            return Box::pin(async { Err(ControlError::Request) });
        }
        self.0
            .lock()
            .expect("native call log lock is not poisoned")
            .push(json!({"method":method,"params":params}));
        let result = match method {
            "claude/state" => {
                json!({"tasks":[],"turns":[{"id":"one","status":"completed","canRestoreFiles":true},{"id":"two","status":"completed","canRestoreFiles":true},{"id":"three","status":"completed","canRestoreFiles":true}]})
            }
            "thread/queue/list" => json!({"data":[]}),
            "thread/rollback" => {
                assert_eq!(params["restoreFiles"], true);
                assert_eq!(params["turnId"], "two");
                json!({"threadId":"thread","nativeId":"restored","filesRestored":true})
            }
            _ => panic!("unexpected method {method}"),
        };
        Box::pin(async move { Ok(result) })
    }
}

#[tokio::test]
async fn combined_rewind_for_shared_workspace_settles_history_and_replays_once() {
    let root = tempfile::tempdir().expect("temporary workspace creates");
    let cwd = root.path().join("shared");
    let state = root.path().join("state");
    std::fs::create_dir(&cwd).expect("shared directory creates");
    std::fs::create_dir(&state).expect("state directory creates");
    seed_state(&state, &cwd).await;
    let store = Arc::new(
        RuntimeStore::open(Some(&state))
            .await
            .expect("runtime store opens"),
    );
    store.transaction(|tx| {
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.workspaceMode','shared','$.worktreeReady',json('false')) WHERE id='agent'",[])?;
        Ok(())
    }).await.expect("shared workspace fixture writes");
    let native = Arc::new(RewindNative(Mutex::new(vec![]), false));
    let controls = Controls::new(
        Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {}))),
        native.clone(),
    );
    let body = json!({"id":"agent","action":"rollback","turn_id":"two","request_id":"combined","restore_files":true});
    let result = controls
        .execute(
            serde_json::from_value::<ControlRequest>(body.clone())
                .expect("rollback request parses"),
        )
        .await
        .expect("combined rewind succeeds");
    assert_eq!(result["filesRestored"], true);
    assert_eq!(
        controls
            .execute(
                serde_json::from_value::<ControlRequest>(body.clone())
                    .expect("replay request parses")
            )
            .await
            .expect("combined rewind replay succeeds"),
        result
    );
    {
        let calls = native
            .0
            .lock()
            .expect("native call log lock is not poisoned");
        assert_eq!(
            calls
                .iter()
                .filter(|call| call["method"] == "thread/rollback")
                .count(),
            1
        );
    }
    let db = store.test_reader().expect("test reader opens");
    let agent = AgentRepository::get(&db, &AgentId("agent".into()))
        .expect("agent query succeeds")
        .expect("agent exists")
        .value();
    assert_eq!(agent["lastCompletedTurn"], "one");
    assert_eq!(agent["status"], "paused");
    assert!(agent["workspaceOperation"].is_null());
    let removed: i64 = db.query_row("SELECT count(*) FROM runtime_items WHERE json_extract(record,'$.afterRestore') IS NOT NULL",[],|row|row.get(0)).expect("restored item count loads");
    assert_eq!(removed, 2);
    let mut changed = body;
    changed["restore_files"] = json!(false);
    assert!(matches!(
        controls
            .execute(
                serde_json::from_value::<ControlRequest>(changed.clone())
                    .expect("changed request parses")
            )
            .await,
        Err(ControlError::Conflict)
    ));
}

#[tokio::test]
async fn combined_rewind_rejection_holds_the_workspace_for_recovery() {
    let root = tempfile::tempdir().expect("temporary workspace creates");
    let cwd = root.path().join("shared");
    let state = root.path().join("state");
    std::fs::create_dir(&cwd).expect("shared directory creates");
    std::fs::create_dir(&state).expect("state directory creates");
    seed_state(&state, &cwd).await;
    let store = Arc::new(
        RuntimeStore::open(Some(&state))
            .await
            .expect("runtime store opens"),
    );
    let controls = Controls::new(
        Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {}))),
        Arc::new(RewindNative(Mutex::new(vec![]), true)),
    );
    let request = || {
        serde_json::from_value::<ControlRequest>(json!({"id":"agent","action":"rollback","turn_id":"two","request_id":"combined","restore_files":true})).expect("rollback request parses")
    };
    assert!(matches!(
        controls.execute(request()).await,
        Err(ControlError::Unknown)
    ));
    let db = store.test_reader().expect("test reader opens");
    let agent = AgentRepository::get(&db, &AgentId("agent".into()))
        .expect("agent query succeeds")
        .expect("agent exists")
        .value();
    assert_eq!(agent["workspaceOperation"], "branch_recovery");
    assert_eq!(agent["status"], "paused");
    let visible: i64 = db.query_row("SELECT count(*) FROM runtime_items WHERE json_extract(record,'$.afterRestore') IS NULL",[],|row|row.get(0)).expect("visible item count loads");
    assert_eq!(visible, 3);
    assert!(matches!(
        controls.execute(request()).await,
        Err(ControlError::Unknown)
    ));
}

struct HeldRewind {
    inner: RewindNative,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
impl ControlSession for HeldRewind {
    fn binding(&self) -> ControlBinding {
        self.inner.binding()
    }
    fn call_scoped(
        &self,
        _scope: ControlScope,
        invocation: ControlInvocation,
    ) -> ControlFuture<'_, ControlReply> {
        let result = self.call(invocation.method().as_str(), invocation.params());
        Box::pin(async move { ControlReply::from_wire(&invocation, result.await?) })
    }
}
impl HeldRewind {
    fn call(&self, method: &str, params: Value) -> ControlFuture<'_, Value> {
        let result = self.inner.call(method, params);
        let hold = method == "thread/rollback";
        Box::pin(async move {
            if hold {
                self.entered.notify_one();
                self.release.notified().await;
            }
            result.await
        })
    }
}
#[tokio::test]
async fn separate_control_instances_cannot_release_an_in_progress_rewind() {
    let root = tempfile::tempdir().expect("temporary workspace creates");
    let cwd = root.path().join("workspace");
    let state = root.path().join("state");
    std::fs::create_dir(&cwd).expect("workspace directory creates");
    std::fs::create_dir(&state).expect("state directory creates");
    seed_state(&state, &cwd).await;
    let store = Arc::new(
        RuntimeStore::open(Some(&state))
            .await
            .expect("runtime store opens"),
    );
    let native = Arc::new(HeldRewind {
        inner: RewindNative(Mutex::new(vec![]), false),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let first = Controls::new(
        Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {}))),
        native.clone(),
    );
    let second = Controls::new(
        Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {}))),
        native.clone(),
    );
    let request = || {
        serde_json::from_value::<ControlRequest>(json!({"id":"agent","action":"rollback","turn_id":"two","request_id":"race","restore_files":true})).expect("rollback request parses")
    };
    let first_request = request();
    let task = tokio::spawn(async move { first.execute(first_request).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), native.entered.notified())
        .await
        .expect("native rewind starts before timeout");
    assert!(matches!(
        second.execute(request()).await,
        Err(ControlError::Busy)
    ));
    let agent = AgentRepository::get(
        &store.test_reader().expect("test reader opens"),
        &AgentId("agent".into()),
    )
    .expect("agent query succeeds")
    .expect("agent exists")
    .value();
    assert!(!agent["workspaceReservationId"].is_null());
    assert_eq!(agent["workspaceOperation"], "branch");
    native.release.notify_one();
    assert_eq!(
        task.await
            .expect("rewind task joins")
            .expect("rewind task succeeds")["filesRestored"],
        true
    );
    assert_eq!(
        second
            .execute(request())
            .await
            .expect("second rewind replay succeeds")["filesRestored"],
        true
    );
    assert_eq!(
        native
            .inner
            .0
            .lock()
            .expect("native call log lock is not poisoned")
            .iter()
            .filter(|call| call["method"] == "thread/rollback")
            .count(),
        1
    );
}

#[tokio::test]
async fn stale_failure_cannot_clear_a_newer_durable_submission() {
    let root = tempfile::tempdir().expect("temporary workspace creates");
    let cwd = root.path().join("workspace");
    let state = root.path().join("state");
    std::fs::create_dir(&cwd).expect("workspace directory creates");
    std::fs::create_dir(&state).expect("state directory creates");
    seed_state(&state, &cwd).await;
    let store = Arc::new(
        RuntimeStore::open(Some(&state))
            .await
            .expect("runtime store opens"),
    );
    let adapter = StoredControls::new(store.clone(), Arc::new(|_| {}));
    let request=serde_json::from_value::<ControlRequest>(json!({"id":"agent","action":"rollback","turn_id":"two","request_id":"race","restore_files":true})).expect("rollback request parses");
    let binding = ControlBinding::new("account".into(), "connection".into(), 1)
        .expect("control binding validates");
    let stale = adapter
        .reserve(request.clone(), binding)
        .await
        .expect("stale rewind reserves");
    let submitted=adapter.submitted(stale.clone(),request,NativeProof::Rollback { state: serde_json::from_value(json!({"tasks":[],"turns":[{"id":"one","status":"completed","canRestoreFiles":true},{"id":"two","status":"completed","canRestoreFiles":true},{"id":"three","status":"completed","canRestoreFiles":true}]})).expect("state"), queue: serde_json::from_value(json!({"data":[]})).expect("queue") }).await.expect("rewind submission persists");
    assert!(matches!(
        adapter
            .failed(
                stale.clone(),
                FailureEvidence::BeforeSubmission,
                ControlError::Request
            )
            .await,
        Err(ControlError::Changed)
    ));
    assert!(matches!(
        adapter
            .failed(stale, FailureEvidence::NotSent, ControlError::NotSent)
            .await,
        Err(ControlError::Changed)
    ));
    let agent = AgentRepository::get(
        &store.test_reader().expect("test reader opens"),
        &AgentId("agent".into()),
    )
    .expect("agent query succeeds")
    .expect("agent exists")
    .value();
    assert_eq!(agent["workspaceOperation"], "branch");
    assert!(!agent["workspaceReservationId"].is_null());
    // Even a caller that reports certainty cannot discard durable submitted evidence.
    adapter
        .failed(
            submitted,
            FailureEvidence::BeforeSubmission,
            ControlError::Request,
        )
        .await
        .expect("failure settlement succeeds");
    let agent = AgentRepository::get(
        &store.test_reader().expect("test reader opens"),
        &AgentId("agent".into()),
    )
    .expect("agent query succeeds")
    .expect("agent exists")
    .value();
    assert_eq!(agent["workspaceOperation"], "branch_recovery");
    assert!(!agent["workspaceReservationId"].is_null());
}

struct NotSentOnce {
    pre_effect: bool,
    inner: RewindNative,
    attempts: std::sync::atomic::AtomicUsize,
}
impl ControlSession for NotSentOnce {
    fn binding(&self) -> ControlBinding {
        self.inner.binding()
    }
    fn call_scoped(
        &self,
        _scope: ControlScope,
        invocation: ControlInvocation,
    ) -> ControlFuture<'_, ControlReply> {
        let result = self.call(invocation.method().as_str(), invocation.params());
        Box::pin(async move { ControlReply::from_wire(&invocation, result.await?) })
    }
}
impl NotSentOnce {
    fn call(&self, method: &str, params: Value) -> ControlFuture<'_, Value> {
        if method == "thread/rollback"
            && self
                .attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 0
        {
            let pre_effect = self.pre_effect;
            return Box::pin(async move {
                Err(if pre_effect {
                    ControlError::RewindNotApplied(
                        "The before-restore backup limit is reached.".into(),
                    )
                } else {
                    ControlError::NotSent
                })
            });
        }
        self.inner.call(method, params)
    }
}
#[tokio::test]
async fn definite_not_sent_releases_and_retries_the_same_request_after_fresh_idle_proof() {
    let root = tempfile::tempdir().expect("temporary workspace creates");
    let cwd = root.path().join("workspace");
    let state = root.path().join("state");
    std::fs::create_dir(&cwd).expect("workspace directory creates");
    std::fs::create_dir(&state).expect("state directory creates");
    seed_state(&state, &cwd).await;
    let store = Arc::new(
        RuntimeStore::open(Some(&state))
            .await
            .expect("runtime store opens"),
    );
    let native = Arc::new(NotSentOnce {
        pre_effect: false,
        inner: RewindNative(Mutex::new(vec![]), false),
        attempts: std::sync::atomic::AtomicUsize::new(0),
    });
    let controls = Controls::new(
        Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {}))),
        native.clone(),
    );
    let request = || {
        serde_json::from_value::<ControlRequest>(json!({"id":"agent","action":"rollback","turn_id":"two","request_id":"not-sent","restore_files":true})).expect("rollback request parses")
    };
    assert!(matches!(
        controls.execute(request()).await,
        Err(ControlError::NotSent)
    ));
    let agent = AgentRepository::get(
        &store.test_reader().expect("test reader opens"),
        &AgentId("agent".into()),
    )
    .expect("agent query succeeds")
    .expect("agent exists")
    .value();
    assert!(agent["workspaceOperation"].is_null());
    assert!(agent["workspaceReservationId"].is_null());
    let monitor = json!({"id":"busy","cwd":cwd,"status":"running"});
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_monitors VALUES('busy',?1)",
                [monitor.to_string()],
            )?;
            Ok(())
        })
        .await
        .expect("rewind retry fixture writes");
    assert!(matches!(
        controls.execute(request()).await,
        Err(ControlError::Busy)
    ));
    assert_eq!(native.attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    store
        .transaction(|tx| {
            tx.execute("DELETE FROM runtime_monitors WHERE id='busy'", [])?;
            Ok(())
        })
        .await
        .expect("fresh idle proof writes");
    assert_eq!(
        controls
            .execute(request())
            .await
            .expect("rewind retry succeeds")["filesRestored"],
        true
    );
    assert_eq!(
        controls
            .execute(request())
            .await
            .expect("rewind replay succeeds")["filesRestored"],
        true
    );
    assert_eq!(native.attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(
        native
            .inner
            .0
            .lock()
            .expect("native call log lock is not poisoned")
            .iter()
            .filter(|call| call["method"] == "thread/rollback")
            .count(),
        1
    );
}

#[tokio::test]
async fn quota_preparation_refusal_releases_and_retries_after_fresh_idle_proof() {
    let root = tempfile::tempdir().expect("temporary workspace creates");
    let cwd = root.path().join("workspace");
    let state = root.path().join("state");
    std::fs::create_dir(&cwd).expect("workspace directory creates");
    std::fs::create_dir(&state).expect("state directory creates");
    seed_state(&state, &cwd).await;
    let store = Arc::new(
        RuntimeStore::open(Some(&state))
            .await
            .expect("runtime store opens"),
    );
    let native = Arc::new(NotSentOnce {
        pre_effect: true,
        inner: RewindNative(Mutex::new(vec![]), false),
        attempts: std::sync::atomic::AtomicUsize::new(0),
    });
    let controls = Controls::new(
        Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {}))),
        native.clone(),
    );
    let request = |id: &str| {
        serde_json::from_value::<ControlRequest>(json!({"id":"agent","action":"rollback","turn_id":"two","request_id":id,"restore_files":true})).expect("rollback request parses")
    };
    assert!(matches!(
        controls.execute(request("quota-refusal")).await,
        Err(ControlError::RewindNotApplied(_))
    ));
    let agent = AgentRepository::get(
        &store.test_reader().expect("test reader opens"),
        &AgentId("agent".into()),
    )
    .expect("agent query succeeds")
    .expect("agent exists")
    .value();
    assert!(agent["workspaceOperation"].is_null());
    assert!(agent["workspaceReservationId"].is_null());
    let monitor = json!({"id":"busy","cwd":cwd,"status":"running"});
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_monitors VALUES('busy',?1)",
                [monitor.to_string()],
            )?;
            Ok(())
        })
        .await
        .expect("quota retry fixture writes");
    assert!(matches!(
        controls.execute(request("quota-refusal")).await,
        Err(ControlError::RewindNotApplied(_))
    ));
    assert!(matches!(
        controls.execute(request("quota-retry")).await,
        Err(ControlError::Busy)
    ));
    assert_eq!(native.attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    store
        .transaction(|tx| {
            tx.execute("DELETE FROM runtime_monitors WHERE id='busy'", [])?;
            Ok(())
        })
        .await
        .expect("fresh idle proof writes");
    assert_eq!(
        controls
            .execute(request("quota-retry"))
            .await
            .expect("rewind retry succeeds")["filesRestored"],
        true
    );
    assert_eq!(
        controls
            .execute(request("quota-retry"))
            .await
            .expect("rewind replay succeeds")["filesRestored"],
        true
    );
    assert_eq!(native.attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(
        native
            .inner
            .0
            .lock()
            .expect("native call log lock is not poisoned")
            .iter()
            .filter(|call| call["method"] == "thread/rollback")
            .count(),
        1
    );
}

struct RejectedRewind(RewindNative);
impl ControlSession for RejectedRewind {
    fn binding(&self) -> ControlBinding {
        self.0.binding()
    }
    fn call_scoped(
        &self,
        _scope: ControlScope,
        invocation: ControlInvocation,
    ) -> ControlFuture<'_, ControlReply> {
        let result = self.call(invocation.method().as_str(), invocation.params());
        Box::pin(async move { ControlReply::from_wire(&invocation, result.await?) })
    }
}
impl RejectedRewind {
    fn call(&self, method: &str, params: Value) -> ControlFuture<'_, Value> {
        if method == "thread/rollback" {
            return Box::pin(async {
                Err(ControlError::RewindRejected(
                    "Claude restored only one file.".into(),
                ))
            });
        }
        self.0.call(method, params)
    }
}
#[tokio::test]
async fn rewind_rejection_preserves_the_reason_without_claiming_no_file_effects() {
    let root = tempfile::tempdir().expect("temporary workspace creates");
    let cwd = root.path().join("workspace");
    let state = root.path().join("state");
    std::fs::create_dir(&cwd).expect("workspace directory creates");
    std::fs::create_dir(&state).expect("state directory creates");
    seed_state(&state, &cwd).await;
    let store = Arc::new(
        RuntimeStore::open(Some(&state))
            .await
            .expect("runtime store opens"),
    );
    let controls = Controls::new(
        Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {}))),
        Arc::new(RejectedRewind(RewindNative(Mutex::new(vec![]), false))),
    );
    let request = serde_json::from_value::<ControlRequest>(json!({"id":"agent","action":"rollback","turn_id":"two","request_id":"quota","restore_files":true})).expect("rollback request parses");
    let result = controls.execute(request).await;
    assert!(
        matches!(&result, Err(ControlError::RewindRejected(message)) if message.contains("only one file"))
    );
    let agent = AgentRepository::get(
        &store.test_reader().expect("test reader opens"),
        &AgentId("agent".into()),
    )
    .expect("agent query succeeds")
    .expect("agent exists")
    .value();
    assert_eq!(agent["workspaceOperation"], "branch_recovery");
    assert!(!agent["workspaceReservationId"].is_null());
}

struct LivePermissionsNative {
    calls: Mutex<Vec<String>>,
    fail: bool,
}
impl ControlSession for LivePermissionsNative {
    fn binding(&self) -> ControlBinding {
        ControlBinding::new("account".into(), "connection".into(), 1).expect("fixture binding")
    }
    fn call_scoped(
        &self,
        _scope: ControlScope,
        invocation: ControlInvocation,
    ) -> ControlFuture<'_, ControlReply> {
        let result = self.call(invocation.method().as_str(), invocation.params());
        Box::pin(async move { ControlReply::from_wire(&invocation, result.await?) })
    }
}
impl LivePermissionsNative {
    fn call(&self, method: &str, params: Value) -> ControlFuture<'_, Value> {
        let method = method.to_owned();
        Box::pin(async move {
            self.calls.lock().expect("calls").push(method.clone());
            match method.as_str() {
                "claude/state" => {
                    Ok(json!({"tasks":[],"turns":[{"id":"three","status":"inProgress"}]}))
                }
                "claude/settings" if self.fail => Err(ControlError::SettingsNotApplied(
                    "fixture live apply failed".into(),
                )),
                "claude/settings" => {
                    let mut settings = params["settings"].clone();
                    settings["providerExtension"] = json!({"number":1.5});
                    Ok(json!({"settings":settings,"extension":null}))
                }
                _ => panic!("unexpected live control {method}"),
            }
        })
    }
}

#[tokio::test]
async fn live_permissions_claude_control_applies_while_running_and_pending_without_replay() {
    for status in ["running", "approval"] {
        for fail in [false, true] {
            let root = tempfile::tempdir().expect("state");
            let cwd = root.path().join("cwd");
            std::fs::create_dir(&cwd).expect("cwd");
            let state = root.path().join("state");
            std::fs::create_dir(&state).expect("state directory");
            seed_state(&state, &cwd).await;
            let store = Arc::new(RuntimeStore::open(Some(&state)).await.expect("store"));
            store.transaction(move |tx| {
                tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.status',?1,'$.inFlight',json('true'),'$.claudeOptions',json('{\"permissionMode\":\"default\"}')) WHERE id='agent'", [status])?;
                Ok(())
            }).await.expect("running state");
            let native = Arc::new(LivePermissionsNative {
                calls: Mutex::new(vec![]),
                fail,
            });
            let unloads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let unload_count = unloads.clone();
            let service = Controls::new(
                Arc::new(StoredControls::new(
                    store.clone(),
                    Arc::new(move |_| {
                        unload_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }),
                )),
                native.clone(),
            );
            let other_setting = serde_json::from_value::<ControlRequest>(json!({"id":"agent","action":"settings","request_id":"thinking","settings":{"permissionMode":"default","thinking":false}})).expect("request");
            assert!(matches!(
                service.execute(other_setting).await,
                Err(ControlError::Busy)
            ));

            let request = serde_json::from_value::<ControlRequest>(json!({"id":"agent","action":"settings","request_id":"live-permission","settings":{"permissionMode":"bypassPermissions","thinking":true}})).expect("request");
            let result = service.execute(request.clone()).await;
            assert_eq!(result.is_ok(), !fail, "{status}: {result:?}");
            if let Ok(result) = &result {
                assert_eq!(
                    result["settings"]["providerExtension"],
                    json!({"number":1.5})
                );
                assert_eq!(result["extension"], Value::Null);
            }
            let agent = AgentRepository::get(
                &store.test_reader().expect("reader"),
                &AgentId("agent".into()),
            )
            .expect("agent")
            .expect("agent")
            .into_value();
            assert_eq!(
                agent["claudeOptions"]["permissionMode"],
                if fail { "default" } else { "bypassPermissions" }
            );
            assert_eq!(agent["status"], status);
            assert_eq!(unloads.load(std::sync::atomic::Ordering::SeqCst), 0);
            if !fail {
                service.execute(request).await.expect("replay");
                assert_eq!(
                    native
                        .calls
                        .lock()
                        .expect("calls")
                        .iter()
                        .filter(|method| *method == "claude/settings")
                        .count(),
                    1
                );
            }
        }
    }
}

#[test]
fn mcp_auto_background_accepts_only_booleans() {
    use studio_runtime_schema::claude_controls::ClaudeSettings;
    let settings = |value: Value| serde_json::from_value::<ClaudeSettings>(value);
    for value in [json!(true), json!(false)] {
        assert!(settings(json!({"mcpAutoBackground":value})).is_ok());
    }
    for value in [json!(null), json!(1), json!("true")] {
        assert!(settings(json!({"mcpAutoBackground":value})).is_err());
    }
}
