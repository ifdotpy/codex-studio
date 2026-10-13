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
    let selected = account_registry::targets(&fixture.store).unwrap();
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
