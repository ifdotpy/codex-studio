//! A fresh account worker reads the existing VM process without another launch.
use super::*;
use crate::linux_vm_guest::{GuestError, GuestRpc};

#[derive(Default)]
struct ExistingGuest {
    inner: VmTurnGuest,
    infos: AtomicUsize,
    info_override: std::sync::Mutex<Option<Value>>,
    info_change: std::sync::Mutex<Option<(Arc<RuntimeStore>, String, String, Value)>>,
    credential_copies_at_turn_start: std::sync::Mutex<Vec<usize>>,
}
impl GuestRpc for ExistingGuest {
    fn stream(
        &self,
        method: &str,
        params: &Value,
        id: Option<&str>,
        timeout: std::time::Duration,
        on_frame: &mut dyn FnMut(Value) -> Result<(), GuestError>,
    ) -> Result<Value, GuestError> {
        if method == "provider.rpc"
            && params["action"] == "write"
            && params["message"]["method"] == "turn/start"
        {
            self.credential_copies_at_turn_start
                .lock()
                .unwrap()
                .push(self.inner.state.lock().unwrap().credential_documents.len());
        }
        if method == "provider.rpc" && params["action"] == "info" {
            self.infos.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some((store, actor, field, value)) = self.info_change.lock().unwrap().take() {
                tokio::runtime::Handle::current()
                    .block_on(store.transaction(move |tx| {
                        let mut record =
                            AgentRepository::get(tx, &AgentId::from(actor.as_str()))?.unwrap();
                        record.extra.insert(field, value);
                        AgentRepository::put(tx, &record)
                    }))
                    .unwrap();
            }
            let mut result = self.inner.stream(method, params, id, timeout, on_frame)?;
            if let Some(overrides) = self.info_override.lock().unwrap().as_ref() {
                for (key, value) in overrides.as_object().unwrap() {
                    result[key] = value.clone();
                }
            }
            return Ok(result);
        }
        self.inner.stream(method, params, id, timeout, on_frame)
    }
}

async fn edit_actor(state: &State, changes: Value) {
    let actor = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut record =
                        AgentRepository::get(tx, &AgentId::from(actor.as_str()))?.unwrap();
                    for (key, value) in changes.as_object().unwrap() {
                        if key == "epoch" {
                            record.epoch = value.as_i64().unwrap();
                        } else {
                            record.extra.insert(key.clone(), value.clone());
                        }
                    }
                    AgentRepository::put(tx, &record)
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn existing_worker(state: &State, guest: Arc<ExistingGuest>) -> (Peer, AccountHandle) {
    let mut resources = test_resources().unwrap();
    resources.guest_rpc = Some(guest);
    resources.vm_provider_launch = Some(Arc::new(NoResolver(Arc::new(AtomicUsize::new(0)))));
    let peer = Peer::new();
    let worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    (peer, worker)
}

#[tokio::test]
async fn vm_existing_reattach_refuses_changed_actor_and_missing_frozen_authority() {
    for changes in [
        json!({"accountKey":"other"}),
        json!({"provider":"codex"}),
        json!({"cwd":"/home/studio/other"}),
        json!({"threadId":"other-thread"}),
        json!({"turnId":"other-turn"}),
        json!({"epoch":99}),
        json!({"lazyAccountTransfer":{"phase":"lazy"}}),
        json!({"accountTransferId":"unfinished"}),
        json!({"executionMove":{"id":"move"}}),
        json!({"deletedAt":1}),
        json!({"startAttempt":null}),
    ] {
        let (state, guest) = existing_actor(false).await;
        edit_actor(&state, changes.clone()).await;
        let (_peer, mut worker) = existing_worker(&state, guest.clone()).await;
        worker_barrier(&worker).await;
        assert_eq!(
            guest.infos.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "{changes}"
        );
        assert!(
            guest.inner.state.lock().unwrap().writes.is_empty(),
            "{changes}"
        );
        worker.shutdown().await.unwrap();
    }
    let (state, guest) = existing_actor(false).await;
    state.bridge.run(|store| async move {
        store.transaction(|tx| {
            tx.execute("UPDATE runtime_execution_attempts SET record=json_remove(record,'$.claimResult') WHERE id='existing-vm-attempt'", [])?;
            Ok(())
        }).await?;
        Ok(())
    }).await.unwrap();
    let (_peer, mut worker) = existing_worker(&state, guest.clone()).await;
    worker_barrier(&worker).await;
    assert_eq!(guest.infos.load(std::sync::atomic::Ordering::SeqCst), 0);
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn vm_existing_reattach_refuses_unproved_native_identity_and_authority_races() {
    for change in [
        json!({"generation":32}),
        json!({"pid":43}),
        json!({"handle":"other"}),
        json!({"state":"exited"}),
        json!({"initResult":null}),
    ] {
        let (state, guest) = existing_actor(false).await;
        *guest.info_override.lock().unwrap() = Some(change.clone());
        let (_peer, mut worker) = existing_worker(&state, guest.clone()).await;
        worker_barrier(&worker).await;
        assert_eq!(
            guest.infos.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "{change}"
        );
        let native = guest.inner.state.lock().unwrap();
        assert!(native.writes.is_empty() && native.stops.is_empty());
        assert_eq!(native.acknowledged, 0);
        drop(native);
        worker.shutdown().await.unwrap();
    }
    let (state, guest) = existing_actor(false).await;
    *guest.info_change.lock().unwrap() = Some((
        state.bridge.store.clone(),
        state.agent.clone(),
        "cwd".into(),
        json!("/home/studio/moved-during-info"),
    ));
    let (_peer, mut worker) = existing_worker(&state, guest.clone()).await;
    worker_barrier(&worker).await;
    assert_eq!(guest.infos.load(std::sync::atomic::Ordering::SeqCst), 1);
    let native = guest.inner.state.lock().unwrap();
    assert!(native.writes.is_empty());
    assert_eq!(native.acknowledged, 0);
    drop(native);
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn vm_existing_reattach_refuses_missing_or_unfinished_start_receipts() {
    for result in [None, Some("reserved"), Some("not_sent")] {
        let (state, guest) = existing_actor(false).await;
        let actor = state.agent.clone();
        let epoch = state.epoch;
        state.bridge.run(move |store| async move {
            store.transaction(move |tx| {
                let key = format!("linux-vm-provider-start:{actor}:{epoch}");
                if let Some(phase) = result {
                    tx.execute("UPDATE runtime_operation_receipts SET result=json_set(result,'$.phase',?1) WHERE id=?2", rusqlite::params![phase,key])?;
                } else {
                    tx.execute("DELETE FROM runtime_operation_receipts WHERE id=?1", [key])?;
                }
                Ok(())
            }).await?;
            Ok(())
        }).await.unwrap();
        let (_peer, mut worker) = existing_worker(&state, guest.clone()).await;
        worker_barrier(&worker).await;
        assert_eq!(
            guest.infos.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "{result:?}"
        );
        assert!(guest.inner.state.lock().unwrap().writes.is_empty());
        worker.shutdown().await.unwrap();
    }
}

#[derive(Default)]
struct Maintenance {
    count: AtomicUsize,
    changed: tokio::sync::Notify,
}
impl preparation::VmProviderLaunchResolver for Maintenance {
    fn resolve(
        &self,
        _: preparation::VmProviderLaunchRequest,
    ) -> preparation::VmProviderLaunchFuture<'_> {
        panic!("reattach cannot resolve a launch");
    }
    fn refresh_claude_credentials(
        &self,
        actors: Vec<preparation::VmProviderLaunchRequest>,
    ) -> preparation::VmProviderRefreshFuture<'_> {
        assert_eq!(actors.len(), 1);
        assert_eq!(actors[0].account_key, "default");
        assert_eq!(actors[0].provider, "claude");
        self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.changed.notify_one();
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn vm_existing_reattach_resumes_credential_maintenance_only_after_normal_interval() {
    let (state, guest) = existing_actor(false).await;
    let maintenance = Arc::new(Maintenance::default());
    let mut resources = test_resources().unwrap();
    resources.guest_rpc = Some(guest.clone());
    resources.vm_provider_launch = Some(maintenance.clone());
    let peer = Peer::new();
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    worker_barrier(&worker).await;
    assert_eq!(
        maintenance.count.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    let refreshed = maintenance.changed.notified();
    tokio::time::pause();
    tokio::time::advance(super::super::VM_CREDENTIAL_REFRESH_INTERVAL).await;
    tokio::task::yield_now().await;
    tokio::time::resume();
    tokio::time::timeout(std::time::Duration::from_secs(10), refreshed)
        .await
        .unwrap();
    assert_eq!(
        maintenance.count.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    let native = guest.inner.state.lock().unwrap();
    assert_eq!(native.starts, 1);
    assert!(native.writes.is_empty());
    drop(native);
    worker.shutdown().await.unwrap();
}

struct AuthRecoveryCredentials {
    store: Arc<RuntimeStore>,
    guest: Arc<ExistingGuest>,
    attempts: AtomicUsize,
    refuse: bool,
}

impl preparation::VmProviderLaunchResolver for AuthRecoveryCredentials {
    fn resolve(
        &self,
        _: preparation::VmProviderLaunchRequest,
    ) -> preparation::VmProviderLaunchFuture<'_> {
        panic!("an existing Ready VM must not resolve or start another process");
    }

    fn refresh_claude_credentials(
        &self,
        actors: Vec<preparation::VmProviderLaunchRequest>,
    ) -> preparation::VmProviderRefreshFuture<'_> {
        Box::pin(async move {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.guest.inner.changed.notify_one();
            assert_eq!(actors.len(), 1);
            let actor = &actors[0];
            assert_eq!(actor.agent_id, FIXTURE_AGENT_ID);
            assert_eq!(actor.epoch, FIXTURE_AGENT_EPOCH);
            assert_eq!(actor.account_key, "default");
            assert_eq!(actor.provider, "claude");
            assert_eq!(actor.generation, 1);
            assert_eq!(actor.cwd, "/home/studio/project");
            if self.refuse {
                return Err(WorkerError::PreparationPending(
                    "test/auth-recovery-copy-refused".into(),
                ));
            }
            crate::linux_vm_provider::copy_guest_credentials(
                self.store.clone(),
                self.guest.clone(),
                crate::linux_vm_provider::GuestCredentialTarget {
                    agent_id: &actor.agent_id,
                    actor_epoch: actor.epoch,
                    provider: "claude",
                    account_key: &actor.account_key,
                    relative_path: &VmClaudeRefreshResolver::credential_path(&actor.account_key),
                },
                br#"{"claudeAiOauth":{"accessToken":"fake-rotated-auth-recovery"}}"#,
            )
            .await
            .map_err(|_| WorkerError::PreparationPending("test/auth-recovery-copy".into()))?;
            Ok(())
        })
    }
}

#[tokio::test]
async fn vm_existing_auth_recovery_copies_rotated_credentials_before_native_submit() {
    auth_recovery_turn(false).await;
}

#[tokio::test]
async fn vm_existing_auth_recovery_copy_failure_keeps_original_input_pending() {
    auth_recovery_turn(true).await;
}

async fn auth_recovery_turn(refuse: bool) {
    let (state, guest) = existing_actor(false).await;
    let resolver = Arc::new(AuthRecoveryCredentials {
        store: state.bridge.store.clone(),
        guest: guest.clone(),
        attempts: AtomicUsize::new(0),
        refuse,
    });
    let mut resources = claude_preparation::claude_resources(&state);
    resources.guest_rpc = Some(guest.clone());
    resources.vm_provider_launch = Some(resolver.clone());
    let mut peer = Peer::new();
    peer.wire = studio_runtime_providers::ProviderKind::Claude.wire();
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    worker_barrier(&worker).await;
    assert_eq!(
        resolver.attempts.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(
        guest
            .inner
            .state
            .lock()
            .unwrap()
            .credential_documents
            .is_empty()
    );
    assert!(guest.inner.state.lock().unwrap().writes.is_empty());

    // The real terminal and continuation owners create the auth retry marker.
    {
        let mut native = guest.inner.state.lock().unwrap();
        VmTurnGuest::enqueue_frame(
            &mut native,
            &format!("linux-worker:{}", state.agent),
            json!({"method":"turn/completed","params":{"threadId":"fake-thread-0",
                "turn":{"id":"vm-turn","status":"failed","error":{"message":"OAuth session expired"}}}}),
        );
        guest.inner.changed.notify_one();
    }
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if state.record().await["usageResume"]["cause"] == "auth" {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the failed native turn records its auth continuation");
    worker_barrier(&worker).await;
    let failed = state.record().await;
    assert_eq!(failed["authResumeAttempt"], 1);
    assert_eq!(failed["usageResume"]["status"], "scheduled");
    assert_eq!(failed["nativeFailureHold"], true);
    assert_eq!(failed["inFlight"], false);
    assert_eq!(state.event_status("input").await, "delivered");
    state.enqueue("auth-recovery-original").await;
    assert_eq!(
        state.event_status("auth-recovery-original").await,
        "pending"
    );

    let actor = state.agent.clone();
    let retry_at = failed["usageResume"]["dueAt"].as_f64().unwrap() + 1.0;
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let limits = crate::usage_resume::LimitsObservation::from_record(
                        json!({"accountKey":"default","at":retry_at,"error":null,"data":{}}),
                        Some("fake-rotated-auth-marker".into()),
                    )
                    .unwrap();
                    let staged = crate::continuation_runtime::stage_tick(
                        tx,
                        &actor,
                        retry_at,
                        Some(&limits),
                    )
                    .unwrap();
                    assert!(staged.changed());
                    Ok::<_, WorkerError>(())
                })
                .await
        })
        .await
        .unwrap();
    let resumed = state.record().await;
    assert_eq!(resumed["usageResume"]["cause"], "auth");
    assert_eq!(resumed["usageResume"]["status"], "started");
    assert_eq!(resumed["authResumeAttempt"], 1);
    assert_eq!(resumed["epoch"], state.epoch);
    assert_eq!(resumed["accountKey"], "default");
    assert_eq!(resumed["threadId"], "fake-thread-0");
    assert_ne!(resumed["nativeFailureHold"], true);
    assert_eq!(
        state.event_status("auth-recovery-original").await,
        "pending"
    );
    assert_eq!(state.event_status("input").await, "delivered");
    let writes_before = guest.inner.state.lock().unwrap().writes.len();
    assert_eq!(writes_before, 0);
    state.wake(&worker).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let changed = guest.inner.changed.notified();
            if resolver.attempts.load(std::sync::atomic::Ordering::SeqCst) > 0
                || guest
                    .inner
                    .state
                    .lock()
                    .unwrap()
                    .writes
                    .iter()
                    .any(|write| write["method"] == "turn/start")
            {
                break;
            }
            changed.await;
        }
    })
    .await
    .expect("the auth retry reaches either its credential gate or the native submit boundary");
    worker_barrier(&worker).await;
    assert_eq!(
        resolver.attempts.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the existing Ready session must refresh before an auth-recovery native submit"
    );
    if refuse {
        assert_eq!(
            guest.inner.state.lock().unwrap().writes.len(),
            writes_before
        );
        assert_eq!(
            state.event_status("auth-recovery-original").await,
            "pending"
        );
        assert!(
            guest
                .inner
                .state
                .lock()
                .unwrap()
                .credential_documents
                .is_empty()
        );
    } else {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while state.event_status("auth-recovery-original").await != "delivered" {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the exact original pending input is delivered after the copy");
        assert_eq!(
            *guest.credential_copies_at_turn_start.lock().unwrap(),
            vec![1]
        );
        assert_eq!(
            guest.inner.state.lock().unwrap().credential_documents.len(),
            1
        );
    }
    assert_eq!(state.event_status("input").await, "delivered");
    assert_eq!(guest.inner.state.lock().unwrap().starts, 1);
    assert!(guest.inner.state.lock().unwrap().stops.is_empty());
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn vm_existing_reattach_on_demand_read_reuses_the_existing_actor_connection() {
    use crate::account_worker::retained::{RequestActor, RequestResult, SessionRequest};
    let (state, guest) = existing_actor(false).await;
    *guest.info_override.lock().unwrap() = Some(json!({"initResult":null}));
    let (_peer, mut worker) = existing_worker(&state, guest.clone()).await;
    worker_barrier(&worker).await;
    assert_eq!(guest.infos.load(std::sync::atomic::Ordering::SeqCst), 1);
    *guest.info_override.lock().unwrap() = None;
    let port = worker.retained_session();
    let request = SessionRequest {
        scope: port.scope().clone(),
        expected_scope: None,
        operation: "existing-vm-read".into(),
        actor: Some(RequestActor {
            agent: state.agent.clone(),
            epoch: state.epoch,
        }),
        transfer: None,
        deleted_actor_session: None,
        method: Method::ThreadRead,
        params: json!({"threadId":"fake-thread-0","includeTurns":false}),
        deadline: (tokio::time::Instant::now() + std::time::Duration::from_secs(10)).into_std(),
    };
    let receipt = port.request(request.clone()).await.unwrap();
    assert!(matches!(receipt.result, RequestResult::Reply(_)));
    let repeated = port.request(request).await.unwrap();
    assert!(repeated.replayed);
    let native = guest.inner.state.lock().unwrap();
    assert_eq!(native.starts, 1);
    assert_eq!(native.writes.len(), 1);
    assert_eq!(native.writes[0]["method"], "thread/read");
    assert!(native.stops.is_empty());
    drop(native);
    worker.shutdown().await.unwrap();
}

struct NoResolver(Arc<AtomicUsize>);
impl preparation::VmProviderLaunchResolver for NoResolver {
    fn resolve(
        &self,
        _: preparation::VmProviderLaunchRequest,
    ) -> preparation::VmProviderLaunchFuture<'_> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async {
            Err(WorkerError::PreparationRefused(
                "unexpected credential/launch resolution".into(),
            ))
        })
    }
}

async fn existing_actor(paused: bool) -> (State, Arc<ExistingGuest>) {
    let state = State::new().await;
    let actor = state.agent.clone();
    let epoch = state.epoch;
    state.bridge.run(move|store|async move{
        store.transaction_with(move|tx|{
            let mut record=AgentRepository::get(tx,&AgentId::from(actor.as_str()))?.unwrap();
            record.status=if paused{studio_runtime_store::AgentStatus::Paused}else{studio_runtime_store::AgentStatus::Running};
            for(key,value)in[
                ("executionMode",json!("vm")),("workspaceMode",json!("layr")),("layrReady",json!(true)),
                ("provider",json!("claude")),("accountKey",json!("default")),("executionSettingsAccountKey",json!("default")),
                ("cwd",json!("/home/studio/project")),("threadId",json!("fake-thread-0")),("turnId",json!("vm-turn")),
                ("inFlight",json!(true)),("autoWake",json!(!paused)),
                ("startAttempt",json!({"id":"existing-vm-attempt","epoch":epoch,"accountKey":"default","submitted":true,"events":["annotation:input"],"threadId":"fake-thread-0","turnId":"vm-turn","connectionGeneration":1,"connectionId":"previous-backend","requestTicket":"original-native-start"})),
            ]{record.extra.insert(key.into(),value);}
            AgentRepository::put(tx,&record)?;
            studio_runtime_store::ExecutionHistoryRepository::record(tx,&record)?;
            tx.execute("UPDATE runtime_events SET status='delivered',turn_id='vm-turn' WHERE agent=?1",[&actor])?;
            let members:i64 = tx.query_row(
                "SELECT COUNT(*) FROM runtime_events WHERE agent=?1 AND epoch=?2 \
                 AND id IN (SELECT value FROM json_each(?3)) \
                 AND status='delivered' AND turn_id='vm-turn'",
                rusqlite::params![actor,epoch,record.extra["startAttempt"]["events"].to_string()],
                |row|row.get(0),
            )?;
            assert_eq!(members,1,"the saved attempt owns its exact delivered input and turn");
            studio_runtime_store::ExecutionHistoryRepository::save_claim(tx,"existing-vm-attempt",&json!([]),&json!({"agent":record}))?;
            Ok::<_,WorkerError>(())
        }).await
    }).await.unwrap();
    let guest = Arc::new(ExistingGuest::default());
    crate::linux_vm_provider::GuestProviderSession::open(
        state.bridge.store.clone(),
        guest.clone(),
        crate::linux_vm_provider::GuestProviderIdentity {
            agent_id: state.agent.clone(),
            epoch: state.epoch,
            generation: 1,
            account_key: "default".into(),
            provider: "claude".into(),
        },
        "profile:default",
        ProviderLaunchSpec::new(
            vec!["synthetic-vm-provider".into()],
            BTreeMap::new(),
            PathBuf::from("/home/studio/project"),
        ),
    )
    .await
    .unwrap();
    state
        .bridge
        .recovered_accounts
        .lock()
        .unwrap()
        .insert("default".into());
    (state, guest)
}

#[tokio::test]
async fn vm_existing_reattach_restores_the_journal_without_launch_or_input_replay() {
    for paused in [false, true] {
        let (state, guest) = existing_actor(paused).await;
        state
            .bridge
            .run(|store| async move {
                store
                    .read_async("vm_reattach_plan", |connection| {
                        let mut plan = connection.prepare(&format!(
                            "EXPLAIN QUERY PLAN {}",
                            super::super::vm_reattach::STARTUP_CANDIDATES
                        ))?;
                        let rows = plan.query_map(
                            rusqlite::params![
                                "default",
                                0,
                                super::super::vm_reattach::STARTUP_PAGE_ACTORS
                            ],
                            |row| row.get::<_, String>(3),
                        )?;
                        let details = rows.collect::<Result<Vec<_>, _>>()?.join(" ");
                        assert!(
                            details.contains("SEARCH runtime_agents")
                                && details.contains("account_key=?"),
                            "{details}"
                        );
                        Ok::<_, StoreError>(())
                    })
                    .await?;
                Ok(())
            })
            .await
            .unwrap();
        let resolver_calls = Arc::new(AtomicUsize::new(0));
        let mut resources = test_resources().unwrap();
        resources.guest_rpc = Some(guest.clone());
        resources.vm_provider_launch = Some(Arc::new(NoResolver(resolver_calls.clone())));
        let peer = Peer::new();
        let mut worker = peer
            .open_worker_with_resources(state.bridge.clone(), resources)
            .await;
        worker_barrier(&worker).await;
        assert!(
            guest.infos.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "startup must inspect the saved VM process even without another turn start"
        );
        let before = state.record().await;
        assert_eq!(before["epoch"], state.epoch);
        assert_eq!(before["status"], if paused { "paused" } else { "running" });
        {
            let mut native = guest.inner.state.lock().unwrap();
            let handle = format!("linux-worker:{}", state.agent);
            VmTurnGuest::enqueue_frame(
                &mut native,
                &handle,
                json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-0","turnId":"vm-turn","itemId":"restored-answer","delta":"retained answer"}}),
            );
            VmTurnGuest::enqueue_frame(
                &mut native,
                &handle,
                json!({"method":"turn/completed","params":{"threadId":"fake-thread-0","turn":{"id":"vm-turn","status":"completed"}}}),
            );
        }
        let settled = async {
            loop {
                let changed = guest.inner.changed.notified();
                if guest.inner.state.lock().unwrap().acknowledged >= 2 {
                    break;
                }
                changed.await;
            }
        };
        tokio::select! {
            result=worker.completion()=>panic!("reattached worker ended: {result:?}"),
            result=tokio::time::timeout(std::time::Duration::from_secs(10),settled)=>result.expect("retained journal commits and ACKs"),
        }
        worker_barrier(&worker).await;
        let record = state.record().await;
        assert_eq!(record["epoch"], state.epoch);
        assert_eq!(record["threadId"], "fake-thread-0");
        assert_eq!(
            record["inFlight"], false,
            "the native terminal reaches the actor lifecycle"
        );
        if paused {
            assert_eq!(record["status"], "paused");
            assert_eq!(record["autoWake"], false);
        }
        assert_eq!(state.event_status("input").await, "delivered");
        assert_eq!(resolver_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let native = guest.inner.state.lock().unwrap();
        assert_eq!(
            native.starts, 1,
            "only the fixture's original process was started"
        );
        assert!(
            native.writes.is_empty(),
            "reattach must not send initialize, initialized, turn/start, or other provider messages"
        );
        assert!(native.stops.is_empty());
        drop(native);
        worker.shutdown().await.unwrap();
    }
}
