#![cfg(unix)]
#![allow(clippy::unwrap_used)]
#[path = "../src/account_registry.rs"]
mod account_registry;
#[path = "../../../packages/runtime-services/tests/support/prepared_python.rs"]
mod prepared_python;

use account_registry::{AccountLaunch, AccountLauncher, AccountRegistry, LaunchFuture};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use studio_runtime_process::{
    EventCursor, Generation, OperationId, ProviderHandle, ProviderLaunchSpec, provider_seam::*,
};
use studio_runtime_providers::{Frame, RequestId, ResponseBody};
use studio_runtime_services::{
    account_worker::{WorkerError, preparation::PreparationResources},
    event_write::{AnnotationWrite, annotate},
    model_catalog::ModelCatalogService,
    provider_session_task::SessionTaskConfig,
};
use studio_runtime_store::{RuntimeStore, StoreError};
use tokio::sync::{Notify, mpsc, watch};

struct Fixture {
    _root: tempfile::TempDir,
    store: Arc<RuntimeStore>,
    agents: Value,
}
impl Fixture {
    fn new() -> Self {
        Self::build("dispatch")
    }
    fn build(mode: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../server/src");
        let result = std::process::Command::new(prepared_python::prepared_python(&source))
            .arg("-B")
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wave7_p01_fixture.py"))
            .arg(root.path())
            .arg(mode)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let agents = serde_json::from_slice(&result.stdout).unwrap();
        let store = Arc::new(RuntimeStore::open(Some(root.path())).unwrap());
        Self {
            _root: root,
            store,
            agents,
        }
    }
    fn enqueue(&self, index: usize, id: &str) {
        self.store
            .transaction_with(|tx| {
                annotate(
                    tx,
                    AnnotationWrite {
                        agent_id: self.agents[index]["id"].as_str().unwrap().into(),
                        id: Some(id.into()),
                        path: "README.md".into(),
                        line: 1,
                        text: id.into(),
                        turn_id: None,
                        signature: id.into(),
                    },
                )
                .map_err(|e| StoreError::SchemaMismatch(e.to_string()))
            })
            .unwrap();
    }
}

struct Scripted {
    opened: Arc<Mutex<Vec<String>>>,
    active: Arc<AtomicUsize>,
    writes: mpsc::UnboundedSender<(String, Value)>,
    failures: AtomicUsize,
    stops: Arc<Mutex<Vec<Shutdown>>>,
    opened_notice: Arc<Notify>,
}
impl AccountLauncher for Scripted {
    fn launch(&self, account: String) -> LaunchFuture<'_> {
        Box::pin(async move {
            if self
                .failures
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_sub(1))
                .is_ok()
            {
                return Err(WorkerError::TaskFailed);
            }
            let factory = Arc::new(WireFactory {
                opened: self.opened.clone(),
                active: self.active.clone(),
                writes: self.writes.clone(),
                stops: self.stops.clone(),
                opened_notice: self.opened_notice.clone(),
            });
            Ok(AccountLaunch {
                factory,
                options: OpenOptions {
                    clock: SessionClock::default(),
                    launch: ProviderLaunchSpec::new(
                        vec!["scripted".into()],
                        BTreeMap::new(),
                        "/var/tmp".into(),
                    ),
                    expected_generation: Some(Generation(7)),
                },
                config: SessionTaskConfig {
                    initialize_params: json!({}),
                    generation: Generation(7),
                    claude: false,
                    timeout_ms: 60_000,
                    initialize_operation: OperationId(format!("init:{account}")),
                    request_id_prefix: None,
                    recovered: vec![],
                },
                resources: PreparationResources::bundled(
                    String::new(),
                    ModelCatalogService::new(None),
                    PathBuf::from("/isolated/python"),
                )
                .unwrap(),
            })
        })
    }
}
struct WireFactory {
    stops: Arc<Mutex<Vec<Shutdown>>>,
    opened_notice: Arc<Notify>,
    opened: Arc<Mutex<Vec<String>>>,
    active: Arc<AtomicUsize>,
    writes: mpsc::UnboundedSender<(String, Value)>,
}
impl ProviderTaskFactory for WireFactory {
    fn open(&self, handle: ProviderHandle, _: OpenOptions, shutdown: Shutdown) -> OpenFuture<'_> {
        self.opened.lock().unwrap().push(handle.as_str().into());
        self.stops.lock().unwrap().push(shutdown.clone());
        self.opened_notice.notify_one();
        self.active.fetch_add(1, Ordering::SeqCst);
        let active = self.active.clone();
        let writes = self.writes.clone();
        Box::pin(async move {
            let (commands, mut inbox) = mpsc::channel::<AdapterCommand>(16);
            let (events, journal) = mpsc::channel(16);
            let (status, observed) = watch::channel(AttachmentStatus {
                handle: handle.clone(),
                open_attempted: true,
                info: Some(AttachmentInfo {
                    handle: handle.clone(),
                    generation: Generation(7),
                    pid: 1,
                    acknowledged: EventCursor(0),
                    resumed: false,
                }),
                initialization: InitializationStatus::AwaitingReply,
            });
            let mut stop = shutdown.subscribe();
            let task = tokio::spawn(async move {
                let _status = status;
                let mut sequence = 0;
                loop {
                    let command = tokio::select! { biased; _=Shutdown::cancelled(&mut stop)=>break, c=inbox.recv()=>match c { Some(c)=>c,None=>break } };
                    match command {
                        AdapterCommand::Write {
                            write,
                            reply,
                            observed,
                        } => {
                            let receipt = match write.message()["method"].as_str() {
                                Some("initialize") => CommandReceipt::CachedInitialize(json!({})),
                                Some("initialized") => CommandReceipt::Written {
                                    remote_id: None,
                                    duplicate: false,
                                },
                                Some("thread/resume") => {
                                    sequence += 1;
                                    events.send(JournalEntry {
                                        received_at_ms: 0,
                                        cursor: EventCursor(sequence),
                                        generation: Generation(7),
                                        payload: JournalPayload::Frame(Frame::Response {
                                            id: serde_json::from_value::<RequestId>(write.message()["id"].clone()).unwrap(),
                                            body: ResponseBody::Result(json!({"thread":{"id":write.message()["params"]["threadId"]}})),
                                            fields: Default::default(),
                                        }),
                                    }).await.unwrap();
                                    CommandReceipt::Written {
                                        remote_id: Some(sequence),
                                        duplicate: false,
                                    }
                                }
                                _ => {
                                    writes
                                        .send((handle.as_str().into(), write.message().clone()))
                                        .unwrap();
                                    // The peer intentionally loses the acceptance response.
                                    // Restart may reattach, but must never resend this input.
                                    CommandReceipt::Unknown
                                }
                            };
                            let _ignored = reply.send(receipt);
                            tokio::select! { biased; _=Shutdown::cancelled(&mut stop)=>break, _=observed=>{} }
                        }
                        AdapterCommand::Ack { cursor, reply } => {
                            let _ignored = reply.send(CommandReceipt::Acknowledged(cursor));
                        }
                    }
                }
                active.fetch_sub(1, Ordering::SeqCst);
                TaskExit {
                    result: Ok(()),
                    cleanup_error: None,
                    recovery_needed: None,
                }
            });
            Ok(ProviderAttachment::from_task(
                commands, journal, observed, shutdown, task,
            ))
        })
    }
}
fn scripted() -> (Arc<Scripted>, mpsc::UnboundedReceiver<(String, Value)>) {
    let (writes, rx) = mpsc::unbounded_channel();
    (
        Arc::new(Scripted {
            opened: Arc::default(),
            active: Arc::default(),
            writes,
            failures: AtomicUsize::new(0),
            stops: Arc::default(),
            opened_notice: Arc::default(),
        }),
        rx,
    )
}

#[tokio::test]
async fn committed_input_reaches_one_retained_session_and_unknown_restart_does_not_resend() {
    let fixture = tokio::task::spawn_blocking(Fixture::new).await.unwrap();
    fixture.enqueue(0, "committed-before-start");
    let (launcher, mut writes) = scripted();
    let mut registry = AccountRegistry::new(fixture.store.clone(), launcher.clone())
        .await
        .unwrap();
    registry.reconcile(None).await.unwrap();
    let (account, write) = tokio::time::timeout(std::time::Duration::from_secs(10), writes.recv())
        .await
        .expect("provider write hang guard")
        .unwrap();
    assert_eq!(account, "account:default");
    assert_eq!(write["method"], "turn/start");
    registry.reconcile(None).await.unwrap();
    assert_eq!(launcher.opened.lock().unwrap().len(), 1);
    registry.shutdown().await.unwrap();
    assert_eq!(launcher.active.load(Ordering::SeqCst), 0);
    let mut restarted = AccountRegistry::new(fixture.store.clone(), launcher.clone())
        .await
        .unwrap();
    restarted.reconcile(None).await.unwrap();
    restarted.shutdown().await.unwrap();
    assert!(
        writes.try_recv().is_err(),
        "uncertain request must not be resubmitted"
    );
}

#[tokio::test]
async fn two_accounts_are_independent_and_shutdown_joins_all_attachments() {
    let fixture = tokio::task::spawn_blocking(Fixture::new).await.unwrap();
    fixture.enqueue(0, "first");
    fixture.enqueue(1, "second");
    let (launcher, mut writes) = scripted();
    let mut registry = AccountRegistry::new(fixture.store.clone(), launcher.clone())
        .await
        .unwrap();
    registry.reconcile(None).await.unwrap();
    let mut accounts = vec![
        tokio::time::timeout(std::time::Duration::from_secs(10), writes.recv())
            .await
            .expect("provider write hang guard")
            .unwrap()
            .0,
        tokio::time::timeout(std::time::Duration::from_secs(10), writes.recv())
            .await
            .expect("provider write hang guard")
            .unwrap()
            .0,
    ];
    accounts.sort();
    assert_eq!(accounts, ["account:default", "account:second"]);
    registry.shutdown().await.unwrap();
    assert_eq!(launcher.active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn start_failure_preserves_input_and_post_commit_notify_dispatches_it() {
    let fixture = tokio::task::spawn_blocking(Fixture::new).await.unwrap();
    fixture.enqueue(0, "retry-start");
    let (launcher, mut writes) = scripted();
    launcher.failures.store(1, Ordering::SeqCst);
    let mut registry = AccountRegistry::new(fixture.store.clone(), launcher.clone())
        .await
        .unwrap();
    assert!(registry.reconcile(None).await.is_err());
    let wake = Arc::new(Notify::new());
    let stop = Shutdown::default();
    wake.notify_one();
    let task = tokio::spawn(registry.run(wake, stop.clone()));
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(10), writes.recv())
            .await
            .expect("provider write hang guard")
            .unwrap()
            .0,
        "account:default"
    );
    stop.cancel();
    task.await.unwrap().unwrap();
    assert_eq!(launcher.active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn accepted_http_message_wakes_retained_worker_and_receipt_replay_does_not_send_again() {
    use axum::{
        body::{Body, to_bytes},
        extract::ConnectInfo,
        http::Request,
    };
    use tower::ServiceExt;
    let fixture = tokio::task::spawn_blocking(Fixture::new).await.unwrap();
    let state = studio_runtime_http::HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        None,
        fixture._root.path().to_string_lossy().into(),
        49171,
        None,
        Some(fixture.store.clone()),
        None,
    );
    let wake = state.event_wake_handle();
    let app = studio_runtime_http::router(state.clone()).unwrap();
    let (launcher, mut writes) = scripted();
    let registry = AccountRegistry::new(fixture.store.clone(), launcher.clone())
        .await
        .unwrap();
    let stop = Shutdown::default();
    let task = tokio::spawn(registry.run(wake, stop.clone()));
    let body = json!({"room":fixture.agents[0]["id"],"id":"b2ba3b37-4ef4-467d-b215-155c09c088e6","text":"committed through HTTP"});
    for _ in 0..2 {
        let request = Request::builder()
            .method("POST")
            .uri("/api/messages")
            .header("host", "127.0.0.1:49171")
            .header("content-type", "application/json")
            .header("x-canvas-token", "fixture-token")
            .header(
                studio_runtime_http::API_SCHEMA_HASH_HEADER,
                studio_runtime_http::schema::api_schema_hash(),
            )
            .extension(ConnectInfo(
                "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
            ))
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    }
    let (account, request) =
        tokio::time::timeout(std::time::Duration::from_secs(10), writes.recv())
            .await
            .expect("provider write hang guard")
            .unwrap();
    assert_eq!(account, "account:default");
    assert_eq!(request["method"], "turn/start");
    stop.cancel();
    task.await.unwrap().unwrap();
    assert_eq!(launcher.opened.lock().unwrap().len(), 1);
    assert!(writes.try_recv().is_err());
    assert_eq!(launcher.active.load(Ordering::SeqCst), 0);
    state.shutdown_services().await.unwrap();
}

#[tokio::test]
async fn shutdown_between_worker_start_and_wake_handoff_leaves_committed_input_recoverable() {
    let fixture = tokio::task::spawn_blocking(Fixture::new).await.unwrap();
    fixture.enqueue(0, "before-wake-handoff");
    let (launcher, mut writes) = scripted();
    let mut registry = AccountRegistry::new(fixture.store.clone(), launcher.clone())
        .await
        .unwrap();
    registry.start("default").await.unwrap();
    registry.shutdown().await.unwrap();
    assert!(writes.try_recv().is_err());
    assert_eq!(launcher.active.load(Ordering::SeqCst), 0);
    let mut restarted = AccountRegistry::new(fixture.store.clone(), launcher.clone())
        .await
        .unwrap();
    restarted.reconcile(None).await.unwrap();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(10), writes.recv())
            .await
            .expect("provider write hang guard")
            .unwrap()
            .0,
        "account:default"
    );
    restarted.shutdown().await.unwrap();
    assert!(writes.try_recv().is_err());
}

#[path = "../src/host_runtime.rs"]
#[allow(dead_code)]
mod host_runtime;
#[path = "../src/runtime_shutdown.rs"]
mod runtime_shutdown;

#[cfg(target_os = "linux")]
#[tokio::test]
async fn runtime_shutdown_reaps_backend_owned_catalog_child() {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncBufReadExt, BufReader};
    let fixture = tokio::task::spawn_blocking(Fixture::new).await.unwrap();
    let socket = fixture._root.path().join("probe.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let executable = fixture._root.path().join("probe");
    std::fs::write(&executable, format!(
        "#!/usr/bin/python3\nimport os,socket,sys\ns=socket.socket(socket.AF_UNIX)\ns.connect({})\ns.sendall((str(os.getpid())+'\\n').encode())\nfor line in sys.stdin: pass\n",
        serde_json::to_string(&socket.to_string_lossy()).unwrap(),
    )).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let state = studio_runtime_http::HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        Some(executable.to_string_lossy().into_owned()),
        fixture._root.path().to_string_lossy().into_owned(),
        49171,
        None,
        Some(fixture.store.clone()),
        None,
    );
    let (launcher, _writes) = scripted();
    let runtime = host_runtime::HostRuntime::with_launcher(
        fixture.store.clone(),
        launcher,
        state.event_wake_handle(),
    )
    .await
    .unwrap();
    let _pending = state
        .model_catalog_handle()
        .read("default", "", "codex", false)
        .await;
    let (peer, _) = tokio::time::timeout(std::time::Duration::from_secs(10), listener.accept())
        .await
        .expect("catalog start hang guard")
        .unwrap();
    let mut peer = BufReader::new(peer);
    let mut pid = String::new();
    peer.read_line(&mut pid).await.unwrap();
    let pid: u32 = pid.trim().parse().unwrap();
    runtime_shutdown::close(runtime, &state).await.unwrap();
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "backend-owned probe must be reaped before shutdown returns"
    );
}

#[test]
fn recovery_selection_matches_python_scheduler_roster_without_changing_stored_records() {
    let fixture = Fixture::build("recovery");
    let selected = account_registry::target_page(&fixture.store, "", &BTreeMap::new()).unwrap();
    assert_eq!(
        json!(selected.keys().collect::<Vec<_>>()),
        fixture.agents["selected"]
    );
    let db = fixture.store.read().unwrap();
    let mut query = db.prepare("SELECT id,record FROM runtime_agents").unwrap();
    let stored = query
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .map(|row| {
            let (id, raw) = row.unwrap();
            (id, serde_json::from_str::<Value>(&raw).unwrap())
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(json!(stored), fixture.agents["stored"]);
    assert_eq!(fixture.agents["cases"], 9);
}

#[tokio::test]
async fn connection_loss_replaces_worker_before_delivering_new_input() {
    let fixture = tokio::task::spawn_blocking(Fixture::new).await.unwrap();
    fixture.store.transaction(|tx| {
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.accountKey','default','$.executionSettingsAccountKey','default') WHERE id=?1", [fixture.agents[1]["id"].as_str().unwrap()])?;
        Ok(())
    }).unwrap();
    fixture.enqueue(0, "unknown-before-disconnect");
    let (launcher, mut writes) = scripted();
    let registry = AccountRegistry::new(fixture.store.clone(), launcher.clone())
        .await
        .unwrap();
    let wake = Arc::new(Notify::new());
    let stop = Shutdown::default();
    let task = tokio::spawn(registry.run(wake.clone(), stop.clone()));
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), writes.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.1["method"], "turn/start");
    launcher.stops.lock().unwrap()[0].cancel();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if launcher.opened.lock().unwrap().len() == 2 {
                break;
            }
            launcher.opened_notice.notified().await;
        }
    })
    .await
    .expect("replacement worker hang guard");
    fixture.enqueue(1, "new-input-after-disconnect");
    wake.notify_one();
    let second = tokio::time::timeout(std::time::Duration::from_secs(10), writes.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.0, "account:default");
    assert_eq!(second.1["method"], "turn/start");
    assert_ne!(
        first.1["params"]["threadId"],
        second.1["params"]["threadId"]
    );
    stop.cancel();
    task.await.unwrap().unwrap();
    assert_eq!(launcher.opened.lock().unwrap().len(), 2);
    assert_eq!(launcher.active.load(Ordering::SeqCst), 0);
    assert!(
        writes.try_recv().is_err(),
        "unknown input must not be resent on replacement"
    );
}

fn seed_backlog(fixture: &Fixture, count: usize, each_account: bool) -> Vec<String> {
    let source = fixture.agents[0]["id"].as_str().unwrap();
    fixture.store.transaction(|tx| {
        let raw: String = tx.query_row("SELECT record FROM runtime_agents WHERE id=?1", [source], |r| r.get(0))?;
        let mut ids = Vec::new();
        for i in 0..count {
            let id = format!("00000000-0000-4000-8000-{i:012}");
            let account = if each_account {format!("page-{i:06}")} else {"default".into()};
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES(?1,json_set(?2,'$.id',?1,'$.accountKey',?3,'$.contextRepairWait','blocked'))", (id.as_str(), raw.as_str(), account.as_str()))?;
            tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES(?1,?1,'annotation','fixture','pending',0,json_extract(?2,'$.epoch'))", (id.as_str(), raw.as_str()))?;
            ids.push(id);
        }
        Ok(ids)
    }).unwrap()
}

#[test]
fn pending_and_account_backlogs_are_bounded_pages_not_total_limits() {
    let fixture = Fixture::new();
    let expected = seed_backlog(&fixture, 4100, false);
    let mut after = BTreeMap::new();
    let mut actual = Vec::new();
    while actual.len() < expected.len() {
        let page = account_registry::target_page(&fixture.store, "", &after).unwrap();
        let rows = &page["default"];
        assert!(rows.len() <= 32);
        actual.extend(rows.iter().map(|(id, _)| id.clone()));
        after.insert("default".into(), rows.last().unwrap().0.clone());
    }
    assert_eq!(actual, expected);
    let wrapped_agents = account_registry::target_page(
        &fixture.store,
        "",
        &BTreeMap::from([("default".into(), actual.last().unwrap().clone())]),
    )
    .unwrap();
    assert_eq!(
        wrapped_agents["default"]
            .iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        actual.iter().take(32).collect::<Vec<_>>()
    );
    let fixture = Fixture::new();
    seed_backlog(&fixture, 130, true);
    let first = account_registry::target_page(&fixture.store, "", &BTreeMap::new()).unwrap();
    assert_eq!(first.len(), 128);
    let second = account_registry::target_page(
        &fixture.store,
        first.last_key_value().unwrap().0,
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(second.len(), 2);
    assert!(second.keys().all(|account| !first.contains_key(account)));
    let wrapped_accounts = account_registry::target_page(
        &fixture.store,
        second.last_key_value().unwrap().0,
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(wrapped_accounts.len(), 128);
    assert_eq!(
        wrapped_accounts.first_key_value().unwrap().0,
        first.first_key_value().unwrap().0
    );

    let fixture = Fixture::new();
    let source = fixture.agents[0]["id"].as_str().unwrap();
    fixture.store.transaction(|tx| {
        let raw: String = tx.query_row("SELECT record FROM runtime_agents WHERE id=?1", [source], |row| row.get(0))?;
        tx.execute("INSERT INTO runtime_agents(id,record) VALUES('bad-account-key',json_set(?1,'$.id','bad-account-key','$.accountKey',7))", [raw])?;
        tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES('bad-account-key-event','bad-account-key','annotation','fixture','pending',0,json_extract((SELECT record FROM runtime_agents WHERE id='bad-account-key'),'$.epoch'))", [])?;
        Ok(())
    }).unwrap();
    assert!(matches!(
        account_registry::target_page(&fixture.store, "", &BTreeMap::new()),
        Err(WorkerError::Store(StoreError::InvalidStoredObject { table: "runtime_agents", id })) if id == "bad-account-key"
    ));
}

#[tokio::test]
async fn saturated_wake_lane_does_not_starve_eligible_agent_after_blocked_prefix() {
    let fixture = tokio::task::spawn_blocking(Fixture::new).await.unwrap();
    seed_backlog(&fixture, 48, false);
    fixture.enqueue(0, "eligible-after-blocked-prefix");
    let (launcher, mut writes) = scripted();
    let registry = AccountRegistry::new(fixture.store.clone(), launcher.clone())
        .await
        .unwrap();
    let stop = Shutdown::default();
    let task = tokio::spawn(registry.run(Arc::new(Notify::new()), stop.clone()));
    let (_, request) = tokio::time::timeout(std::time::Duration::from_secs(10), writes.recv())
        .await
        .unwrap_or_else(|_| {
            let db = fixture.store.read().unwrap();
            let state: String = db.query_row("SELECT json_object('status',json_extract(record,'$.status'),'error',json_extract(record,'$.error'),'hold',json_extract(record,'$.startOutcomeHold'),'start',json_extract(record,'$.startAttempt')) FROM runtime_agents WHERE id=?1",[fixture.agents[0]["id"].as_str().unwrap()],|r|r.get(0)).unwrap();
            panic!("fair wake hang guard: {state}")
        })
        .unwrap();
    assert_eq!(request["method"], "turn/start");
    stop.cancel();
    task.await.unwrap().unwrap();
    assert_eq!(launcher.opened.lock().unwrap().len(), 1);
    let db = fixture.store.read().unwrap();
    let blocked: i64 = db.query_row("SELECT count(*) FROM runtime_events WHERE kind='annotation' AND text='fixture' AND status='pending'",[],|r|r.get(0)).unwrap();
    assert_eq!(blocked, 48);
}

fn python_launch(
    root: &std::path::Path,
    environment: &BTreeMap<String, String>,
    home: &std::path::Path,
    isolated: bool,
    executable: &str,
) -> Value {
    use std::io::Write;
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../server/src");
    let mut child = std::process::Command::new(prepared_python::prepared_python(&source))
        .arg("-B")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wave7_p01_launch_oracle.py"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(json!({"root":root,"environment":environment,"home":home,"isolated":isolated,"executable":executable}).to_string().as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
async fn wire_read(peer: &mut tokio::io::BufReader<tokio::net::UnixStream>) -> Value {
    use tokio::io::AsyncBufReadExt;
    let mut line = String::new();
    peer.read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}
async fn wire_send(peer: &mut tokio::io::BufReader<tokio::net::UnixStream>, value: Value) {
    use tokio::io::AsyncWriteExt;
    peer.get_mut()
        .write_all(format!("{value}\n").as_bytes())
        .await
        .unwrap();
}

async fn provider_input_barrier(root: &std::path::Path) -> Value {
    use tokio::io::AsyncWriteExt;
    let mut peer = tokio::io::BufReader::new(
        tokio::net::UnixStream::connect(root.join("supervisor.sock"))
            .await
            .unwrap(),
    );
    wire_send(
        &mut peer,
        json!({"protocol":1,"stateDir":root,"backendId":"python-cutover-fixture"}),
    )
    .await;
    assert!(wire_read(&mut peer).await["error"].is_null());
    wire_send(
        &mut peer,
        json!({
            "requestId":1,
            "action":"write",
            "handle":"account:default",
            "operationId":"fixture:provider-input-barrier",
            "nativeId":9001,
            "message":{"id":9001,"method":"fixture/barrier","params":{}}
        }),
    )
    .await;
    let result = wire_read(&mut peer).await;
    assert!(
        result["error"].is_null(),
        "barrier request must reach provider"
    );
    peer.get_mut().shutdown().await.unwrap();
    result
}

#[tokio::test]
async fn every_launch_signature_field_matches_real_python_appserver() {
    for isolated in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("codex");
        std::fs::create_dir(&home).unwrap();
        let environment = BTreeMap::from([
            ("HOME".into(), root.path().to_string_lossy().into_owned()),
            ("CODEX_HOME".into(), home.to_string_lossy().into_owned()),
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("CODEX_AGENTS_SUPERVISOR_MODE".into(), "1".into()),
            ("CODEX_AGENTS_BACKEND_ID".into(), "backend-only".into()),
            ("CODEX_NODE".into(), "backend-only-node".into()),
            ("OPENAI_API_KEY".into(), "fixture-openai-key".into()),
            ("CODEX_API_KEY".into(), "fixture-codex-key".into()),
            ("UNCHANGED".into(), "Unicode café".into()),
        ]);
        let oracle = python_launch(root.path(), &environment, &home, isolated, "/fixture/codex");
        let launch = host_runtime::codex_launch(
            Some("/fixture/codex"),
            environment,
            home.to_string_lossy().into_owned(),
            isolated,
            root.path().into(),
        );
        let listener = tokio::net::UnixListener::bind(root.path().join("supervisor.sock")).unwrap();
        let factory = studio_runtime_process::provider_task::SupervisorProviderTaskFactory::new(
            studio_runtime_process::BackendIdentity {
                backend_id: studio_runtime_process::BackendId("fixture".into()),
                state_dir: root.path().into(),
            },
        );
        let preparing = tokio::spawn(async move {
            factory
                .prepare(
                    ProviderHandle::for_account("default"),
                    OpenOptions {
                        launch,
                        clock: SessionClock::default(),
                        expected_generation: None,
                    },
                    Shutdown::default(),
                )
                .await
                .unwrap()
        });
        let (peer, _) = listener.accept().await.unwrap();
        let mut peer = tokio::io::BufReader::new(peer);
        let _hello = wire_read(&mut peer).await;
        wire_send(&mut peer, json!({"ok":true})).await;
        let status = wire_read(&mut peer).await;
        wire_send(
            &mut peer,
            json!({"requestId":status["requestId"],"error":"Unknown supervisor handle"}),
        )
        .await;
        let open = wire_read(&mut peer).await;
        for field in ["command", "env", "cwd"] {
            assert_eq!(open[field], oracle[field], "{field}");
        }
        let command: Vec<String> = serde_json::from_value(open["command"].clone()).unwrap();
        let environment: BTreeMap<String, String> =
            serde_json::from_value(open["env"].clone()).unwrap();
        assert_eq!(
            studio_runtime_store::supervisor_journal::launch_signature(
                &command,
                &environment,
                open["cwd"].as_str()
            ),
            oracle["signature"]
        );
        wire_send(&mut peer,json!({"requestId":open["requestId"],"result":{"resumed":false,"initResult":null,"acknowledged":0,"sequence":0,"generation":1,"returnCode":null,"pid":1}})).await;
        drop(preparing.await.unwrap());
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn supervised_child_observes_explicit_and_inherited_working_directories() {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncBufReadExt;
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("provider");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wave7_p01_native_fixture.py"),
        &executable,
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let stderr = root.path().join("supervisor.log");
    let child = std::process::Command::new("python3")
        .arg("-B")
        .arg(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../server/src/codex_process_supervisor.py"),
        )
        .arg("--state")
        .arg(root.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&stderr).unwrap())
        .spawn()
        .unwrap();
    let identity = studio_runtime_process::BackendIdentity {
        backend_id: studio_runtime_process::BackendId("cwd-fixture".into()),
        state_dir: root.path().into(),
    };
    let mut supervisor = SupervisorFixture {
        child,
        native: Vec::new(),
    };
    supervisor_ready::wait(&mut supervisor.child, identity.clone(), &stderr);
    let factory =
        studio_runtime_process::provider_task::SupervisorProviderTaskFactory::new(identity);
    let explicit = root.path().join("explicit-cwd");
    std::fs::create_dir(&explicit).unwrap();
    for (name, working_dir) in [("explicit", Some(explicit.clone())), ("inherited", None)] {
        let observation_socket = root.path().join(format!("{name}.sock"));
        let observation_listener = tokio::net::UnixListener::bind(&observation_socket).unwrap();
        let mut environment: BTreeMap<String, String> = std::env::vars().collect();
        environment.insert(
            "CODEX_HOME".into(),
            root.path()
                .join("codex-home")
                .to_string_lossy()
                .into_owned(),
        );
        environment.insert(
            "FIXTURE_CWD_SOCKET".into(),
            observation_socket.to_string_lossy().into_owned(),
        );
        let launch = match working_dir {
            Some(ref path) => ProviderLaunchSpec::new(
                vec![
                    executable.to_string_lossy().into_owned(),
                    "app-server".into(),
                    "--listen".into(),
                    "stdio://".into(),
                ],
                environment,
                path.clone(),
            ),
            None => ProviderLaunchSpec::new(
                vec![
                    executable.to_string_lossy().into_owned(),
                    "app-server".into(),
                    "--listen".into(),
                    "stdio://".into(),
                ],
                environment,
                root.path().into(),
            )
            .with_inherited_cwd(),
        };
        let prepared = factory
            .prepare(
                ProviderHandle::for_account(name),
                OpenOptions {
                    launch,
                    clock: SessionClock::default(),
                    expected_generation: None,
                },
                Shutdown::default(),
            )
            .await
            .unwrap();
        let (stream, _) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            observation_listener.accept(),
        )
        .await
        .expect("child cwd observation hang guard")
        .unwrap();
        let mut observed = tokio::io::BufReader::new(stream);
        let mut line = String::new();
        observed.read_line(&mut line).await.unwrap();
        let observed: Value = serde_json::from_str(&line).unwrap();
        let expected = working_dir
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap());
        let expected = expected.to_string_lossy();
        assert_eq!(observed["cwd"], expected.as_ref());
        supervisor
            .native
            .push(observed["pid"].as_u64().unwrap() as u32);
        drop(prepared);
    }
}

fn account_driver(
    root: &std::path::Path,
) -> (
    Arc<studio_runtime_services::account_driver::AccountDriver>,
    PathBuf,
) {
    let home = root.join("account-home");
    std::fs::create_dir_all(&home).unwrap();
    let registry = root.join("accounts/registry.json");
    std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
    std::fs::write(&registry,json!({"version":1,"defaultAccountKey":"default","accounts":{"default":{"id":"default","home":home,"label":"Fixture","source":"Codex CLI","status":"signedOut"}},"logins":{},"deleteReceipts":{}}).to_string()).unwrap();
    (
        Arc::new(studio_runtime_services::account_driver::AccountDriver::open(registry).unwrap()),
        home,
    )
}

#[test]
fn launch_admission_refreshes_restored_and_changed_credentials() {
    let root = tempfile::tempdir().unwrap();
    let (driver, home) = account_driver(root.path());
    assert!(host_runtime::account_home(&driver, "default").is_err());
    std::fs::write(
        home.join("auth.json"),
        json!({"OPENAI_API_KEY":"fixture-key-one"}).to_string(),
    )
    .unwrap();
    assert_eq!(
        host_runtime::account_home(&driver, "default").unwrap().0,
        home.to_string_lossy()
    );
    std::fs::write(
        home.join("auth.json"),
        json!({"OPENAI_API_KEY":"fixture-key-two"}).to_string(),
    )
    .unwrap();
    assert!(host_runtime::account_home(&driver, "default").is_err());
    assert_eq!(driver.snapshot().unwrap().accounts[0]["status"], "changed");
}

#[test]
fn claude_launch_refusal_precedes_auth_refresh_and_registry_write() {
    use std::sync::atomic::AtomicUsize;
    struct CountingReader(Arc<AtomicUsize>);
    impl studio_runtime_services::account_driver::ProviderAuthReader for CountingReader {
        fn read(
            &self,
            _profile: &studio_runtime_services::account_driver::AuthProfile,
        ) -> studio_runtime_services::account_driver::AuthMetadata {
            self.0.fetch_add(1, Ordering::SeqCst);
            studio_runtime_services::account_driver::AuthMetadata {
                status: studio_runtime_services::account_driver::AuthStatus::Ready,
                account_id: Some("would-have-refreshed".into()),
                email: None,
                plan: None,
                credential_identity: Some("would-have-refreshed".into()),
                error: None,
                error_kind: None,
            }
        }
    }

    let root = tempfile::tempdir().unwrap();
    let (_, home) = account_driver(root.path());
    let registry = root.path().join("accounts/registry.json");
    let mut value: Value = serde_json::from_slice(&std::fs::read(&registry).unwrap()).unwrap();
    value["accounts"]["default"]["provider"] = json!("claude");
    value["accounts"]["default"]["status"] = json!("signedOut");
    value["accounts"]["default"]["home"] = json!(home);
    std::fs::write(&registry, serde_json::to_vec(&value).unwrap()).unwrap();
    let before = std::fs::read(&registry).unwrap();
    let reads = Arc::new(AtomicUsize::new(0));
    let driver = studio_runtime_services::account_driver::AccountDriver::open_with_reader(
        &registry,
        CountingReader(reads.clone()),
    )
    .unwrap();
    let revision = driver.revision().unwrap();

    assert!(matches!(
        host_runtime::account_home(&driver, "default"),
        Err(WorkerError::PreparationRefused(reason))
            if reason == "provider-session/claude-runtime-launch"
    ));
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    assert_eq!(driver.revision().unwrap(), revision);
    assert_eq!(std::fs::read(registry).unwrap(), before);
}

#[path = "../../../packages/runtime-process/tests/support/supervisor_ready.rs"]
mod supervisor_ready;

struct SupervisorFixture {
    child: std::process::Child,
    native: Vec<u32>,
}
impl Drop for SupervisorFixture {
    fn drop(&mut self) {
        for pid in &self.native {
            let _result = std::process::Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
        let _result = self.child.kill();
        let _result = self.child.wait();
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn host_start_reattaches_python_launch_and_delivers_http_input() {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tower::ServiceExt;
    // Isolate the supervisor-mode environment without mutating process globals.
    if std::env::var_os("STUDIO_W7_P01_HOST_CHILD").is_none() {
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "host_start_reattaches_python_launch_and_delivers_http_input",
                "--nocapture",
            ])
            .env("STUDIO_W7_P01_HOST_CHILD", "1")
            .env("STUDIO_W7_P01_COMPLETE_TURN", "1")
            .env("CODEX_AGENTS_SUPERVISOR_MODE", "1")
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let fixture = tokio::task::spawn_blocking(Fixture::new).await.unwrap();
    let (accounts, home) = account_driver(fixture._root.path());
    std::fs::write(
        home.join("auth.json"),
        json!({"OPENAI_API_KEY":"fixture-host-key"}).to_string(),
    )
    .unwrap();
    let executable = fixture._root.path().join("provider");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wave7_p01_native_fixture.py"),
        &executable,
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let environment: BTreeMap<String, String> = std::env::vars().collect();
    let oracle = python_launch(
        fixture._root.path(),
        &environment,
        &home,
        false,
        executable.to_str().unwrap(),
    );
    let root = fixture._root.path().to_path_buf();
    let mut supervisor = tokio::task::spawn_blocking(move || {
        let stderr = root.join("supervisor.log");
        let child = std::process::Command::new("python3")
            .arg("-B")
            .arg(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../server/src/codex_process_supervisor.py"),
            )
            .arg("--state")
            .arg(&root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        let mut fixture = SupervisorFixture {
            child,
            native: Vec::new(),
        };
        supervisor_ready::wait(
            &mut fixture.child,
            studio_runtime_process::BackendIdentity {
                backend_id: studio_runtime_process::BackendId("python-cutover-fixture".into()),
                state_dir: root,
            },
            &stderr,
        );
        fixture
    })
    .await
    .unwrap();
    let mut peer = tokio::io::BufReader::new(
        tokio::net::UnixStream::connect(fixture._root.path().join("supervisor.sock"))
            .await
            .unwrap(),
    );
    wire_send(
        &mut peer,
        json!({"protocol":1,"stateDir":fixture._root.path(),"backendId":"python-cutover-fixture"}),
    )
    .await;
    assert!(wire_read(&mut peer).await["error"].is_null());
    wire_send(&mut peer,json!({"requestId":1,"action":"open","handle":"account:default","command":oracle["command"],"env":oracle["env"],"cwd":oracle["cwd"]})).await;
    let opened = wire_read(&mut peer).await;
    assert!(opened["error"].is_null(), "supervisor fixture open failed");
    let pid = opened["result"]["pid"].as_u64().unwrap() as u32;
    let generation = opened["result"]["generation"].as_u64().unwrap();
    supervisor.native.push(pid);
    peer.get_mut().shutdown().await.unwrap();
    let mut discarded = Vec::new();
    peer.read_to_end(&mut discarded).await.unwrap();
    drop(peer);
    let observer = tokio::net::UnixListener::bind(fixture._root.path().join("turn.sock")).unwrap();
    let mut state = studio_runtime_http::HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        Some(executable.to_string_lossy().into_owned()),
        fixture._root.path().to_string_lossy().into_owned(),
        49171,
        None,
        Some(fixture.store.clone()),
        None,
    )
    .with_account_driver(accounts.clone());
    let runtime = host_runtime::HostRuntime::start(
        &mut state,
        fixture.store.clone(),
        accounts.clone(),
        Some(executable.to_string_lossy().into_owned()),
    )
    .await
    .unwrap();
    let app = studio_runtime_http::router(state.clone()).unwrap();
    let body = json!({"room":fixture.agents[0]["id"],"id":"65f07ce1-42b9-42a3-b42e-8710a0c54617","text":"host composition cutover"});
    let initial_revision = state.sync_changes_handle().snapshot().revision;
    let mut changes = state.sync_changes_handle().subscribe();
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/messages")
        .header("host", "127.0.0.1:49171")
        .header("content-type", "application/json")
        .header("x-canvas-token", "fixture-token")
        .header(
            studio_runtime_http::API_SCHEMA_HASH_HEADER,
            studio_runtime_http::schema::api_schema_hash(),
        )
        .extension(axum::extract::ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    assert_eq!(app.clone().oneshot(request).await.unwrap().status(), 200);
    let (mut reported, _) =
        tokio::time::timeout(std::time::Duration::from_secs(15), observer.accept())
            .await
            .expect("host delivery hang guard")
            .unwrap();
    let mut bytes = Vec::new();
    reported.read_to_end(&mut bytes).await.unwrap();
    let request: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(request["pid"], pid);
    assert_eq!(request["count"], 1);
    assert_eq!(
        request["cwd"],
        std::env::current_dir().unwrap().to_string_lossy().as_ref()
    );
    assert_eq!(request["request"]["method"], "turn/start");
    assert_eq!(
        std::fs::read_to_string(fixture._root.path().join("startup-cwd")).unwrap(),
        std::env::current_dir().unwrap().to_string_lossy().as_ref()
    );

    let agent_id = fixture.agents[0]["id"].as_str().unwrap();
    let terminal_agent = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let row = fixture
                .store
                .read()
                .unwrap()
                .query_row(
                    "SELECT record FROM runtime_agents WHERE id=?1",
                    [agent_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap();
            let value: Value = serde_json::from_str(&row).unwrap();
            if value["status"] == "completed" && value["inFlight"] != true {
                break value;
            }
            changes
                .changed()
                .await
                .expect("store publication remains open");
        }
    })
    .await
    .expect("scripted provider terminal notification reaches the agent record");
    assert_eq!(terminal_agent["status"], "completed");
    assert!(state.sync_changes_handle().snapshot().revision > initial_revision);

    let request = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/api/transcript?id={agent_id}"))
        .header("host", "127.0.0.1:49171")
        .header("origin", "http://127.0.0.1:49171")
        .header("x-canvas-token", "fixture-token")
        .header(
            studio_runtime_http::API_SCHEMA_HASH_HEADER,
            studio_runtime_http::schema::api_schema_hash(),
        )
        .extension(axum::extract::ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let transcript_status = response.status();
    let transcript_body = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    assert_eq!(
        transcript_status,
        200,
        "{}",
        String::from_utf8_lossy(&transcript_body)
    );
    let transcript: Value = serde_json::from_slice(&transcript_body).unwrap();
    assert!(transcript.to_string().contains("host composition cutover"));
    assert!(
        transcript
            .to_string()
            .contains("fixture assistant response")
    );

    let request = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/api/transcript/page?id={agent_id}&limit=50"))
        .header("host", "127.0.0.1:49171")
        .header("origin", "http://127.0.0.1:49171")
        .header("x-canvas-token", "fixture-token")
        .header(
            studio_runtime_http::API_SCHEMA_HASH_HEADER,
            studio_runtime_http::schema::api_schema_hash(),
        )
        .extension(axum::extract::ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let page: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(page.to_string().contains("host composition cutover"));
    assert!(page.to_string().contains("fixture assistant response"));

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/messages")
        .header("host", "127.0.0.1:49171")
        .header("content-type", "application/json")
        .header("x-canvas-token", "fixture-token")
        .header(
            studio_runtime_http::API_SCHEMA_HASH_HEADER,
            studio_runtime_http::schema::api_schema_hash(),
        )
        .extension(axum::extract::ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    assert_eq!(app.clone().oneshot(request).await.unwrap().status(), 200);
    runtime_shutdown::close(runtime, &state).await.unwrap();
    assert!(std::path::Path::new(&format!("/proc/{pid}")).exists());
    let launch = host_runtime::codex_launch(
        Some(executable.to_str().unwrap()),
        environment,
        home.to_string_lossy().into_owned(),
        false,
        fixture._root.path().into(),
    );
    let factory = studio_runtime_process::provider_task::SupervisorProviderTaskFactory::new(
        studio_runtime_process::BackendIdentity {
            backend_id: studio_runtime_process::BackendId("verify-cutover".into()),
            state_dir: fixture._root.path().into(),
        },
    );
    let retained = factory
        .prepare(
            ProviderHandle::for_account("default"),
            OpenOptions {
                launch,
                clock: SessionClock::default(),
                expected_generation: None,
            },
            Shutdown::default(),
        )
        .await
        .unwrap();
    assert_eq!(retained.generation(), Generation(generation));
    drop(retained);
    // The provider consumes this barrier after all earlier stdin messages;
    // shutdown joined the Rust worker before the barrier was written.
    provider_input_barrier(fixture._root.path()).await;
    let (mut barrier, _) =
        tokio::time::timeout(std::time::Duration::from_secs(10), observer.accept())
            .await
            .expect("provider barrier hang guard")
            .unwrap();
    let mut bytes = Vec::new();
    barrier.read_to_end(&mut bytes).await.unwrap();
    let barrier: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(barrier["method"], "fixture/barrier");
    assert_eq!(
        barrier["count"], 1,
        "exactly one turn/start reached provider"
    );
    tokio::task::spawn_blocking(move || drop(supervisor))
        .await
        .unwrap();
}
