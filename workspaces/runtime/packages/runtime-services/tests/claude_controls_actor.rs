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
impl ControlSession for Native {
    fn binding(&self) -> ControlBinding {
        ControlBinding::new(
            "account".into(),
            "connection".into(),
            self.generation.load(std::sync::atomic::Ordering::SeqCst),
        )
        .expect("valid fixture")
    }
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
            let body = json!({"id":"agent","action":action,"task_id":"task"});
            service
                .read_or_stop(&agent, &body, None)
                .await
                .expect("read action");
        }

        let request = || {
            ControlRequest::from_value(&json!({"id":"agent","action":"rollback","request_id":"control-one","turn_id":"two"})).expect("valid fixture")
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
        native: Value,
        queue: Value,
    ) -> ControlFuture<'_, studio_runtime_services::claude_controls::ControlSnapshot> {
        Box::pin(async move {
            self.at("submitted", false).await;
            let result = self
                .inner
                .submitted(expected, request, native, queue)
                .await?;
            self.at("submitted", true).await;
            Ok(result)
        })
    }
    fn provider(
        &self,
        expected: studio_runtime_services::claude_controls::ControlSnapshot,
        result: Value,
    ) -> ControlFuture<'_, studio_runtime_services::claude_controls::ControlSnapshot> {
        Box::pin(async move {
            self.at("provider", false).await;
            let result = self.inner.provider(expected, result).await?;
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
    ) -> ControlFuture<'_, ()> {
        Box::pin(async move {
            self.at("failed", false).await;
            self.inner.failed(expected, evidence).await?;
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
                ControlRequest::from_value(&json!({"id":"agent","action":"rollback","request_id":"control-one","turn_id":"two"})).expect("valid fixture")
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
async fn combined_rewind_for_image_workspace_settles_history_and_replays_once() {
    let root = tempfile::tempdir().expect("temporary workspace creates");
    let cwd = root.path().join("image");
    let state = root.path().join("state");
    std::fs::create_dir(&cwd).expect("image directory creates");
    std::fs::create_dir(&state).expect("state directory creates");
    seed_state(&state, &cwd).await;
    let store = Arc::new(
        RuntimeStore::open(Some(&state))
            .await
            .expect("runtime store opens"),
    );
    store.transaction(|tx| {
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.imageWorkspace',json('true'),'$.worktreeReady',json('false')) WHERE id='agent'",[])?;
        Ok(())
    }).await.expect("image workspace fixture writes");
    let native = Arc::new(RewindNative(Mutex::new(vec![]), false));
    let controls = Controls::new(
        Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {}))),
        native.clone(),
    );
    let body = json!({"id":"agent","action":"rollback","turn_id":"two","request_id":"combined","restore_files":true});
    let result = controls
        .execute(ControlRequest::from_value(&body).expect("rollback request parses"))
        .await
        .expect("combined rewind succeeds");
    assert_eq!(result["filesRestored"], true);
    assert_eq!(
        controls
            .execute(ControlRequest::from_value(&body).expect("replay request parses"))
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
            .execute(ControlRequest::from_value(&changed).expect("changed request parses"))
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
        ControlRequest::from_value(&json!({"id":"agent","action":"rollback","turn_id":"two","request_id":"combined","restore_files":true})).expect("rollback request parses")
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
        ControlRequest::from_value(&json!({"id":"agent","action":"rollback","turn_id":"two","request_id":"race","restore_files":true})).expect("rollback request parses")
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
    let request=ControlRequest::from_value(&json!({"id":"agent","action":"rollback","turn_id":"two","request_id":"race","restore_files":true})).expect("rollback request parses");
    let binding = ControlBinding::new("account".into(), "connection".into(), 1)
        .expect("control binding validates");
    let stale = adapter
        .reserve(request.clone(), binding)
        .await
        .expect("stale rewind reserves");
    let submitted=adapter.submitted(stale.clone(),request,json!({"tasks":[],"turns":[{"id":"one","status":"completed","canRestoreFiles":true},{"id":"two","status":"completed","canRestoreFiles":true},{"id":"three","status":"completed","canRestoreFiles":true}]}),json!({"data":[]})).await.expect("rewind submission persists");
    assert!(matches!(
        adapter
            .failed(stale.clone(), FailureEvidence::BeforeSubmission)
            .await,
        Err(ControlError::Changed)
    ));
    assert!(matches!(
        adapter.failed(stale, FailureEvidence::NotSent).await,
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
        .failed(submitted, FailureEvidence::BeforeSubmission)
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
        ControlRequest::from_value(&json!({"id":"agent","action":"rollback","turn_id":"two","request_id":"not-sent","restore_files":true})).expect("rollback request parses")
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
    let request = || {
        ControlRequest::from_value(&json!({"id":"agent","action":"rollback","turn_id":"two","request_id":"quota-refusal","restore_files":true})).expect("rollback request parses")
    };
    assert!(matches!(
        controls.execute(request()).await,
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

struct RejectedRewind(RewindNative);
impl ControlSession for RejectedRewind {
    fn binding(&self) -> ControlBinding {
        self.0.binding()
    }
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
    let request = ControlRequest::from_value(&json!({"id":"agent","action":"rollback","turn_id":"two","request_id":"quota","restore_files":true})).expect("rollback request parses");
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

#[test]
fn mcp_auto_background_accepts_only_booleans() {
    use studio_runtime_services::claude_controls::settings;
    for value in [json!(true), json!(false)] {
        assert!(settings(&json!({"mcpAutoBackground":value})).is_ok());
    }
    for value in [json!(null), json!(1), json!("true")] {
        assert!(settings(&json!({"mcpAutoBackground":value})).is_err());
    }
}
