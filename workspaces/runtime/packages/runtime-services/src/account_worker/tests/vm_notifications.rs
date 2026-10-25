//! Real VM journal notifications keep account authority separate from cursor scope.
use super::*;
use crate::linux_vm_guest::{GuestError, GuestRpc};

#[derive(Default)]
struct NotificationGuest(VmTurnGuest, std::sync::Mutex<()>);

impl GuestRpc for NotificationGuest {
    fn stream(
        &self,
        method: &str,
        params: &Value,
        id: Option<&str>,
        timeout: std::time::Duration,
        on_frame: &mut dyn FnMut(Value) -> Result<(), GuestError>,
    ) -> Result<Value, GuestError> {
        // Publish the complete scripted batch before another guest lane can read it.
        let _exchange = self.1.lock().expect("scripted guest exchange");
        let result = self.0.stream(method, params, id, timeout, on_frame)?;
        if method == "provider.rpc"
            && params["action"] == "write"
            && params["message"]["method"] == "turn/start"
        {
            let handle = params["handle"].as_str().expect("guest handle");
            let thread = VmTurnGuest::thread_id(&params["handle"]);
            let mut state = self.0.state.lock().expect("guest state");
            let terminal = state.events.pop_back().expect("queued native terminal");
            state.sequence -= 1;
            for payload in [
                json!({"method":"item/agentMessage/delta","params":{
                    "threadId":thread,"turnId":"vm-turn","itemId":"answer:thinking","delta":""}}),
                json!({"method":"item/agentMessage/delta","params":{
                    "threadId":thread,"turnId":"vm-turn","itemId":"answer","delta":"VM answer"}}),
                json!({"method":"item/completed","params":{
                    "threadId":thread,"turnId":"vm-turn","item":{"id":"answer","type":"agentMessage",
                        "text":"VM answer","phase":"final_answer"}}}),
                serde_json::from_str(terminal["payload"].as_str().expect("terminal payload"))
                    .expect("terminal JSON"),
            ] {
                VmTurnGuest::enqueue_frame(&mut state, handle, payload);
            }
            self.0.changed.notify_one();
        }
        Ok(result)
    }
}

struct Resolver;
impl preparation::VmProviderLaunchResolver for Resolver {
    fn resolve(
        &self,
        request: preparation::VmProviderLaunchRequest,
    ) -> preparation::VmProviderLaunchFuture<'_> {
        Box::pin(async move {
            Ok(preparation::VmProviderLaunch {
                provider: request.provider,
                profile: format!("profile:{}", request.account_key),
                launch: ProviderLaunchSpec::new(
                    vec!["synthetic-vm-provider".into()],
                    BTreeMap::new(),
                    PathBuf::from(request.cwd),
                ),
                bootstrap: None,
            })
        })
    }
}

async fn fixture(provider: &'static str) -> (State, String) {
    let mut state = State::new().await;
    state.bridge.disk_gate = Some(Arc::new(crate::disk_space::DiskGate::with_sampler(
        state.root.clone(),
        Arc::new(|_| Some(crate::disk_space::WORKER_STORAGE_MIN_FREE_BYTES)),
    )));
    let primary = state.agent.clone();
    let sibling = format!("{primary}-sibling");
    let sibling_id = sibling.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let mut record = AgentRepository::get(tx, &AgentId::from(primary.as_str()))?
                        .expect("primary actor");
                    for (key, value) in [
                        ("executionMode", json!("vm")),
                        ("workspaceMode", json!("layr")),
                        ("layrReady", json!(true)),
                        ("provider", json!(provider)),
                        ("cwd", json!("/home/studio/project")),
                        ("threadId", Value::Null),
                    ] {
                        record.extra.insert(key.into(), value);
                    }
                    AgentRepository::put(tx, &record)?;
                    record.id = AgentId::from(sibling_id.as_str());
                    record.root_id = record.id.clone();
                    record.extra.remove("startAttempt");
                    record.extra.remove("preparationWake");
                    AgentRepository::put(tx, &record)
                })
                .await?;
            Ok(())
        })
        .await
        .expect("VM actors");
    (state, sibling)
}

async fn wait_for_ack(guest: &NotificationGuest, worker: &mut AccountHandle, handle: &str) {
    let progress = async {
        loop {
            let changed = guest.0.changed.notified();
            if guest
                .0
                .state
                .lock()
                .expect("guest state")
                .acknowledged_by_handle
                .iter()
                .any(|(owner, cursor)| owner == handle && *cursor >= 6)
            {
                break;
            }
            changed.await;
        }
    };
    tokio::select! {
        result = worker.completion() => panic!("VM notification ended the worker: {result:?}"),
        result = tokio::time::timeout(std::time::Duration::from_secs(10), progress) => {
            result.expect("VM notifications commit and acknowledge");
        }
    }
}

#[tokio::test]
async fn vm_notifications_commit_real_account_streams_and_preserve_scoped_acks() {
    for provider in ["codex", "claude"] {
        let (state, sibling) = fixture(provider).await;
        let guest = Arc::new(NotificationGuest::default());
        let mut resources = test_resources().expect("resources");
        resources.guest_rpc = Some(guest.clone());
        resources.vm_provider_launch = Some(Arc::new(Resolver));
        let peer = Peer::new();
        let mut worker = peer
            .open_worker_with_resources(state.bridge.clone(), resources)
            .await;
        let mut ready = peer.ready.clone();
        while !*ready.borrow_and_update() {
            ready.changed().await.expect("worker ready");
        }
        state.wake(&worker).await;
        wait_for_ack(
            &guest,
            &mut worker,
            &format!("linux-worker:{}", state.agent),
        )
        .await;
        assert_eq!(state.event_status("input").await, "delivered");
        state.enqueue_for(&sibling, "sibling-input").await;
        worker
            .send(AccountCommand::Wake {
                agent_id: sibling.clone(),
                epoch: state.epoch,
            })
            .await
            .expect("wake sibling");
        wait_for_ack(&guest, &mut worker, &format!("linux-worker:{sibling}")).await;
        assert_eq!(state.event_status("sibling-input").await, "delivered");
        let primary = state.agent.clone();
        state.bridge.run(move |store| async move {
            store.read_async("test/vm-notification-transcript", move |tx| {
                for actor in [&primary, &sibling] {
                    let text: String = tx.query_row("SELECT json_extract(record,'$.text') FROM runtime_items WHERE id=?1",
                        [format!("{actor}:answer")], |row| row.get(0))?;
                    assert_eq!(text, "VM answer");
                    let cursor: String = tx.query_row("SELECT result FROM runtime_operation_receipts WHERE id=?1",
                        [format!("account-worker:cursor:default:vm:{actor}:1:ack")], |row| row.get(0))?;
                    assert_eq!(cursor, "6");
                }
                Ok::<_, StoreError>(())
            }).await?;
            Ok(())
        }).await.expect("exact transcript and independent cursor proofs");
        worker.shutdown().await.expect("healthy worker detaches");
        let native = guest.0.state.lock().expect("guest state");
        assert_eq!(native.starts, 2);
        assert_eq!(
            native
                .writes
                .iter()
                .filter(|message| message["method"] == "turn/start")
                .count(),
            2
        );
        assert!(native.stops.is_empty());
    }
}

fn delta(actor: &str, text: &str) -> Frame {
    let payload = json!({"method":"item/agentMessage/delta","params":{
        "threadId":format!("thread-{actor}"),"turnId":"turn","itemId":"answer","delta":text}});
    match studio_runtime_providers::decode_journal_value(0, 0, &payload).payload {
        studio_runtime_providers::DecodedJournalPayload::Frame(frame) => frame,
        other => panic!("invalid notification: {other:?}"),
    }
}

fn running(tx: &rusqlite::Transaction<'_>, actor: &str, native: bool) -> Result<(), StoreError> {
    let mut record = AgentRepository::get(tx, &AgentId::from(actor))?.expect("actor");
    record.status = studio_runtime_store::AgentStatus::Running;
    record
        .extra
        .insert("threadId".into(), json!(format!("thread-{actor}")));
    record.extra.insert("turnId".into(), json!("turn"));
    record.extra.insert("inFlight".into(), json!(true));
    if native {
        record.extra.insert("executionMode".into(), json!("native"));
    }
    AgentRepository::put(tx, &record)
}

#[tokio::test]
async fn vm_notification_scope_keeps_native_and_sibling_receipts_and_observations_distinct() {
    use crate::notification_dispatch::{
        NotificationStreamBuffer, ProviderDeliveryIdentity, PublicationContext,
        dispatch_notification_with_effects,
    };
    let (state, sibling) = fixture("codex").await;
    let primary = state.agent.clone();
    let epoch = state.epoch;
    state.bridge.run(move |store| async move {
        store.transaction_with(move |tx| {
            let mut native = AgentRepository::get(tx, &AgentId::from(primary.as_str()))?.expect("actor");
            native.id = AgentId::from("native-actor");
            native.root_id = native.id.clone();
            AgentRepository::put(tx, &native)?;
            let authority = ProviderDeliveryIdentity { provider_handle:"account:default".into(), generation:1, cursor:7 };
            for (actor, native) in [(&primary[..], false), (&sibling[..], false), ("native-actor", true)] {
                running(tx, actor, native)?;
                let context = if native { PublicationContext::default() } else {
                    PublicationContext::default().with_vm_journal("default", actor, epoch,
                        &format!("default:vm:{actor}")).expect("exact VM scope")
                };
                let expected_handle = if native { "account:default".into() } else { format!("account:default:vm:{actor}") };
                let frame = delta(actor, "same cursor, independent source");
                let dispatched = dispatch_notification_with_effects(tx, actor, &context,
                    &NotificationStreamBuffer::default(), &authority, &frame, now(),
                    &crate::notification_runtime_effects::RuntimeNotificationEffects).expect("distinct source admission");
                let effects = serde_json::to_string(&dispatched.post_commit).expect("observations");
                assert!(effects.contains(&expected_handle), "observation lost its journal namespace: {effects}");
                let (metadata, _) = dispatched.stream_update.expect("stream update").persisted_text().expect("stream metadata");
                let metadata: Value = serde_json::from_str(&metadata).expect("metadata JSON");
                assert_eq!(metadata["entries"][0]["key"]["account"], "default");
                let hash = studio_runtime_store::sha256_hex(json!({"providerHandle":expected_handle,"generation":1,"cursor":7}).to_string().as_bytes());
                assert!(EventWriteRepository::receipt(tx, &format!("provider-notification:{hash}"))?.is_some());
                let replay = dispatch_notification_with_effects(tx, actor, &context,
                    &NotificationStreamBuffer::default(), &authority, &frame, now(),
                    &crate::notification_runtime_effects::RuntimeNotificationEffects).expect("same source replay");
                assert!(replay.post_commit.is_empty());
                assert!(replay.stream_update.is_none());
                assert!(dispatch_notification_with_effects(tx, actor, &context,
                    &NotificationStreamBuffer::default(), &authority, &delta(actor, "changed payload"), now(),
                    &crate::notification_runtime_effects::RuntimeNotificationEffects).is_err());
            }
            let count: i64 = tx.query_row("SELECT count(*) FROM runtime_operation_receipts WHERE id GLOB 'provider-notification:*'", [], |row| row.get(0))?;
            assert_eq!(count, 3, "native and both VM cursors must not alias");
            Ok::<_, StoreError>(())
        }).await?;
        Ok(())
    }).await.expect("scoped identity boundary");
}

#[tokio::test]
async fn vm_notification_scope_refuses_wrong_actor_account_epoch_and_host_namespace() {
    use crate::notification_dispatch::{
        NotificationStreamBuffer, ProviderDeliveryIdentity, PublicationContext,
        dispatch_notification,
    };
    let (state, sibling) = fixture("codex").await;
    let primary = state.agent.clone();
    let epoch = state.epoch;
    state.bridge.run(move |store| async move {
        store.transaction_with(move |tx| {
            running(tx, &primary, false)?;
            running(tx, &sibling, false)?;
            let authority = ProviderDeliveryIdentity { provider_handle:"account:default".into(), generation:1, cursor:7 };
            for (account, actor, selected_epoch) in [("default", sibling.as_str(), epoch),
                ("another-account", primary.as_str(), epoch), ("default", primary.as_str(), epoch+1)] {
                let context = PublicationContext::default().with_vm_journal(account, actor, selected_epoch,
                    &format!("{account}:vm:{actor}")).expect("syntactically exact scope");
                assert!(dispatch_notification(tx, &primary, &context, &NotificationStreamBuffer::default(),
                    &authority, &delta(&primary, "must not apply"), now()).is_err());
            }
            for wrong_namespace in ["default".to_owned(), format!("default:vm:{sibling}")] {
                assert!(PublicationContext::default().with_vm_journal("default", &primary, epoch,
                    &wrong_namespace).is_err());
            }
            assert!(dispatch_notification(tx, &primary, &PublicationContext::default(),
                &NotificationStreamBuffer::default(), &authority, &delta(&primary, "host scope cannot serve VM"), now()).is_err());
            let count: i64 = tx.query_row("SELECT count(*) FROM runtime_operation_receipts WHERE id GLOB 'provider-notification:*' OR id GLOB 'account-worker:cursor:*'", [], |row| row.get(0))?;
            assert_eq!(count, 0, "a refused scope cannot settle or advance a cursor");
            Ok::<_, StoreError>(())
        }).await?;
        Ok(())
    }).await.expect("scope refusal boundary");
}
