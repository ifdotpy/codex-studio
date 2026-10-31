#![cfg(unix)]
#![allow(clippy::unwrap_used)]
use studio_runtime_server::{account_registry, host_runtime};

use account_registry::{
    AccountLaunch, AccountLauncher, AccountRegistry, AccountRegistryConfig, AccountSessionBroker,
    LaunchFuture,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use studio_runtime_process::{
    EventCursor, Generation, OperationId, ProviderHandle, ProviderLaunchSpec, provider_seam::*,
};
use studio_runtime_providers::{Frame, Method, NotificationFrame, RequestId, ResponseBody};
use studio_runtime_services::{
    account_session_runtime::RetainedAccountSession,
    account_worker::{WorkerError, preparation::PreparationResources},
    event_write::{AnnotationWrite, annotate},
    model_catalog::ModelCatalogService,
    provider_session_task::SessionTaskConfig,
};
use studio_runtime_store::path_identity::CanonicalPath;
use studio_runtime_store::{RuntimeStore, StoreError};
use tokio::sync::{Notify, mpsc, watch};

use std::path::Path;
use studio_runtime_services::provider_adapter::{HostLaunch, ProviderKind, adapter};
fn codex_launch(
    native_path: Option<&str>,
    environment: BTreeMap<String, String>,
    home: String,
    isolated: bool,
    root: PathBuf,
) -> ProviderLaunchSpec {
    adapter(ProviderKind::Codex)
        .host_launch(HostLaunch {
            profile: None,
            home,
            isolated,
            codex_executable: native_path,
            claude_bridge_dir: Path::new(""),
            account: "default",
            root,
            environment,
        })
        .expect("Codex launch is infallible")
        .spec
}

fn claude_launch(
    profile: &serde_json::Value,
    bridge_dir: &Path,
    state_root: &Path,
    account: &str,
    environment: BTreeMap<String, String>,
) -> Result<ProviderLaunchSpec, WorkerError> {
    let profile = studio_runtime_services::claude_bridge::ClaudeLaunchProfile::from_record(profile)
        .map_err(|_| {
            WorkerError::PreparationRefused(
                studio_runtime_services::provider_adapter::CLAUDE_ACCOUNT_IDENTITY_REFUSAL.into(),
            )
        })?;
    adapter(ProviderKind::Claude)
        .host_launch(HostLaunch {
            profile: Some(&profile),
            home: String::new(),
            isolated: false,
            codex_executable: None,
            claude_bridge_dir: bridge_dir,
            account,
            root: state_root.to_path_buf(),
            environment,
        })
        .map(|launch| launch.spec)
}

async fn start_host(
    state: &mut studio_runtime_http::HttpState,
    store: Arc<RuntimeStore>,
    accounts: Arc<studio_runtime_services::account_driver::AccountDriver>,
    native_path: Option<String>,
    config: AccountRegistryConfig,
) -> Result<host_runtime::HostRuntime, WorkerError> {
    host_runtime::HostRuntime::start_with_monitor_wake_and_native_runtime(
        state,
        store,
        accounts,
        native_path,
        Arc::new(Notify::new()),
        AccountSessionBroker::new(),
        None,
        Arc::new(Mutex::new(BTreeMap::new())),
        config,
        None,
        None,
    )
    .await
}

fn test_registry_config() -> AccountRegistryConfig {
    AccountRegistryConfig::default()
}
/// Below the CI nextest kill (3 x 60 s), so a hang reports its state.
const TEST_HANG_GUARD: std::time::Duration = std::time::Duration::from_secs(120);
const SATURATED_WAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

struct Fixture {
    _root: tempfile::TempDir,
    store: Arc<RuntimeStore>,
    agents: Value,
    changes: watch::Receiver<u64>,
    event_ids: Mutex<BTreeMap<String, String>>,
}
impl Fixture {
    async fn new() -> Self {
        Self::build(crate::fixture_support::FixtureKind::Dispatch).await
    }
    async fn build(kind: crate::fixture_support::FixtureKind) -> Self {
        let root = tempfile::tempdir().unwrap();
        let (store, changes) = store_with_change_signal(root.path()).await;
        let agents = crate::fixture_support::seed(&store, root.path(), kind).await;
        Self {
            _root: root,
            store,
            agents,
            changes,
            event_ids: Mutex::new(BTreeMap::new()),
        }
    }
    async fn enqueue(&self, index: usize, id: &str) {
        let agent_id = self.agents[index]["id"].as_str().unwrap().to_owned();
        let event_id = if id.len() == 36 {
            id.to_owned()
        } else {
            studio_runtime_schema::persistence::RequestIdentity::issue_root()
        };
        self.event_ids
            .lock()
            .unwrap()
            .insert(format!("annotation:{id}"), format!("annotation:{event_id}"));
        self.store
            .transaction_with(move |tx| {
                annotate(
                    tx,
                    AnnotationWrite {
                        agent_id,
                        id: Some(event_id.clone()),
                        path: "README.md".into(),
                        line: 1,
                        text: event_id.clone(),
                        turn_id: None,
                        signature: event_id,
                    },
                )
                .map_err(|e| StoreError::SchemaMismatch(e.to_string()))
            })
            .await
            .unwrap();
    }

    fn event_status(&self, id: &str) -> String {
        let id = self
            .event_ids
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_owned());
        self.store
            .test_reader()
            .unwrap()
            .query_row(
                "SELECT status FROM runtime_events WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
    }
}

async fn store_with_change_signal(
    root: &std::path::Path,
) -> (Arc<RuntimeStore>, watch::Receiver<u64>) {
    let store = Arc::new(RuntimeStore::open(Some(root)).await.unwrap());
    // Registry protocol fixtures do not implement idle thread inventory.
    // Dedicated account-worker idle tests exercise that provider protocol.
    store
        .transaction(|tx| {
            studio_runtime_store::OrchestrationSettingsRepository::set(
                tx,
                studio_runtime_store::IDLE_SESSION_UNLOAD_SETTING,
                0,
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (sender, changes) = watch::channel(0_u64);
    store.add_commit_observer(move || {
        sender.send_modify(|generation| *generation = generation.wrapping_add(1));
    });
    (store, changes)
}

async fn wait_for_durable_event_status(fixture: &Fixture, id: &str, expected: &str) {
    tokio::time::timeout(TEST_HANG_GUARD, async {
        let mut changes = fixture.changes.clone();
        loop {
            if fixture.event_status(id) == expected {
                break;
            }
            changes
                .changed()
                .await
                .expect("fixture store change notifier remains open");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("durable event {id} status {expected:?} hang guard"));
}

async fn wait_for_durable_agent_status(fixture: &Fixture, agent_id: &str, expected: &str) {
    tokio::time::timeout(TEST_HANG_GUARD, async {
        let mut changes = fixture.changes.clone();
        loop {
            let status: String = fixture
                .store
                .test_reader()
                .unwrap()
                .query_row(
                    "SELECT json_extract(record,'$.status') FROM runtime_agents WHERE id=?1",
                    [agent_id],
                    |row| row.get(0),
                )
                .unwrap();
            if status == expected {
                break;
            }
            changes
                .changed()
                .await
                .expect("fixture store change notifier remains open");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("durable agent {agent_id} status {expected:?} hang guard"));
}

/// File-backed provider observations wake the fixture without polling.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn watch_file_changes(
    path: &std::path::Path,
) -> (
    notify::RecommendedWatcher,
    tokio::sync::mpsc::UnboundedReceiver<()>,
) {
    use notify::Watcher;
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(
        move |_: notify::Result<notify::Event>| {
            if let Ok(()) = sender.send(()) {}
        },
    )
    .expect("file watcher starts");
    watcher
        .watch(
            path.parent().expect("watched file has a parent"),
            notify::RecursiveMode::NonRecursive,
        )
        .expect("parent directory can be watched");
    (watcher, receiver)
}

#[cfg(target_os = "linux")]
async fn wait_for_file_lines(
    path: &std::path::Path,
    expected_lines: usize,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<()>,
) {
    tokio::time::timeout(TEST_HANG_GUARD, async {
        loop {
            if std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .count()
                >= expected_lines
            {
                break;
            }
            events.recv().await.expect("file watcher remains open");
        }
    })
    .await
    .expect("watched file update hang guard");
}

struct Scripted {
    opened: Arc<Mutex<Vec<String>>>,
    active: Arc<AtomicUsize>,
    writes: mpsc::UnboundedSender<(String, Value)>,
    unknown_writes: bool,
    record_resumes: bool,
    unknown_first_launch_only: bool,
    launches: AtomicUsize,
    failures: AtomicUsize,
    /// Accounts for which this predicate holds fail every launch, however often
    /// the registry retries, so a retry timer cannot change the outcome.
    unavailable: std::sync::OnceLock<fn(&str) -> bool>,
    unavailable_attempts: AtomicUsize,
    /// Every launch is refused because the account has no readable Claude
    /// login, so its saved account identity is missing.
    identity_refused: std::sync::atomic::AtomicBool,
    launch_attempts: std::sync::OnceLock<mpsc::UnboundedSender<String>>,
    stops: Arc<Mutex<Vec<Shutdown>>>,
    opened_notice: Arc<Notify>,
    ready_notice: Arc<Notify>,
    initialize_holds: InitializeHolds,
    /// Read-only history of the retained turn in the callback restart witness.
    retained_history: std::sync::OnceLock<(String, String)>,
}
/// Provider handles whose `initialize` reply waits for the paired release.
type InitializeHolds = Arc<Mutex<BTreeMap<String, Arc<Notify>>>>;
impl Scripted {
    /// Keep this provider uninitialized, so its account worker cannot drain
    /// queued commands, until the returned notification is released.
    fn hold_initialize(&self, handle: &str) -> Arc<Notify> {
        let release = Arc::new(Notify::new());
        self.initialize_holds
            .lock()
            .unwrap()
            .insert(handle.to_owned(), release.clone());
        release
    }
}
impl AccountLauncher for Scripted {
    fn launch(&self, account: String) -> LaunchFuture<'_> {
        Box::pin(async move {
            if let Some(attempts) = self.launch_attempts.get() {
                drop(attempts.send(account.clone()));
            }
            if self
                .unavailable
                .get()
                .is_some_and(|unavailable| unavailable(&account))
            {
                self.unavailable_attempts.fetch_add(1, Ordering::SeqCst);
                return Err(WorkerError::TaskFailed);
            }
            if self.identity_refused.load(Ordering::SeqCst) {
                return Err(WorkerError::PreparationRefused(
                    studio_runtime_services::provider_adapter::CLAUDE_ACCOUNT_IDENTITY_REFUSAL
                        .into(),
                ));
            }
            if self
                .failures
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_sub(1))
                .is_ok()
            {
                return Err(WorkerError::TaskFailed);
            }
            let launch = self.launches.fetch_add(1, Ordering::SeqCst);
            let unknown_writes =
                self.unknown_writes && (!self.unknown_first_launch_only || launch == 0);
            let factory = Arc::new(WireFactory {
                opened: self.opened.clone(),
                active: self.active.clone(),
                writes: self.writes.clone(),
                unknown_writes,
                record_resumes: self.record_resumes,
                stops: self.stops.clone(),
                opened_notice: self.opened_notice.clone(),
                ready_notice: self.ready_notice.clone(),
                initialize_holds: self.initialize_holds.clone(),
                retained_history: self.retained_history.get().cloned(),
            });
            let mut resources =
                PreparationResources::bundled(String::new(), ModelCatalogService::new()).unwrap();
            // HostLauncher supplies the account pin for every provider.
            resources.claude_account_pin =
                json!({"provider":studio_runtime_schema::ProviderKind::Codex});
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
                    wire: studio_runtime_providers::ProviderKind::Codex.wire(),
                    timeout_ms: 60_000,
                    initialize_operation: OperationId(format!("init:{account}")),
                    request_id_prefix: None,
                    recovered: vec![],
                },
                resources,
                publication_sink: Arc::new(
                    studio_runtime_services::notification_runtime_effects::UnavailablePostCommitSink,
                ),
            })
        })
    }
}
struct WireFactory {
    stops: Arc<Mutex<Vec<Shutdown>>>,
    opened_notice: Arc<Notify>,
    ready_notice: Arc<Notify>,
    opened: Arc<Mutex<Vec<String>>>,
    active: Arc<AtomicUsize>,
    writes: mpsc::UnboundedSender<(String, Value)>,
    unknown_writes: bool,
    record_resumes: bool,
    initialize_holds: InitializeHolds,
    retained_history: Option<(String, String)>,
}
impl ProviderTaskFactory for WireFactory {
    fn open(&self, handle: ProviderHandle, _: OpenOptions, shutdown: Shutdown) -> OpenFuture<'_> {
        self.opened.lock().unwrap().push(handle.as_str().into());
        self.stops.lock().unwrap().push(shutdown.clone());
        self.opened_notice.notify_one();
        self.active.fetch_add(1, Ordering::SeqCst);
        let active = self.active.clone();
        let writes = self.writes.clone();
        let unknown_writes = self.unknown_writes;
        let record_resumes = self.record_resumes;
        let ready_notice = self.ready_notice.clone();
        let initialize_hold = self
            .initialize_holds
            .lock()
            .unwrap()
            .get(handle.as_str())
            .cloned();
        let retained_history = self.retained_history.clone();
        Box::pin(async move {
            let (commands, mut inbox) = mpsc::channel::<AdapterCommand>(16);
            let (events, journal) = mpsc::channel(16);
            let response_events = events.clone();
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
                let status = status;
                let events = response_events;
                let mut sequence = 0;
                loop {
                    let command = tokio::select! { biased; _=Shutdown::cancelled(&mut stop)=>break, c=inbox.recv()=>match c { Some(c)=>c,None=>break } };
                    match command {
                        AdapterCommand::Write {
                            write,
                            reply,
                            observed,
                        } => {
                            if write.message()["method"] == "initialize"
                                && let Some(release) = &initialize_hold
                            {
                                tokio::select! {
                                    biased;
                                    _ = Shutdown::cancelled(&mut stop) => break,
                                    _ = release.notified() => {}
                                }
                            }
                            let mut observed_message = None;
                            let receipt = match write.message()["method"].as_str() {
                                Some("initialize") => CommandReceipt::CachedInitialize(json!({})),
                                Some("initialized") => {
                                    // The real supervisor publishes readiness before its write receipt.
                                    status.send_modify(|current| {
                                        current.initialization = InitializationStatus::Ready;
                                    });
                                    CommandReceipt::Written {
                                        remote_id: None,
                                        duplicate: false,
                                    }
                                }
                                Some("thread/resume" | "thread/start") => {
                                    if record_resumes {
                                        writes
                                            .send((handle.as_str().into(), write.message().clone()))
                                            .unwrap();
                                    }
                                    CommandReceipt::Written {
                                        remote_id: Some(sequence),
                                        duplicate: false,
                                    }
                                }
                                _ => {
                                    observed_message = Some(write.message().clone());
                                    if unknown_writes {
                                        // This mode models a lost acceptance response.
                                        CommandReceipt::Unknown
                                    } else {
                                        CommandReceipt::Written {
                                            remote_id: None,
                                            duplicate: false,
                                        }
                                    }
                                }
                            };
                            let _ignored = reply.send(receipt);
                            if matches!(
                                write.message()["method"].as_str(),
                                Some("thread/resume" | "thread/start")
                            ) {
                                if observed.await.is_err() {
                                    break;
                                }
                                status.send_modify(|current| {
                                    current.initialization = InitializationStatus::Ready;
                                });
                                let thread_id = write.message()["params"]["threadId"].clone();
                                sequence += 1;
                                events.send(JournalEntry {
                                    received_at_ms: 0,
                                    cursor: EventCursor(sequence),
                                    generation: Generation(7),
                                    payload: JournalPayload::Frame(Frame::Notification(
                                        studio_runtime_providers::NotificationFrame {
                                            method: studio_runtime_providers::Method::ThreadStatusChanged,
                                            params: Some(json!({"threadId":thread_id,"status":{"type":"idle"}})),
                                            fields: Default::default(),
                                        },
                                    )),
                                }).await.unwrap();
                                sequence += 1;
                                events
                                    .send(JournalEntry {
                                        received_at_ms: 0,
                                        cursor: EventCursor(sequence),
                                        generation: Generation(7),
                                        payload: JournalPayload::Frame(Frame::Response {
                                            id: serde_json::from_value::<RequestId>(
                                                write.message()["id"].clone(),
                                            )
                                            .unwrap(),
                                            body: ResponseBody::Result(
                                                json!({"thread":{"id":thread_id}}),
                                            ),
                                            fields: Default::default(),
                                        }),
                                    })
                                    .await
                                    .unwrap();
                                continue;
                            }
                            let observed = tokio::select! {
                                biased;
                                result = observed => result.is_ok(),
                                _ = Shutdown::cancelled(&mut stop) => false,
                            };
                            if observed {
                                if write.message()["method"] == "initialized" {
                                    ready_notice.notify_one();
                                }
                                if let Some(message) = observed_message {
                                    if let Some((thread, turn)) = &retained_history {
                                        let result = match message["method"].as_str() {
                                            Some("thread/read") => Some(
                                                json!({"thread":{"id":thread,"status":{"type":"active"}}}),
                                            ),
                                            Some("thread/turns/list") => Some(
                                                json!({"data":[{"id":turn,"threadId":thread,"status":"inProgress","itemsView":"full","items":[]}],"nextCursor":null}),
                                            ),
                                            _ => None,
                                        };
                                        if let Some(result) = result {
                                            assert_eq!(message["params"]["threadId"], *thread);
                                            sequence += 1;
                                            events
                                                .send(JournalEntry {
                                                    received_at_ms: 0,
                                                    cursor: EventCursor(sequence),
                                                    generation: Generation(7),
                                                    payload: JournalPayload::Frame(
                                                        Frame::Response {
                                                            id: serde_json::from_value(
                                                                message["id"].clone(),
                                                            )
                                                            .unwrap(),
                                                            body: ResponseBody::Result(result),
                                                            fields: Default::default(),
                                                        },
                                                    ),
                                                })
                                                .await
                                                .unwrap();
                                            continue;
                                        }
                                    }
                                    if message["method"] == "turn/start" && !unknown_writes {
                                        sequence += 1;
                                        events
                                            .send(JournalEntry {
                                                received_at_ms: 0,
                                                cursor: EventCursor(sequence),
                                                generation: Generation(7),
                                                payload: JournalPayload::Frame(Frame::Response {
                                                    id: serde_json::from_value::<RequestId>(
                                                        message["id"].clone(),
                                                    )
                                                    .unwrap(),
                                                    body: ResponseBody::Result(
                                                        json!({"turn":{"id":"turn"}}),
                                                    ),
                                                    fields: Default::default(),
                                                }),
                                            })
                                            .await
                                            .unwrap();
                                        sequence += 1;
                                        events
                                            .send(JournalEntry {
                                                received_at_ms: 0,
                                                cursor: EventCursor(sequence),
                                                generation: Generation(7),
                                                payload: JournalPayload::Frame(Frame::Notification(
                                                    NotificationFrame {
                                                        method: Method::TurnCompleted,
                                                        params: Some(json!({
                                                            "threadId": message["params"]["threadId"],
                                                            "turn": {"id":"turn","status":"completed"}
                                                        })),
                                                        fields: Default::default(),
                                                    },
                                                )),
                                            })
                                            .await
                                            .unwrap();
                                    }
                                    writes.send((handle.as_str().into(), message)).unwrap();
                                }
                            } else {
                                break;
                            }
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
                    provider_retained: false,
                }
            });
            Ok(ProviderAttachment::from_task(
                commands, journal, observed, shutdown, task,
            ))
        })
    }
}
fn scripted() -> (Arc<Scripted>, mpsc::UnboundedReceiver<(String, Value)>) {
    scripted_with_unknown_writes(true)
}
fn scripted_with_unknown_writes(
    unknown_writes: bool,
) -> (Arc<Scripted>, mpsc::UnboundedReceiver<(String, Value)>) {
    scripted_with_policy(unknown_writes, false, false)
}
fn scripted_with_resume_recording(
    unknown_writes: bool,
    record_resumes: bool,
) -> (Arc<Scripted>, mpsc::UnboundedReceiver<(String, Value)>) {
    scripted_with_policy(unknown_writes, record_resumes, false)
}
fn scripted_with_unknown_first_launch() -> (Arc<Scripted>, mpsc::UnboundedReceiver<(String, Value)>)
{
    scripted_with_policy(true, false, true)
}
/// Drain the recorded provider writes. A start whose outcome is unknown when its
/// provider connection closes gets one native history check of `thread` by the
/// next account worker, so such reads are allowed (the scripted provider does
/// not answer them). Any other write fails with `context`. Returns the reads.
fn drain_history_reads(
    writes: &mut mpsc::UnboundedReceiver<(String, Value)>,
    thread: &Value,
    context: &str,
) -> usize {
    let mut reads = 0;
    while let Ok((_, write)) = writes.try_recv() {
        assert_eq!(write["method"], "thread/turns/list", "{context}: {write}");
        assert_eq!(&write["params"]["threadId"], thread, "{context}: {write}");
        reads += 1;
    }
    reads
}
fn scripted_with_policy(
    unknown_writes: bool,
    record_resumes: bool,
    unknown_first_launch_only: bool,
) -> (Arc<Scripted>, mpsc::UnboundedReceiver<(String, Value)>) {
    let (writes, rx) = mpsc::unbounded_channel();
    (
        Arc::new(Scripted {
            opened: Arc::default(),
            active: Arc::default(),
            writes,
            unknown_writes,
            record_resumes,
            unknown_first_launch_only,
            launches: AtomicUsize::new(0),
            failures: AtomicUsize::new(0),
            unavailable: std::sync::OnceLock::new(),
            unavailable_attempts: AtomicUsize::new(0),
            identity_refused: std::sync::atomic::AtomicBool::new(false),
            launch_attempts: std::sync::OnceLock::new(),
            stops: Arc::default(),
            opened_notice: Arc::default(),
            ready_notice: Arc::default(),
            initialize_holds: Arc::default(),
            retained_history: std::sync::OnceLock::new(),
        }),
        rx,
    )
}

fn radio_snapshot(mode: &str) -> Value {
    let fixture = match mode {
        "interrupt" => include_str!("fixtures/radio-interrupt.json"),
        "queue" => include_str!("fixtures/radio-queue.json"),
        other => panic!("unknown radio fixture mode {other}"),
    };
    serde_json::from_str(fixture).unwrap()
}

fn radio_interrupt_snapshot() -> Value {
    radio_snapshot("interrupt")
}

fn radio_queue_snapshot() -> Value {
    radio_snapshot("queue")
}

#[allow(clippy::disallowed_methods)]
async fn seed_radio_interrupt(fixture: &Fixture, snapshot: &Value) {
    let project_root = fixture._root.path().join("radio-project");
    fs::create_dir_all(&project_root).unwrap();
    // A project id is its resolved path, as Python stores it. The radio skips a
    // project whose id differs from its resolved path, such as one that names
    // the macOS temporary folder through the /var link.
    let project_root = CanonicalPath::existing(&project_root).unwrap();
    std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&project_root)
        .status()
        .unwrap();
    let identity = fixture.store.root().join("multi-server");
    fs::create_dir_all(&identity).unwrap();
    fs::write(
        identity.join("identity.json"),
        r#"{"serverId":"local-test","label":"fixture","publicKey":"fixture"}"#,
    )
    .unwrap();
    fixture
        .store
        .set_local_server_id(Some("local-test".to_owned()))
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&identity, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(
            identity.join("identity.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    let snapshot = snapshot.clone();
    let stored_project_root = project_root.clone();
    fixture
        .store
        .transaction(move |db| {
            for (table, rows) in snapshot.as_object().unwrap() {
                // Historical ingress receipts are already retired in a current store.
                if table == "operation_receipts" {
                    continue;
                }
                let table_name = match table.as_str() {
                    "sync_entities" => "sync_entities".to_owned(),
                    "sidebar_order" => "runtime_sidebar_order".to_owned(),
                    _ => format!("runtime_{table}"),
                };
                db.execute(&format!("DELETE FROM {table_name}"), [])?;
                for row in rows.as_array().unwrap() {
                    let mut row = row.clone();
                    if table == "projects" {
                        let record: Value =
                            serde_json::from_str(row["record"].as_str().unwrap()).unwrap();
                        let mut record = record;
                        record["id"] = json!(stored_project_root.to_string_lossy());
                        record["path"] = json!(stored_project_root.to_string_lossy());
                        if let Some(locations) = record["locations"].as_array_mut() {
                            for location in locations {
                                location["path"] = json!(stored_project_root.to_string_lossy());
                            }
                        }
                        row["id"] = json!(stored_project_root.to_string_lossy());
                        row["record"] = json!(record.to_string());
                    }
                    if table == "agents" {
                        let mut record: Value =
                            serde_json::from_str(row["record"].as_str().unwrap()).unwrap();
                        record["cwd"] = json!(stored_project_root.to_string_lossy());
                        if record.get("projectId").is_some() {
                            record["projectId"] = json!(stored_project_root.to_string_lossy());
                        }
                        row["record"] = json!(record.to_string());
                    }
                    if table == "rooms" {
                        let mut record: Value =
                            serde_json::from_str(row["record"].as_str().unwrap()).unwrap();
                        record["projectPath"] = json!(stored_project_root.to_string_lossy());
                        row["record"] = json!(record.to_string());
                    }
                    let fields = row.as_object().unwrap();
                    let names = fields.keys().cloned().collect::<Vec<_>>().join(",");
                    let placeholders = vec!["?"; fields.len()].join(",");
                    let values = fields
                        .values()
                        .map(|value| match value {
                            Value::Null => rusqlite::types::Value::Null,
                            Value::String(value) => rusqlite::types::Value::Text(value.clone()),
                            Value::Number(value) if value.is_i64() => {
                                rusqlite::types::Value::Integer(value.as_i64().unwrap())
                            }
                            Value::Number(value) => {
                                rusqlite::types::Value::Real(value.as_f64().unwrap())
                            }
                            Value::Bool(_) | Value::Array(_) | Value::Object(_) => {
                                panic!("unsupported radio fixture SQL value")
                            }
                        })
                        .collect::<Vec<_>>();
                    db.execute(
                        &format!("INSERT INTO {table_name}({names}) VALUES({placeholders})"),
                        rusqlite::params_from_iter(values),
                    )?;
                }
            }
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn closing_worker_does_not_invent_a_transfer_deadline_expiry() {
    use studio_runtime_services::account_session_runtime::{
        NotSentReason, ProviderRequest, RequestCertainty, SessionOutcome, TransferProviderRequest,
    };
    use studio_runtime_services::account_worker::COMMAND_CAPACITY;

    let fixture = Fixture::new().await;
    let (launcher, _writes) = scripted();
    let _initialize_release = launcher.hold_initialize("account:default");
    let broker = AccountSessionBroker::new();
    let mut registry = AccountRegistry::new_with_broker(
        fixture.store.clone(),
        launcher.clone(),
        Arc::new(Notify::new()),
        broker.clone(),
        test_registry_config(),
        None,
    )
    .await
    .unwrap();
    registry.start("default").await.unwrap();
    let scope = broker.current_scope("default").unwrap();
    let deadline = std::time::Instant::now() + TEST_HANG_GUARD;
    // Initialization is held, so the last request waits for mailbox capacity.
    // Closing that real worker proves non-delivery before the future deadline.
    let mut pending = Vec::new();
    for index in 0..=COMMAND_CAPACITY {
        let mut request = broker.transfer_request(TransferProviderRequest {
            root_request_id: studio_runtime_schema::persistence::RequestIdentity::issue_root(),
            admitted_request: None,
            transfer_id: "closing-transfer".into(),
            member: "member".into(),
            source_account: "default".into(),
            thread_id: "source-thread".into(),
            operation: format!("transfer:closing-transfer:member:read-{index}"),
            expected_scope: scope.clone(),
            method: Method::ThreadQueueList,
            params: json!({"threadId":"source-thread"}),
            deadline,
        });
        std::future::poll_fn(|context| {
            assert!(request.as_mut().poll(context).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        pending.push(request);
    }
    let mut ordinary = broker.request(ProviderRequest {
        root_request_id: studio_runtime_schema::persistence::RequestIdentity::issue_root(),
        admitted_request: None,
        account: "default".into(),
        operation: "model-list:closing-worker".into(),
        actor: None,
        expected_scope: Some(scope.clone()),
        deleted_actor_session: None,
        method: Method::ModelList,
        params: json!({"limit":100}),
        deadline,
    });
    std::future::poll_fn(|context| {
        assert!(ordinary.as_mut().poll(context).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    registry.close_worker_for_test("default").await.unwrap();
    assert!(std::time::Instant::now() < deadline);
    for (receipt, operation) in [
        (
            pending.pop().unwrap().await.unwrap(),
            format!("transfer:closing-transfer:member:read-{COMMAND_CAPACITY}"),
        ),
        (ordinary.await.unwrap(), "model-list:closing-worker".into()),
    ] {
        assert_eq!(receipt.scope, scope);
        assert_eq!(receipt.operation, operation);
        assert_eq!(receipt.certainty(), RequestCertainty::NotSent);
        assert!(receipt.ticket().is_none());
        assert_eq!(
            receipt.not_sent_reason(),
            Some(NotSentReason::WorkerNotSent)
        );
        assert!(matches!(receipt.outcome, SessionOutcome::NotHandedOff));
    }
    drop(pending);
    // close_worker_for_test already joined the only worker.
    assert_eq!(launcher.active.load(Ordering::SeqCst), 0);
    drop(registry);
}

#[tokio::test]
async fn concurrent_ensure_session_starts_one_cold_account_worker() {
    let fixture = Fixture::new().await;
    let (launcher, _writes) = scripted();
    let broker = AccountSessionBroker::new();
    let registry = AccountRegistry::new_with_broker(
        fixture.store.clone(),
        launcher.clone(),
        Arc::new(Notify::new()),
        broker.clone(),
        test_registry_config(),
        None,
    )
    .await
    .unwrap();
    let stop = Shutdown::default();
    let task = tokio::spawn(registry.run(
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        stop.clone(),
    ));

    let (first, second) = tokio::time::timeout(TEST_HANG_GUARD, async {
        tokio::join!(
            broker.ensure_session("default".into()),
            broker.ensure_session("default".into())
        )
    })
    .await
    .expect("ensure session hang guard");
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first, second);
    assert_eq!(first.account, "default");
    assert_eq!(launcher.opened.lock().unwrap().len(), 1);

    stop.cancel();
    task.await.unwrap().unwrap();
    assert_eq!(launcher.active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn production_reconcile_delivers_radio_interrupt_once_with_deterministic_clock() {
    let fixture = Fixture::new().await;
    let snapshot = radio_interrupt_snapshot();
    seed_radio_interrupt(&fixture, &snapshot).await;
    let (launcher, _writes) = scripted();
    let mut registry =
        AccountRegistry::new(fixture.store.clone(), launcher, test_registry_config())
            .await
            .unwrap();
    registry.set_radio_clock_for_test(Arc::new(|| 1_000.0));
    registry.start("default").await.unwrap();

    registry.reconcile(None).await.unwrap();
    let room = || {
        let db = fixture.store.test_reader().unwrap();
        let raw: String = db
            .query_row("SELECT record FROM runtime_rooms LIMIT 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        serde_json::from_str::<Value>(&raw).unwrap()
    };
    let delivered = room();
    assert_eq!(delivered["radio"]["status"], "stopping");
    assert_eq!(delivered["radio"]["active"]["interruptState"], "accepted");
    registry.reconcile(None).await.unwrap();
    let replay = room();
    assert_eq!(replay["radio"]["revision"], delivered["radio"]["revision"]);
    assert_eq!(replay["radio"]["active"]["interruptState"], "accepted");
    registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn production_reconcile_admits_radio_queue_event_to_retained_worker() {
    let fixture = Fixture::new().await;
    let snapshot = radio_queue_snapshot();
    seed_radio_interrupt(&fixture, &snapshot).await;
    let (launcher, mut writes) = scripted_with_resume_recording(true, true);
    let mut registry = AccountRegistry::new(
        fixture.store.clone(),
        launcher.clone(),
        test_registry_config(),
    )
    .await
    .unwrap();
    registry.set_radio_clock_for_test(Arc::new(|| 2_000_000_000.0));

    registry.reconcile(None).await.unwrap();
    let received = tokio::time::timeout(TEST_HANG_GUARD, writes.recv()).await;
    if received.is_err() {
        let db = fixture.store.test_reader().unwrap();
        let event: (String, Option<String>) = db
            .query_row(
                "SELECT status,error FROM runtime_events WHERE kind='radio_turn'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let agent: String = db
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        panic!("radio queue worker delivery timed out; event={event:?}; agent={agent}");
    }
    let (resume_account, resume) = received.unwrap().expect("retained worker provider write");
    assert_eq!(resume_account, "account:default");
    assert!(matches!(
        resume["method"].as_str(),
        Some("thread/resume" | "thread/start")
    ));
    let turn_write = tokio::time::timeout(TEST_HANG_GUARD, writes.recv()).await;
    if turn_write.is_err() {
        let db = fixture.store.test_reader().unwrap();
        let mut statement = db
            .prepare("SELECT id,result FROM request_results ORDER BY rowid DESC LIMIT 8")
            .unwrap();
        let receipts = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        panic!("radio turn provider write timed out after resume; receipts={receipts:?}");
    }
    let (account, write) = turn_write.unwrap().expect("retained worker turn write");
    assert_eq!(account, "account:default");
    assert_eq!(write["method"], "turn/start");
    let db = fixture.store.test_reader().unwrap();
    let (event_id, kind, status): (String, String, String) = db
        .query_row(
            "SELECT id,kind,status FROM runtime_events WHERE kind='radio_turn'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(kind, "radio_turn");
    assert!(event_id.starts_with("radio-turn:"));
    assert!(matches!(status.as_str(), "dispatching" | "uncertain"));
    drop(db);
    // The unknown write closes the scripted session. Its turn/start outcome is
    // unknown, so the disconnect holds the agent for one native history check
    // of that input, as a backend restart does, instead of interrupting it.
    // Reconcile after its settlement so the radio tick prepares from the
    // settled agent row.
    wait_for_durable_agent_status(&fixture, "a", "queued").await;
    let error: String = fixture
        .store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT json_extract(record,'$.error') FROM runtime_agents WHERE id='a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        error.starts_with("Context repair waits for a confirmed input receipt"),
        "{error}"
    );
    let thread = write["params"]["threadId"].clone();

    registry.reconcile(None).await.unwrap();
    drain_history_reads(&mut writes, &thread, "radio queue event was sent twice");
    // The worker ends on the closed session unless shutdown reaches it first.
    let shutdown = registry.shutdown().await;
    assert!(
        matches!(
            shutdown,
            Ok(())
                | Err(WorkerError::Session(
                    studio_runtime_services::provider_session_task::SessionTaskError::Closed
                ))
        ),
        "{shutdown:?}"
    );
    let status_before_restart: String = fixture
        .store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT status FROM runtime_events WHERE id=?1",
            [&event_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        status_before_restart, "uncertain",
        "shutdown must retain the unknown submitted radio turn as uncertain"
    );
    let mut restarted =
        AccountRegistry::new(fixture.store.clone(), launcher, test_registry_config())
            .await
            .unwrap();
    let recovery = restarted.reconcile(None).await;
    assert!(
        recovery.is_ok(),
        "restarting a worker with an uncertain radio turn must preserve it without rejecting the actor; recovery={recovery:?}. workspaceMode is only a preference for this non-worktree actor; Python creates its worktree only when worktree=true (codex_runtime.py:5001-5002, 6307-6308)"
    );
    let status_after_recovery: String = fixture
        .store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT status FROM runtime_events WHERE id=?1",
            [&event_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status_after_recovery, status_before_restart);
    restarted.shutdown().await.unwrap();
    drain_history_reads(
        &mut writes,
        &thread,
        "unknown radio turn must not be resent after worker restart",
    );
}

#[tokio::test]
async fn production_reconcile_keeps_closed_worker_interrupt_unknown_without_resending() {
    let fixture = Fixture::new().await;
    let snapshot = radio_interrupt_snapshot();
    seed_radio_interrupt(&fixture, &snapshot).await;
    let (launcher, _writes) = scripted();
    let mut registry =
        AccountRegistry::new(fixture.store.clone(), launcher, test_registry_config())
            .await
            .unwrap();
    registry.set_radio_clock_for_test(Arc::new(|| 1_000.0));
    registry.start("default").await.unwrap();
    registry.close_worker_for_test("default").await.unwrap();

    registry.reconcile(None).await.unwrap();
    let room = || {
        let db = fixture.store.test_reader().unwrap();
        let raw: String = db
            .query_row("SELECT record FROM runtime_rooms LIMIT 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        serde_json::from_str::<Value>(&raw).unwrap()
    };
    let unknown = room();
    assert_eq!(unknown["radio"]["status"], "stopping");
    assert_eq!(unknown["radio"]["active"]["interruptState"], "unknown");
    let revision = unknown["radio"]["revision"].clone();
    registry.reconcile(None).await.unwrap();
    let replay = room();
    assert_eq!(replay["radio"]["revision"], revision);
    assert_eq!(replay["radio"]["active"]["interruptState"], "unknown");
    drop(registry);
}

#[tokio::test]
async fn committed_input_reaches_one_retained_session_and_unknown_restart_does_not_resend() {
    let fixture = Fixture::new().await;
    fixture.enqueue(0, "committed-before-start").await;
    let (launcher, mut writes) = scripted();
    let mut registry = AccountRegistry::new(
        fixture.store.clone(),
        launcher.clone(),
        test_registry_config(),
    )
    .await
    .unwrap();
    registry.reconcile(None).await.unwrap();
    let (account, write) = tokio::time::timeout(TEST_HANG_GUARD, writes.recv())
        .await
        .expect("provider write hang guard")
        .unwrap();
    assert_eq!(account, "account:default");
    assert_eq!(write["method"], "turn/start");
    registry.reconcile(None).await.unwrap();
    assert_eq!(launcher.opened.lock().unwrap().len(), 1);
    registry.shutdown().await.unwrap();
    assert_eq!(launcher.active.load(Ordering::SeqCst), 0);
    let mut restarted = AccountRegistry::new(
        fixture.store.clone(),
        launcher.clone(),
        test_registry_config(),
    )
    .await
    .unwrap();
    restarted.reconcile(None).await.unwrap();
    restarted.shutdown().await.unwrap();
    // Reconciliation may read the provider history; only a new turn is a resend.
    let resent: Vec<_> = std::iter::from_fn(|| writes.try_recv().ok())
        .filter(|(_, write)| write["method"] == "turn/start")
        .collect();
    assert!(
        resent.is_empty(),
        "uncertain request must not be resubmitted: {resent:?}"
    );
}

/// Legacy image rows remain stored and never start a provider after removal.
#[tokio::test]
async fn review_regression_legacy_pending_recovery_does_not_start_provider() {
    let fixture = Fixture::new().await;
    let id = fixture.agents[0]["id"].as_str().unwrap().to_owned();
    fixture.store.transaction(move |tx| {
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.workspaceMode','image','$.status','interrupted','$.autoWake',json('false'),'$.restartRecovery',json(?2)) WHERE id=?1", rusqlite::params![id,json!({"stage":"pending","turnId":"old-turn"}).to_string()])?;
        Ok(())
    }).await.unwrap();
    let (launcher, _writes) = scripted();
    let mut registry = AccountRegistry::new(
        fixture.store.clone(),
        launcher.clone(),
        test_registry_config(),
    )
    .await
    .unwrap();
    registry.reconcile(None).await.unwrap();
    assert!(
        launcher.opened.lock().unwrap().is_empty(),
        "Legacy recovery must not start a provider"
    );
    registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn legacy_image_rows_finish_without_provider_launch_or_row_loss() {
    let fixture = Fixture::new().await;
    let agent_id = fixture.agents[0]["id"].as_str().unwrap().to_owned();
    let transaction_agent_id = agent_id.clone();
    fixture
        .store
        .transaction(move |tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.workspaceMode','image') WHERE id=?1",
                [&transaction_agent_id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let event_id = studio_runtime_schema::persistence::RequestIdentity::issue_root();
    fixture.enqueue(0, &event_id).await;
    let (launcher, mut writes) = scripted();
    let mut registry = AccountRegistry::new(
        fixture.store.clone(),
        launcher.clone(),
        test_registry_config(),
    )
    .await
    .unwrap();

    registry.reconcile(None).await.unwrap();

    assert!(launcher.opened.lock().unwrap().is_empty());
    let record = || {
        let db = fixture.store.test_reader().unwrap();
        db.query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [agent_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
    };
    let db = fixture.store.test_reader().unwrap();
    let event_status: String = db
        .query_row(
            "SELECT status FROM runtime_events WHERE id=?1",
            [format!("annotation:{event_id}")],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(event_status, "pending");
    drop(db);
    let held: Value = serde_json::from_str(&record()).unwrap();
    assert_eq!(held["status"], "completed");
    assert_eq!(held["workspaceMode"], "image");
    assert_eq!(held["legacyWorkspaceFinished"], true);
    assert_eq!(held["autoWake"], false);
    registry.shutdown().await.unwrap();

    let mut restarted = AccountRegistry::new(
        fixture.store.clone(),
        launcher.clone(),
        test_registry_config(),
    )
    .await
    .unwrap();
    restarted.reconcile(None).await.unwrap();
    assert_eq!(serde_json::from_str::<Value>(&record()).unwrap(), held);
    assert!(launcher.opened.lock().unwrap().is_empty());
    assert!(writes.try_recv().is_err());
    let db = fixture.store.test_reader().unwrap();
    let event_status: String = db
        .query_row(
            "SELECT status FROM runtime_events WHERE id=?1",
            [format!("annotation:{event_id}")],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(event_status, "pending");
    drop(db);
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn two_accounts_are_independent_and_shutdown_joins_all_attachments() {
    let fixture = Fixture::new().await;
    fixture.enqueue(0, "first").await;
    fixture.enqueue(1, "second").await;
    // This case exercises independent accounts and orderly attachment shutdown.
    // Lost write acceptance is covered by the adjacent restart test and can make
    // an already-failed provider task race the explicit shutdown.
    let (launcher, mut writes) = scripted_with_unknown_writes(false);
    let mut registry = AccountRegistry::new(
        fixture.store.clone(),
        launcher.clone(),
        test_registry_config(),
    )
    .await
    .unwrap();
    registry.reconcile(None).await.unwrap();
    let mut accounts = vec![
        tokio::time::timeout(TEST_HANG_GUARD, writes.recv())
            .await
            .expect("provider write hang guard")
            .unwrap()
            .0,
        tokio::time::timeout(TEST_HANG_GUARD, writes.recv())
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
    let fixture = Fixture::new().await;
    fixture.enqueue(0, "retry-start").await;
    let (launcher, mut writes) = scripted_with_unknown_writes(false);
    launcher.failures.store(1, Ordering::SeqCst);
    let mut registry = AccountRegistry::new(
        fixture.store.clone(),
        launcher.clone(),
        test_registry_config(),
    )
    .await
    .unwrap();
    assert!(registry.reconcile(None).await.is_err());
    let wake = Arc::new(Notify::new());
    let stop = Shutdown::default();
    wake.notify_one();
    let task = tokio::spawn(registry.run(wake, Arc::new(Notify::new()), stop.clone()));
    assert_eq!(
        tokio::time::timeout(TEST_HANG_GUARD, writes.recv())
            .await
            .expect("provider write hang guard")
            .unwrap()
            .0,
        "account:default"
    );
    wait_for_durable_event_status(&fixture, "annotation:retry-start", "delivered").await;
    wait_for_durable_agent_status(
        &fixture,
        fixture.agents[0]["id"].as_str().unwrap(),
        "completed",
    )
    .await;
    stop.cancel();
    tokio::time::timeout(TEST_HANG_GUARD, task)
        .await
        .expect("account registry shutdown hang guard")
        .unwrap()
        .unwrap();
    assert_eq!(launcher.active.load(Ordering::SeqCst), 0);
}

/// A Claude account without a readable login has no saved account identity,
/// so its provider cannot launch and no account worker can reach the sign-in
/// gate. The queued agent must not wait for a provider session without a
/// message: it shows the sign-in reason and action, keeps its input, and the
/// held-turn release requeues it once the account is ready.
#[tokio::test]
async fn claude_account_without_a_readable_login_holds_its_start_for_sign_in() {
    let fixture = Fixture::new().await;
    fixture.enqueue(0, "needs-login").await;
    let (launcher, _writes) = scripted_with_unknown_writes(false);
    launcher.identity_refused.store(true, Ordering::SeqCst);
    let mut registry = AccountRegistry::new(
        fixture.store.clone(),
        launcher.clone(),
        test_registry_config(),
    )
    .await
    .unwrap();
    assert!(registry.reconcile(None).await.is_err());
    let agent_id = fixture.agents[0]["id"].as_str().unwrap();
    let (status, error, wait): (String, Option<String>, Option<String>) = fixture
        .store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT json_extract(record,'$.status'),json_extract(record,'$.error'),json_extract(record,'$.startWait.reason') FROM runtime_agents WHERE id=?1",
            [agent_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (status.as_str(), error.as_deref(), wait.as_deref()),
        ("failed", Some("Sign in with claude auth login first"), None),
        "the owner sees why the agent does not start and what to do"
    );
    assert_eq!(fixture.event_status("annotation:needs-login"), "pending");
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
    let fixture = Fixture::new().await;
    let state = studio_runtime_http::HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        fixture._root.path().to_string_lossy().into(),
        49171,
        None,
        Some(fixture.store.clone()),
        None,
    );
    let wake = state.event_wake_handle();
    let app = studio_runtime_http::router(state.clone()).unwrap();
    let (launcher, mut writes) = scripted_with_unknown_writes(false);
    let registry = AccountRegistry::new(
        fixture.store.clone(),
        launcher.clone(),
        test_registry_config(),
    )
    .await
    .unwrap();
    let stop = Shutdown::default();
    let task = tokio::spawn(registry.run(wake, Arc::new(Notify::new()), stop.clone()));
    let message_id = studio_runtime_schema::persistence::RequestIdentity::issue_root();
    let body =
        json!({"room":fixture.agents[0]["id"],"id":message_id,"text":"committed through HTTP"});
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
    let (account, request) = tokio::time::timeout(TEST_HANG_GUARD, writes.recv())
        .await
        .expect("provider write hang guard")
        .unwrap();
    assert_eq!(account, "account:default");
    assert_eq!(request["method"], "turn/start");
    wait_for_durable_event_status(&fixture, &message_id, "delivered").await;
    wait_for_durable_agent_status(
        &fixture,
        fixture.agents[0]["id"].as_str().unwrap(),
        "completed",
    )
    .await;
    stop.cancel();
    tokio::time::timeout(TEST_HANG_GUARD, task)
        .await
        .expect("account registry shutdown hang guard")
        .unwrap()
        .unwrap();
    assert_eq!(launcher.opened.lock().unwrap().len(), 1);
    assert!(writes.try_recv().is_err());
    assert_eq!(launcher.active.load(Ordering::SeqCst), 0);
    state.shutdown_services().await.unwrap();
}

#[tokio::test]
async fn shutdown_between_worker_start_and_wake_handoff_leaves_committed_input_recoverable() {
    let fixture = Fixture::new().await;
    fixture.enqueue(0, "before-wake-handoff").await;
    let (launcher, mut writes) = scripted_with_unknown_writes(false);
    let mut registry = AccountRegistry::new(
        fixture.store.clone(),
        launcher.clone(),
        test_registry_config(),
    )
    .await
    .unwrap();
    registry.start("default").await.unwrap();
    tokio::time::timeout(TEST_HANG_GUARD, launcher.ready_notice.notified())
        .await
        .expect("provider initialization hang guard");
    assert_eq!(
        fixture.event_status("annotation:before-wake-handoff"),
        "pending"
    );
    tokio::time::timeout(TEST_HANG_GUARD, registry.shutdown())
        .await
        .expect("initial registry shutdown hang guard")
        .unwrap();
    assert!(writes.try_recv().is_err());
    assert_eq!(launcher.active.load(Ordering::SeqCst), 0);
    let mut restarted = AccountRegistry::new(
        fixture.store.clone(),
        launcher.clone(),
        test_registry_config(),
    )
    .await
    .unwrap();
    restarted.reconcile(None).await.unwrap();
    assert_eq!(
        tokio::time::timeout(TEST_HANG_GUARD, writes.recv())
            .await
            .expect("provider write hang guard")
            .unwrap()
            .0,
        "account:default"
    );
    wait_for_durable_event_status(&fixture, "annotation:before-wake-handoff", "delivered").await;
    wait_for_durable_agent_status(
        &fixture,
        fixture.agents[0]["id"].as_str().unwrap(),
        "completed",
    )
    .await;
    tokio::time::timeout(TEST_HANG_GUARD, restarted.shutdown())
        .await
        .expect("restarted registry shutdown hang guard")
        .unwrap();
    assert!(writes.try_recv().is_err());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "../src/runtime_shutdown.rs"]
mod runtime_shutdown;

/// Keep the ordinary account-worker setup, replacing only its provider port
/// with the real supervisor adapter connected to a deterministic Unix peer.
async fn fixture_supervisor_store(
    root: &std::path::Path,
    account: &str,
    seed: i64,
) -> Arc<RuntimeStore> {
    static NEXT_ACCOUNT_STATE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = root.join(format!(
        "account-state-{}",
        NEXT_ACCOUNT_STATE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir(&path).unwrap();
    let store = Arc::new(RuntimeStore::open(Some(&path)).await.unwrap());
    let account = account.to_owned();
    store
        .transaction(move |tx| {
            studio_runtime_store::account_state::ensure_account(tx, &account)?;
            tx.execute(
                "UPDATE runtime_account_state SET next_generation=?1 WHERE account=?2",
                rusqlite::params![seed, account],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    store
}

struct SupervisorDetachLauncher {
    store: Arc<RuntimeStore>,
    scripted: Arc<Scripted>,
    root: PathBuf,
}

struct CallbackRecoveryLauncher(Arc<Scripted>);
struct CallbackRecoveryFactory(Arc<dyn ProviderTaskFactory>);

impl ProviderTaskFactory for CallbackRecoveryFactory {
    fn open(
        &self,
        handle: ProviderHandle,
        options: OpenOptions,
        shutdown: Shutdown,
    ) -> OpenFuture<'_> {
        Box::pin(async move {
            self.0
                .open(handle, options, shutdown)
                .await
                .map(ProviderAttachment::with_supervisor_write_deduplication)
        })
    }
}

impl AccountLauncher for CallbackRecoveryLauncher {
    fn launch(&self, account: String) -> LaunchFuture<'_> {
        Box::pin(async move {
            let mut launch = self.0.launch(account).await?;
            launch.factory = Arc::new(CallbackRecoveryFactory(launch.factory));
            Ok(launch)
        })
    }
}

#[tokio::test]
async fn callback_only_saved_reply_starts_its_account_without_any_pending_input() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut changes) = store_with_change_signal(root.path()).await;
    let response =
        json!({"id":"saved-tool-reply","result":{"existingChild":"created-once","success":true}});
    let encoded = format!("{response}\n");
    let saved = json!({"version":1,"handle":"account:default","generation":7,"pid":1,
        "frame":encoded,"sha256":studio_runtime_store::sha256_hex(encoded.as_bytes()),
        "authority":{"context":{"agent":"managed-actor","account":"default","stream_id":"default","connection":"original-connection",
            "generation":7,"epoch":0,"thread":"retained-thread","turn":"retained-turn"},"root":"managed-actor","provider":"claude",
            "parent":null,"role":"implementer","isLead":true,"executionMode":null,"moving":false}});
    let saved_response = response.clone();
    store.transaction(move |tx| {
        let actor = json!({"compactions":0,"id":"managed-actor","rootId":"managed-actor","epoch":0,"status":"running","autoWake":true,"inFlight":true,
            "provider":"claude","accountKey":"default","threadId":"retained-thread","turnId":"retained-turn","parentId":null,"role":"implementer","isLead":true,
            "turnEpoch":0,"startAttempt":{"id":"retained-attempt","epoch":0,"accountKey":"default",
                "threadId":"retained-thread","turnId":"retained-turn","submitted":true,"created":1.0,
                "events":["delivered-input"],"activeAtReservation":false,"settingsFixed":true,
                "requestTicket":"retained-start","connectionGeneration":7,"connectionId":"reattached-connection"}});
        tx.execute("INSERT INTO runtime_agents(id,record) VALUES ('managed-actor',?1)", [actor.to_string()])?;
        tx.execute("INSERT INTO runtime_events(id,agent,epoch,kind,text,status,created,turn_id) VALUES ('delivered-input','managed-actor',0,'user','original input','delivered',1.0,'retained-turn')", [])?;
        tx.execute("INSERT INTO runtime_account_state(account,next_generation,journal_ack,recovery_required,streams) VALUES ('default',7,0,0,'null') ON CONFLICT(account) DO UPDATE SET next_generation=7", [])?;
        let accepted = json!({"account":"default","journalScope":"default","generation":7,"cursor":1,
            "requestId":"saved-tool-reply","method":"item/tool/call",
            "frame":"{\"id\":\"saved-tool-reply\",\"method\":\"item/tool/call\",\"params\":{\"threadId\":\"retained-thread\",\"turnId\":\"retained-turn\",\"callId\":\"saved-spawn\",\"tool\":\"orchestration_spawn\",\"arguments\":{}}}\n",
            "actor":null,"epoch":null});
        use studio_runtime_providers::{decode_journal_value, DecodedJournalPayload};
        use studio_runtime_services::provider_callbacks::{self, CallbackContext, FrameAdmission};
        use studio_runtime_schema::persistence::{RequestIdentity, ToolRequestKind, ToolRequestStatus};
        let context = CallbackContext { agent: "managed-actor".into(), account: "default".into(), stream_id: "default".into(), connection: "original-connection".into(), generation: 7, epoch: 0, thread: "retained-thread".into(), turn: "retained-turn".into() };
        let request: Value = serde_json::from_str(accepted["frame"].as_str().unwrap()).unwrap();
        let DecodedJournalPayload::Frame(request) = decode_journal_value(1, 7, &request).payload else { panic!("request frame") };
        let DecodedJournalPayload::Frame(reply) = decode_journal_value(1, 7, &saved_response).payload else { panic!("response frame") };
        let identity = provider_callbacks::save_frame_response(tx, &context, &request, &reply).unwrap();
        assert_eq!(provider_callbacks::begin_frame(tx, "default", 7, &identity.operation_id(), &reply).unwrap(), FrameAdmission::Send);
        let key = provider_callbacks::callback_origin_key(&context, &identity.rpc_id);
        let owner: String = tx.query_row("SELECT id FROM runtime_tool_requests WHERE origin_key=?1", [&key], |row| row.get(0))?;
        let task_id = RequestIdentity::issue_root();
        let task = json!({"id":task_id,"kind":ToolRequestKind::ProviderCallback,"originKey":"callback:default:7:1","status":ToolRequestStatus::Running,"updated":1,"payload":{"accepted":accepted,"submitted":{"handle":saved["handle"],"generation":7,"pid":1,"response_key":key,"sha256":saved["sha256"],"authority":saved["authority"]},"recovery_authorized":true,"recovery_attempted":false,"request_fingerprint":studio_runtime_store::sha256_hex(accepted["frame"].as_str().unwrap().as_bytes()),"root_request_id":owner}});
        tx.execute("INSERT INTO runtime_tool_requests(id,record) VALUES (?1,?2)", [&task_id, &task.to_string()])?;
        Ok(())
    }).await.unwrap();
    let db = store.test_reader().unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM runtime_events WHERE status='pending'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        account_registry::target_page(&db, "", &BTreeMap::new())
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["default"],
        "the callback itself supplies durable account demand"
    );
    drop(db);
    let (scripted, mut writes) = scripted_with_unknown_writes(false);
    scripted
        .retained_history
        .set(("retained-thread".into(), "retained-turn".into()))
        .unwrap();
    let mut registry = AccountRegistry::new(
        store.clone(),
        Arc::new(CallbackRecoveryLauncher(scripted)),
        test_registry_config(),
    )
    .await
    .unwrap();
    registry.reconcile(None).await.unwrap();
    let progress = async {
        loop {
            let reader = store.test_reader().unwrap();
            let refused: bool = reader.query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_tool_requests WHERE origin_key='callback:default:7:1' AND status='unknown')", [], |row| row.get(0)).unwrap();
            assert!(
                !refused,
                "the saved callback must retain valid actor authority through startup recovery"
            );
            drop(reader);
            tokio::select! {
                write = writes.recv() => break write,
                changed = changes.changed() => changed.unwrap(),
            }
        }
    };
    let (handle, delivered) = tokio::time::timeout(TEST_HANG_GUARD, progress)
        .await
        .unwrap_or_else(|_| {
            let db = store.test_reader().unwrap();
            let phases: Vec<(String, String)> = db
                .prepare("SELECT origin_key,record FROM runtime_tool_requests WHERE origin_key='callback:default:7:1'")
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            let actor: String = db.query_row("SELECT record FROM runtime_agents WHERE id='managed-actor'", [], |row| row.get(0)).unwrap();
            panic!("the saved callback reply is missing; phases={phases:?}, actor={actor}")
        })
        .unwrap();
    assert_eq!(handle, "account:default");
    assert_eq!(
        delivered, response,
        "send only the existing result, never the spawn request"
    );
    tokio::time::timeout(TEST_HANG_GUARD, async {
        loop {
            let remaining = store.test_reader().unwrap().query_row(
                "SELECT count(*) FROM runtime_outbox WHERE kind='provider_callback' AND account='default'", [], |row| row.get::<_, i64>(0)).unwrap();
            if remaining == 0 { break; }
            changes.changed().await.unwrap();
        }
    }).await.unwrap();
    let actor: String = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT record FROM runtime_agents WHERE id='managed-actor'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let actor: Value = serde_json::from_str(&actor).unwrap();
    assert_eq!(actor["startAttempt"]["id"], "retained-attempt");
    assert_eq!(actor["status"], "running");
    assert_eq!(actor["autoWake"], true);
    assert_eq!(actor["inFlight"], true);
    registry.shutdown().await.unwrap();
    assert!(writes.try_recv().is_err());
}

impl AccountLauncher for SupervisorDetachLauncher {
    fn launch(&self, account: String) -> LaunchFuture<'_> {
        Box::pin(async move {
            let mut launch = self.scripted.launch(account).await?;
            launch.options.expected_generation = None;
            launch.factory = Arc::new(
                studio_runtime_process::provider_task::SupervisorProviderTaskFactory::new(
                    studio_runtime_process::BackendIdentity {
                        backend_id: studio_runtime_process::BackendId("detach-budget".into()),
                        state_dir: self.root.clone(),
                    },
                )
                .with_account_store(self.store.clone()),
            );
            Ok(launch)
        })
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn backend_shutdown_reports_both_supervisor_detach_timeouts() {
    use tokio::io::{AsyncReadExt, BufReader};

    tokio::time::timeout(TEST_HANG_GUARD, async {
        let fixture = Fixture::new().await;
        fixture.enqueue(0, &studio_runtime_schema::persistence::RequestIdentity::issue_root()).await;
        let listener =
            tokio::net::UnixListener::bind(fixture._root.path().join("supervisor.sock")).unwrap();
        let (scripted, _writes) = scripted_with_unknown_writes(false);
        let runtime = host_runtime::HostRuntime::with_launcher(
            fixture.store.clone(),
            Arc::new(SupervisorDetachLauncher {
                store: fixture_supervisor_store(fixture._root.path(), "default", 6).await,
                scripted,
                root: fixture._root.path().into(),
            }),
            Arc::new(Notify::new()),
            test_registry_config(),
        )
        .await
        .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let mut control = BufReader::new(socket);
        wire_read(&mut control).await;
        wire_send(&mut control, json!({"ok":true,"protocol":3})).await;
        let status = wire_read(&mut control).await;
        assert_eq!(status["action"], "status");
        wire_send(
            &mut control,
            json!({"requestId":status["requestId"],"error":"Unknown supervisor handle"}),
        )
        .await;
        let open = wire_read(&mut control).await;
        assert_eq!(open["action"], "open");
        wire_send(
            &mut control,
            json!({"requestId":open["requestId"],"result":{"pid":1,"generation":7,"resumed":false,"sequence":0,"acknowledged":0}}),
        )
        .await;
        let (socket, _) = listener.accept().await.unwrap();
        let mut events = BufReader::new(socket);
        wire_read(&mut events).await;
        wire_send(&mut events, json!({"ok":true,"protocol":3})).await;
        assert_eq!(wire_read(&mut events).await["action"], "nextBatch");
        let state = studio_runtime_http::HttpState::new(
            "token".into(),
            "fixture".into(),
            fixture._root.path().display().to_string(),
            4620,
            None,
            Some(fixture.store.clone()),
            None,
        );
        let closing = tokio::spawn(async move {
            runtime_shutdown::close(runtime, &state)
                .await
                .map_err(|error| error.to_string())
        });
        assert_eq!(events.read(&mut [0]).await.unwrap(), 0);
        // Neither peer confirms ownership release. Advance past the intended
        // detach budget, then resume the clock for real SQLite worker cleanup.
        tokio::time::pause();
        tokio::time::advance(
            studio_runtime_process::provider_task::SUPERVISOR_DETACH_TIMEOUT * 2,
        )
        .await;
        tokio::time::resume();
        let error = closing.await.unwrap().unwrap_err();
        assert!(
            error.contains("supervisor detach was not confirmed"),
            "backend lost the adapter cleanup receipt: {error}"
        );
        assert_eq!(
            error.matches(&AdapterError::Timeout.to_string()).count(),
            2,
            "both socket detach timeouts must reach the backend: {error}"
        );
        // A command already in flight may precede the control FIN.
        tokio::io::copy(&mut control, &mut tokio::io::sink())
            .await
            .unwrap();
    })
    .await
    .expect("backend detach timeout regression hang guard");
}

#[tokio::test]
async fn recovery_selection_matches_frozen_scheduler_roster_without_changing_stored_records() {
    let fixture = Fixture::build(crate::fixture_support::FixtureKind::Recovery).await;
    let selected = account_registry::target_page(
        &fixture.store.test_reader().expect("reader"),
        "",
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(
        json!(selected.keys().collect::<Vec<_>>()),
        fixture.agents["selected"]
    );
    let db = fixture.store.test_reader().unwrap();
    let mut query = db.prepare("SELECT id,record FROM runtime_agents").unwrap();
    let mut stored = query
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .map(|row| {
            let (id, raw) = row.unwrap();
            let mut record = serde_json::from_str::<Value>(&raw).unwrap();
            studio_runtime_store::agent_volatile::merge(&db, &id, &mut record).unwrap();
            (id, record)
        })
        .collect::<BTreeMap<_, _>>();
    let mut expected = fixture.agents["stored"].clone();
    for record in expected
        .as_object_mut()
        .expect("agent records")
        .values_mut()
        .chain(stored.values_mut())
    {
        let fields = record.as_object_mut().expect("agent record");
        for key in studio_runtime_store::agent_volatile::AGENT_VOLATILE_FIELDS
            .into_iter()
            .chain(["parentId", "threadId", "turnId"])
        {
            if fields.get(key).is_some_and(Value::is_null) {
                fields.remove(key);
            }
        }
    }
    assert_eq!(json!(stored), expected);
    assert_eq!(fixture.agents["cases"], 9);
}

#[tokio::test]
async fn connection_loss_replaces_worker_before_delivering_new_input() {
    let fixture = Fixture::new().await;
    let worker_agent_id = fixture.agents[1]["id"].as_str().unwrap().to_owned();
    fixture.store.transaction(move |tx| {
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.accountKey','default','$.executionSettingsAccountKey','default') WHERE id=?1", [&worker_agent_id])?;
        Ok(())
    }).await.unwrap();
    fixture.enqueue(0, "unknown-before-disconnect").await;
    let (launcher, mut writes) = scripted_with_unknown_first_launch();
    let registry = AccountRegistry::new(
        fixture.store.clone(),
        launcher.clone(),
        test_registry_config(),
    )
    .await
    .unwrap();
    let wake = Arc::new(Notify::new());
    let stop = Shutdown::default();
    let task = tokio::spawn(registry.run(wake.clone(), Arc::new(Notify::new()), stop.clone()));
    let first = tokio::time::timeout(TEST_HANG_GUARD, writes.recv())
        .await
        .expect("initial provider write hang guard")
        .unwrap();
    assert_eq!(first.1["method"], "turn/start");
    launcher.stops.lock().unwrap()[0].cancel();
    tokio::time::timeout(TEST_HANG_GUARD, async {
        loop {
            if launcher.opened.lock().unwrap().len() == 2 {
                break;
            }
            launcher.opened_notice.notified().await;
        }
    })
    .await
    .expect("replacement worker hang guard");
    fixture.enqueue(1, "new-input-after-disconnect").await;
    wake.notify_one();
    // The first turn/start lost its acceptance, so its outcome is unknown when
    // the provider exits. The replacement worker checks that exact input in
    // native history once, as after a backend restart; the scripted provider
    // leaves the read unanswered, so the input stays uncertain and unsent.
    let mut history_reads = 0;
    let second = loop {
        let write = tokio::time::timeout(TEST_HANG_GUARD, writes.recv())
            .await
            .expect("post-disconnect provider write hang guard")
            .unwrap();
        if write.1["method"] != "thread/turns/list" {
            break write;
        }
        assert_eq!(write.1["params"]["threadId"], first.1["params"]["threadId"]);
        history_reads += 1;
    };
    assert_eq!(second.0, "account:default");
    assert_eq!(second.1["method"], "turn/start");
    assert_ne!(
        first.1["params"]["threadId"],
        second.1["params"]["threadId"]
    );
    wait_for_durable_event_status(
        &fixture,
        "annotation:new-input-after-disconnect",
        "delivered",
    )
    .await;
    wait_for_durable_agent_status(
        &fixture,
        fixture.agents[1]["id"].as_str().unwrap(),
        "completed",
    )
    .await;
    stop.cancel();
    tokio::time::timeout(TEST_HANG_GUARD, task)
        .await
        .expect("account registry shutdown hang guard")
        .unwrap()
        .unwrap();
    assert_eq!(launcher.opened.lock().unwrap().len(), 2);
    assert_eq!(launcher.active.load(Ordering::SeqCst), 0);
    history_reads += drain_history_reads(
        &mut writes,
        &first.1["params"]["threadId"],
        "unknown input must not be resent on replacement",
    );
    assert!(
        history_reads > 0,
        "the replacement worker checks the unknown input in native history"
    );
    assert_eq!(
        fixture.event_status("annotation:unknown-before-disconnect"),
        "uncertain"
    );
}

async fn seed_backlog(fixture: &Fixture, count: usize, each_account: bool) -> Vec<String> {
    let source = fixture.agents[0]["id"].as_str().unwrap().to_owned();
    fixture
        .store
        .transaction(move |tx| seed_backlog_rows(tx, &source, count, each_account))
        .await
        .unwrap()
}

fn seed_backlog_rows(
    tx: &rusqlite::Transaction<'_>,
    source: &str,
    count: usize,
    each_account: bool,
) -> Result<Vec<String>, StoreError> {
    // One recursive insert per table keeps this 4,100-record paging fixture
    // from recompiling and executing two SQL statements for every row.
    let sequence = "WITH RECURSIVE seq(i) AS (
                SELECT 0 WHERE ?1 > 0
                UNION ALL SELECT i + 1 FROM seq WHERE i + 1 < ?1
            ), source(record) AS (
                SELECT record FROM runtime_agents WHERE id = ?2
            )";
    // Seed the current split storage format; migration is not under test.
    let record: String = tx.query_row(
        "SELECT record FROM runtime_agents WHERE id=?1",
        [&source],
        |row| row.get(0),
    )?;
    tx.execute(
            &format!("{sequence}
                INSERT INTO runtime_agents(id, record)
                SELECT printf('00000000-0000-4000-8000-%012d', seq.i),
                       json_set(?4, '$.id', printf('00000000-0000-4000-8000-%012d', seq.i),
                                '$.accountKey', CASE WHEN ?3 THEN printf('page-%06d', seq.i) ELSE 'default' END,
                                '$.executionSettingsAccountKey', CASE WHEN ?3 THEN printf('page-%06d', seq.i) ELSE 'default' END,
                                '$.contextRepairWait', 'blocked')
                FROM seq CROSS JOIN source"),
            (count as i64, source, each_account, record),
        )?;
    let columns = studio_runtime_store::agent_volatile::AGENT_VOLATILE_FIELDS
        .map(|field| format!("\"{field}\""))
        .join(",");
    tx.execute(
        &format!(
            "{sequence} INSERT INTO runtime_agent_volatile(id,{columns})
                SELECT printf('00000000-0000-4000-8000-%012d', seq.i), {columns}
                FROM seq CROSS JOIN runtime_agent_volatile WHERE id=?2"
        ),
        (count as i64, source),
    )?;
    tx.execute(
        &format!(
            "{sequence}
                INSERT INTO runtime_events(id, agent, kind, text, status, created, epoch)
                SELECT printf('00000000-0000-4000-8000-%012d', seq.i),
                       printf('00000000-0000-4000-8000-%012d', seq.i),
                       'user', 'fixture', 'pending', 0, json_extract(source.record, '$.epoch')
                FROM seq CROSS JOIN source"
        ),
        (count as i64, source),
    )?;
    Ok((0..count)
        .map(|i| format!("00000000-0000-4000-8000-{i:012}"))
        .collect())
}

// Paging is a read query: use the production Canvas catalog without writer actors
// or publishing thousands of agent entities unrelated to cursor selection.
fn backlog_database(count: usize, each_account: bool) -> (rusqlite::Connection, Vec<String>) {
    let mut db = studio_runtime_store::test_canvas();
    let tx = db.transaction().unwrap();
    let agent = studio_runtime_store::AgentRecord::from_value(
        json!({"compactions":0,"provider":studio_runtime_schema::ProviderKind::Codex,
            "id":"source", "rootId":"source", "epoch":1, "status":"queued",
            "autoWake":true, "accountKey":"default", "contextRepairWait":"blocked"
        }),
    )
    .unwrap();
    studio_runtime_store::AgentRepository::put(&tx, &agent).unwrap();
    let ids = seed_backlog_rows(&tx, "source", count, each_account).unwrap();
    tx.commit().unwrap();
    (db, ids)
}

#[test]
fn pending_backlog_pages_traverse_and_wrap() {
    let (db, expected) = backlog_database(65, false);
    let mut after = BTreeMap::new();
    let mut actual = Vec::new();
    while actual.len() < expected.len() {
        let page = account_registry::target_page(&db, "", &after).unwrap();
        let rows = &page["default"];
        assert!(rows.len() <= 32);
        actual.extend(rows.iter().map(|row| row.agent_id.clone()));
        after.insert("default".into(), rows.last().unwrap().agent_id.clone());
    }
    assert_eq!(actual, expected);
    let wrapped_agents = account_registry::target_page(
        &db,
        "",
        &BTreeMap::from([("default".into(), actual.last().unwrap().clone())]),
    )
    .unwrap();
    assert_eq!(
        wrapped_agents["default"]
            .iter()
            .map(|row| &row.agent_id)
            .collect::<Vec<_>>(),
        actual.iter().take(32).collect::<Vec<_>>()
    );
}

#[test]
fn pending_backlog_tail_beyond_the_historical_total_limit_is_reachable() {
    let (db, expected) = backlog_database(4100, false);
    let page = account_registry::target_page(
        &db,
        "",
        &BTreeMap::from([("default".into(), expected[4095].clone())]),
    )
    .unwrap();
    assert_eq!(
        page["default"]
            .iter()
            .map(|row| &row.agent_id)
            .collect::<Vec<_>>(),
        expected[4096..].iter().collect::<Vec<_>>()
    );
}

#[test]
fn account_backlog_pages_traverse_and_wrap() {
    let (db, _) = backlog_database(130, true);
    let first = account_registry::target_page(&db, "", &BTreeMap::new()).unwrap();
    assert_eq!(first.len(), 128);
    let second =
        account_registry::target_page(&db, first.last_key_value().unwrap().0, &BTreeMap::new())
            .unwrap();
    assert_eq!(second.len(), 2);
    assert!(second.keys().all(|account| !first.contains_key(account)));
    let wrapped_accounts =
        account_registry::target_page(&db, second.last_key_value().unwrap().0, &BTreeMap::new())
            .unwrap();
    assert_eq!(wrapped_accounts.len(), 128);
    assert_eq!(
        wrapped_accounts.first_key_value().unwrap().0,
        first.first_key_value().unwrap().0
    );
}

#[tokio::test]
async fn account_backlog_rejects_invalid_stored_account() {
    let fixture = Fixture::new().await;
    let source = fixture.agents[0]["id"].as_str().unwrap();
    let source = source.to_owned();
    fixture.store.transaction(move |tx| {
        let raw: String = tx.query_row("SELECT record FROM runtime_agents WHERE id=?1", [&source], |row| row.get(0))?;
        tx.execute("INSERT INTO runtime_agents(id,record) VALUES('bad-account-key',json_set(?1,'$.id','bad-account-key','$.accountKey',7))", [raw])?;
        tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES('bad-account-key-event','bad-account-key','user','fixture','pending',0,json_extract((SELECT record FROM runtime_agents WHERE id='bad-account-key'),'$.epoch'))", [])?;
        Ok(())
    }).await.unwrap();
    assert!(matches!(
        account_registry::target_page(&fixture.store.test_reader().expect("reader"), "", &BTreeMap::new()),
        Err(WorkerError::Store(StoreError::InvalidStoredObject { table: "runtime_agents", id })) if id == "bad-account-key"
    ));
}

#[tokio::test]
async fn saturated_wake_lane_does_not_starve_eligible_agent_after_blocked_prefix() {
    let fixture = Fixture::new().await;
    seed_backlog(&fixture, 48, false).await;
    fixture.enqueue(0, "eligible-after-blocked-prefix").await;
    fixture.enqueue(1, "eligible-other-account").await;
    // Written receipts: a lost turn/start receipt closes the session by design,
    // and that worker exit would race this test's registry shutdown.
    let (launcher, mut writes) = scripted_with_unknown_writes(false);
    // An uninitialized default worker cannot drain its lane, so the registry
    // sees it full for the whole first pass and must still reach the second
    // account. Without the hold, the order of the two starts is a race.
    let release_default = launcher.hold_initialize("account:default");
    let registry = AccountRegistry::new(
        fixture.store.clone(),
        launcher.clone(),
        test_registry_config(),
    )
    .await
    .unwrap();
    let stop = Shutdown::default();
    let task = tokio::spawn(registry.run(
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        stop.clone(),
    ));
    let (account, request) = tokio::time::timeout(SATURATED_WAKE_TIMEOUT, writes.recv())
        .await
        .unwrap_or_else(|_| {
            let db = fixture.store.test_reader().unwrap();
            let state: String = db.query_row("SELECT json_object('status',json_extract(record,'$.status'),'error',json_extract(record,'$.error'),'hold',json_extract(record,'$.startOutcomeHold'),'start',json_extract(record,'$.startAttempt')) FROM runtime_agents WHERE id=?1",[fixture.agents[0]["id"].as_str().unwrap()],|r|r.get(0)).unwrap();
            panic!("fair wake hang guard: {state}")
        })
        .unwrap();
    assert_eq!(account, "account:second");
    assert_eq!(request["method"], "turn/start");
    release_default.notify_one();
    let (account, request) = tokio::time::timeout(SATURATED_WAKE_TIMEOUT, writes.recv())
        .await
        .unwrap_or_else(|_| panic!("eligible agent behind saturated queue was not started"))
        .unwrap();
    assert_eq!(account, "account:default");
    assert_eq!(request["method"], "turn/start");
    stop.cancel();
    tokio::time::timeout(SATURATED_WAKE_TIMEOUT, task)
        .await
        .expect("registry shutdown completes after saturated queue work")
        .unwrap()
        .unwrap();
    assert_eq!(launcher.opened.lock().unwrap().len(), 2);
    let db = fixture.store.test_reader().unwrap();
    let blocked: i64 = db.query_row("SELECT count(*) FROM runtime_events WHERE kind='user' AND text='fixture' AND status='pending'",[],|r|r.get(0)).unwrap();
    assert_eq!(blocked, 48);
}

#[tokio::test]
async fn registry_drains_strict_account_pages_after_one_global_wake() {
    use studio_runtime_services::account_start_gate::AgentStartWait;

    let fixture = Fixture::new().await;
    seed_backlog(&fixture, 130, true).await;
    let final_agent = "00000000-0000-4000-8000-000000000129";
    let provider_wait = serde_json::to_string(&AgentStartWait::ProviderSession {
        epoch: fixture.agents[0]["epoch"].as_i64().unwrap(),
        until: None,
    })
    .unwrap();
    fixture
        .store
        .transaction(move |tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_remove(record,'$.contextRepairWait') WHERE id=?1",
                [final_agent],
            )?;
            // These accounts remain unavailable on every attempt. Seed their
            // existing wait so this paging witness does not first publish 128
            // unrelated start-wait changes through the lifecycle pipeline.
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.startWait',json(?1))
                 WHERE json_extract(record,'$.accountKey') >= 'page-000000'
                   AND json_extract(record,'$.accountKey') < 'page-000128'",
                [provider_wait],
            )?;
            // Count source writes rather than relying on how quickly a host
            // can publish the deliberately unavailable first account page.
            tx.execute_batch(
                "CREATE TEMP TABLE strict_account_prefix_writes(count INTEGER NOT NULL);
                 INSERT INTO strict_account_prefix_writes VALUES(0);
                 CREATE TEMP TRIGGER strict_account_prefix_write
                 AFTER UPDATE ON main.runtime_agents
                 WHEN json_extract(NEW.record,'$.accountKey') >= 'page-000000'
                  AND json_extract(NEW.record,'$.accountKey') < 'page-000128'
                 BEGIN UPDATE strict_account_prefix_writes SET count=count+1; END;",
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let db = fixture.store.test_reader().unwrap();
    let first = account_registry::target_page(&db, "", &BTreeMap::new()).unwrap();
    assert_eq!(first.len(), 128);
    let second =
        account_registry::target_page(&db, first.last_key_value().unwrap().0, &BTreeMap::new())
            .unwrap();
    assert_eq!(
        second.keys().map(String::as_str).collect::<Vec<_>>(),
        ["page-000128", "page-000129"]
    );
    drop(db);

    // Written receipts: a lost turn/start receipt closes the session by design,
    // and that worker exit would race this test's registry shutdown.
    let (launcher, mut writes) = scripted_with_unknown_writes(false);
    let (attempted, mut attempts) = mpsc::unbounded_channel();
    launcher.launch_attempts.set(attempted).unwrap();
    // Every paged account except the last two fails to start on every attempt, so
    // the registry's retry timer cannot open another launch; the strict
    // continuation then reaches the eligible tail without a timer or another
    // store commit.
    launcher
        .unavailable
        .set(|account| {
            account.starts_with("page-") && !matches!(account, "page-000128" | "page-000129")
        })
        .expect("unavailable predicate is set once");
    let registry = AccountRegistry::new(
        fixture.store.clone(),
        launcher.clone(),
        test_registry_config(),
    )
    .await
    .unwrap();
    let stop = Shutdown::default();
    let task = tokio::spawn(registry.run(
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        stop.clone(),
    ));
    // Check strict forward progress at every account. A repeated page fails
    // immediately; slow but ordered traversal does not consume one total deadline.
    for index in 0..130 {
        let account = tokio::time::timeout(TEST_HANG_GUARD, attempts.recv())
            .await
            .expect("strict account continuation makes progress")
            .expect("registry retains the launcher");
        assert_eq!(account, format!("page-{index:06}"));
    }
    // The pass stores a blocked start for each of 128 accounts before it reaches
    // the tail, which takes 13 to 21 s in a debug build on a loaded host. No
    // timer can open the tail instead: a retry pass begins at the first page
    // again. So this wait is only the hang guard.
    let (account, request) = tokio::time::timeout(TEST_HANG_GUARD, writes.recv())
        .await
        .expect("strict account continuation reaches the eligible tail")
        .unwrap();
    assert_eq!(account, "account:page-000129");
    assert_eq!(request["method"], "turn/start");
    stop.cancel();
    tokio::time::timeout(TEST_HANG_GUARD, task)
        .await
        .expect("registry shutdown completes after strict account paging")
        .unwrap()
        .unwrap();
    assert_eq!(launcher.opened.lock().unwrap().len(), 2);
    assert_eq!(launcher.launches.load(Ordering::SeqCst), 2);
    assert_eq!(launcher.unavailable_attempts.load(Ordering::SeqCst), 128);
    let prefix_writes: i64 = fixture
        .store
        .transaction(|tx| {
            Ok(tx.query_row(
                "SELECT count FROM strict_account_prefix_writes",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(
        prefix_writes, 0,
        "one paging wake must not publish the unavailable prefix"
    );
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchFixture {
    command: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<String>,
}

fn launch_fixture(root: &std::path::Path, isolated: bool, supervised: bool) -> LaunchFixture {
    let data = match (supervised, isolated) {
        (true, false) => include_str!("fixtures/launch-supervisor-false.json"),
        (true, true) => include_str!("fixtures/launch-supervisor-true.json"),
        (false, false) => include_str!("fixtures/launch-direct-false.json"),
        (false, true) => include_str!("fixtures/launch-direct-true.json"),
    };
    serde_json::from_str(&data.replace(
        crate::fixture_support::STATE_PLACEHOLDER,
        &root.to_string_lossy(),
    ))
    .unwrap()
}

fn with_unload_override(fixture: &LaunchFixture) -> Vec<String> {
    let mut command = fixture.command.clone();
    const APP_SERVER_ARGUMENT_COUNT: usize = 4;
    command.splice(
        APP_SERVER_ARGUMENT_COUNT..APP_SERVER_ARGUMENT_COUNT,
        ["-c".into(), "thread_unload_delay_secs=0".into()],
    );
    command
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn rust_supervisor_binary() -> PathBuf {
    std::env::var_os("STUDIO_SUPERVISOR_BINARY")
        .or_else(|| option_env!("CARGO_BIN_EXE_codex-process-supervisor").map(Into::into))
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            panic!(
                "Rust supervisor integration requires the built codex-process-supervisor; run the fast Rust test recipe so STUDIO_SUPERVISOR_BINARY is set"
            )
        })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn spawn_rust_supervisor(root: &std::path::Path, stderr: &std::path::Path) -> std::process::Child {
    std::process::Command::new(rust_supervisor_binary())
        .arg("--state")
        .arg(root)
        .env("XDG_CACHE_HOME", root.join("cache"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(stderr).unwrap())
        .spawn()
        .unwrap_or_else(|error| panic!("could not start Rust supervisor test fixture: {error}"))
}

#[cfg(target_os = "linux")]
async fn provider_input_barrier(root: &std::path::Path, backend_id: &str) -> Value {
    use tokio::io::AsyncWriteExt;
    let mut peer = tokio::io::BufReader::new(
        tokio::net::UnixStream::connect(root.join("supervisor.sock"))
            .await
            .unwrap(),
    );
    wire_send(
        &mut peer,
        json!({"protocol":studio_runtime_process::SUPERVISOR_PROTOCOL_VERSION,"stateDir":root,"backendId":backend_id}),
    )
    .await;
    let attached = wire_read(&mut peer).await;
    assert!(
        attached["error"].is_null(),
        "supervisor attach failed: {attached}"
    );
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
async fn every_launch_signature_field_matches_frozen_appserver_contract() {
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
        let oracle = launch_fixture(root.path(), isolated, true);
        let launch = codex_launch(
            Some("/fixture/codex"),
            environment,
            home.to_string_lossy().into_owned(),
            isolated,
            root.path().into(),
        );
        let reattach_commands = launch.reattach_commands.clone();
        let listener = tokio::net::UnixListener::bind(root.path().join("supervisor.sock")).unwrap();
        let factory = studio_runtime_process::provider_task::SupervisorProviderTaskFactory::new(
            studio_runtime_process::BackendIdentity {
                backend_id: studio_runtime_process::BackendId("fixture".into()),
                state_dir: root.path().into(),
            },
        )
        .with_account_store(fixture_supervisor_store(root.path(), "default", 0).await);
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
        let hello = wire_read(&mut peer).await;
        assert_eq!(hello["protocol"], 3);
        assert_eq!(hello["stateDir"], root.path().to_string_lossy().as_ref());
        assert_eq!(hello["backendId"], "fixture");
        wire_send(&mut peer, json!({"ok":true,"protocol":3})).await;
        let status = wire_read(&mut peer).await;
        wire_send(
            &mut peer,
            json!({"requestId":status["requestId"],"error":"Unknown supervisor handle"}),
        )
        .await;
        let open = wire_read(&mut peer).await;
        assert_eq!(open["env"], json!(oracle.env));
        assert_eq!(open["cwd"], json!(oracle.cwd));
        assert_eq!(open["command"], json!(with_unload_override(&oracle)));
        let environment: BTreeMap<String, String> =
            serde_json::from_value(open["env"].clone()).unwrap();
        // The frozen pre-unload-override command remains eligible for reattachment.
        assert_eq!(reattach_commands, std::slice::from_ref(&oracle.command));
        assert_eq!(
            studio_runtime_store::supervisor_journal::launch_signature(
                &reattach_commands[0],
                &environment,
                open["cwd"].as_str()
            ),
            studio_runtime_store::supervisor_journal::launch_signature(
                &oracle.command,
                &oracle.env,
                oracle.cwd.as_deref()
            )
        );
        wire_send(&mut peer,json!({"requestId":open["requestId"],"result":{"resumed":false,"initResult":null,"acknowledged":0,"sequence":0,"generation":1,"returnCode":null,"pid":1}})).await;
        drop(preparing.await.unwrap());
    }
}

#[test]
fn direct_launch_command_and_environment_match_frozen_contract() {
    for isolated in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("codex");
        std::fs::create_dir(&home).unwrap();
        let environment = BTreeMap::from([
            ("HOME".into(), root.path().to_string_lossy().into_owned()),
            ("CODEX_HOME".into(), home.to_string_lossy().into_owned()),
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("CODEX_BACKEND_MARKER".into(), "backend-only".into()),
            ("OPENAI_API_KEY".into(), "fixture-openai-key".into()),
            ("CODEX_API_KEY".into(), "fixture-codex-key".into()),
            ("UNCHANGED".into(), "Unicode café".into()),
        ]);
        let oracle = launch_fixture(root.path(), isolated, false);
        let launch = codex_launch(
            Some("/fixture/codex"),
            environment,
            home.to_string_lossy().into_owned(),
            isolated,
            root.path().into(),
        );
        assert_eq!(launch.command, with_unload_override(&oracle));
        assert_eq!(
            launch.reattach_commands,
            std::slice::from_ref(&oracle.command)
        );
        assert_eq!(
            serde_json::to_value(launch.environment).unwrap(),
            json!(oracle.env)
        );
        assert_eq!(
            launch
                .cwd
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            oracle.cwd
        );
    }
}

/// The live install keeps Codex app-servers that earlier builds started with the
/// frozen commands (no thread unload override). The current host launch must
/// reattach each one with its own command: same PID and generation, never a
/// refusal for different launch settings and never a second child.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
#[allow(
    clippy::disallowed_methods,
    reason = "test fixture; `which` prints one line and exits at once"
)]
async fn running_app_server_started_with_the_frozen_command_reattaches() {
    use std::os::unix::fs::PermissionsExt;
    use studio_runtime_process::{HealthRequest, SupervisorClient};
    use studio_runtime_store::supervisor_journal::launch_signature;
    let directory = tempfile::Builder::new().prefix("rs-").tempdir().unwrap();
    // The supervisor compares the handshake state directory with its resolved
    // root; macOS temporary directories sit behind the /var symlink.
    let root = CanonicalPath::existing(directory.path())
        .unwrap()
        .to_path_buf();
    // A non-platform executable: macOS hides the launch environment of
    // platform binaries such as /bin/sh, so their reattach is unverifiable.
    let node = std::process::Command::new("which")
        .arg("node")
        .output()
        .unwrap();
    assert!(node.status.success());
    let node = String::from_utf8(node.stdout).unwrap().trim().to_owned();
    let executable = root.join("codex");
    std::fs::write(&executable, format!("#!{node}\nprocess.stdin.resume();\n")).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let identity = studio_runtime_process::BackendIdentity {
        backend_id: studio_runtime_process::BackendId("frozen-command".into()),
        state_dir: root.clone(),
    };
    let mut supervisor = {
        let identity = identity.clone();
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-server/tests/wave7_p01_registry",
            move || {
                let stderr = identity.state_dir.join("supervisor.log");
                let child = spawn_rust_supervisor(&identity.state_dir, &stderr);
                let mut fixture = SupervisorFixture {
                    child,
                    native: Vec::new(),
                };
                supervisor_ready::wait(&mut fixture.child, identity, &stderr);
                fixture
            },
        )
        .await
        .unwrap()
    };
    let factory =
        studio_runtime_process::provider_task::SupervisorProviderTaskFactory::new(identity.clone())
            .with_account_store(fixture_supervisor_store(&root, "default", 0).await);
    let attach = |handle: ProviderHandle, launch: ProviderLaunchSpec| {
        let factory = &factory;
        async move {
            let options = || OpenOptions {
                clock: SessionClock::default(),
                launch: launch.clone(),
                expected_generation: None,
            };
            let prepared = factory
                .prepare(handle.clone(), options(), Shutdown::default())
                .await?;
            let mut reopened = options();
            reopened.expected_generation = Some(prepared.generation());
            let mut attachment = prepared.open(handle, reopened, Shutdown::default()).await?;
            let info = attachment.status.borrow().info.clone().unwrap();
            drop(attachment.shutdown().await);
            Ok::<_, AdapterError>(info)
        }
    };
    for isolated in [false, true] {
        let account = if isolated { "isolated" } else { "default" };
        let home = root.join(account);
        std::fs::create_dir(&home).unwrap();
        let environment = BTreeMap::from([
            ("HOME".into(), root.to_string_lossy().into_owned()),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ]);
        let launch = codex_launch(
            executable.to_str(),
            environment,
            home.to_string_lossy().into_owned(),
            isolated,
            root.clone(),
        );
        // The command an earlier build started this account's app-server with.
        let mut frozen = launch_fixture(&root, isolated, true).command;
        frozen[0] = executable.to_string_lossy().into_owned();
        assert_ne!(frozen, launch.command);
        let mut earlier = launch.clone();
        earlier.command = frozen.clone();
        let handle = ProviderHandle::for_account(account);
        let started = attach(handle.clone(), earlier).await.unwrap();
        supervisor.native.push(started.pid);

        let reattached = attach(handle, launch.clone())
            .await
            .unwrap_or_else(|error| {
                panic!("the current Codex launch refused the {account} app-server: {error:?}")
            });
        assert!(reattached.resumed);
        assert_eq!(reattached.pid, started.pid);
        assert_eq!(reattached.generation, started.generation);
        let socket = root.join("supervisor.sock");
        let identity = identity.clone();
        let health = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-server/tests/wave7_p01_registry",
            move || {
                SupervisorClient::connect(&socket, identity)
                    .unwrap()
                    .health(HealthRequest::default())
                    .unwrap()
            },
        )
        .await
        .unwrap();
        let running = health
            .handles
            .iter()
            .find(|running| running.id == format!("account:{account}"))
            .unwrap();
        assert_eq!(running.pid, Some(started.pid));
        assert_eq!(
            running.signature,
            launch_signature(&frozen, &launch.environment, None),
            "the running app-server keeps its own command"
        );
    }
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-server/tests/wave7_p01_registry",
        move || drop(supervisor),
    )
    .await
    .unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn supervised_child_observes_explicit_and_inherited_working_directories() {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncBufReadExt;
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("provider.mts");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native-provider.mts"),
        &executable,
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let stderr = root.path().join("supervisor.log");
    let child = spawn_rust_supervisor(root.path(), &stderr);
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
        studio_runtime_process::provider_task::SupervisorProviderTaskFactory::new(identity)
            .with_account_store(fixture_supervisor_store(root.path(), "default", 0).await);
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
        let (stream, _) = tokio::time::timeout(TEST_HANG_GUARD, observation_listener.accept())
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
    std::fs::write(&registry,json!({"version":studio_runtime_schema::shared_values::ACCOUNT_REGISTRY_FORMAT_VERSION,"defaultAccountKey":"default","accounts":{"default":{"provider":studio_runtime_schema::ProviderKind::Codex,"id":"default","home":home,"label":"Fixture","source":"Codex CLI","status":"signedOut"}},"logins":{},"deleteReceipts":{}}).to_string()).unwrap();
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
    assert_eq!(
        driver.snapshot().unwrap().accounts[0].status.as_str(),
        Some("changed")
    );
}

#[test]
fn sign_in_dispatch_admits_launch_of_an_account_that_is_not_ready() {
    let root = tempfile::tempdir().unwrap();
    let login_home = root.path().join("accounts/login-fixture");
    std::fs::create_dir_all(&login_home).unwrap();
    let registry = root.path().join("accounts/registry.json");
    let write = |status: &str| {
        std::fs::write(
            &registry,
            json!({"version":studio_runtime_schema::shared_values::ACCOUNT_REGISTRY_FORMAT_VERSION,"defaultAccountKey":"default",
                "accounts":{
                    "default":{"provider":studio_runtime_schema::ProviderKind::Codex,"id":"default","home":root.path(),"label":"Fixture","source":"Codex CLI","status":"signedOut"},
                    "login-fixture":{"provider":studio_runtime_schema::ProviderKind::Codex,"id":"login-fixture","home":login_home,"label":"New account","source":"Codex Agents","accountId":null,"email":null,"plan":null,"status":"pending"}
                },
                "logins":{"fixture-request":{"requestId":"fixture-request","accountKey":"login-fixture","status":status,"createdAt":1.0}},
                "deleteReceipts":{}})
            .to_string(),
        )
        .unwrap();
        studio_runtime_services::account_driver::AccountDriver::open(&registry).unwrap()
    };
    let driver = write("starting");
    assert_eq!(
        host_runtime::account_home(&driver, "login-fixture").unwrap(),
        (login_home.to_string_lossy().into_owned(), true)
    );
    assert!(host_runtime::account_home(&driver, "default").is_err());
    for other in ["pending", "error", "cancelled"] {
        let driver = write(other);
        assert!(
            host_runtime::account_home(&driver, "login-fixture").is_err(),
            "{other}"
        );
    }
}

#[test]
fn claude_profile_is_not_refreshed_through_codex_account_home_admission() {
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
                organization_name: None,
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

#[cfg(unix)]
#[test]
fn claude_launch_uses_resolved_node_bridge_and_profile_cli_without_invoking_it() {
    use std::os::unix::fs::PermissionsExt;

    let root_guard = tempfile::tempdir().unwrap();
    // The launch resolves its Node executable, so the expectation names the
    // macOS temporary folder through /private/var, not the /var link.
    let root = CanonicalPath::existing(root_guard.path()).unwrap();
    let bridge_dir = root.join("bridge");
    let bin_dir = root.join("bin");
    let sdk = bridge_dir.join("node_modules/@anthropic-ai/claude-agent-sdk");
    std::fs::create_dir_all(&sdk).unwrap();
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::write(bridge_dir.join("bridge.mjs"), "// scripted fixture\n").unwrap();
    std::fs::write(sdk.join("package.json"), "{}\n").unwrap();
    let node = bin_dir.join("node");
    let claude = bin_dir.join("claude-fixture");
    for executable in [&node, &claude] {
        std::fs::write(executable, "#!/bin/sh\nexit 99\n").unwrap();
        std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let profile = json!({
        "provider":"claude", "status":"ready", "email":"fixture@example.test",
        "accountId":"claude:fixture@example.test",
        "_credentialIdentity":"claude:fixture@example.test",
        "claudeOptions":{"customModels":[], "launchArgs":"--chrome"}
    });
    let launch = claude_launch(
        &profile,
        &bridge_dir,
        &root,
        "claude-fixture-account",
        BTreeMap::from([
            (
                "STUDIO_NODE_BIN".into(),
                node.to_string_lossy().into_owned(),
            ),
            (
                "STUDIO_CLAUDE_BIN".into(),
                claude.to_string_lossy().into_owned(),
            ),
            ("PATH".into(), bin_dir.to_string_lossy().into_owned()),
            ("ANTHROPIC_API_KEY".into(), "fixture-only".into()),
        ]),
    )
    .unwrap();
    assert_eq!(launch.command[0], node.to_string_lossy());
    assert_eq!(
        launch.command[1],
        bridge_dir.join("bridge.mjs").to_string_lossy()
    );
    // Python's per-account server directory holds the bridge session snapshots,
    // so a Claude agent that Python started resumes after the cutover.
    assert_eq!(
        launch.command[2],
        root.join("account-servers/claude-fixture-account")
            .to_string_lossy()
    );
    assert_eq!(
        launch.environment["STUDIO_CLAUDE_BIN"],
        claude.to_string_lossy()
    );
    assert!(!launch.environment.contains_key("ANTHROPIC_API_KEY"));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "../../../packages/runtime-process/tests/support/supervisor_ready.rs"]
mod supervisor_ready;

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct SupervisorFixture {
    child: std::process::Child,
    native: Vec<u32>,
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for SupervisorFixture {
    #[allow(clippy::disallowed_methods)]
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
async fn host_start_reattaches_existing_launch_and_delivers_http_input() {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tower::ServiceExt;
    // Isolate the supervisor-mode environment without mutating process globals.
    if std::env::var_os("STUDIO_W7_P01_HOST_CHILD").is_none() {
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "wave7_p01_registry::host_start_reattaches_existing_launch_and_delivers_http_input",
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
    let fixture = Fixture::new().await;
    fixture.store.transaction(|tx| {
        tx.execute_batch("CREATE TABLE publication_audit(id TEXT PRIMARY KEY,payload TEXT NOT NULL); CREATE TRIGGER capture_publication AFTER INSERT ON runtime_outbox WHEN NEW.kind='publication' BEGIN INSERT OR REPLACE INTO publication_audit VALUES(NEW.id,NEW.payload); END;")?;
        Ok(())
    }).await.unwrap();
    let (accounts, home) = account_driver(fixture._root.path());
    std::fs::write(
        home.join("auth.json"),
        json!({"OPENAI_API_KEY":"fixture-host-key"}).to_string(),
    )
    .unwrap();
    let executable = fixture._root.path().join("provider.mts");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native-provider.mts"),
        &executable,
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let environment: BTreeMap<String, String> = std::env::vars().collect();
    let mut launch = codex_launch(
        Some(executable.to_str().unwrap()),
        environment.clone(),
        home.to_string_lossy().into_owned(),
        false,
        fixture._root.path().into(),
    );
    // Seed the supervisor with the pre-unload-override command to verify reattachment.
    launch.command = launch.reattach_commands.remove(0);
    let root = fixture._root.path().to_path_buf();
    let mut supervisor = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-server/tests/wave7_p01_registry",
        move || {
            let stderr = root.join("supervisor.log");
            let child = spawn_rust_supervisor(&root, &stderr);
            let mut fixture = SupervisorFixture {
                child,
                native: Vec::new(),
            };
            supervisor_ready::wait(
                &mut fixture.child,
                studio_runtime_process::BackendIdentity {
                    backend_id: studio_runtime_process::BackendId("reattach-fixture".into()),
                    state_dir: root,
                },
                &stderr,
            );
            fixture
        },
    )
    .await
    .unwrap();
    let mut peer = tokio::io::BufReader::new(
        tokio::net::UnixStream::connect(fixture._root.path().join("supervisor.sock"))
            .await
            .unwrap(),
    );
    wire_send(
        &mut peer,
        json!({"protocol":studio_runtime_process::SUPERVISOR_PROTOCOL_VERSION,"stateDir":fixture._root.path(),"backendId":"reattach-fixture"}),
    )
    .await;
    assert!(wire_read(&mut peer).await["error"].is_null());
    wire_send(&mut peer,json!({"requestId":1,"action":"open","handle":"account:default","generation":1,"command":launch.command,"env":launch.environment,"cwd":launch.cwd})).await;
    let opened = wire_read(&mut peer).await;
    assert!(
        opened["error"].is_null(),
        "supervisor fixture open failed: {opened}"
    );
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
        fixture._root.path().to_string_lossy().into_owned(),
        49171,
        None,
        Some(fixture.store.clone()),
        None,
    )
    .with_account_driver(accounts.clone());
    let runtime = start_host(
        &mut state,
        fixture.store.clone(),
        accounts.clone(),
        Some(executable.to_string_lossy().into_owned()),
        test_registry_config(),
    )
    .await
    .unwrap();
    let app = studio_runtime_http::router(state.clone()).unwrap();
    let body = json!({"room":fixture.agents[0]["id"],"id":studio_runtime_schema::persistence::RequestIdentity::issue_root(),"text":"host composition cutover"});
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
    let (mut reported, _) = tokio::time::timeout(TEST_HANG_GUARD, observer.accept())
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
    let terminal_agent = tokio::time::timeout(TEST_HANG_GUARD, async {
        loop {
            let row = fixture
                .store
                .test_reader()
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
    let publication_rows: Vec<String> = fixture
        .store
        .test_reader()
        .unwrap()
        .prepare("SELECT payload FROM publication_audit ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!publication_rows.is_empty(), "P06 effects are journaled");
    for row in publication_rows {
        let publication: Value = serde_json::from_str(&row).unwrap();
        assert_eq!(publication["account"], "default");
        assert!(
            publication["sequence"]
                .as_u64()
                .is_some_and(|value| value > 0)
        );
        assert!(publication["index"].as_u64().is_some());
        assert!(
            publication.get("effect").is_some() || publication["analyticsOverflow"].is_number()
        );
    }

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
    let launch = codex_launch(
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
    )
    .with_account_store(fixture.store.clone());
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
    provider_input_barrier(fixture._root.path(), "verify-cutover").await;
    let (mut barrier, _) = tokio::time::timeout(TEST_HANG_GUARD, observer.accept())
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
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-server/tests/wave7_p01_registry",
        move || drop(supervisor),
    )
    .await
    .unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[allow(clippy::disallowed_methods)]
async fn rust_direct_claude_launch_runs_the_scripted_bridge_turn() {
    use std::os::unix::fs::PermissionsExt;
    use tower::ServiceExt;
    if std::env::var_os("STUDIO_W8_CLAUDE_CHILD").is_none() {
        let temp = tempfile::tempdir().unwrap();
        let sdk = temp.path().join("scripted-sdk.mts");
        let loader = temp.path().join("sdk-loader.mjs");
        let loader_log = temp.path().join("loader.log");
        let node_log = temp.path().join("node.stderr");
        let fake_cli = temp.path().join("claude-fixture");
        let home = temp.path().join("home");
        let config = temp.path().join("claude-config");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            &fake_cli,
            "#!/bin/sh\n[ \"$1 $2 $3\" = \"auth status --json\" ] || exit 2\nprintf '{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"email\":\"test@example.test\",\"subscriptionType\":\"Claude Max\"}\\n'\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        let bridge_node_modules = CanonicalPath::existing(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../providers/apps/claude-bridge/node_modules"),
        )
        .unwrap();
        std::os::unix::fs::symlink(bridge_node_modules, temp.path().join("node_modules")).unwrap();
        std::fs::write(&sdk, include_str!("fixtures/claude-sdk.mts")).unwrap();
        std::fs::write(
            &loader,
            "import fs from 'node:fs'; import {registerHooks} from 'node:module'; import {pathToFileURL} from 'node:url'; fs.appendFileSync(process.env.STUDIO_TEST_CLAUDE_LOG,'loader-loaded\\n'); registerHooks({resolve(specifier,context,nextResolve){if(specifier==='@anthropic-ai/claude-agent-sdk'){fs.appendFileSync(process.env.STUDIO_TEST_CLAUDE_LOG,'sdk-resolved\\n');return {url:pathToFileURL(process.env.STUDIO_TEST_CLAUDE_SDK).href,shortCircuit:true};}return nextResolve(specifier,context);}});\n",
        )
        .unwrap();
        let node = std::process::Command::new("which")
            .arg("node")
            .output()
            .unwrap();
        assert!(node.status.success());
        let node = String::from_utf8(node.stdout).unwrap().trim().to_owned();
        let node_wrapper = temp.path().join("node-wrapper");
        std::fs::write(
            &node_wrapper,
            format!("#!/bin/sh\nexec {node} \"$@\" 2>>\"$STUDIO_TEST_CLAUDE_NODE_LOG\"\n"),
        )
        .unwrap();
        std::fs::set_permissions(&node_wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "wave7_p01_registry::rust_direct_claude_launch_runs_the_scripted_bridge_turn",
                "--nocapture",
            ])
            .env("STUDIO_W8_CLAUDE_CHILD", "1")
            .env("STUDIO_W8_CLAUDE_TEST_ROOT", temp.path())
            .env("STUDIO_TEST_CLAUDE_SDK", &sdk)
            .env("STUDIO_TEST_CLAUDE_LOG", &loader_log)
            .env("STUDIO_TEST_CLAUDE_NODE_LOG", &node_log)
            .env("STUDIO_CLAUDE_BIN", &fake_cli)
            .env("STUDIO_NODE_BIN", &node_wrapper)
            .env("CODEX_NODE", &node_wrapper)
            .env("NODE_OPTIONS", format!("--import={}", loader.display()))
            .env("CLAUDE_CONFIG_DIR", &config)
            .env("HOME", &home)
            .env_remove("CODEX_AGENTS_SUPERVISOR_MODE")
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        assert!(
            child.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        return;
    }

    let fixture = Fixture::new().await;
    let agent_id = fixture.agents[0]["id"].as_str().unwrap().to_owned();
    let transaction_agent_id = agent_id.clone();
    fixture
        .store
        .transaction_with(move |tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.provider','claude','$.model','default','$.threadId',NULL,'$.accountKey','default','$.executionSettingsAccountKey','default','$.nativeEffort',NULL,'$.autoWake',json('true'),'$.isLead',json('false'),'$.status','queued') WHERE id=?1",
                [&transaction_agent_id],
            )?;
            Ok::<_, StoreError>(())
        })
        .await
        .unwrap();
    let root = PathBuf::from(std::env::var("STUDIO_W8_CLAUDE_TEST_ROOT").unwrap());
    let account_home = root.join("account-home");
    std::fs::create_dir_all(&account_home).unwrap();
    let registry = root.join("accounts/registry.json");
    std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
    let identity = "claude:test@example.test";
    std::fs::write(
        &registry,
        json!({"version":studio_runtime_schema::shared_values::ACCOUNT_REGISTRY_FORMAT_VERSION,"defaultAccountKey":"default","accounts":{"default":{"id":"default","provider":"claude","home":account_home,"label":"Scripted Claude","source":"Claude Code","status":"ready","accountId":identity,"email":"test@example.test","plan":"Claude Max","_credentialIdentity":identity,"claudeOptions":{"configDir":std::env::var("CLAUDE_CONFIG_DIR").unwrap(),"customModels":[]}}},"logins":{},"deleteReceipts":{}}).to_string(),
    )
    .unwrap();
    let accounts =
        Arc::new(studio_runtime_services::account_driver::AccountDriver::open(registry).unwrap());
    let mut state = studio_runtime_http::HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        fixture._root.path().to_string_lossy().into(),
        49171,
        None,
        Some(fixture.store.clone()),
        None,
    )
    .with_account_driver(accounts.clone());
    let runtime = start_host(
        &mut state,
        fixture.store.clone(),
        accounts.clone(),
        None,
        test_registry_config(),
    )
    .await
    .unwrap();
    let app = studio_runtime_http::router(state.clone()).unwrap();
    let mut changes = state.sync_changes_handle().subscribe();
    let message_id = studio_runtime_schema::persistence::RequestIdentity::issue_root();
    assert_eq!(
        app.clone()
            .oneshot(direct_message_post(&agent_id, &message_id, "split-stream"))
            .await
            .unwrap()
            .status(),
        200
    );
    let completed = tokio::time::timeout(TEST_HANG_GUARD, async {
        loop {
            let raw = fixture
                .store
                .test_reader()
                .unwrap()
                .query_row(
                    "SELECT record FROM runtime_agents WHERE id=?1",
                    [&agent_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap();
            let record: Value = serde_json::from_str(&raw).unwrap();
            if record["status"] == "completed" && record["inFlight"] != true {
                break record;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap_or_else(|_| {
        let log = std::fs::read_to_string(std::env::var("STUDIO_TEST_CLAUDE_LOG").unwrap())
            .unwrap_or_default();
        let node_log = std::fs::read_to_string(std::env::var("STUDIO_TEST_CLAUDE_NODE_LOG").unwrap()).unwrap_or_default();
        panic!(
            "Claude Rust turn did not complete: {}; direct-generation={}; sdk-log={log:?}; node-stderr={node_log:?}",
            direct_agent_debug(&fixture.store, &agent_id),
            direct_generation(&fixture.store)
        );
    });
    assert_eq!(completed["status"], "completed");
    let transcript = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!("/api/transcript?id={agent_id}"))
                .header("host", "127.0.0.1:49171")
                .header("x-canvas-token", "fixture-token")
                .header(
                    studio_runtime_http::API_SCHEMA_HASH_HEADER,
                    studio_runtime_http::schema::api_schema_hash(),
                )
                .extension(axum::extract::ConnectInfo(
                    "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
                ))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(transcript.status(), 200);
    let transcript: Value = serde_json::from_slice(
        &axum::body::to_bytes(transcript.into_body(), 1_000_000)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(transcript["agent"]["status"], "completed");
    let rust_items = transcript["items"].as_array().unwrap();
    assert_eq!(transcript["agent"]["inFlight"], false);
    let normalize_ids = |items: &[Value], scope_id: &str| {
        items
            .iter()
            .map(|item| {
                let item_id = item["id"].as_str().unwrap();
                let id = item_id
                    .strip_prefix(&format!("{scope_id}:"))
                    .unwrap_or(item_id);
                json!({
                    "id": id,
                    "kind": item
                        .get("type")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| match item.get("role").and_then(Value::as_str) {
                            Some("user") => Some("userMessage".to_owned()),
                            Some("assistant") => Some("agentMessage".to_owned()),
                            _ => item.get("kind").and_then(Value::as_str).map(str::to_owned),
                        })
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                    "phase": item.get("phase").cloned().unwrap_or(Value::Null),
                    "text": item
                        .get("text")
                        .cloned()
                        .or_else(|| {
                            item["content"][0]["text"]
                                .as_str()
                                .map(|text| Value::String(text.to_owned()))
                        })
                        .unwrap_or(Value::Null),
                })
            })
            .collect::<Vec<_>>()
    };
    let mut expected: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/claude-stream-items.json")).unwrap();
    // The input includes runtime instructions; compare its complete persisted text.
    expected[0]["text"] = transcript["items"][0]["text"].clone();
    expected[0]["id"] = json!(message_id);
    assert_eq!(normalize_ids(rust_items, &agent_id), expected);
    // Human text and Studio instructions retain separate origins, but only
    // the final SDK message queries. The prefix's zero-turn receipt must not
    // complete this turn or produce a second assistant reply.
    let cwd = PathBuf::from(completed["cwd"].as_str().unwrap());
    let sdk_inputs: Vec<Value> = std::fs::read_to_string(cwd.join(".sdk-inputs"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(sdk_inputs.len(), 2);
    assert_eq!(sdk_inputs[0]["shouldQuery"], false);
    assert_eq!(sdk_inputs[0]["origin"], json!({"kind":"task-notification"}));
    assert_eq!(sdk_inputs[0]["isSynthetic"], true);
    assert_eq!(sdk_inputs[0]["text_not_typed"], true);
    assert!(sdk_inputs[1].get("shouldQuery").is_none());
    assert!(sdk_inputs[1].get("origin").is_none());
    assert!(sdk_inputs[1].get("isSynthetic").is_none());
    assert!(sdk_inputs[1].get("text_not_typed").is_none());
    assert_eq!(
        sdk_inputs[1]["message"]["content"],
        json!([{"type":"text","text":"split-stream"}])
    );
    assert_eq!(sdk_inputs[1]["uuid"], expected[0]["id"]);
    assert_ne!(sdk_inputs[0]["uuid"], sdk_inputs[1]["uuid"]);
    assert!(
        sdk_inputs[0]["message"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .trim_start()
            .starts_with("[Studio subagent concurrency,")
    );
    let answer = rust_items
        .iter()
        .find(|item| item["phase"] == "final_answer")
        .unwrap();
    assert_eq!(
        answer["id"].as_str().unwrap().rsplit(':').next(),
        Some("stream-blocks")
    );
    let queries = cwd.join(".queries");
    assert_eq!(
        std::fs::read_to_string(&queries).unwrap().lines().count(),
        1
    );
    let (_query_watcher, mut query_events) = watch_file_changes(&queries);
    assert_eq!(
        app.clone()
            .oneshot(direct_message_post(
                &agent_id,
                &studio_runtime_schema::persistence::RequestIdentity::issue_root(),
                "orchestration-peers",
            ))
            .await
            .unwrap()
            .status(),
        200
    );
    wait_for_durable_agent_status(&fixture, &agent_id, "completed").await;
    let managed_calls: i64 = fixture.store.test_reader().unwrap().query_row(
        "SELECT count(*) FROM runtime_tool_requests t JOIN request_results r ON r.id=json_extract(t.record,'$.admitted.id') WHERE json_extract(t.record,'$.agent')=?1 AND json_extract(t.record,'$.tool')='orchestration_peers' AND json_extract(t.record,'$.stage')='completed' AND json_extract(r.result,'$.reply.success')=1",
        [&agent_id],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(
        managed_calls, 0,
        "the successful managed read leaves no saved receipt after delivery"
    );
    // A loaded thread with unchanged settings keeps its live query for the
    // next turn. Changed thread settings resume it in a new query, which the
    // in-flight restart below observes.
    let settings_agent_id = agent_id.clone();
    fixture
        .store
        .transaction_with(move |tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.profileInstructions','Resume with these instructions.') WHERE id=?1",
                [&settings_agent_id],
            )?;
            Ok::<_, StoreError>(())
        })
        .await
        .unwrap();
    let restart_message_id = studio_runtime_schema::persistence::RequestIdentity::issue_root();
    assert_eq!(
        app.clone()
            .oneshot(direct_message_post(&agent_id, &restart_message_id, "wait",))
            .await
            .unwrap()
            .status(),
        200
    );
    wait_for_file_lines(&queries, 2, &mut query_events).await;
    runtime_shutdown::close(runtime, &state).await.unwrap();
    let mut restarted_state = state.clone();
    let restarted = start_host(
        &mut restarted_state,
        fixture.store.clone(),
        accounts,
        None,
        test_registry_config(),
    )
    .await
    .unwrap();
    let mut restart_changes = restarted_state.sync_changes_handle().subscribe();
    // Shutdown closes the bridge's query; whether the bridge persists the
    // aborted turn as interrupted before it exits is a race. An unknown input
    // is held, a confirmed interruption continues with a restart notice, and
    // an accepted turn without conclusive native history is paused. Shutdown
    // also drains the bridge's stdout, so the `turn/start` reply can reach
    // Studio before the exit; the start is then accepted, and a turn that
    // native history shows as completed (this fixture's SDK finishes an
    // aborted query successfully) is reconciled as finished. No outcome
    // resends the interrupted input.
    let recovered = tokio::time::timeout(TEST_HANG_GUARD, async {
        loop {
            let raw = fixture
                .store
                .test_reader()
                .unwrap()
                .query_row(
                    "SELECT record FROM runtime_agents WHERE id=?1",
                    [&agent_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap();
            let record: Value = serde_json::from_str(&raw).unwrap();
            let held = record["status"] == "queued"
                && record["autoWake"] == true
                && record["inFlight"] != true
                && record["restartRecovery"]["stage"] == "held"
                && record["contextRepairWait"]["scope"] == "native"
                && record["contextRepairWait"]["events"]
                    == json!([restart_message_id]);
            // A held input may also stop as interrupted without auto-wake when
            // the restart notice reaches the agent before native repair starts.
            let held_interrupted = record["status"] == "interrupted"
                && record["autoWake"] == false
                && record["inFlight"] != true
                && record["restartRecovery"]["stage"] == "held";
            let continued = record["restartRecovery"]["stage"] == "continued"
                && record["status"] == "completed"
                && record["inFlight"] != true;
            let finished = record["restartRecovery"]["stage"] == "finished"
                && record["status"] == "completed"
                && record["autoWake"] == true
                && record["inFlight"] != true
                && record["connectionRecovery"]["outcome"] == "completed";
            let unconfirmed = record["restartRecovery"]["stage"] == "finished"
                && record["status"] == "paused"
                && record["autoWake"] == false
                && record["inFlight"] != true
                && record["nativeFailureHold"] == true
                && record["lastCompletedTurnStatus"] == "uncertain";
            if held || held_interrupted || continued || finished || unconfirmed {
                break record;
            }
            restart_changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "in-flight Claude turn should be held, continued, finished, or paused as unconfirmed: {}",
            direct_agent_debug(&fixture.store, &agent_id)
        )
    });
    let second_event_status: String = fixture
        .store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT status FROM runtime_events WHERE id=?1",
            [&restart_message_id],
            |row| row.get(0),
        )
        .unwrap();
    runtime_shutdown::close(restarted, &restarted_state)
        .await
        .unwrap();
    let queries_started = std::fs::read_to_string(&queries).unwrap().lines().count();
    if recovered["restartRecovery"]["stage"] == "held" {
        assert_eq!(second_event_status, "uncertain");
        assert_eq!(queries_started, 2, "restart must not resend the turn");
    } else if recovered["restartRecovery"]["stage"] == "finished" {
        assert_eq!(second_event_status, "delivered");
        assert_eq!(queries_started, 2, "restart must not resend the turn");
        if recovered["status"] == "paused" {
            assert_eq!(
                recovered["error"]["message"],
                "Studio could not confirm the Claude turn outcome. Review the native transcript before continuing."
            );
            assert_eq!(
                recovered["nativeTurnError"]["turnId"],
                recovered["lastCompletedTurn"]
            );
            assert!(recovered["startAttempt"].is_null());
        }
    } else {
        assert_eq!(second_event_status, "delivered");
        let continued_events = recovered["startAttempt"]["events"].as_array().unwrap();
        assert_eq!(continued_events.len(), 1);
        assert!(
            continued_events[0]
                .as_str()
                .unwrap()
                .starts_with("restart:"),
            "the continuation carries only the restart notice: {continued_events:?}"
        );
        assert_eq!(
            queries_started, 3,
            "only the continuation starts a query; the input is not resent"
        );
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[allow(clippy::disallowed_methods)]
async fn direct_launch_mode_completes_http_turn_and_reaps_child() {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncReadExt;
    use tower::ServiceExt;
    if std::env::var_os("STUDIO_W7_P01_DIRECT_CHILD").is_none() {
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "wave7_p01_registry::direct_launch_mode_completes_http_turn_and_reaps_child",
                "--nocapture",
            ])
            .env("STUDIO_W7_P01_DIRECT_CHILD", "1")
            .env("STUDIO_W7_P01_COMPLETE_TURN", "1")
            .env_remove("CODEX_AGENTS_SUPERVISOR_MODE")
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
    let fixture = Fixture::new().await;
    let (accounts, home) = account_driver(fixture._root.path());
    std::fs::write(
        home.join("auth.json"),
        json!({"OPENAI_API_KEY":"fixture-host-key"}).to_string(),
    )
    .unwrap();
    let executable = fixture._root.path().join("provider.mts");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native-provider.mts"),
        &executable,
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let observer = tokio::net::UnixListener::bind(fixture._root.path().join("turn.sock")).unwrap();
    let mut state = studio_runtime_http::HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        fixture._root.path().to_string_lossy().into_owned(),
        49171,
        None,
        Some(fixture.store.clone()),
        None,
    )
    .with_account_driver(accounts.clone());
    let runtime = start_host(
        &mut state,
        fixture.store.clone(),
        accounts,
        Some(executable.to_string_lossy().into_owned()),
        test_registry_config(),
    )
    .await
    .unwrap();
    let app = studio_runtime_http::router(state.clone()).unwrap();
    let agent_id = fixture.agents[0]["id"].as_str().unwrap();
    let body = json!({"room":agent_id,"id":studio_runtime_schema::persistence::RequestIdentity::issue_root(),"text":"direct mode turn"});
    let mut changes = state.sync_changes_handle().subscribe();
    let post = || {
        axum::http::Request::builder()
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
            .unwrap()
    };
    assert_eq!(app.clone().oneshot(post()).await.unwrap().status(), 200);
    let (mut reported, _) = tokio::time::timeout(TEST_HANG_GUARD, observer.accept())
        .await
        .expect("direct provider receives turn/start")
        .unwrap();
    let mut bytes = Vec::new();
    reported.read_to_end(&mut bytes).await.unwrap();
    let started: Value = serde_json::from_slice(&bytes).unwrap();
    let pid = started["pid"].as_u64().unwrap() as u32;
    assert_eq!(started["request"]["method"], "turn/start");
    assert_eq!(started["count"], 1);
    let expected_priority =
        match studio_runtime_process::child_priority::ChildPriority::from_environment() {
            studio_runtime_process::child_priority::ChildPriority::Background => 5,
            studio_runtime_process::child_priority::ChildPriority::Normal => {
                let output = std::process::Command::new("ps")
                    .args(["-o", "ni=", "-p", &std::process::id().to_string()])
                    .output()
                    .expect("backend priority");
                assert!(output.status.success());
                String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .parse::<i64>()
                    .expect("numeric backend priority")
            }
        };
    assert_eq!(
        started["priority"].as_i64(),
        Some(expected_priority),
        "account provider uses the configured child priority"
    );
    let terminal = tokio::time::timeout(TEST_HANG_GUARD, async {
        loop {
            let row = fixture
                .store
                .test_reader()
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
    .expect("provider streamed completion reaches runtime state");
    assert_eq!(terminal["status"], "completed");
    let transcript = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!("/api/transcript?id={agent_id}"))
                .header("host", "127.0.0.1:49171")
                .header("x-canvas-token", "fixture-token")
                .header(
                    studio_runtime_http::API_SCHEMA_HASH_HEADER,
                    studio_runtime_http::schema::api_schema_hash(),
                )
                .extension(axum::extract::ConnectInfo(
                    "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
                ))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(transcript.status(), 200);
    let transcript: Value = serde_json::from_slice(
        &axum::body::to_bytes(transcript.into_body(), 1_000_000)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(
        transcript
            .to_string()
            .contains("fixture assistant response")
    );
    assert_eq!(app.clone().oneshot(post()).await.unwrap().status(), 200);
    assert_eq!(
        std::fs::read_to_string(fixture._root.path().join("turn-count")).unwrap(),
        "1",
        "duplicate HTTP post does not start another provider turn"
    );
    runtime_shutdown::close(runtime, &state).await.unwrap();
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "backend shutdown terminates and reaps its direct provider"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn direct_generation_and_journal_identity_survive_backend_restart() {
    use std::os::unix::fs::PermissionsExt;
    use tower::ServiceExt;
    if std::env::var_os("STUDIO_W7_P01_DIRECT_RESTART_CHILD").is_none() {
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "wave7_p01_registry::direct_generation_and_journal_identity_survive_backend_restart",
                "--nocapture",
            ])
            .env("STUDIO_W7_P01_DIRECT_RESTART_CHILD", "1")
            .env("STUDIO_W7_P01_COMPLETE_TURN", "1")
            .env_remove("CODEX_AGENTS_SUPERVISOR_MODE")
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
    let fixture = Fixture::new().await;
    fixture
        .store
        .transaction_with(|tx| {
            tx.execute_batch(
                "CREATE TABLE direct_journal_audit(id TEXT PRIMARY KEY,generation INTEGER NOT NULL,result TEXT NOT NULL);
                 CREATE TRIGGER capture_direct_journal AFTER INSERT ON runtime_outbox
                 WHEN NEW.kind='journal_entry' AND NEW.stream_id='default'
                 BEGIN INSERT OR REPLACE INTO direct_journal_audit VALUES(NEW.id,NEW.generation,NEW.payload); END;",
            )?;
            Ok::<(), StoreError>(())
        })
        .await
        .unwrap();
    std::fs::write(
        fixture._root.path().join("emit-generation-notification"),
        "1",
    )
    .unwrap();
    let launch_observer =
        tokio::net::UnixListener::bind(fixture._root.path().join("launch.sock")).unwrap();
    let turn_observer =
        tokio::net::UnixListener::bind(fixture._root.path().join("turn.sock")).unwrap();
    let (accounts, home) = account_driver(fixture._root.path());
    std::fs::write(
        home.join("auth.json"),
        json!({"OPENAI_API_KEY":"fixture-host-key"}).to_string(),
    )
    .unwrap();
    let executable = fixture._root.path().join("provider.mts");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native-provider.mts"),
        &executable,
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut state = studio_runtime_http::HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        fixture._root.path().to_string_lossy().into_owned(),
        49171,
        None,
        Some(fixture.store.clone()),
        None,
    )
    .with_account_driver(accounts.clone());
    let mut changes = state.sync_changes_handle().subscribe();
    let runtime = start_host(
        &mut state,
        fixture.store.clone(),
        accounts.clone(),
        Some(executable.to_string_lossy().into_owned()),
        test_registry_config(),
    )
    .await
    .unwrap();
    let app = studio_runtime_http::router(state.clone()).unwrap();
    let agent_id = fixture.agents[0]["id"].as_str().unwrap();
    assert_eq!(
        app.clone()
            .oneshot(direct_message_post(
                agent_id,
                &studio_runtime_schema::persistence::RequestIdentity::issue_root(),
                "first backend generation",
            ))
            .await
            .unwrap()
            .status(),
        200
    );
    let first_launch = read_direct_report(&launch_observer).await;
    assert_eq!(first_launch["count"], 1);
    assert_eq!(read_direct_report(&turn_observer).await["count"], 1);
    let first_generation = direct_generation(&fixture.store);
    wait_for_direct_journal(&fixture.store, first_generation, &mut changes).await;
    let first_rows = direct_journal_rows(&fixture.store, first_generation);
    assert!(!first_rows.is_empty(), "first provider wrote journal rows");
    wait_for_direct_completion(&fixture.store, agent_id, &mut changes).await;
    runtime_shutdown::close(runtime, &state).await.unwrap();
    assert!(
        !std::path::Path::new(&format!("/proc/{}", first_launch["pid"].as_u64().unwrap())).exists(),
        "first backend shutdown reaps its direct provider"
    );

    let second_agent_id = fixture.agents[1]["id"].as_str().unwrap();
    make_direct_agent_runnable(&fixture.store, second_agent_id, "default").await;
    let runtime = start_host(
        &mut state,
        fixture.store.clone(),
        accounts,
        Some(executable.to_string_lossy().into_owned()),
        test_registry_config(),
    )
    .await
    .unwrap();
    let app = studio_runtime_http::router(state.clone()).unwrap();
    assert_eq!(
        app.clone()
            .oneshot(direct_message_post(
                second_agent_id,
                &studio_runtime_schema::persistence::RequestIdentity::issue_root(),
                "restarted backend generation",
            ))
            .await
            .unwrap()
            .status(),
        200
    );
    let second_launch = read_direct_report(&launch_observer).await;
    assert_eq!(second_launch["count"], 2);
    let second_generation = direct_generation(&fixture.store);
    assert!(second_generation > first_generation);
    wait_for_direct_journal(&fixture.store, second_generation, &mut changes).await;
    let second_rows = direct_journal_rows(&fixture.store, second_generation);
    assert!(
        !second_rows.is_empty(),
        "restarted provider writes journal rows"
    );
    assert_ne!(
        first_rows, second_rows,
        "journal identities include distinct generations"
    );
    runtime_shutdown::close(runtime, &state).await.unwrap();
    assert!(
        !std::path::Path::new(&format!("/proc/{}", second_launch["pid"].as_u64().unwrap()))
            .exists(),
        "second backend shutdown reaps its direct provider"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn direct_child_sigkill_settles_unknown_without_resend() {
    use std::os::unix::fs::PermissionsExt;
    use tower::ServiceExt;
    if std::env::var_os("STUDIO_W7_P01_DIRECT_CRASH_CHILD").is_none() {
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "wave7_p01_registry::direct_child_sigkill_settles_unknown_without_resend",
                "--nocapture",
            ])
            .env("STUDIO_W7_P01_DIRECT_CRASH_CHILD", "1")
            .env_remove("CODEX_AGENTS_SUPERVISOR_MODE")
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
    let fixture = Fixture::new().await;
    std::fs::write(fixture._root.path().join("hang-first-turn"), "1").unwrap();
    let launch_observer =
        tokio::net::UnixListener::bind(fixture._root.path().join("launch.sock")).unwrap();
    let turn_observer =
        tokio::net::UnixListener::bind(fixture._root.path().join("turn.sock")).unwrap();
    let (accounts, home) = account_driver(fixture._root.path());
    std::fs::write(
        home.join("auth.json"),
        json!({"OPENAI_API_KEY":"fixture-host-key"}).to_string(),
    )
    .unwrap();
    let executable = fixture._root.path().join("provider.mts");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native-provider.mts"),
        &executable,
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut state = studio_runtime_http::HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        fixture._root.path().to_string_lossy().into_owned(),
        49171,
        None,
        Some(fixture.store.clone()),
        None,
    )
    .with_account_driver(accounts.clone());
    let runtime = start_host(
        &mut state,
        fixture.store.clone(),
        accounts,
        Some(executable.to_string_lossy().into_owned()),
        test_registry_config(),
    )
    .await
    .unwrap();
    let app = studio_runtime_http::router(state.clone()).unwrap();
    let agent_id = fixture.agents[0]["id"].as_str().unwrap();
    let body = json!({"room":agent_id,"id":studio_runtime_schema::persistence::RequestIdentity::issue_root(),"text":"crash in flight"});
    let post = || {
        axum::http::Request::builder()
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
            .unwrap()
    };
    assert_eq!(app.clone().oneshot(post()).await.unwrap().status(), 200);
    let first_launch = read_direct_report(&launch_observer).await;
    assert_eq!(first_launch["count"], 1);
    let crash = read_direct_report(&turn_observer).await;
    assert_eq!(crash["count"], 1);
    assert_eq!(crash["request"]["method"], "turn/start");
    let killed_pid = crash["pid"].as_u64().unwrap();
    assert!(
        tokio::process::Command::new("kill")
            .args(["-KILL", &killed_pid.to_string()])
            .status()
            .await
            .unwrap()
            .success()
    );
    let second_launch = read_direct_report(&launch_observer).await;
    assert!(
        !std::path::Path::new(&format!("/proc/{killed_pid}")).exists(),
        "the unresponsive provider is reaped before its replacement starts"
    );
    assert_eq!(
        second_launch["count"], 2,
        "registry replaces crashed provider"
    );
    let mut changes = state.sync_changes_handle().subscribe();
    let held = tokio::time::timeout(TEST_HANG_GUARD, async {
        loop {
            let raw = fixture
                .store
                .test_reader()
                .unwrap()
                .query_row(
                    "SELECT record FROM runtime_agents WHERE id=?1",
                    [agent_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap();
            let record: Value = serde_json::from_str(&raw).unwrap();
            // Unknown starts use the native-history hold introduced in 1987a373:
            // queued is a reconciliation state, not permission to resend input.
            if record["restartRecovery"]["startAttempt"]["executionOutcome"] == "unknown"
                && record["restartRecovery"]["stage"] == "held"
                && record["status"] == "queued"
                && record["error"].as_str().is_some_and(|error| {
                    error.starts_with("Context repair waits for a confirmed input receipt")
                })
            {
                break record;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "unknown direct provider write is held; state={}",
            direct_agent_debug(&fixture.store, agent_id)
        )
    });
    assert_eq!(held["autoWake"], true);
    assert_eq!(held["inFlight"], false);
    assert_eq!(held["contextRepairWait"]["events"], json!([body["id"]]));
    assert_eq!(
        fixture
            .store
            .test_reader()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM runtime_events WHERE id=?1 AND status='uncertain'",
                [body["id"].as_str().unwrap()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
    let generation = direct_generation(&fixture.store);
    assert!(generation >= 2);
    assert_eq!(
        std::fs::read_to_string(fixture._root.path().join("turn-count")).unwrap(),
        "1"
    );
    assert_eq!(app.clone().oneshot(post()).await.unwrap().status(), 200);
    assert_eq!(
        std::fs::read_to_string(fixture._root.path().join("turn-count")).unwrap(),
        "1"
    );
    runtime_shutdown::close(runtime, &state).await.unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn direct_child_crash_before_turn_write_retries_queued_input_once() {
    use std::os::unix::fs::PermissionsExt;
    use tower::ServiceExt;
    if std::env::var_os("STUDIO_W7_P01_DIRECT_NOT_SENT_CHILD").is_none() {
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "wave7_p01_registry::direct_child_crash_before_turn_write_retries_queued_input_once",
                "--nocapture",
            ])
            .env("STUDIO_W7_P01_DIRECT_NOT_SENT_CHILD", "1")
            .env("STUDIO_W7_P01_COMPLETE_TURN", "1")
            .env_remove("CODEX_AGENTS_SUPERVISOR_MODE")
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
    let fixture = Fixture::new().await;
    std::fs::write(fixture._root.path().join("crash-first-initialize"), "1").unwrap();
    let launch_observer =
        tokio::net::UnixListener::bind(fixture._root.path().join("launch.sock")).unwrap();
    let turn_observer =
        tokio::net::UnixListener::bind(fixture._root.path().join("turn.sock")).unwrap();
    let (accounts, home) = account_driver(fixture._root.path());
    std::fs::write(
        home.join("auth.json"),
        json!({"OPENAI_API_KEY":"fixture-host-key"}).to_string(),
    )
    .unwrap();
    let executable = fixture._root.path().join("provider.mts");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native-provider.mts"),
        &executable,
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut state = studio_runtime_http::HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        fixture._root.path().to_string_lossy().into_owned(),
        49171,
        None,
        Some(fixture.store.clone()),
        None,
    )
    .with_account_driver(accounts.clone());
    let app = studio_runtime_http::router(state.clone()).unwrap();
    let agent_id = fixture.agents[0]["id"].as_str().unwrap();
    let body = json!({"room":agent_id,"id":studio_runtime_schema::persistence::RequestIdentity::issue_root(),"text":"queued before provider ready"});
    let post = || {
        axum::http::Request::builder()
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
            .unwrap()
    };
    assert_eq!(app.clone().oneshot(post()).await.unwrap().status(), 200);
    let mut changes = state.sync_changes_handle().subscribe();
    let runtime = start_host(
        &mut state,
        fixture.store.clone(),
        accounts,
        Some(executable.to_string_lossy().into_owned()),
        test_registry_config(),
    )
    .await
    .unwrap();
    let first_launch = read_direct_report(&launch_observer).await;
    assert_eq!(first_launch["count"], 1);
    let second_launch = read_direct_report(&launch_observer).await;
    assert_eq!(
        second_launch["count"], 2,
        "registry replaces pre-write crash"
    );
    let turn = read_direct_report(&turn_observer).await;
    assert_eq!(turn["count"], 1);
    assert_eq!(turn["request"]["method"], "turn/start");
    let terminal = tokio::time::timeout(TEST_HANG_GUARD, async {
        loop {
            let raw = fixture
                .store
                .test_reader()
                .unwrap()
                .query_row(
                    "SELECT record FROM runtime_agents WHERE id=?1",
                    [agent_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap();
            let record: Value = serde_json::from_str(&raw).unwrap();
            if record["status"] == "completed" && record["inFlight"] != true {
                break record;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("queued input completes after the pre-write child crash");
    assert_eq!(terminal["status"], "completed");
    assert_eq!(
        std::fs::read_to_string(fixture._root.path().join("turn-count")).unwrap(),
        "1"
    );
    runtime_shutdown::close(runtime, &state).await.unwrap();
}

#[cfg(target_os = "linux")]
async fn read_direct_report(listener: &tokio::net::UnixListener) -> Value {
    use tokio::io::AsyncReadExt;
    let (mut socket, _) = tokio::time::timeout(TEST_HANG_GUARD, listener.accept())
        .await
        .expect("direct child report hang guard")
        .unwrap();
    let mut bytes = Vec::new();
    socket.read_to_end(&mut bytes).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[cfg(target_os = "linux")]
fn direct_message_post(
    agent_id: &str,
    id: &str,
    text: &str,
) -> axum::http::Request<axum::body::Body> {
    let body = json!({"room":agent_id,"id":id,"text":text});
    axum::http::Request::builder()
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
        .unwrap()
}

#[cfg(target_os = "linux")]
fn direct_generation(store: &RuntimeStore) -> u64 {
    let raw = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT connection_generation FROM runtime_account_state WHERE account='default'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    u64::try_from(raw).unwrap()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn make_direct_agent_runnable(store: &RuntimeStore, agent_id: &str, account: &str) {
    let agent_id = agent_id.to_owned();
    let account = account.to_owned();
    store
        .transaction_with(move |tx| {
            let raw: String = tx.query_row(
                "SELECT record FROM runtime_agents WHERE id=?1",
                [&agent_id],
                |row| row.get(0),
            )?;
            let mut record: Value = serde_json::from_str(&raw)
                .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
            record["accountKey"] = json!(account);
            record["status"] = json!("queued");
            record["autoWake"] = json!(true);
            tx.execute(
                "UPDATE runtime_agents SET record=?1 WHERE id=?2",
                [record.to_string(), agent_id],
            )?;
            Ok::<(), StoreError>(())
        })
        .await
        .unwrap();
}

#[cfg(target_os = "linux")]
fn direct_agent_debug(store: &RuntimeStore, agent_id: &str) -> String {
    let db = store.test_reader().unwrap();
    let raw = db
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [agent_id],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    let agent: Value = serde_json::from_str(&raw).unwrap();
    let events = db
        .prepare("SELECT id,status FROM runtime_events ORDER BY rowid")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    format!(
        "status={},account={},provider={},autoWake={},error={},attempt={},inFlight={},outcome={},recovery={},events={events:?}",
        agent["status"],
        agent["accountKey"],
        agent["provider"],
        agent["autoWake"],
        agent["error"],
        agent["startAttempt"],
        agent["inFlight"],
        agent["restartRecovery"]["startAttempt"]["executionOutcome"],
        agent["restartRecovery"]["stage"]
    )
}

#[cfg(target_os = "linux")]
fn direct_journal_rows(store: &RuntimeStore, generation: u64) -> Vec<(String, String)> {
    let db = store.test_reader().unwrap();
    let mut query = db
        .prepare("SELECT id,result FROM direct_journal_audit WHERE generation=?1 ORDER BY id")
        .unwrap();
    query
        .query_map([i64::try_from(generation).unwrap()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

#[cfg(target_os = "linux")]
async fn wait_for_direct_journal<T: Clone + Send + Sync + 'static>(
    store: &RuntimeStore,
    generation: u64,
    changes: &mut watch::Receiver<T>,
) {
    tokio::time::timeout(TEST_HANG_GUARD, async {
        loop {
            if !direct_journal_rows(store, generation).is_empty() {
                break;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("direct provider journal publication hang guard");
}

#[cfg(target_os = "linux")]
async fn wait_for_direct_completion<T: Clone + Send + Sync + 'static>(
    store: &RuntimeStore,
    agent_id: &str,
    changes: &mut watch::Receiver<T>,
) -> Value {
    tokio::time::timeout(TEST_HANG_GUARD, async {
        loop {
            let raw = store
                .test_reader()
                .unwrap()
                .query_row(
                    "SELECT record FROM runtime_agents WHERE id=?1",
                    [agent_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap();
            let record: Value = serde_json::from_str(&raw).unwrap();
            if record["status"] == "completed" && record["inFlight"] != true {
                break record;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "direct provider turn completion hang guard: {}",
            direct_agent_debug(store, agent_id)
        )
    })
}

/// Requests that the restart witness provider finished recording, in order.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn restart_provider_requests(root: &std::path::Path) -> Vec<Value> {
    let bytes = match std::fs::read(root.join("requests.jsonl")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => panic!("read restart provider request log: {error}"),
    };
    // The fixture's trailing newline publishes each record. A filesystem event
    // can expose an unfinished append, including a split UTF-8 character, so
    // decode only completed records and let the file watcher wake us again.
    bytes
        .split_inclusive(|byte| *byte == b'\n')
        .filter(|line| line.ends_with(b"\n"))
        .map(|line| serde_json::from_slice(line).expect("completed provider request is valid JSON"))
        .collect()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn restart_provider_requests_waits_for_the_record_newline() {
    use std::io::Write;

    let root = tempfile::tempdir().unwrap();
    assert!(restart_provider_requests(root.path()).is_empty());
    let mut writer = std::fs::File::create(root.path().join("requests.jsonl")).unwrap();
    let first = json!({"method":"turn/start","params":{"threadId":"first"}});
    writeln!(writer, "{first}").unwrap();
    assert_eq!(restart_provider_requests(root.path()), vec![first.clone()]);

    // A filesystem event can arrive before appendFileSync finishes its record.
    writer
        .write_all(br#"{"method":"turn/interrupt","params":{"threadId":"sec"#)
        .unwrap();
    assert_eq!(restart_provider_requests(root.path()), vec![first.clone()]);
    writer.write_all(br#"ond"}}"#).unwrap();
    assert_eq!(restart_provider_requests(root.path()), vec![first.clone()]);
    writer.write_all(b"\n").unwrap();
    assert_eq!(
        restart_provider_requests(root.path()),
        vec![
            first,
            json!({"method":"turn/interrupt","params":{"threadId":"second"}})
        ]
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn restart_provider_requests_keeps_completed_records_before_partial_utf8() {
    use std::io::Write;

    let root = tempfile::tempdir().unwrap();
    let mut writer = std::fs::File::create(root.path().join("requests.jsonl")).unwrap();
    let first = json!({"method":"turn/start"});
    writeln!(writer, "{first}").unwrap();
    writer.write_all(b"{\"text\":\"\xc3").unwrap();
    assert_eq!(restart_provider_requests(root.path()), vec![first.clone()]);
    writer.write_all(b"\xa9\"}\n").unwrap();
    assert_eq!(
        restart_provider_requests(root.path()),
        vec![first, json!({"text":"é"})]
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
#[should_panic(expected = "completed provider request is valid JSON")]
fn restart_provider_requests_rejects_a_malformed_completed_record() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("requests.jsonl"), b"{\"method\":\n").unwrap();
    restart_provider_requests(root.path());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn restart_provider_count(root: &std::path::Path, method: &str, thread: &str) -> usize {
    restart_provider_requests(root)
        .iter()
        .filter(|request| request["method"] == method && request["params"]["threadId"] == thread)
        .count()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn restart_agent(store: &RuntimeStore, id: &str) -> Value {
    let raw: String = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .unwrap();
    serde_json::from_str(&raw).unwrap()
}

/// The recovery fields of an agent record, for a failure message.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn restart_summary(agent: &Value) -> Value {
    json!({
        "status":agent["status"], "epoch":agent["epoch"], "autoWake":agent["autoWake"],
        "inFlight":agent["inFlight"], "turnId":agent["turnId"], "error":agent["error"],
        "startAttempt":agent["startAttempt"], "disconnectRecovery":agent["disconnectRecovery"],
        "restartRecovery":agent["restartRecovery"], "connectionRecovery":agent["connectionRecovery"],
        "lastCompletedTurn":agent["lastCompletedTurn"],
    })
}

/// Wait for committed store changes or provider file events, or report the hang guard.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn restart_wait(
    label: &str,
    fixture: &Fixture,
    mut ready: impl FnMut() -> bool,
    state: impl Fn() -> String,
) {
    let (_watcher, mut files) = watch_file_changes(&fixture._root.path().join("requests.jsonl"));
    let mut changes = fixture.changes.clone();
    let waited = tokio::time::timeout(TEST_HANG_GUARD, async {
        while !ready() {
            tokio::select! {
                result = files.recv() => { result.expect("provider file watcher remains open"); }
                result = changes.changed() => { result.expect("store observer remains open"); }
            }
        }
    })
    .await;
    if waited.is_err() {
        panic!("{label} hang guard\n{}", state());
    }
}

/// Every install restarts the backend while the process supervisor keeps
/// each account's provider process. One witness for the restart fixes
/// together, with the real supervisor and a scripted Codex app-server: agents
/// A and B run turns, the backend stops, the owner stops B while no backend
/// runs, and a new backend process attaches to the same provider generation.
/// A's turn keeps running without a false disconnect and completes on the new
/// backend, B's turn gets one interrupt and one command cleanup, and no input
/// is sent twice.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
#[allow(
    clippy::disallowed_methods,
    reason = "test fixture; `which` prints one line and exits at once"
)]
async fn backend_restart_keeps_live_work_and_ends_the_stopped_turn_once() {
    use std::os::unix::fs::PermissionsExt;
    if std::env::var_os("STUDIO_RESTART_FIXES_CHILD").is_none() {
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "wave7_p01_registry::backend_restart_keeps_live_work_and_ends_the_stopped_turn_once",
                "--nocapture",
            ])
            .env("STUDIO_RESTART_FIXES_CHILD", "1")
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
    let started = std::time::Instant::now();
    let directory = tempfile::Builder::new().prefix("rr-").tempdir().unwrap();
    // The supervisor compares the handshake state directory with its
    // resolved root; macOS temporary directories sit behind a symlink.
    let root = CanonicalPath::existing(directory.path())
        .unwrap()
        .to_path_buf();
    let (store, _changes) = store_with_change_signal(&root).await;
    let agents =
        crate::fixture_support::seed(&store, &root, crate::fixture_support::FixtureKind::Dispatch)
            .await;
    let a = agents[0]["id"].as_str().unwrap().to_owned();
    let b = agents[1]["id"].as_str().unwrap().to_owned();
    make_direct_agent_runnable(&store, &b, "default").await;
    // B runs on A's account, with that account's execution settings.
    let settings_agent = b.clone();
    store
        .transaction(move |tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.executionSettingsAccountKey','default') WHERE id=?1",
                [&settings_agent],
            )?;
            Ok::<_, StoreError>(())
        })
        .await
        .unwrap();
    let fixture = Fixture {
        _root: directory,
        store: store.clone(),
        agents: agents.clone(),
        changes: _changes,
        event_ids: Mutex::new(BTreeMap::new()),
    };
    fixture.enqueue(0, "a-input").await;
    fixture.enqueue(1, "b-input").await;
    let (accounts, home) = account_driver(&root);
    std::fs::write(
        home.join("auth.json"),
        json!({"OPENAI_API_KEY":"fixture-host-key"}).to_string(),
    )
    .unwrap();
    // A non-platform interpreter path: macOS hides the launch environment of
    // platform binaries such as /usr/bin/env, so their reattach is unverifiable.
    let node = std::process::Command::new("which")
        .arg("node")
        .output()
        .unwrap();
    assert!(node.status.success());
    let node = String::from_utf8(node.stdout).unwrap().trim().to_owned();
    let script = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/restart-provider.mts"),
    )
    .unwrap();
    let body = script.split_once('\n').map_or("", |(_, body)| body);
    let executable = root.join("provider.mts");
    std::fs::write(&executable, format!("#!{node}\n{body}")).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let supervisor_root = root.clone();
    let mut supervisor = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-server/tests/wave7_p01_registry",
        move || {
            let stderr = supervisor_root.join("supervisor.log");
            let child = spawn_rust_supervisor(&supervisor_root, &stderr);
            let mut fixture = SupervisorFixture {
                child,
                native: Vec::new(),
            };
            supervisor_ready::wait(
                &mut fixture.child,
                studio_runtime_process::BackendIdentity {
                    backend_id: studio_runtime_process::BackendId("restart-fixture".into()),
                    state_dir: supervisor_root,
                },
                &stderr,
            );
            fixture
        },
    )
    .await
    .unwrap();
    let state_text = |store: &RuntimeStore, root: &std::path::Path, a: &str, b: &str| {
        format!(
            "A: {}\nB: {}\nrequests: {:?}",
            restart_summary(&restart_agent(store, a)),
            restart_summary(&restart_agent(store, b)),
            restart_provider_requests(root)
                .iter()
                .map(|request| format!("{} {}", request["method"], request["params"]["threadId"]))
                .collect::<Vec<_>>()
        )
    };
    let mut state = studio_runtime_http::HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        root.to_string_lossy().into_owned(),
        49171,
        None,
        Some(store.clone()),
        None,
    )
    .with_account_driver(accounts.clone());

    // The first backend starts both turns.
    let runtime = start_host(
        &mut state,
        store.clone(),
        accounts.clone(),
        Some(executable.to_string_lossy().into_owned()),
        test_registry_config(),
    )
    .await
    .unwrap();
    restart_wait(
        "both turns start",
        &fixture,
        || {
            restart_provider_count(&root, "turn/start", "fake-thread-0") == 1
                && restart_provider_count(&root, "turn/start", "fake-thread-1") == 1
                && restart_agent(&store, &a)["status"] == "running"
                && restart_agent(&store, &b)["status"] == "running"
        },
        || state_text(&store, &root, &a, &b),
    )
    .await;
    let now = studio_runtime_services::event_write::unix_time().unwrap();

    // The backend stops; the supervisor keeps the provider and its work.
    runtime_shutdown::close(runtime, &state).await.unwrap();
    let pids = std::fs::read_to_string(root.join("provider-pids")).unwrap();
    supervisor
        .native
        .extend(pids.lines().map(|pid| pid.parse::<u32>().unwrap()));
    assert_eq!(pids.lines().count(), 1, "one provider process");

    // The owner stops B while no backend runs.
    let detached = Arc::new(
        studio_runtime_services::account_session_runtime::AccountSessionRuntime::new(
            accounts.clone(),
            None,
            tokio::runtime::Handle::current(),
            root.join("accounts"),
            home.clone(),
            home.clone(),
        ),
    );
    let ports = Arc::new(
        studio_runtime_services::lifecycle_effects::ProductionLifecyclePorts::new(
            store.clone(),
            Arc::new(Notify::new()),
            Arc::new(Notify::new()),
            detached,
            None,
        ),
    );
    ports
        .execute(
            &studio_runtime_services::agent_management::Action::Stop {
                target: b.clone(),
                descendants: false,
                reason: None,
                sender: None,
                sender_epoch: None,
            },
            now + 1.0,
        )
        .await
        .unwrap();
    assert_eq!(restart_agent(&store, &b)["status"], "paused");

    // A new backend process attaches to the same provider generation.
    let runtime = start_host(
        &mut state,
        store.clone(),
        accounts,
        Some(executable.to_string_lossy().into_owned()),
        test_registry_config(),
    )
    .await
    .unwrap();
    restart_wait(
        "the stopped turn gets its interrupt and command cleanup",
        &fixture,
        || {
            restart_provider_count(&root, "turn/interrupt", "fake-thread-1") >= 1
                && restart_provider_count(
                    &root,
                    "thread/backgroundTerminals/clean",
                    "fake-thread-1",
                ) >= 1
        },
        || state_text(&store, &root, &a, &b),
    )
    .await;
    let live = restart_agent(&store, &a);
    assert_eq!(live["status"], "running", "A's turn keeps running: {live}");
    assert!(live["error"].is_null(), "no false disconnect: {live}");

    std::fs::write(root.join("complete-fake-thread-0"), "1").unwrap();
    restart_wait(
        "A's continued turn completes",
        &fixture,
        || restart_agent(&store, &a)["lastCompletedTurn"] == "turn-fake-thread-0",
        || state_text(&store, &root, &a, &b),
    )
    .await;

    for thread in ["fake-thread-0", "fake-thread-1"] {
        assert_eq!(
            restart_provider_count(&root, "turn/start", thread),
            1,
            "{thread}: one turn start"
        );
    }
    assert_eq!(
        restart_provider_count(&root, "turn/interrupt", "fake-thread-1"),
        1
    );
    assert_eq!(
        restart_provider_count(&root, "thread/backgroundTerminals/clean", "fake-thread-1"),
        1
    );
    assert_eq!(
        restart_provider_count(&root, "turn/interrupt", "fake-thread-0"),
        0
    );
    let disconnects: i64 = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM runtime_events WHERE text LIKE '%disconnected%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(disconnects, 0, "no parent notice of a false disconnect");
    runtime_shutdown::close(runtime, &state).await.unwrap();
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-server/tests/wave7_p01_registry",
        move || drop(supervisor),
    )
    .await
    .unwrap();
    drop(fixture);
    eprintln!("restart witness ran in {:?}", started.elapsed());
}

/// SQLite emits these receipts after the actual checkpoint, while a taken
/// scheduling claim alone no longer proves that the WAL reached the database.
async fn wait_for_wal_flush(
    events: &mut mpsc::UnboundedReceiver<studio_runtime_store::StoreDiagnosticsEvent>,
) -> bool {
    use studio_runtime_store::{StoreDatabaseKind, StoreDiagnosticsEvent};
    // SQLite runs on dedicated threads; their completion must not race an
    // automatically advancing virtual hang guard.
    tokio::time::resume();
    let flushed = tokio::time::timeout(SATURATED_WAKE_TIMEOUT, async {
        for expected in [StoreDatabaseKind::Canvas, StoreDatabaseKind::Analytics] {
            match events
                .recv()
                .await
                .expect("checkpoint observer remains open")
            {
                StoreDiagnosticsEvent::WalCheckpoint {
                    database,
                    busy,
                    log_frames,
                    checkpointed_frames,
                    ..
                } => {
                    assert_eq!(database, expected);
                    assert_eq!(busy, 0);
                    assert_eq!(checkpointed_frames, log_frames);
                }
                StoreDiagnosticsEvent::WriterAdmission { .. }
                | StoreDiagnosticsEvent::WriterWaiters { .. }
                | StoreDiagnosticsEvent::ReadAdmission { .. }
                | StoreDiagnosticsEvent::ReadWaiters { .. }
                | StoreDiagnosticsEvent::WriterTransaction { .. }
                | StoreDiagnosticsEvent::SqlProfile { .. }
                | StoreDiagnosticsEvent::CommitDuration { .. }
                | StoreDiagnosticsEvent::SqliteLockError { .. } => {
                    unreachable!("observer sends only checkpoint events")
                }
            }
        }
    })
    .await
    .is_ok();
    tokio::time::pause();
    flushed
}

// With synchronous=NORMAL the registry's checkpoint is the flush that makes
// commits durable. A commit that wakes no scheduler (here a table nobody
// observes) must still be flushed one interval later, every time.
#[tokio::test(start_paused = true)]
async fn a_commit_without_a_registry_wake_is_flushed_after_one_interval() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    static CHECKPOINTS: std::sync::OnceLock<
        mpsc::UnboundedSender<studio_runtime_store::StoreDiagnosticsEvent>,
    > = std::sync::OnceLock::new();
    fn checkpointed(event: studio_runtime_store::StoreDiagnosticsEvent) {
        if matches!(
            event,
            studio_runtime_store::StoreDiagnosticsEvent::WalCheckpoint { .. }
        ) {
            // A failed fixture may already have closed its receiver.
            if let Some(sender) = CHECKPOINTS.get() {
                let _closed = sender.send(event);
            }
        }
    }
    let (sender, mut checkpoints) = mpsc::unbounded_channel();
    CHECKPOINTS.set(sender).unwrap();
    store
        .set_diagnostics_callback(Some(checkpointed))
        .await
        .unwrap();
    let (launcher, _writes) = scripted_with_unknown_writes(false);
    let registry = AccountRegistry::new(store.clone(), launcher, test_registry_config())
        .await
        .unwrap();
    let stop = Shutdown::default();
    let task = tokio::spawn(registry.run(
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        stop.clone(),
    ));
    let commit = |value: i64| {
        let store = store.clone();
        async move {
            store
                .transaction(move |tx| {
                    tx.execute_batch("CREATE TABLE IF NOT EXISTS unobserved_flush(v INTEGER);")?;
                    tx.execute("INSERT INTO unobserved_flush VALUES (?1)", [value])?;
                    Ok(())
                })
                .await
                .unwrap();
        }
    };
    let interval = studio_runtime_store::WAL_FLUSH_INTERVAL;
    for value in 0..2 {
        commit(value).await;
        assert!(store.wal_checkpoint_retry_after().unwrap().is_some());
        tokio::time::sleep(interval).await;
        assert!(
            wait_for_wal_flush(&mut checkpoints).await,
            "commit {value} was not flushed one interval later"
        );
    }
    // Paused time would let the hang guard expire while shutdown awaits SQLite work.
    tokio::time::resume();
    stop.cancel();
    tokio::time::timeout(TEST_HANG_GUARD, task)
        .await
        .expect("account registry shutdown hang guard")
        .unwrap()
        .unwrap();
    store.set_diagnostics_callback(None).await.unwrap();
}
