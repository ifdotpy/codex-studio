//! Busy VM input waits for the next turn and reuses verified attachment copies.
use super::*;
use crate::linux_vm_guest::{GuestError, GuestRpc};

struct Guest {
    provider: VmTurnGuest,
    files: Arc<crate::vm_attachments::tests::Guest>,
    turns: std::sync::atomic::AtomicUsize,
}
impl GuestRpc for Guest {
    fn stream(
        &self,
        method: &str,
        params: &Value,
        id: Option<&str>,
        timeout: std::time::Duration,
        on_frame: &mut dyn FnMut(Value) -> Result<(), GuestError>,
    ) -> Result<Value, GuestError> {
        if matches!(method, "agent.context" | "layr.exec" | "layr.file.stat") {
            return self.files.call(method, params, id, timeout);
        }
        if method == "provider.rpc"
            && params["action"] == "write"
            && params["message"]["method"] == "turn/start"
        {
            // Distinct accepted native turns. The test supplies each terminal
            // notice only after checking its busy queue boundary.
            let sequence = self.turns.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            let turn = if sequence == 1 {
                "vm-turn".to_owned()
            } else {
                format!("vm-turn-{sequence}")
            };
            let message = &params["message"];
            let handle = params["handle"].as_str().expect("handle");
            let mut state = self.provider.state.lock().expect("provider");
            state.writes.push(message.clone());
            state.turn_start_handles.push(handle.into());
            VmTurnGuest::enqueue_frame(
                &mut state,
                handle,
                json!({"id":message["id"],"result":{"turn":{"id":turn,"status":"inProgress"}}}),
            );
            self.provider.changed.notify_one();
            return Ok(json!({"accepted":true,"remoteId":message["id"]}));
        }
        self.provider.stream(method, params, id, timeout, on_frame)
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

async fn attach(state: &State, id: &'static str, assets: Vec<String>) {
    state.bridge.run(move |store| async move {
        store.transaction_with(move |tx| {
            tx.execute("UPDATE runtime_events SET kind='user' WHERE id=?1",[id])?;
            tx.execute("INSERT INTO runtime_event_meta(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
                rusqlite::params![id,json!({"assets":assets}).to_string()])?;
            Ok::<_,StoreError>(())
        }).await?;
        Ok(())
    }).await.expect("event assets");
}

async fn delivered(state: &State, id: &str, guest: &Guest) {
    let result = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while state.event_status(id).await != "delivered" {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    if result.is_err() {
        let record = state.record().await;
        panic!(
            "event {id} is not delivered: status={}, error={}, guest failures={:?}, methods={:?}",
            record["status"],
            record["error"],
            guest.files.failures(),
            guest
                .provider
                .state
                .lock()
                .expect("provider")
                .writes
                .iter()
                .map(|value| value["method"].clone())
                .collect::<Vec<_>>()
        );
    }
}

fn terminal(guest: &Guest, turn: &str) {
    let mut state = guest.provider.state.lock().expect("provider");
    let owner = state
        .turn_start_handles
        .last()
        .expect("actual native handle")
        .clone();
    VmTurnGuest::enqueue_frame(
        &mut state,
        &owner,
        json!({"method":"turn/completed","params":{
        "threadId":"fake-thread-0","turn":{"id":turn,"status":"completed"}}}),
    );
    guest.provider.changed.notify_one();
}

async fn start_and_next_turn(provider: &'static str) {
    use base64::Engine as _;
    let state = State::new().await;
    let home = tempfile::tempdir().expect("separate synthetic guest home");
    let home_text = home.path().to_string_lossy().into_owned();
    let image = b"\x89PNG\r\n\x1a\nsynthetic-image".to_vec();
    let text = b"original attachment bytes".to_vec();
    let agent = state.agent.clone();
    let image_input = base64::engine::general_purpose::STANDARD.encode(&image);
    let text_input = base64::engine::general_purpose::STANDARD.encode(&text);
    let home_record = home_text.clone();
    let assets = state
        .bridge
        .run(move |store| async move {
            let first =
                crate::payload_store::upload(&store, &agent, "image.png", None, &image_input)
                    .await
                    .map_err(|_| StoreError::RecordContract("image upload"))?;
            let second =
                crate::payload_store::upload(&store, &agent, "notes.txt", None, &text_input)
                    .await
                    .map_err(|_| StoreError::RecordContract("text upload"))?;
            store
                .transaction(move |tx| {
                    let mut record =
                        AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.expect("actor");
                    for (key, value) in [
                        ("executionMode", json!("vm")),
                        ("workspaceMode", json!("layr")),
                        ("layrReady", json!(true)),
                        ("layrHome", json!(home_record)),
                        ("layrProjectId", json!("project")),
                        ("layrLine", json!("main")),
                        ("provider", json!(provider)),
                        ("cwd", json!("/guest/line")),
                        ("threadId", Value::Null),
                    ] {
                        record.extra.insert(key.into(), value);
                    }
                    AgentRepository::put(tx, &record)
                })
                .await?;
            Ok(vec![
                first["id"].as_str().expect("id").to_owned(),
                second["id"].as_str().expect("id").to_owned(),
            ])
        })
        .await
        .expect("VM actor and uploads");
    attach(&state, "annotation:input", assets.clone()).await;
    let files =
        crate::vm_attachments::tests::Guest::new(json!({"agentId":state.agent,"home":home_text,
        "projectId":"project","line":"main","cwd":"/guest/line"}));
    let guest = Arc::new(Guest {
        provider: VmTurnGuest::default(),
        files,
        turns: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut resources = test_resources().expect("resources");
    resources.guest_rpc = Some(guest.clone());
    resources.vm_provider_launch = Some(Arc::new(Resolver));
    let peer = Peer::new();
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    let mut ready = peer.ready.clone();
    while !*ready.borrow_and_update() {
        ready.changed().await.expect("ready");
    }
    state.wake(&worker).await;
    delivered(&state, "input", &guest).await;
    let agent = state.agent.clone();
    let epoch = state.epoch;
    state.bridge.run(move |store| async move {
        store.transaction_with(move |tx| {
            tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES('annotation:next',?1,'user','next','pending',?2,?3)",
                rusqlite::params![agent,now(),epoch])?;
            tx.execute("INSERT INTO runtime_event_meta(id,record) VALUES('annotation:next',?1)",[json!({"assets":assets}).to_string()])?;
            Ok::<_,StoreError>(())
        }).await?;
        Ok(())
    }).await.expect("atomic busy input and attachment admission");
    state.wake(&worker).await;
    worker_barrier(&worker).await;
    assert_eq!(
        state.event_status("next").await,
        "pending",
        "VM input does not steer an active turn"
    );
    assert_eq!(guest.turns.load(std::sync::atomic::Ordering::SeqCst), 1);
    terminal(&guest, "vm-turn");
    delivered(&state, "next", &guest).await;
    let writes = guest
        .provider
        .state
        .lock()
        .expect("provider")
        .writes
        .clone();
    let starts: Vec<_> = writes
        .iter()
        .filter(|write| write["method"] == "turn/start")
        .collect();
    let steers: Vec<_> = writes
        .iter()
        .filter(|write| write["method"] == "turn/steer")
        .collect();
    assert_eq!(starts.len(), 2, "two original inputs, no historical replay");
    assert!(steers.is_empty(), "VM steering remains disabled");
    for (write, event) in [
        (starts[0], "annotation:input"),
        (starts[1], "annotation:next"),
    ] {
        assert_eq!(write["params"]["clientUserMessageId"], event);
        let inputs = write["params"]["input"].as_array().expect("input");
        let path = inputs
            .iter()
            .find(|input| input["type"] == "localImage")
            .expect("image")["path"]
            .as_str()
            .expect("image path");
        assert!(path.starts_with(home.path().to_str().expect("home")));
        assert!(!path.starts_with(state.root.to_str().expect("host root")));
        assert_eq!(std::fs::read(path).expect("guest image"), image);
        let description = inputs
            .iter()
            .filter_map(|input| input["text"].as_str())
            .find(|text| text.contains("Attached file:"))
            .expect("file description");
        let path = description
            .split("Read the file at ")
            .nth(1)
            .expect("file path")
            .split(" (")
            .next()
            .expect("file path");
        assert_eq!(std::fs::read(path).expect("guest text file"), text);
        assert!(!description.contains(state.root.to_str().expect("host root")));
    }
    assert_eq!(
        guest.files.write_count(),
        4,
        "one chunk and finalization per asset, both turns share copies"
    );
    let record = state.record().await;
    assert_eq!(record["epoch"], state.epoch);
    assert_eq!(record["turnId"], "vm-turn-2");
    assert_eq!(record["threadId"], "fake-thread-0");
    terminal(&guest, "vm-turn-2");
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while state.record().await["inFlight"] == true {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("terminal notice");
    assert_eq!(state.record().await["status"], "completed");
    worker.shutdown().await.expect("worker detaches");
    assert!(
        guest
            .provider
            .state
            .lock()
            .expect("provider")
            .stops
            .is_empty()
    );
}

#[tokio::test]
async fn vm_attachment_busy_input_waits_then_reuses_guest_copies_codex() {
    start_and_next_turn("codex").await;
}

#[tokio::test]
async fn vm_attachment_busy_input_waits_then_reuses_guest_copies_claude() {
    start_and_next_turn("claude").await;
}
