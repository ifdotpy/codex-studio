#![cfg(unix)]
#![allow(clippy::unwrap_used)]
#[path = "../src/account_registry.rs"]
mod account_registry;
#[path = "../../../packages/runtime-services/tests/support/prepared_python.rs"]
mod prepared_python;

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
use studio_runtime_store::{RuntimeStore, StoreError};
use tokio::sync::{Notify, OnceCell, mpsc, watch};

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
}
struct FixtureTemplate {
    _root: tempfile::TempDir,
    entries: Vec<FixtureEntry>,
    agents: Value,
}
struct FixtureEntry {
    path: PathBuf,
    contents: Option<Vec<u8>>,
    mode: u32,
}

fn snapshot_fixture(
    path: &std::path::Path,
    root: &std::path::Path,
    entries: &mut Vec<FixtureEntry>,
) {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path).unwrap();
    let relative = path.strip_prefix(root).unwrap().to_path_buf();
    let contents = if metadata.is_dir() {
        None
    } else {
        Some(std::fs::read(path).unwrap())
    };
    entries.push(FixtureEntry {
        path: relative,
        contents,
        mode: metadata.permissions().mode() & 0o777,
    });
    if metadata.is_dir() {
        for child in std::fs::read_dir(path).unwrap() {
            snapshot_fixture(&child.unwrap().path(), root, entries);
        }
    }
}

async fn dispatch_template() -> &'static FixtureTemplate {
    static TEMPLATE: OnceCell<FixtureTemplate> = OnceCell::const_new();
    TEMPLATE
        .get_or_init(|| async {
            let root = tempfile::tempdir().unwrap();
            let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../server/src");
            let script =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wave7_p01_fixture.py");
            let mut command = std::process::Command::new(prepared_python::prepared_python(&source));
            command
                .args(["-B"])
                .arg(&script)
                .arg(root.path())
                .arg("dispatch");
            let result = prepared_python::output_with_timeout(
                &mut command,
                &script,
                prepared_python::PYTHON_ORACLE_TIMEOUT,
            )
            .unwrap_or_else(|error| panic!("wave7_p01_fixture.py dispatch: {error}"));
            assert!(
                result.status.success(),
                "wave7_p01_fixture.py dispatch: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            let agents = serde_json::from_slice(&result.stdout).unwrap();
            // Run migrations once before snapshotting so each test only copies a
            // ready-to-open SQLite image instead of starting Python and migrating.
            let store = RuntimeStore::open(Some(root.path())).await.unwrap();
            store
                .checkpoint_wal_passive()
                .await
                .expect("checkpoint fixture databases before snapshot");
            let mut entries = Vec::new();
            snapshot_fixture(root.path(), root.path(), &mut entries);
            drop(store);
            FixtureTemplate {
                _root: root,
                entries,
                agents,
            }
        })
        .await
}

async fn recovery_template() -> &'static FixtureTemplate {
    static TEMPLATE: OnceCell<FixtureTemplate> = OnceCell::const_new();
    TEMPLATE
        .get_or_init(|| async {
            let root = tempfile::tempdir().unwrap();
            let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../server/src");
            let script =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wave7_p01_fixture.py");
            let mut command = std::process::Command::new(prepared_python::prepared_python(&source));
            command
                .args(["-B"])
                .arg(&script)
                .arg(root.path())
                .arg("recovery");
            let result = prepared_python::output_with_timeout(
                &mut command,
                &script,
                prepared_python::PYTHON_ORACLE_TIMEOUT,
            )
            .unwrap_or_else(|error| panic!("wave7_p01_fixture.py recovery: {error}"));
            assert!(
                result.status.success(),
                "wave7_p01_fixture.py recovery: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            let agents = serde_json::from_slice(&result.stdout).unwrap();
            let mut entries = Vec::new();
            snapshot_fixture(root.path(), root.path(), &mut entries);
            FixtureTemplate {
                _root: root,
                entries,
                agents,
            }
        })
        .await
}

impl Fixture {
    async fn new() -> Self {
        Self::build("dispatch").await
    }
    async fn build(mode: &str) -> Self {
        if mode == "dispatch" {
            use std::os::unix::fs::PermissionsExt;

            let template = dispatch_template().await;
            let root = tempfile::tempdir().unwrap();
            for entry in &template.entries {
                let path = root.path().join(&entry.path);
                if let Some(contents) = &entry.contents {
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(&path, contents).unwrap();
                } else {
                    std::fs::create_dir_all(&path).unwrap();
                }
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(entry.mode))
                    .unwrap();
            }
            let (store, changes) = store_with_change_signal(root.path()).await;
            let cwd = root.path().to_string_lossy().into_owned();
            store
                .transaction_with::<_, StoreError>(move |tx| {
                    tx.execute(
                        "UPDATE runtime_agents SET record=json_set(record,'$.cwd',?1)",
                        [&cwd],
                    )?;
                    Ok(())
                })
                .await
                .unwrap();
            return Self {
                _root: root,
                store,
                agents: template.agents.clone(),
                changes,
            };
        }
        if mode == "recovery" {
            use std::os::unix::fs::PermissionsExt;

            let template = recovery_template().await;
            let root = tempfile::tempdir().unwrap();
            for entry in &template.entries {
                let path = root.path().join(&entry.path);
                if let Some(contents) = &entry.contents {
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(&path, contents).unwrap();
                } else {
                    std::fs::create_dir_all(&path).unwrap();
                }
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(entry.mode))
                    .unwrap();
            }
            let (store, changes) = store_with_change_signal(root.path()).await;
            return Self {
                _root: root,
                store,
                agents: template.agents.clone(),
                changes,
            };
        }
        let root = tempfile::tempdir().unwrap();
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../server/src");
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wave7_p01_fixture.py");
        let mut command = std::process::Command::new(prepared_python::prepared_python(&source));
        command.args(["-B"]).arg(&script).arg(root.path()).arg(mode);
        let result = prepared_python::output_with_timeout(
            &mut command,
            &script,
            prepared_python::PYTHON_ORACLE_TIMEOUT,
        )
        .unwrap_or_else(|error| panic!("wave7_p01_fixture.py {mode}: {error}"));
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let agents = serde_json::from_slice(&result.stdout).unwrap();
        let (store, changes) = store_with_change_signal(root.path()).await;
        Self {
            _root: root,
            store,
            agents,
            changes,
        }
    }
    async fn enqueue(&self, index: usize, id: &str) {
        let agent_id = self.agents[index]["id"].as_str().unwrap().to_owned();
        let event_id = id.to_owned();
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
        self.store
            .read()
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
                .read()
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
    stops: Arc<Mutex<Vec<Shutdown>>>,
    opened_notice: Arc<Notify>,
    ready_notice: Arc<Notify>,
    initialize_holds: InitializeHolds,
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
            if self
                .unavailable
                .get()
                .is_some_and(|unavailable| unavailable(&account))
            {
                return Err(WorkerError::TaskFailed);
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
                    wire: studio_runtime_providers::ProviderKind::Codex.wire(),
                    timeout_ms: 60_000,
                    initialize_operation: OperationId(format!("init:{account}")),
                    request_id_prefix: None,
                    recovered: vec![],
                },
                resources: PreparationResources::bundled(
                    String::new(),
                    ModelCatalogService::new(),
                )
                .unwrap(),
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
                                Some("initialized") => CommandReceipt::Written {
                                    remote_id: None,
                                    duplicate: false,
                                },
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
            stops: Arc::default(),
            opened_notice: Arc::default(),
            ready_notice: Arc::default(),
            initialize_holds: Arc::default(),
        }),
        rx,
    )
}

fn radio_snapshot(mode: &str) -> Value {
    use std::sync::OnceLock;

    static INTERRUPT: OnceLock<Value> = OnceLock::new();
    static QUEUE: OnceLock<Value> = OnceLock::new();
    let cache = match mode {
        "interrupt" => &INTERRUPT,
        "queue" => &QUEUE,
        other => panic!("unknown radio fixture mode {other}"),
    };
    cache
        .get_or_init(|| {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = manifest.join("../server/src");
    let script = r#"
import importlib.util, json, pathlib, sqlite3, sys
from unittest.mock import patch
mode = sys.argv[2]
tests = pathlib.Path(sys.argv[1])
sys.path.insert(0, str(tests))
spec = importlib.util.spec_from_file_location('radio_fixture', tests / 'radio-contract.py')
fixture = importlib.util.module_from_spec(spec); spec.loader.exec_module(fixture)
radio = fixture.Radio('test_two_replies_share_all_text_and_stop')
radio.setUp()
try:
    for key in ('a', 'b', 'c', 'd'):
        status = 'queued' if mode == 'queue' and key == 'a' else 'idle'
        radio.mutate_agent(key, epoch=1, name=key, status=status, inFlight=False, events=0,
                           provider='codex', model='fake-model', role='orchestrator', depth=0,
                           prompt='', worktree=False, accountKey='default', autoWake=True,
                           nativeEffort=None, threadId='native-' + key)
    radio.action('send', text='question', request_id='seed-send')
    if mode == 'interrupt':
        radio.tick()
        active = radio.room()['radio']['active']
        radio.mutate_agent('a', inFlight=True, status='running', turnId='turn',
                           startAttempt=dict(id='fixture-attempt', events=[active['eventId']], epoch=1, submitted=True,
                                             threadId='native-a', observedTurnId='turn'))
        with radio.runtime.db() as db:
            db.execute("UPDATE runtime_events SET status='delivered',turn_id='turn'")
        radio.action('stop')
    tables = ('agents','projects','rooms','events','chat_messages','requests',
              'completed_turns','items','operation_receipts','sync_entities','sidebar_order')
    mapping = {'sync_entities':'sync_entities','sidebar_order':'runtime_sidebar_order'}
    snapshot = {}
    with radio.runtime.db() as db:
        for table in tables:
            actual = mapping.get(table, 'runtime_' + table)
            try:
                snapshot[table] = [dict(row) for row in db.execute('SELECT * FROM ' + actual)]
            except sqlite3.OperationalError as error:
                if 'no such table' not in str(error): raise
    print(json.dumps(snapshot))
finally:
    radio.doCleanups()
"#;
    let mut command = std::process::Command::new(prepared_python::prepared_python(&source));
    let isolated_environment = prepared_python::IsolatedTestEnvironment::new();
    isolated_environment.apply(&mut command);
    command
        .arg("-c")
        .arg(script)
        .arg(manifest.join("../server/tests"))
        .arg(mode);
    let output = prepared_python::output_with_timeout(
        &mut command,
        &manifest.join("tests/wave7_p01_radio_fixture.py"),
        TEST_HANG_GUARD,
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<Value>(&output.stdout).unwrap()
        })
        .clone()
}

fn radio_interrupt_snapshot() -> Value {
    radio_snapshot("interrupt")
}

fn radio_queue_snapshot() -> Value {
    radio_snapshot("queue")
}

async fn seed_radio_interrupt(fixture: &Fixture, snapshot: &Value) {
    let project_root = fixture._root.path().join("radio-project");
    fs::create_dir_all(&project_root).unwrap();
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
                            _ => panic!("unsupported radio fixture SQL value"),
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
        let db = fixture.store.read().unwrap();
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
        let db = fixture.store.read().unwrap();
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
        let db = fixture.store.read().unwrap();
        let mut statement = db
            .prepare("SELECT id,result FROM runtime_operation_receipts ORDER BY rowid DESC LIMIT 8")
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
    let db = fixture.store.read().unwrap();
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

    registry.reconcile(None).await.unwrap();
    assert!(
        writes.try_recv().is_err(),
        "radio queue event was sent twice"
    );
    registry.shutdown().await.unwrap();
    let status_before_restart: String = fixture
        .store
        .read()
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
        .read()
        .unwrap()
        .query_row(
            "SELECT status FROM runtime_events WHERE id=?1",
            [&event_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status_after_recovery, status_before_restart);
    assert!(
        writes.try_recv().is_err(),
        "unknown radio turn must not be resent after worker restart"
    );
    restarted.shutdown().await.unwrap();
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
        let db = fixture.store.read().unwrap();
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
    assert!(
        writes.try_recv().is_err(),
        "uncertain request must not be resubmitted"
    );
}

/// A host without the image backend refuses image turns before a provider starts.
#[cfg(not(target_os = "macos"))]
#[tokio::test]
async fn image_workspace_pending_event_is_refused_before_account_provider_launch() {
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
    fixture.enqueue(0, "image-workspace-refused").await;
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
        let db = fixture.store.read().unwrap();
        db.query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [agent_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
    };
    let db = fixture.store.read().unwrap();
    let event_status: String = db
        .query_row(
            "SELECT status FROM runtime_events WHERE id='annotation:image-workspace-refused'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(event_status, "pending");
    drop(db);
    let held: Value = serde_json::from_str(&record()).unwrap();
    assert_eq!(held["status"], "paused");
    assert_eq!(held["error"], "workspace-state/image-execution");
    assert_eq!(held["nativeFailureHold"], true);
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
    let db = fixture.store.read().unwrap();
    let event_status: String = db
        .query_row(
            "SELECT status FROM runtime_events WHERE id='annotation:image-workspace-refused'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(event_status, "pending");
    drop(db);
    restarted.shutdown().await.unwrap();
}

/// Rehearsal 15, agent d0c9c794 (workspaceMode "image"): on macOS the account
/// worker starts for an image agent's input and its restart check like for any
/// agent. Workspace preparation mounts the image and runs the turn in
/// `<mount>/repo`, as Python does; the registry neither refuses nor holds it.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn image_workspace_pending_event_starts_the_account_provider_on_macos() {
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
    fixture.enqueue(0, "image-workspace-input").await;
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
        !launcher.opened.lock().unwrap().is_empty(),
        "the account provider starts for the image agent"
    );
    let record: Value = {
        let db = fixture.store.read().unwrap();
        serde_json::from_str(
            &db.query_row(
                "SELECT record FROM runtime_agents WHERE id=?1",
                [agent_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        )
        .unwrap()
    };
    assert_ne!(
        record["error"], "workspace-state/image-execution",
        "{record}"
    );
    assert_ne!(record["status"], "paused", "{record}");
    registry.shutdown().await.unwrap();
}

/// Rehearsal 15: the restart check of an image agent that ran at the backend
/// swap waited until another agent of the account started the worker. On macOS
/// its own recovery row starts the account worker, whose first pass reads it.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn an_interrupted_image_agent_starts_its_own_restart_check_on_macos() {
    let fixture = Fixture::new().await;
    let agent_id = fixture.agents[0]["id"].as_str().unwrap().to_owned();
    let transaction_agent_id = agent_id.clone();
    fixture
        .store
        .transaction(move |tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,
                   '$.workspaceMode','image','$.status','interrupted','$.autoWake',json('false'),
                   '$.inFlight',json('false'),'$.turnId','turn-at-swap',
                   '$.error','Server restarted during a turn. Review history, then send a new instruction.',
                   '$.restartRecovery',json_object('stage','pending','autoWake',json('true'),
                      'turnId','turn-at-swap','epoch',json_extract(record,'$.epoch'),
                      'accountKey',json_extract(record,'$.accountKey'),
                      'threadId',json_extract(record,'$.threadId'),'at',1.0,'source','restart'),
                   '$.disconnectRecovery',json_object('source','restart','autoWake',json('true'),
                      'turnId','turn-at-swap','epoch',json_extract(record,'$.epoch'),
                      'accountKey',json_extract(record,'$.accountKey'),
                      'threadId',json_extract(record,'$.threadId'),'at',1.0)) WHERE id=?1",
                [&transaction_agent_id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
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
        !launcher.opened.lock().unwrap().is_empty(),
        "the image agent's recovery starts the account worker"
    );
    registry.shutdown().await.unwrap();
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
    let (account, request) = tokio::time::timeout(TEST_HANG_GUARD, writes.recv())
        .await
        .expect("provider write hang guard")
        .unwrap();
    assert_eq!(account, "account:default");
    assert_eq!(request["method"], "turn/start");
    wait_for_durable_event_status(
        &fixture,
        "b2ba3b37-4ef4-467d-b215-155c09c088e6",
        "delivered",
    )
    .await;
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

#[path = "../src/host_runtime.rs"]
#[allow(dead_code)]
mod host_runtime;
#[path = "../src/provider_ports.rs"]
#[allow(dead_code)]
mod provider_ports;
#[path = "../src/runtime_shutdown.rs"]
mod runtime_shutdown;

#[tokio::test]
async fn recovery_selection_matches_python_scheduler_roster_without_changing_stored_records() {
    let fixture = Fixture::build("recovery").await;
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
            let mut record = serde_json::from_str::<Value>(&raw).unwrap();
            studio_runtime_store::agent_volatile::merge(&db, &id, &mut record).unwrap();
            (id, record)
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(json!(stored), fixture.agents["stored"]);
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
    let second = tokio::time::timeout(TEST_HANG_GUARD, writes.recv())
        .await
        .expect("post-disconnect provider write hang guard")
        .unwrap();
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
    assert!(
        writes.try_recv().is_err(),
        "unknown input must not be resent on replacement"
    );
}

async fn seed_backlog(fixture: &Fixture, count: usize, each_account: bool) -> Vec<String> {
    let source = fixture.agents[0]["id"].as_str().unwrap().to_owned();
    fixture.store.transaction(move |tx| {
        // One recursive insert per table keeps this 4,100-record paging fixture
        // from recompiling and executing two SQL statements for every row.
        let sequence = "WITH RECURSIVE seq(i) AS (
                SELECT 0 WHERE ?1 > 0
                UNION ALL SELECT i + 1 FROM seq WHERE i + 1 < ?1
            ), source(record) AS (
                SELECT record FROM runtime_agents WHERE id = ?2
            )";
        // Clones copy the whole agent, volatile fields included, and then move
        // those fields like an older state's (format 2026-10-20).
        let whole = studio_runtime_store::agent_volatile::merged_record(tx, &source)?
            .expect("source agent")
            .to_string();
        tx.execute(
            &format!("{sequence}
                INSERT INTO runtime_agents(id, record)
                SELECT printf('00000000-0000-4000-8000-%012d', seq.i),
                       json_set(?4, '$.id', printf('00000000-0000-4000-8000-%012d', seq.i),
                                '$.accountKey', CASE WHEN ?3 THEN printf('page-%06d', seq.i) ELSE 'default' END,
                                '$.executionSettingsAccountKey', CASE WHEN ?3 THEN printf('page-%06d', seq.i) ELSE 'default' END,
                                '$.contextRepairWait', 'blocked')
                FROM seq CROSS JOIN source"),
            (count as i64, source.as_str(), each_account, whole),
        )?;
        studio_runtime_store::agent_volatile::migrate_2026_10_20(tx)?;
        tx.execute(
            &format!("{sequence}
                INSERT INTO runtime_events(id, agent, kind, text, status, created, epoch)
                SELECT printf('00000000-0000-4000-8000-%012d', seq.i),
                       printf('00000000-0000-4000-8000-%012d', seq.i),
                       'annotation', 'fixture', 'pending', 0, json_extract(source.record, '$.epoch')
                FROM seq CROSS JOIN source"),
            (count as i64, source.as_str()),
        )?;
        Ok((0..count)
            .map(|i| format!("00000000-0000-4000-8000-{i:012}"))
            .collect())
    }).await.unwrap()
}

#[tokio::test]
async fn pending_and_account_backlogs_are_bounded_pages_not_total_limits() {
    let fixture = Fixture::new().await;
    let expected = seed_backlog(&fixture, 4100, false).await;
    let mut after = BTreeMap::new();
    let mut actual = Vec::new();
    while actual.len() < expected.len() {
        let page = account_registry::target_page(&fixture.store, "", &after).unwrap();
        let rows = &page["default"];
        assert!(rows.len() <= 32);
        actual.extend(rows.iter().map(|(id, _, _)| id.clone()));
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
            .map(|(id, _, _)| id)
            .collect::<Vec<_>>(),
        actual.iter().take(32).collect::<Vec<_>>()
    );
    let fixture = Fixture::new().await;
    seed_backlog(&fixture, 130, true).await;
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

    let fixture = Fixture::new().await;
    let source = fixture.agents[0]["id"].as_str().unwrap();
    let source = source.to_owned();
    fixture.store.transaction(move |tx| {
        let raw: String = tx.query_row("SELECT record FROM runtime_agents WHERE id=?1", [&source], |row| row.get(0))?;
        tx.execute("INSERT INTO runtime_agents(id,record) VALUES('bad-account-key',json_set(?1,'$.id','bad-account-key','$.accountKey',7))", [raw])?;
        tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES('bad-account-key-event','bad-account-key','annotation','fixture','pending',0,json_extract((SELECT record FROM runtime_agents WHERE id='bad-account-key'),'$.epoch'))", [])?;
        Ok(())
    }).await.unwrap();
    assert!(matches!(
        account_registry::target_page(&fixture.store, "", &BTreeMap::new()),
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
            let db = fixture.store.read().unwrap();
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
    let db = fixture.store.read().unwrap();
    let blocked: i64 = db.query_row("SELECT count(*) FROM runtime_events WHERE kind='annotation' AND text='fixture' AND status='pending'",[],|r|r.get(0)).unwrap();
    assert_eq!(blocked, 48);
}

#[tokio::test]
async fn registry_drains_strict_account_pages_after_one_global_wake() {
    let fixture = Fixture::new().await;
    seed_backlog(&fixture, 130, true).await;
    let final_agent = "00000000-0000-4000-8000-000000000129";
    fixture
        .store
        .transaction(move |tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_remove(record,'$.contextRepairWait') WHERE id=?1",
                [final_agent],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    // Written receipts: a lost turn/start receipt closes the session by design,
    // and that worker exit would race this test's registry shutdown.
    let (launcher, mut writes) = scripted_with_unknown_writes(false);
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
    let (account, request) = tokio::time::timeout(SATURATED_WAKE_TIMEOUT, writes.recv())
        .await
        .expect("strict account continuation reaches the eligible tail")
        .unwrap();
    assert_eq!(account, "account:page-000129");
    assert_eq!(request["method"], "turn/start");
    stop.cancel();
    tokio::time::timeout(SATURATED_WAKE_TIMEOUT, task)
        .await
        .expect("registry shutdown completes after strict account paging")
        .unwrap()
        .unwrap();
    assert_eq!(launcher.opened.lock().unwrap().len(), 2);
}

fn python_launch(
    root: &std::path::Path,
    environment: &BTreeMap<String, String>,
    home: &std::path::Path,
    isolated: bool,
    executable: &str,
    mode: &str,
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
    child.stdin.take().unwrap().write_all(json!({"root":root,"environment":environment,"home":home,"isolated":isolated,"executable":executable,"mode":mode}).to_string().as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
fn oracle_command(oracle: &Value) -> Vec<String> {
    serde_json::from_value(oracle["command"].clone()).unwrap()
}
/// The Rust launch is the Python launch plus Codex's thread unload override
/// after the app-server arguments; the Python launch stays its reattach command.
fn with_unload_override(oracle: &Value) -> Vec<String> {
    let mut command = oracle_command(oracle);
    command.splice(4..4, ["-c".into(), "thread_unload_delay_secs=0".into()]);
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

async fn provider_input_barrier(root: &std::path::Path, backend_id: &str) -> Value {
    use tokio::io::AsyncWriteExt;
    let mut peer = tokio::io::BufReader::new(
        tokio::net::UnixStream::connect(root.join("supervisor.sock"))
            .await
            .unwrap(),
    );
    wire_send(
        &mut peer,
        json!({"protocol":1,"stateDir":root,"backendId":backend_id}),
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
        let oracle = python_launch(
            root.path(),
            &environment,
            &home,
            isolated,
            "/fixture/codex",
            "supervisor",
        );
        let launch = host_runtime::codex_launch(
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
        let hello = wire_read(&mut peer).await;
        assert_eq!(hello["protocol"], 2);
        assert_eq!(hello["stateDir"], root.path().to_string_lossy().as_ref());
        assert_eq!(hello["backendId"], "fixture");
        wire_send(&mut peer, json!({"ok":true,"protocol":2})).await;
        let status = wire_read(&mut peer).await;
        wire_send(
            &mut peer,
            json!({"requestId":status["requestId"],"error":"Unknown supervisor handle"}),
        )
        .await;
        let open = wire_read(&mut peer).await;
        for field in ["env", "cwd"] {
            assert_eq!(open[field], oracle[field], "{field}");
        }
        assert_eq!(open["command"], json!(with_unload_override(&oracle)));
        let environment: BTreeMap<String, String> =
            serde_json::from_value(open["env"].clone()).unwrap();
        // The Python launch is the reattach command, so its app-servers reattach.
        assert_eq!(reattach_commands, [oracle_command(&oracle)]);
        assert_eq!(
            studio_runtime_store::supervisor_journal::launch_signature(
                &reattach_commands[0],
                &environment,
                open["cwd"].as_str()
            ),
            oracle["signature"]
        );
        wire_send(&mut peer,json!({"requestId":open["requestId"],"result":{"resumed":false,"initResult":null,"acknowledged":0,"sequence":0,"generation":1,"returnCode":null,"pid":1}})).await;
        drop(preparing.await.unwrap());
    }
}

#[test]
fn direct_launch_command_and_environment_match_python() {
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
        let oracle = python_launch(
            root.path(),
            &environment,
            &home,
            isolated,
            "/fixture/codex",
            "direct",
        );
        let launch = host_runtime::codex_launch(
            Some("/fixture/codex"),
            environment,
            home.to_string_lossy().into_owned(),
            isolated,
            root.path().into(),
        );
        assert_eq!(launch.command, with_unload_override(&oracle));
        assert_eq!(launch.reattach_commands, [oracle_command(&oracle)]);
        assert_eq!(
            serde_json::to_value(launch.environment).unwrap(),
            oracle["env"]
        );
        assert_eq!(
            launch
                .cwd
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            oracle["cwd"].as_str().map(str::to_owned)
        );
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

    let root = tempfile::tempdir().unwrap();
    let bridge_dir = root.path().join("bridge");
    let bin_dir = root.path().join("bin");
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
    let launch = host_runtime::claude_launch(
        profile,
        &bridge_dir,
        root.path(),
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
        root.path()
            .join("account-servers/claude-fixture-account")
            .to_string_lossy()
    );
    assert_eq!(
        launch.environment["STUDIO_CLAUDE_BIN"],
        claude.to_string_lossy()
    );
    assert!(!launch.environment.contains_key("ANTHROPIC_API_KEY"));
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
    let fixture = Fixture::new().await;
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
        "supervisor",
    );
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
                    backend_id: studio_runtime_process::BackendId("python-cutover-fixture".into()),
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
        test_registry_config(),
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
    let publication_rows: Vec<String> = fixture
        .store
        .read()
        .unwrap()
        .prepare(
            "SELECT result FROM runtime_operation_receipts WHERE id LIKE 'account-worker:notification-publication:default:%:pending' ORDER BY id",
        )
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
async fn rust_direct_claude_launch_runs_the_scripted_bridge_turn() {
    use std::os::unix::fs::PermissionsExt;
    use tower::ServiceExt;
    if std::env::var_os("STUDIO_W8_CLAUDE_CHILD").is_none() {
        let temp = tempfile::tempdir().unwrap();
        let sdk = temp.path().join("scripted-sdk.mjs");
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
        let exporter = "import pathlib,runpy,sys; sys.path.insert(0,str(pathlib.Path(sys.argv[1]).parent)); ns=runpy.run_path(sys.argv[1]); sdk=ns['SDK'].replace(\"text==='split-stream'\",\"text.startsWith('split-stream')\").replace(\"text==='wait'\",\"text.startsWith('wait')\"); open(sys.argv[2],'w').write(sdk)";
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../server/src");
        let python = prepared_python::prepared_python(&source);
        let bridge_node_modules = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../providers/apps/claude-bridge/node_modules")
            .canonicalize()
            .unwrap();
        std::os::unix::fs::symlink(bridge_node_modules, temp.path().join("node_modules")).unwrap();
        let output = std::process::Command::new(&python)
            .args(["-c", exporter, "../server/tests/claude-bridge-contract.py"])
            .arg(&sdk)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
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
                "rust_direct_claude_launch_runs_the_scripted_bridge_turn",
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
        json!({"version":1,"defaultAccountKey":"default","accounts":{"default":{"id":"default","provider":"claude","home":account_home,"label":"Scripted Claude","source":"Claude Code","status":"ready","accountId":identity,"email":"test@example.test","plan":"Claude Max","_credentialIdentity":identity,"claudeOptions":{"configDir":std::env::var("CLAUDE_CONFIG_DIR").unwrap(),"customModels":[]}}},"logins":{},"deleteReceipts":{}}).to_string(),
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
    let runtime = host_runtime::HostRuntime::start(
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
    assert_eq!(
        app.clone()
            .oneshot(direct_message_post(
                &agent_id,
                "086cc3e3-68a8-429f-9ec1-895aad72f181",
                "split-stream"
            ))
            .await
            .unwrap()
            .status(),
        200
    );
    let completed = tokio::time::timeout(TEST_HANG_GUARD, async {
        loop {
            let raw = fixture
                .store
                .read()
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
    let bridge_root = std::env::var("STUDIO_W8_CLAUDE_TEST_ROOT").unwrap();
    let python_source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../server/src");
    let python = prepared_python::prepared_python(&python_source);
    let user_text = transcript["items"][0]["text"].as_str().unwrap();
    let oracle_script = "import json,pathlib,runpy,sys; path=pathlib.Path(sys.argv[1]); sys.path.insert(0,str(path.parent)); ns=runpy.run_path(str(path)); bridge_type=ns['Bridge']; globals=bridge_type.setUp.__globals__; globals['SDK']=globals['SDK'].replace(\"text==='split-stream'\",\"text.startsWith('split-stream')\"); bridge=bridge_type('test_stream_deltas_and_block_snapshots_form_one_final_answer'); bridge.setUp();\ntry:\n bridge.turn(sys.argv[2],sys.argv[3]); turn=bridge.completed(); history=bridge.call('thread/read',{'threadId':bridge.thread,'includeTurns':True}); print(json.dumps({'turn':turn,'thread':history['thread']},sort_keys=True))\nfinally: bridge.doCleanups()";
    let oracle = std::process::Command::new(python)
        .args([
            "-c",
            oracle_script,
            "../server/tests/claude-bridge-contract.py",
            user_text,
            "086cc3e3-68a8-429f-9ec1-895aad72f181",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("STUDIO_W8_CLAUDE_TEST_ROOT", bridge_root)
        .output()
        .unwrap();
    assert!(
        oracle.status.success(),
        "Python bridge oracle failed: {}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    let oracle: Value = serde_json::from_slice(&oracle.stdout).unwrap();
    let python_turn = &oracle["thread"]["turns"][0];
    let python_items = python_turn["items"].as_array().unwrap();
    let rust_items = transcript["items"].as_array().unwrap();
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
    assert_eq!(
        normalize_ids(rust_items, &agent_id),
        normalize_ids(python_items, oracle["thread"]["id"].as_str().unwrap())
    );
    assert_eq!(
        python_turn["clientUserMessageId"],
        "086cc3e3-68a8-429f-9ec1-895aad72f181"
    );
    assert_eq!(python_turn["status"], completed["status"]);
    let python_final_state = json!({
        "status": python_turn["status"],
        "inFlight": oracle["thread"]["status"]["type"] != "idle"
    });
    let rust_final_state = json!({
        "status": transcript["agent"]["status"],
        "inFlight": transcript["agent"]["inFlight"]
    });
    assert_eq!(rust_final_state, python_final_state);
    let answer = rust_items
        .iter()
        .find(|item| item["phase"] == "final_answer")
        .unwrap();
    assert_eq!(
        answer["id"].as_str().unwrap().rsplit(':').next(),
        Some("stream-blocks")
    );
    let queries = PathBuf::from(completed["cwd"].as_str().unwrap()).join(".queries");
    let (_query_watcher, mut query_events) = watch_file_changes(&queries);
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
    assert_eq!(
        app.clone()
            .oneshot(direct_message_post(
                &agent_id,
                "a19e61c8-6156-4874-b8d8-9111b0a49fc4",
                "wait",
            ))
            .await
            .unwrap()
            .status(),
        200
    );
    wait_for_file_lines(&queries, 2, &mut query_events).await;
    runtime_shutdown::close(runtime, &state).await.unwrap();
    let mut restarted_state = state.clone();
    let restarted = host_runtime::HostRuntime::start(
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
    // aborted turn as interrupted before it exits is a race. Either outcome
    // is safe: an unknown turn is held, a confirmed interruption continues
    // with a restart notice. Neither resends the interrupted input.
    let recovered = tokio::time::timeout(TEST_HANG_GUARD, async {
        loop {
            let raw = fixture
                .store
                .read()
                .unwrap()
                .query_row(
                    "SELECT record FROM runtime_agents WHERE id=?1",
                    [&agent_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap();
            let record: Value = serde_json::from_str(&raw).unwrap();
            let held = record["status"] == "interrupted"
                && record["autoWake"] == false
                && record["restartRecovery"]["stage"] == "held";
            let continued = record["restartRecovery"]["stage"] == "continued"
                && record["status"] == "completed"
                && record["inFlight"] != true;
            if held || continued {
                break record;
            }
            restart_changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "in-flight Claude turn should be held or continued: {}",
            direct_agent_debug(&fixture.store, &agent_id)
        )
    });
    let second_event_status: String = fixture
        .store
        .read()
        .unwrap()
        .query_row(
            "SELECT status FROM runtime_events WHERE id='a19e61c8-6156-4874-b8d8-9111b0a49fc4'",
            [],
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
async fn direct_launch_mode_completes_http_turn_and_reaps_child() {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncReadExt;
    use tower::ServiceExt;
    if std::env::var_os("STUDIO_W7_P01_DIRECT_CHILD").is_none() {
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "direct_launch_mode_completes_http_turn_and_reaps_child",
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
    let executable = fixture._root.path().join("provider");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wave7_p01_native_fixture.py"),
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
    let runtime = host_runtime::HostRuntime::start(
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
    let body = json!({"room":agent_id,"id":"65f07ce1-42b9-42a3-b42e-8710a0c54618","text":"direct mode turn"});
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
                "direct_generation_and_journal_identity_survive_backend_restart",
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
                "CREATE TABLE direct_journal_audit(id TEXT PRIMARY KEY,result TEXT NOT NULL);
                 CREATE TRIGGER capture_direct_journal AFTER INSERT ON runtime_operation_receipts
                 WHEN NEW.id LIKE 'account-worker:journal:default:%'
                 BEGIN INSERT OR REPLACE INTO direct_journal_audit VALUES(NEW.id,NEW.result); END;",
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
    let executable = fixture._root.path().join("provider");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wave7_p01_native_fixture.py"),
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
    let runtime = host_runtime::HostRuntime::start(
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
                "9e63d185-cf3f-4938-8205-064d1bd02411",
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
    let runtime = host_runtime::HostRuntime::start(
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
                "9e63d185-cf3f-4938-8205-064d1bd02412",
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
async fn direct_child_crash_holds_written_turn_without_resending_after_relaunch() {
    use std::os::unix::fs::PermissionsExt;
    use tower::ServiceExt;
    if std::env::var_os("STUDIO_W7_P01_DIRECT_CRASH_CHILD").is_none() {
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "direct_child_crash_holds_written_turn_without_resending_after_relaunch",
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
    std::fs::write(fixture._root.path().join("crash-first-turn"), "1").unwrap();
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
    let executable = fixture._root.path().join("provider");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wave7_p01_native_fixture.py"),
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
    let runtime = host_runtime::HostRuntime::start(
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
    let body = json!({"room":agent_id,"id":"a1eabc96-5d2b-4fe0-bcbc-18a93c539b17","text":"crash in flight"});
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
    let second_launch = read_direct_report(&launch_observer).await;
    assert_eq!(
        second_launch["count"], 2,
        "registry replaces crashed provider"
    );
    let mut changes = state.sync_changes_handle().subscribe();
    let held = tokio::time::timeout(TEST_HANG_GUARD, async {
        loop {
            let raw = fixture
                .store
                .read()
                .unwrap()
                .query_row(
                    "SELECT record FROM runtime_agents WHERE id=?1",
                    [agent_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap();
            let record: Value = serde_json::from_str(&raw).unwrap();
            // A provider crash while the backend runs is a disconnect, not a restart.
            if record["disconnectRecovery"]["startAttempt"]["executionOutcome"] == "unknown"
                && record["error"] == "Codex disconnected. Review the transcript before resuming."
                && record["restartRecovery"].is_null()
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
    assert_eq!(held["autoWake"], false);
    assert_eq!(
        fixture
            .store
            .read()
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
                "direct_child_crash_before_turn_write_retries_queued_input_once",
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
    let executable = fixture._root.path().join("provider");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wave7_p01_native_fixture.py"),
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
    let body = json!({"room":agent_id,"id":"50ba7350-4055-437d-946f-9a44d87caa38","text":"queued before provider ready"});
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
    let runtime = host_runtime::HostRuntime::start(
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
                .read()
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
        .read()
        .unwrap()
        .query_row(
            "SELECT result FROM runtime_operation_receipts WHERE id='account-worker:direct-generation:default:next'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    serde_json::from_str(&raw).unwrap()
}

#[cfg(target_os = "linux")]
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
    let db = store.read().unwrap();
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
    let db = store.read().unwrap();
    let prefix = format!("account-worker:journal:default:{generation}:");
    let upper = format!("{prefix}~");
    let mut query = db
        .prepare("SELECT id,result FROM direct_journal_audit WHERE id>=?1 AND id<?2 ORDER BY id")
        .unwrap();
    query
        .query_map((prefix.as_str(), upper.as_str()), |row| {
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
                .read()
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
