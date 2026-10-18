#![allow(clippy::unwrap_used)]
mod auth_wait_schedule;
mod claude_preparation;
mod retained;
mod steer;
use super::*;
use crate::event_write::{AnnotationWrite, annotate};
use std::{
    path::PathBuf,
    process::Command,
    sync::{Arc, atomic::AtomicUsize},
};
use studio_runtime_process::{EventCursor, ProviderLaunchSpec, provider_seam::*};
use studio_runtime_store::{AgentRecord, EventWriteRepository};
use tokio::sync::oneshot;

const FIXTURE_LOCK_HANG_GUARD: std::time::Duration = std::time::Duration::from_secs(300);

fn test_resources() -> Result<preparation::PreparationResources, WorkerError> {
    preparation::PreparationResources::bundled(
        String::new(),
        crate::model_catalog::ModelCatalogService::new(Some(
            std::env::temp_dir()
                .join("absent-provider-for-test")
                .to_string_lossy()
                .into_owned(),
        )),
    )
}
const CREATE_AGENTS: &str = r#"
import json, pathlib, sqlite3, sys
sys.path.insert(0, sys.argv[4])
from oracle_guard import install_guard
_GUARD = install_guard()  # Runtime initialization owns a local Node crypto helper.
sys.path.insert(0, sys.argv[2])
from codex_runtime import Runtime
import codex_runtime
from unittest.mock import patch
class QuietRuntime(Runtime):
    def schedule(self):
        return
root = pathlib.Path(sys.argv[1])
# Enable the runtime WAL mode before its first schema transaction.
with sqlite3.connect(root / "canvas.sqlite3") as db:
    db.execute("PRAGMA journal_mode=WAL")
# Only disposable schema construction skips syncs. All tested Rust transactions
# reopen this database with their normal durability settings.
_connect = codex_runtime.sqlite_connect
def fixture_connect(*args, **kwargs):
    db = _connect(*args, **kwargs)
    db.execute("PRAGMA synchronous=OFF")
    return db
with patch.object(codex_runtime, "sqlite_connect", fixture_connect):
    runtime = QuietRuntime(root, lambda *_args, **_kwargs: None)
agents = []
for index in range(int(sys.argv[3])):
    agent = runtime.create({"name":"Rust turn fixture", "cwd":str(root), "prompt":"temporary"}, defer=True)
    agent.update(autoWake=True, status="queued", accountKey="default",
                 threadId="fake-thread-" + str(index), model="fake-model",
                 worktree=False, workspaceMode="shared", nativeEffort=None)
    with runtime.lock, runtime.db() as db:
        runtime.put(db, "agents", agent)
    agents.append({"id":agent["id"], "epoch":agent["epoch"]})
print(json.dumps(agents))
runtime.close()
"#;

// The bridge observes both sides of each whole store operation. No production
// transaction is changed by injection; after-commit failures retain its writes.
#[derive(Default)]
pub(super) struct Faults {
    pub(super) hold_admission: std::sync::atomic::AtomicBool,
    pub(super) admission_held: tokio::sync::Notify,
    pub(super) release_admission: tokio::sync::Notify,
    count: std::sync::atomic::AtomicUsize,
    pub(super) journal_count: std::sync::atomic::AtomicUsize,
    pub(super) journal_transactions: std::sync::atomic::AtomicUsize,
    pub(super) hold_journal_batch: std::sync::atomic::AtomicBool,
    pub(super) journal_batch_ready: tokio::sync::Notify,
    pub(super) release_journal_batch: tokio::sync::Notify,
    pub(super) fail_journal_ack_at: std::sync::Mutex<Option<usize>>,
    cycles: std::sync::atomic::AtomicUsize,
    cycled: tokio::sync::Notify,
    pause_at: std::sync::Mutex<Option<usize>>,
    pause_operation: std::sync::Mutex<Option<String>>,
    paused: tokio::sync::Notify,
    release: tokio::sync::Notify,
    fired: std::sync::atomic::AtomicBool,
    target: std::sync::Mutex<Option<(usize, bool)>>,
    named_target: std::sync::Mutex<Option<NamedBoundary>>,
    operations: std::sync::Mutex<Vec<Boundary>>,
    committed: std::sync::Mutex<Vec<Boundary>>,
    entered: tokio::sync::Notify,
    fired_notify: tokio::sync::Notify,
    pub(super) shutdown_wait: tokio::sync::Notify,
    pub(super) startup_recovery_done: std::sync::atomic::AtomicBool,
    pub(super) startup_recovery: tokio::sync::Notify,
    pub(super) shutdown_callbacks_draining: tokio::sync::Notify,
    pub(super) shutdown_phase: std::sync::Mutex<&'static str>,
    pub(super) callback_phase: std::sync::Mutex<&'static str>,
}
type Boundary = (String, usize, Option<String>);
type NamedBoundary = (String, usize, Option<String>, bool);
pub(super) struct RejectingPublicationSink {
    pub(super) attempted: Arc<tokio::sync::Notify>,
}
impl crate::notification_runtime_effects::NotificationPostCommitSink for RejectingPublicationSink {
    fn publish<'a>(
        &'a self,
        _: &'a crate::notification_dispatch::NotificationPostCommit,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<(), crate::notification_dispatch::NotificationDispatchError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.attempted.notify_one();
            Err(
                crate::notification_dispatch::NotificationDispatchError::Dependency(
                    "test/publication-held",
                ),
            )
        })
    }
}
impl Faults {
    pub(super) fn mark_shutdown_phase(&self, phase: &'static str) {
        *self.shutdown_phase.lock().unwrap() = phase;
    }

    pub(super) fn current_shutdown_phase(&self) -> &'static str {
        *self.shutdown_phase.lock().unwrap()
    }

    pub(super) fn mark_callback_phase(&self, phase: &'static str) {
        *self.callback_phase.lock().unwrap() = phase;
    }

    pub(super) fn current_callback_phase(&self) -> &'static str {
        *self.callback_phase.lock().unwrap()
    }

    pub(super) async fn pause(&self, index: usize) {
        let pause = *self.pause_at.lock().unwrap() == Some(index)
            || self
                .operations
                .lock()
                .unwrap()
                .get(index)
                .is_some_and(|(name, _, _)| {
                    self.pause_operation.lock().unwrap().as_ref() == Some(name)
                });
        if pause {
            self.paused.notify_one();
            self.release.notified().await;
        }
    }
    pub(super) fn cycle(&self) {
        self.cycles
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.cycled.notify_one();
    }
    async fn after_cycle(&self, previous: usize) {
        while self.cycles.load(std::sync::atomic::Ordering::SeqCst) <= previous {
            tokio::time::timeout(std::time::Duration::from_secs(10), self.cycled.notified())
                .await
                .expect("worker returns to selection while initialization is pending");
        }
    }
    fn arm(&self, target: Option<(usize, bool)>) {
        self.count.store(0, std::sync::atomic::Ordering::SeqCst);
        self.fired.store(false, std::sync::atomic::Ordering::SeqCst);
        *self.target.lock().unwrap() = target;
        *self.named_target.lock().unwrap() = None;
        *self.pause_at.lock().unwrap() = None;
        *self.pause_operation.lock().unwrap() = None;
        self.hold_admission
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.operations.lock().unwrap().clear();
        self.committed.lock().unwrap().clear();
    }
    pub(super) fn before(
        &self,
        name: &str,
        identity: Option<String>,
    ) -> Result<usize, WorkerError> {
        assert!(
            !self.fired.load(std::sync::atomic::Ordering::SeqCst),
            "bridge remains crashed"
        );
        let mut operations = self.operations.lock().unwrap();
        let occurrence = operations
            .iter()
            .filter(|(n, _, old_identity)| n == name && old_identity == &identity)
            .count();
        operations.push((name.into(), occurrence, identity));
        drop(operations);
        let index = self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.entered.notify_one();
        self.check(index, false)?;
        Ok(index)
    }
    pub(super) fn after(&self, index: usize, success: bool) -> Result<(), WorkerError> {
        if success {
            self.committed
                .lock()
                .unwrap()
                .push(self.operations.lock().unwrap()[index].clone());
        }
        self.check(index, true)
    }
    fn check(&self, index: usize, after: bool) -> Result<(), WorkerError> {
        let named = self.named_target.lock().unwrap().clone();
        let operation = self.operations.lock().unwrap().get(index).cloned();
        if *self.target.lock().unwrap() == Some((index, after))
            || named.is_some_and(|(name, occurrence, identity, side)| {
                side == after && operation == Some((name, occurrence, identity))
            })
        {
            self.fired.store(true, std::sync::atomic::Ordering::SeqCst);
            self.fired_notify.notify_one();
            // Unwind the worker future immediately: no error handler or finish runs.
            panic!("injected crash at bridge boundary {operation:?} after={after}")
        } else {
            Ok(())
        }
    }
}

#[test]
fn outcome_boundary_identity_is_stable_across_replay_interleaving() {
    fn selected_boundary(methods: &[&str]) -> Boundary {
        let faults = Faults::default();
        *faults.named_target.lock().unwrap() = Some((
            "outcome".into(),
            5,
            Some("generation=1;request-method=turn/start".into()),
            false,
        ));
        for method in methods {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                faults.before(
                    "outcome",
                    Some(format!("generation=1;request-method={method}")),
                )
            }));
            if result.is_err() {
                return faults.operations.lock().unwrap().last().unwrap().clone();
            }
        }
        panic!("named outcome boundary was not reached");
    }

    // Matrix scenario 18 maps to integration scenario 25. The retained-submit
    // replay can interleave interrupt outcomes while start outcomes are draining.
    // The sixth start has absolute outcome ordinal 5 in baseline and 7 in replay.
    let baseline = selected_boundary(&[
        "turn/start",
        "turn/start",
        "turn/start",
        "turn/start",
        "turn/start",
        "turn/start",
    ]);
    let replay = selected_boundary(&[
        "turn/start",
        "turn/interrupt",
        "turn/start",
        "turn/interrupt",
        "turn/start",
        "turn/start",
        "turn/start",
        "turn/start",
    ]);
    let expected = (
        "outcome".to_owned(),
        5,
        Some("generation=1;request-method=turn/start".to_owned()),
    );
    assert_eq!(baseline, expected);
    assert_eq!(replay, expected);
}

fn copy_template(source: &std::path::Path, target: &std::path::Path) {
    std::fs::create_dir_all(target).unwrap();
    std::fs::set_permissions(target, std::fs::metadata(source).unwrap().permissions()).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let destination = target.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_template(&entry.path(), &destination);
        } else {
            std::fs::copy(entry.path(), destination).unwrap();
        }
    }
}
fn test_root() -> PathBuf {
    std::env::var_os("STUDIO_ACCOUNT_TEST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("studio-account-worker-tests"))
}
fn template() -> (PathBuf, Value) {
    let parent = test_root();
    std::fs::create_dir_all(&parent).unwrap();
    let digest = studio_runtime_store::sha256_hex(CREATE_AGENTS.as_bytes());
    let root = parent.join(format!("template-{}", &digest[..16]));
    let lock = std::fs::File::create(parent.join("template.lock")).unwrap();
    let (lock_ready, acquired_lock) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = lock.lock().map(|()| lock);
        lock_ready.send(result).ok();
    });
    let _lock = acquired_lock
        .recv_timeout(FIXTURE_LOCK_HANG_GUARD)
        .expect("fixture lock acquisition hang guard")
        .expect("fixture lock acquisition");
    let manifest = root.join("agents.json");
    if !manifest.exists() {
        std::fs::create_dir_all(&root).unwrap();
        let output = Command::new("timeout")
            .arg("120")
            .arg(crate::agents_create::prepared_python(
                &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
            ))
            .arg("-c")
            .arg(CREATE_AGENTS)
            .arg(&root)
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"))
            .arg("1")
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/turn_lifecycle"))
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::write(&manifest, output.stdout).unwrap();
    }
    let agents = serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
    (root, agents)
}

struct Factory(std::sync::Mutex<Option<ProviderAttachment>>);
impl ProviderTaskFactory for Factory {
    fn open(&self, _: ProviderHandle, _: OpenOptions, _: Shutdown) -> OpenFuture<'_> {
        let attachment = self.0.lock().unwrap().take().unwrap();
        Box::pin(async { Ok(attachment) })
    }
}
struct Wire {
    value: Value,
    receipt: oneshot::Sender<CommandReceipt>,
}
struct Peer {
    claude: bool,
    account: String,
    generation: Generation,
    factory: Arc<Factory>,
    entries: mpsc::Sender<JournalEntry>,
    writes: mpsc::Receiver<Wire>,
    acks: mpsc::Receiver<EventCursor>,
    stop: Shutdown,
    clock: SessionClock,
    cursor: u64,
    ready: watch::Receiver<bool>,
}
impl Peer {
    fn new() -> Self {
        Self::gated(None)
    }
    fn gated(gate: Option<oneshot::Receiver<()>>) -> Self {
        Self::identity("default", 1, gate)
    }
    fn identity(account: &str, generation: u64, gate: Option<oneshot::Receiver<()>>) -> Self {
        Self::identity_with_cleanup(account, generation, gate, None)
    }
    fn identity_with_cleanup(
        account: &str,
        generation: u64,
        mut gate: Option<oneshot::Receiver<()>>,
        cleanup_error: Option<String>,
    ) -> Self {
        let generation = Generation(generation);
        MATRIX
            .try_with(|context| {
                context
                    .generation
                    .store(generation.0, std::sync::atomic::Ordering::SeqCst)
            })
            .ok();
        let stop = Shutdown::default();
        let mut stopped = stop.subscribe();
        let (commands, mut inbox) = mpsc::channel::<AdapterCommand>(16);
        let (entries, journal) = mpsc::channel(16);
        let (out, writes) = mpsc::channel(16);
        let (ack, acks) = mpsc::channel(16);
        let (status, observed) = watch::channel(AttachmentStatus {
            handle: ProviderHandle::for_account(account),
            open_attempted: true,
            info: Some(AttachmentInfo {
                handle: ProviderHandle::for_account(account),
                generation,
                pid: 1,
                acknowledged: EventCursor(0),
                resumed: false,
            }),
            initialization: InitializationStatus::AwaitingReply,
        });
        let (ready_tx, ready) = watch::channel(false);
        let matrix = MATRIX.try_with(Arc::clone).ok();
        let task = tokio::spawn(async move {
            let _status = status;
            loop {
                let command = tokio::select! { biased; _=Shutdown::cancelled(&mut stopped)=>break, command=inbox.recv()=>match command {Some(c)=>c,None=>break}};
                match command {
                    AdapterCommand::Write {
                        write,
                        reply,
                        observed,
                    } => {
                        let initialized = write.message()["method"] == "initialized";
                        let result = match write.message()["method"].as_str() {
                            Some("initialize") => {
                                if let Some(gate) = gate.take() {
                                    gate.await.unwrap();
                                }
                                CommandReceipt::CachedInitialize(json!({}))
                            }
                            Some("initialized") => CommandReceipt::Written {
                                remote_id: None,
                                duplicate: false,
                            },
                            _ => {
                                if let Some(context) = &matrix {
                                    context.writes.lock().unwrap().push(write.message().clone());
                                }
                                let (receipt, result) = oneshot::channel();
                                tokio::select! {biased; _=Shutdown::cancelled(&mut stopped)=>break, sent=out.send(Wire {value:write.message().clone(),receipt})=>{sent.unwrap();}}
                                tokio::select! {biased; _=Shutdown::cancelled(&mut stopped)=>break, r=result=>r.unwrap_or(CommandReceipt::Unknown)}
                            }
                        };
                        let _ignored = reply.send(result);
                        tokio::select! {biased; _=Shutdown::cancelled(&mut stopped)=>break, _=observed=>{}}
                        if initialized {
                            ready_tx.send_replace(true);
                        }
                    }
                    AdapterCommand::Ack { cursor, reply } => {
                        if let Some(context) = &matrix {
                            let mut acknowledged = context.acknowledged.lock().unwrap();
                            let previous = acknowledged.entry(generation.0).or_default();
                            *previous = (*previous).max(cursor.0);
                        }
                        let _ignored = reply.send(CommandReceipt::Acknowledged(cursor));
                        tokio::select! {biased; _=Shutdown::cancelled(&mut stopped)=>break, sent=ack.send(cursor)=>{sent.unwrap();}}
                    }
                }
            }
            TaskExit {
                result: Ok(()),
                cleanup_error,
                recovery_needed: None,
            }
        });
        let factory = Arc::new(Factory(std::sync::Mutex::new(Some(
            ProviderAttachment::from_task(commands, journal, observed, stop.clone(), task),
        ))));
        Self {
            claude: false,
            account: account.into(),
            generation,
            factory,
            entries,
            writes,
            acks,
            stop,
            clock: SessionClock::default(),
            cursor: 0,
            ready,
        }
    }
    async fn worker(&self, bridge: StoreBridge) -> AccountHandle {
        let worker = self.open_worker(bridge).await;
        let mut ready = self.ready.clone();
        while !*ready.borrow_and_update() {
            ready.changed().await.unwrap();
        }
        worker
    }
    async fn open_worker(&self, bridge: StoreBridge) -> AccountHandle {
        self.open_worker_with_resources(bridge, test_resources().unwrap())
            .await
    }
    async fn open_worker_with_resources(
        &self,
        bridge: StoreBridge,
        resources: preparation::PreparationResources,
    ) -> AccountHandle {
        self.open_worker_with_resources_and_sink(
            bridge,
            resources,
            Arc::new(crate::notification_runtime_effects::UnavailablePostCommitSink),
        )
        .await
    }
    async fn open_worker_with_resources_and_sink(
        &self,
        bridge: StoreBridge,
        resources: preparation::PreparationResources,
        sink: Arc<dyn crate::notification_runtime_effects::NotificationPostCommitSink>,
    ) -> AccountHandle {
        start_account_worker_with_publication_sink(
            bridge,
            self.account.clone(),
            self.factory.clone(),
            OpenOptions {
                clock: self.clock.clone(),
                launch: ProviderLaunchSpec::new(
                    vec!["fake".into()],
                    BTreeMap::new(),
                    "/var/tmp".into(),
                ),
                expected_generation: Some(self.generation),
            },
            SessionTaskConfig {
                initialize_params: json!({}),
                generation: self.generation,
                claude: self.claude,
                timeout_ms: 60_000,
                initialize_operation: OperationId("init".into()),
                request_id_prefix: None,
                recovered: vec![],
            },
            resources,
            sink,
        )
        .await
        .unwrap()
    }
    async fn next_write(&mut self) -> Wire {
        tokio::time::timeout(std::time::Duration::from_secs(120), self.writes.recv())
            .await
            .expect("provider write deadline")
            .expect("worker submits request")
    }
    async fn write(&mut self, method: &str) -> Value {
        let wire = self.next_write().await;
        assert_eq!(wire.value["method"], method);
        let _ignored = wire.receipt.send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        });
        wire.value
    }
    async fn frame(&mut self, value: Value) -> u64 {
        self.cursor += 1;
        MATRIX
            .try_with(|context| {
                context
                    .frames
                    .lock()
                    .unwrap()
                    .insert((self.generation.0, self.cursor), value.clone())
            })
            .ok();
        self.entries
            .send(JournalEntry {
                received_at_ms: self.clock.now_ms(),
                cursor: EventCursor(self.cursor),
                generation: self.generation,
                payload: JournalPayload::Frame(
                    studio_runtime_providers::LineDecoder::new()
                        .push(format!("{value}\n").as_bytes())
                        .unwrap()
                        .remove(0),
                ),
            })
            .await
            .unwrap();
        self.cursor
    }
    async fn acknowledged(&mut self, cursor: u64) {
        loop {
            let got = tokio::time::timeout(std::time::Duration::from_secs(120), self.acks.recv())
                .await
                .expect("settlement deadline")
                .expect("session acknowledges")
                .0;
            if got >= cursor {
                assert_eq!(got, cursor);
                break;
            }
        }
    }
    async fn acknowledged_or_worker_exits(&mut self, cursor: u64, worker: &mut AccountHandle) {
        loop {
            let received = tokio::select! {
                result = tokio::time::timeout(std::time::Duration::from_secs(120), self.acks.recv()) =>
                    result.expect("settlement deadline"),
                result = worker.completion() => panic!("worker exited before cursor {cursor} ACK: {result:?}"),
            };
            let got = received.expect("session acknowledges").0;
            if got >= cursor {
                assert_eq!(got, cursor);
                return;
            }
        }
    }
    async fn reply(&mut self, request: &Value, result: Value) {
        let cursor = self
            .frame(json!({"id":request["id"],"result":result}))
            .await;
        self.acknowledged(cursor).await;
    }
    async fn accepted(&mut self, request: &Value) {
        self.reply(request, json!({"turn":{"id":"turn"}})).await;
    }
    async fn completed(&mut self) {
        let cursor=self.frame(json!({"method":"turn/completed","params":{"threadId":"fake-thread-0","turn":{"id":"turn","status":"completed"}}})).await;
        self.acknowledged(cursor).await;
    }
}
struct State {
    bridge: StoreBridge,
    agent: String,
    epoch: i64,
    root: PathBuf,
    retained: bool,
}
impl State {
    async fn new() -> Self {
        let (agent, epoch, root) = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/account_worker/tests",
            || {
                let root = test_root().join(format!(
                    "worker-{}",
                    uuid::Uuid::new_v5(
                        &uuid::Uuid::NAMESPACE_OID,
                        format!(
                            "{}-{:?}-{:?}",
                            std::process::id(),
                            SystemTime::now(),
                            std::thread::current().id()
                        )
                        .as_bytes()
                    )
                ));
                let (source, agents) = template();
                copy_template(&source, &root);
                (
                    agents[0]["id"].as_str().unwrap().to_owned(),
                    agents[0]["epoch"].as_i64().unwrap(),
                    root,
                )
            },
        )
        .await
        .unwrap();
        let store = Arc::new(RuntimeStore::open(Some(&root)).await.unwrap());
        let mut bridge = StoreBridge::new(store).await.unwrap();
        let retained = MATRIX
            .try_with(|context| {
                bridge.faults = context.faults.clone();
                *context.state.lock().unwrap() =
                    Some((bridge.clone(), agent.clone(), epoch, root.clone()));
            })
            .is_ok();
        let state = Self {
            bridge,
            agent,
            epoch,
            root,
            retained,
        };
        if let Ok(context) = MATRIX.try_with(Arc::clone) {
            let agent = state.agent.clone();
            let seed = state
                .bridge
                .run(move |store| async move {
                    Ok(
                        AgentRepository::get(&*store.read()?, &AgentId::from(agent.as_str()))?
                            .expect("seed"),
                    )
                })
                .await
                .unwrap();
            *context.agent_seed.lock().unwrap() = Some(seed);
        }
        state.enqueue("input").await;
        state
    }
    async fn enqueue(&self, id: &str) {
        MATRIX
            .try_with(|context| {
                context
                    .inputs
                    .lock()
                    .unwrap()
                    .push(format!("annotation:{id}"))
            })
            .ok();
        self.enqueue_for(&self.agent, id).await;
    }
    async fn enqueue_for(&self, agent: &str, id: &str) {
        let agent = agent.to_owned();
        let id = id.to_owned();
        self.bridge
            .run(move |store| async move {
                store
                    .transaction_with(move |tx| {
                        annotate(
                            tx,
                            AnnotationWrite {
                                agent_id: agent,
                                id: Some(id.clone()),
                                path: "README.md".into(),
                                line: 1,
                                text: id.clone(),
                                turn_id: None,
                                signature: id,
                            },
                        )
                        .map_err(|e| StoreError::SchemaMismatch(e.to_string()))
                    })
                    .await?;
                Ok(())
            })
            .await
            .unwrap();
    }
    /// Mark queued input as explicit after-turn delivery, which never steers.
    async fn deliver_after_turn(&self, id: &str) {
        let id = format!("annotation:{id}");
        self.bridge
            .run(move |store| async move {
                store
                    .transaction_with(move |tx| {
                        tx.execute(
                            "INSERT INTO runtime_event_meta(id,record) VALUES(?1,'{\"delivery\":\"after_turn\"}') \
                             ON CONFLICT(id) DO UPDATE SET record=json_set(record,'$.delivery','after_turn')",
                            [id],
                        )?;
                        Ok::<_, StoreError>(())
                    })
                    .await?;
                Ok(())
            })
            .await
            .unwrap();
    }
    async fn record(&self) -> Value {
        let agent = self.agent.clone();
        self.bridge
            .run(move |store| async move {
                let raw: String = store.read()?.query_row(
                    "SELECT record FROM runtime_agents WHERE id=?1",
                    [agent],
                    |r| r.get(0),
                )?;
                Ok(serde_json::from_str(&raw).unwrap())
            })
            .await
            .unwrap()
    }
    async fn event_status(&self, id: &str) -> String {
        let id = format!("annotation:{id}");
        self.bridge
            .run(move |store| async move {
                Ok(store.read()?.query_row(
                    "SELECT status FROM runtime_events WHERE id=?1",
                    [id],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap()
    }
    async fn wake(&self, worker: &AccountHandle) {
        worker
            .send(AccountCommand::Wake {
                agent_id: self.agent.clone(),
                epoch: self.epoch,
            })
            .await
            .unwrap();
    }
    async fn start(&self, peer: &mut Peer, worker: &AccountHandle) -> Value {
        self.wake(worker).await;
        let resume = peer.write("thread/resume").await;
        peer.reply(&resume, json!({"thread":{"id":"fake-thread-0"}}))
            .await;
        let wire = peer.next_write().await;
        assert_eq!(wire.value["method"], "turn/start");
        let operation = wire.value["id"].as_str().unwrap().to_owned();
        self.bridge
            .run(move |store| async move {
                store
                    .transaction_with(move |tx| {
                        assert!(
                            store::receipt(tx, &operation, "submitted")?.is_some(),
                            "Submitted must commit before transport admission"
                        );
                        Ok::<_, WorkerError>(())
                    })
                    .await?;
                Ok(())
            })
            .await
            .unwrap();
        wire.receipt
            .send(CommandReceipt::Written {
                remote_id: None,
                duplicate: false,
            })
            .ok()
            .expect("write receipt receiver");
        wire.value
    }
}
impl Drop for State {
    fn drop(&mut self) {
        if !self.retained {
            let _ignored = std::fs::remove_dir_all(&self.root);
        }
    }
}
async fn scenario_normal_turn_commits_transcript_before_ack_and_pending_event_waits_for_settlement()
{
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    state.enqueue("after").await;
    state.deliver_after_turn("after").await;
    state.wake(&worker).await;
    let marker=peer.frame(json!({"method":"item/completed","params":{"threadId":"fake-thread-0","turnId":"turn","item":{"id":"answer","type":"agentMessage","text":"done"}}})).await;
    peer.acknowledged(marker).await;
    assert_eq!(state.event_status("after").await, "pending");
    assert!(
        peer.writes.try_recv().is_err(),
        "nothing sent into the active turn"
    );
    let count = state
        .bridge
        .run(move |store| async move {
            Ok(store.read()?.query_row(
                "SELECT COUNT(*) FROM runtime_items WHERE json_extract(record,'$.role')='assistant'",
                [],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(count, 1, "notification committed before ACK");
    peer.completed().await;
    let resume = peer.write("thread/resume").await;
    peer.reply(&resume, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    let next = peer.write("turn/start").await;
    assert!(
        next["params"]["input"][0]["text"]
            .as_str()
            .unwrap()
            .contains("after")
    );
    assert_eq!(state.event_status("after").await, "dispatching");
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_agent_record_is_held_without_stopping_other_agent_dispatch() {
    let state = State::new().await;
    let bad_id = "invalid-neighbor".to_owned();
    let good_id = state.agent.clone();
    let good_epoch = state.epoch;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    assert_eq!(start["method"], "turn/start");
    assert!(start["params"]["input"].to_string().contains("input"));
    peer.accepted(&start).await;
    let original_record = state
        .bridge
        .run({
            let bad_id = bad_id.clone();
            let good_id = good_id.clone();
            move |store| async move {
                let write_bad_id = bad_id.clone();
                let write_good_id = good_id.clone();
                store.transaction_with(move |tx| {
                    let invalid = json!({
                        "id": write_bad_id.clone(),
                        "rootId": write_good_id.clone(),
                        "epoch": good_epoch,
                        "status": "queued",
                        "accountKey": "default",
                        "autoWake": true,
                        "isLead": true,
                        "threadId": "invalid-neighbor-thread"
                    });
                    AgentRepository::put(
                        tx,
                        &studio_runtime_store::AgentRecord::from_value(invalid)?,
                    )?;
                    annotate(
                        tx,
                        AnnotationWrite {
                            agent_id: write_good_id.clone(),
                            id: Some("valid-neighbor-input".into()),
                            path: "README.md".into(),
                            line: 1,
                            text: "valid neighbor input".into(),
                            turn_id: None,
                            signature: "valid-neighbor-input".into(),
                        },
                    )
                    .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
                    let mut good = AgentRepository::get(tx, &AgentId::from(write_good_id.as_str()))?
                        .expect("valid neighbor agent");
                    good.extra.insert("nativeEffort".into(), Value::Null);
                    good.extra.insert("executionSettingsAccountKey".into(), json!("default"));
                    AgentRepository::put(tx, &good)?;
                    tx.execute(
                        "UPDATE runtime_agents SET record=json_set(record,'$.rootId',json('23')) WHERE id=?1",
                        [&write_bad_id],
                    )?;
                    Ok::<_, StoreError>(())
                }).await?;
                Ok(store.read()?.query_row(
                    "SELECT record FROM runtime_agents WHERE id=?1",
                    [&bad_id],
                    |row| row.get::<_, String>(0),
                )?)
            }
        })
        .await
        .expect("seed one malformed agent and a pending valid-agent event");
    worker
        .send(AccountCommand::Wake {
            agent_id: bad_id.clone(),
            epoch: good_epoch,
        })
        .await
        .expect("worker accepts malformed-agent wake");
    let (barrier, reached) = tokio::sync::oneshot::channel();
    worker
        .send(AccountCommand::Barrier(barrier))
        .await
        .expect("worker accepts barrier after malformed-agent wake");
    reached
        .await
        .expect("worker reaches barrier after malformed-agent wake");
    let (stored_record, diagnostic, json_extract_safe, projected) = state
        .bridge
        .run({
            let bad_id = bad_id.clone();
            move |store| async move {
                let db = store.read()?;
                let raw: String = db.query_row(
                    "SELECT record FROM runtime_agents WHERE id=?1",
                    [&bad_id],
                    |row| row.get(0),
                )?;
                let receipt_id = format!(
                    "{}{bad_id}",
                    studio_runtime_store::INVALID_AGENT_HOLD_RECEIPT_PREFIX
                );
                let diagnostic: String = db.query_row(
                    "SELECT result FROM runtime_operation_receipts WHERE id=?1 AND json_valid(result)",
                    [receipt_id],
                    |row| row.get(0),
                )?;
                let json_extract_safe: bool = db.query_row(
                    "SELECT json_extract(result,'$.held')=1 AND json_extract(result,'$.diagnostic') IS NOT NULL FROM runtime_operation_receipts WHERE id=?1",
                    [format!("{}{bad_id}", studio_runtime_store::INVALID_AGENT_HOLD_RECEIPT_PREFIX)],
                    |row| row.get(0),
                )?;
                let projected: String = db.query_row(
                    "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?1",
                    [&bad_id],
                    |row| row.get(0),
                )?;
                Ok((raw, diagnostic, json_extract_safe, projected))
            }
        })
        .await
        .expect("read original agent row and separate hold");
    assert!(
        json_extract_safe,
        "held receipt remains safe for JSON SQL consumers"
    );
    let projected: Value = serde_json::from_str(&projected).expect("held agent projection JSON");
    assert_eq!(projected["value"]["status"], "failed");
    assert_eq!(projected["value"]["autoWake"], false);
    assert_eq!(projected["value"]["rootId"], bad_id);
    assert_eq!(projected["value"]["isLead"], true);
    assert!(
        projected["value"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("runtime_agents/invalid-neighbor"))
    );
    assert_eq!(
        stored_record, original_record,
        "held row bytes remain untouched"
    );
    let held: Value = serde_json::from_str(&stored_record).expect("original JSON row");
    let receipt_result: Value =
        serde_json::from_str(&diagnostic).expect("held result remains valid JSON");
    assert_eq!(receipt_result["held"], true);
    let message = receipt_result["diagnostic"]
        .as_str()
        .expect("diagnostic text");
    assert!(
        message.contains(&format!("runtime_agents/{bad_id}")),
        "{message}"
    );
    assert!(message.contains("/rootId"), "{message}");
    assert!(message.contains("type"), "{message}");
    assert!(message.contains("value type number"), "{message}");
    assert!(message.contains("length not applicable"), "{message}");
    assert_eq!(held["rootId"], 23);
    assert!(
        peer.writes.try_recv().is_err(),
        "invalid agent has no provider effect"
    );

    // Finish the fixture's initial turn and prove the valid neighbor is still
    // dispatched by this same account worker.
    peer.completed().await;
    state.wake(&worker).await;
    let resume = peer.write("thread/resume").await;
    peer.reply(&resume, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    let next = peer.write("turn/start").await;
    assert!(
        next["params"]["input"][0]["text"]
            .as_str()
            .unwrap()
            .contains("valid neighbor input")
    );
    peer.accepted(&next).await;

    worker
        .send(AccountCommand::Wake {
            agent_id: bad_id.clone(),
            epoch: good_epoch,
        })
        .await
        .expect("worker accepts malformed-agent wake");
    let (barrier, reached) = tokio::sync::oneshot::channel();
    worker
        .send(AccountCommand::Barrier(barrier))
        .await
        .expect("worker accepts barrier after malformed-agent wake");
    reached
        .await
        .expect("worker reaches barrier after malformed-agent wake");
    assert!(
        peer.writes.try_recv().is_err(),
        "invalid agent has no provider effect"
    );

    worker
        .shutdown()
        .await
        .expect("account worker remains healthy");
}

#[tokio::test]
async fn invalid_agent_hold_rechecks_the_same_field_before_publishing() {
    let directory = tempfile::tempdir().expect("temporary state directory");
    let store = Arc::new(
        RuntimeStore::open(Some(directory.path()))
            .await
            .expect("runtime store opens"),
    );
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_agents(id,record) VALUES('racy',?1)",
                [json!({
                    "id":"racy", "rootId":17, "epoch":1,
                    "status":"idle", "isLead":true
                })
                .to_string()],
            )?;
            Ok(())
        })
        .await
        .expect("seed malformed row");

    let observed = {
        let reader = store.read().expect("read connection opens");
        AgentRepository::get(&reader, &AgentId::from("racy"))
            .expect_err("malformed root id is rejected")
    };
    let (pointer, reason) = match observed {
        StoreError::InvalidSyncObjectField {
            scope,
            id,
            pointer,
            reason,
        } if scope == "runtime_agents" && id == "racy" => (pointer, reason),
        error => panic!("unexpected malformed-agent error: {error}"),
    };

    store
        .transaction(move |tx| {
            AgentRepository::put(
                tx,
                &studio_runtime_store::AgentRecord::from_value(json!({
                    "id":"racy", "rootId":"racy", "epoch":1,
                    "status":"idle", "isLead":true
                }))?,
            )?;
            Ok(())
        })
        .await
        .expect("repair the row before the hold transaction");

    store::hold_invalid_agent_record(&store, "racy", &pointer, &reason, "stale diagnostic")
        .await
        .expect("stale hold is safely ignored");

    let reader = store.read().expect("read connection opens");
    let receipt: bool = reader
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_operation_receipts WHERE id=?1)",
            [format!(
                "{}racy",
                studio_runtime_store::INVALID_AGENT_HOLD_RECEIPT_PREFIX
            )],
            |row| row.get(0),
        )
        .expect("hold receipt query succeeds");
    assert!(!receipt, "a repaired row must not receive a stale hold");
    assert!(
        AgentRepository::get(&reader, &AgentId::from("racy"))
            .expect("repaired record remains readable")
            .is_some()
    );
}
async fn scenario_interrupt_before_start_reply_uses_observed_turn_and_revokes_epoch() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    let cursor=peer.frame(json!({"method":"turn/started","params":{"threadId":"fake-thread-0","turn":{"id":"turn"}}})).await;
    peer.acknowledged(cursor).await;
    worker
        .send(AccountCommand::Stop {
            agent_id: state.agent.clone(),
        })
        .await
        .unwrap();
    let interrupt = peer.write("turn/interrupt").await;
    assert_eq!(interrupt["params"]["turnId"], "turn");
    peer.reply(&interrupt, json!({})).await;
    peer.accepted(&start).await;
    assert!(state.record().await["epoch"].as_i64().unwrap() > state.epoch);
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn unresolved_receipt_for_held_invalid_agent_does_not_block_recovery() {
    let state = State::new().await;
    let agent_id = state.agent.clone();
    let (recovered, unresolved) = state
        .bridge
        .run({
            let agent_id = agent_id.clone();
            move |store| async move {
                let transaction_agent = agent_id.clone();
                let (pointer, reason) = store.transaction(move |tx| {
                    tx.execute(
                        "UPDATE runtime_agents SET record=json_set(record,'$.rootId',json('23')) WHERE id=?1",
                        [&transaction_agent],
                    )?;
                    let invalid = AgentRepository::get(tx, &AgentId::from(transaction_agent.as_str()))
                        .expect_err("corrupt root id is rejected");
                    match invalid {
                        StoreError::InvalidSyncObjectField {
                            pointer, reason, ..
                        } => Ok((pointer, reason)),
                        error => Err(error),
                    }
                }).await?;
                store::hold_invalid_agent_record(
                    &store,
                    &agent_id,
                    &pointer,
                    &reason,
                    "runtime_agents/rootId invalid",
                ).await?;
                store.transaction(move |tx| {
                    let context = json!({
                        "intent": {
                            "agent_id": agent_id, "epoch": 1, "attempt_id": "uncertain-start",
                            "account_key": "default", "provider": "codex",
                            "thread_id": "fake-thread-0", "cwd": null, "model": null,
                            "events": [], "radio_authorized": false
                        },
                        "method": "thread/start", "operation": "uncertain-start"
                    });
                    let result = json!({"context":context,"request":{}});
                    tx.execute(
                        "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES (?1,'submitted',?2)",
                        rusqlite::params![
                            "account-worker:account:default:uncertain-start:submitted",
                            result.to_string()
                        ],
                    )?;
                    Ok(())
                }).await?;

                let recovered = store::recover(&store, "default", Generation(1), true).await?;
                let unresolved: i64 = store.read()?.query_row(
                    "SELECT COUNT(*) FROM runtime_operation_receipts \
                     WHERE id='account-worker:account:default:uncertain-start:submitted'",
                    [],
                    |row| row.get(0),
                )?;
                Ok((recovered.len(), unresolved))
            }
        })
        .await
        .expect("held invalid agent does not make recovery loop");
    assert_eq!(recovered, 0);
    assert_eq!(
        unresolved, 1,
        "unknown submitted receipt remains unresolved"
    );
}
async fn scenario_shutdown_full_queue_and_silent_provider_is_bounded() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    if state
        .bridge
        .faults
        .fired
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    let blocked = peer.writes.recv().await.unwrap();
    for _ in 0..COMMAND_CAPACITY {
        worker
            .try_send(AccountCommand::Wake {
                agent_id: state.agent.clone(),
                epoch: state.epoch,
            })
            .unwrap();
    }
    worker.cancel();
    tokio::time::timeout(SHUTDOWN_GRACE * 2, worker.completion())
        .await
        .expect("bounded shutdown")
        .unwrap();
    drop(blocked);
    assert_ne!(state.event_status("input").await, "delivered");
}
async fn scenario_deadline_unknown_is_not_resent_and_late_reply_refines_it() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(61)).await;
    tokio::time::resume();
    // A journal barrier proves the timeout outcome was consumed before inspection.
    let cursor = peer
        .frame(json!({"method":"test/barrier","params":{}}))
        .await;
    peer.acknowledged(cursor).await;
    assert_eq!(state.event_status("input").await, "uncertain");
    state.wake(&worker).await;
    peer.accepted(&start).await;
    assert_eq!(state.event_status("input").await, "delivered");
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}
async fn scenario_provider_exit_mid_turn_retains_unknown_without_retry() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let _start = state.start(&mut peer, &worker).await;
    peer.stop.cancel();
    let _result = worker.completion().await;
    if MATRIX
        .try_with(|context| {
            context
                .faults
                .fired
                .load(std::sync::atomic::Ordering::SeqCst)
        })
        .unwrap_or(false)
    {
        return;
    }
    assert_eq!(state.event_status("input").await, "uncertain");
    assert!(peer.writes.try_recv().is_err());
}
async fn scenario_duplicate_stale_and_completion_before_reply_preserve_order() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    let completed = json!({"method":"turn/completed","params":{"threadId":"fake-thread-0","turn":{"id":"turn","status":"completed"}}});
    let cursor = peer.frame(completed.clone()).await;
    assert!(peer.acks.try_recv().is_err());
    let reply = peer
        .frame(json!({"id":start["id"],"result":{"turn":{"id":"turn"}}}))
        .await;
    if wait_for_cursor_or_injection(&mut peer, reply).await {
        return;
    }
    assert!(reply > cursor);
    assert_eq!(state.record().await["status"], "completed");
    let duplicate = peer.frame(completed).await;
    peer.acknowledged(duplicate).await;
    let stale=peer.frame(json!({"method":"item/completed","params":{"threadId":"fake-thread-0","turnId":"stale","item":{"id":"stale","type":"agentMessage","text":"wrong"}}})).await;
    peer.acknowledged(stale).await;
    let count=state.bridge.run(move |store| async move { Ok(store.read()?.query_row("SELECT COUNT(*) FROM runtime_items WHERE json_extract(record,'$.role')='assistant'",[],|r|r.get::<_,i64>(0))?) }).await.unwrap();
    assert_eq!(count, 0);
    worker.shutdown().await.unwrap();
}
async fn scenario_cancel_queued_input_prevents_provider_admission() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    worker
        .send(AccountCommand::Stop {
            agent_id: state.agent.clone(),
        })
        .await
        .unwrap();
    state.wake(&worker).await;
    // A read through the worker lane proves Stop and Wake were processed.
    worker_barrier(&worker).await;
    assert!(state.record().await["epoch"].as_i64().unwrap() > state.epoch);
    worker.shutdown().await.unwrap();
    assert!(peer.writes.try_recv().is_err());
    assert_ne!(state.event_status("input").await, "delivered");
}
async fn scenario_generation_closed_holds_the_old_submission() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let _start = state.start(&mut peer, &worker).await;
    peer.entries
        .send(JournalEntry {
            received_at_ms: peer.clock.now_ms(),
            cursor: EventCursor(1),
            generation: Generation(2),
            payload: JournalPayload::Stderr(json!({"text":"new generation"})),
        })
        .await
        .unwrap();
    let _result = worker.completion().await;
    if MATRIX
        .try_with(|context| {
            context
                .faults
                .fired
                .load(std::sync::atomic::Ordering::SeqCst)
        })
        .unwrap_or(false)
    {
        return;
    }
    assert_eq!(state.event_status("input").await, "uncertain");
    assert!(peer.writes.try_recv().is_err());
}
async fn scenario_restart_after_submitted_before_written_recovers_unknown_without_resend() {
    let state = State::new().await;
    let agent = state.agent.clone();
    let epoch = state.epoch;
    let id = state
        .bridge
        .run(move |store| async move {
            let intent = store::claim(Arc::clone(&store), "default", &agent, epoch)
                .await?
                .unwrap();
            let operation = format!("account:default:{}:turn/start", intent.attempt_id);
            let request = RecoveredRequest {
                ticket: studio_runtime_providers::RequestTicket {
                    id: RequestId::String(operation.clone()),
                    generation: 1,
                    method: Method::TurnStart,
                },
                operation_id: OperationId(operation.clone()),
                thread_id: intent.thread_id.clone(),
                lightweight: false,
                deadline_ms: 60_000,
                unknown: false,
                remote_id: None,
            };
            let prepared = preparation::prepare(
                Arc::clone(&store),
                &test_resources()?,
                &intent,
                Method::TurnStart,
                1,
            )
            .await?;
            let request_id = request.ticket.id.clone();
            assert!(
                store::submitted(
                    &store,
                    Pending {
                        thread: prepared.thread,
                        compiled: prepared.compiled,
                        intent: prepared.intent,
                        method: "turn/start".into(),
                        operation
                    },
                    "default",
                    request,
                    store::SubmissionEvidence {
                        turn: None,
                        submission: prepared.submission,
                        guard: Some(prepared.guard),
                        connection_id: "test-connection".into(),
                    },
                )
                .await?
            );
            Ok(request_id)
        })
        .await
        .unwrap();
    // Reconstruct the runtime boundary as after a process crash. There is no Written receipt.
    let mut bridge = StoreBridge::new(state.bridge.store.clone()).await.unwrap();
    bridge.faults = state.bridge.faults.clone();
    let mut peer = Peer::new();
    let mut worker = peer.worker(bridge).await;
    let barrier = peer
        .frame(json!({"method":"test/barrier","params":{}}))
        .await;
    peer.acknowledged(barrier).await;
    let history = peer.write("thread/turns/list").await;
    peer.reply(&history, json!({"data":[]})).await;
    for _ in 0..2 {
        let thread = peer.write("thread/read").await;
        peer.reply(
            &thread,
            json!({"thread":{"id":"fake-thread-0","status":{"type":"idle"}}}),
        )
        .await;
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if state.record().await["startRecoveryCheck"]["reason"] == "start-outcome-unconfirmed" {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("history absence remains inconclusive while start is unresolved");
    assert_eq!(state.event_status("input").await, "uncertain");
    assert!(
        peer.writes.try_recv().is_err(),
        "unknown input must never be submitted again"
    );
    let cursor = peer
        .frame(json!({"id":id,"result":{"turn":{"id":"turn"}}}))
        .await;
    peer.acknowledged(cursor).await;
    assert_eq!(state.event_status("input").await, "delivered");
    assert_eq!(state.record().await["status"], "running");
    assert!(
        peer.writes.try_recv().is_err(),
        "confirmed turn continues without resubmission"
    );
    worker.shutdown().await.unwrap();
}

async fn unsupported_callback(method: &str) {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let cursor = peer
        .frame(json!({"id":"callback", "method":method,"params":{}}))
        .await;
    let wire = peer.next_write().await;
    assert_eq!(wire.value["id"], "callback");
    if method == "item/tool/call" {
        assert_eq!(wire.value["result"]["success"], false);
        assert_eq!(wire.value["result"]["contentItems"][0]["type"], "inputText");
    } else {
        assert_eq!(wire.value["error"]["code"], -32601);
        assert_eq!(wire.value["error"]["data"]["kind"], "unsupported_callback");
    }
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    assert!(
                        store::receipt(tx, &format!("journal:default:1:{cursor}"), "settled")?
                            .is_some()
                    );
                    assert!(
                        store::receipt(tx, &format!("callback:default:1:{cursor}"), "submitted")?
                            .is_some()
                    );
                    Ok(())
                })
                .await
        })
        .await
        .unwrap();
    assert!(
        peer.acks.try_recv().is_err(),
        "callback not ACKed before its answer is written"
    );
    wire.receipt
        .send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        })
        .ok()
        .unwrap();
    peer.acknowledged(cursor).await;
    worker.shutdown().await.unwrap();
}
async fn scenario_unsupported_approval_is_answered_after_commit() {
    unsupported_callback("item/commandExecution/requestApproval").await;
}
async fn scenario_unsupported_question_is_answered_after_commit() {
    unsupported_callback("item/tool/requestUserInput").await;
}
async fn scenario_unsupported_dynamic_tool_is_answered_after_commit() {
    unsupported_callback("item/tool/call").await;
}
async fn scenario_unknown_provider_method_is_answered_after_commit() {
    unsupported_callback("unknown/providerMethod").await;
}

struct SameAccountProviderStatus {
    session: super::retained::RetainedSessionPort,
    actor: String,
    epoch: i64,
    started: tokio::sync::mpsc::UnboundedSender<()>,
    result: tokio::sync::mpsc::UnboundedSender<
        Result<super::retained::RequestResult, super::retained::PortError>,
    >,
    release: tokio::sync::Mutex<oneshot::Receiver<()>>,
}

struct GatedShutdownStatus {
    started: tokio::sync::mpsc::UnboundedSender<()>,
    release: tokio::sync::Mutex<oneshot::Receiver<()>>,
}

impl crate::provider_callbacks::tools::StatusWatchPort for GatedShutdownStatus {
    fn status<'a>(
        &'a self,
        _store: &'a RuntimeStore,
        _actor: &'a crate::managed_host_tools::Actor,
        _args: &'a Value,
        _now: f64,
    ) -> futures_util::future::BoxFuture<'a, Result<Value, crate::managed_host_tools::Error>> {
        Box::pin(async move {
            self.started
                .send(())
                .map_err(|_| crate::managed_host_tools::Error::Unavailable("test/shutdown-gate"))?;
            let mut release = self.release.lock().await;
            (&mut *release)
                .await
                .map_err(|_| crate::managed_host_tools::Error::Unavailable("test/shutdown-gate"))?;
            Ok(json!({"shutdownResponse":"written"}))
        })
    }

    fn rules<'a>(
        &'a self,
        _store: &'a RuntimeStore,
        _actor: &'a crate::managed_host_tools::Actor,
        _args: &'a Value,
        _request_id: &'a str,
        _now: f64,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<
            crate::provider_callbacks::tools::RulesActionResult,
            crate::managed_host_tools::Error,
        >,
    > {
        Box::pin(async move {
            Ok(crate::provider_callbacks::tools::RulesActionResult {
                value: Value::Null,
                cancel_monitor_ids: Vec::new(),
            })
        })
    }
}

impl crate::provider_callbacks::tools::StatusWatchPort for SameAccountProviderStatus {
    fn status<'a>(
        &'a self,
        _store: &'a RuntimeStore,
        _actor: &'a crate::managed_host_tools::Actor,
        _args: &'a Value,
        _now: f64,
    ) -> futures_util::future::BoxFuture<'a, Result<Value, crate::managed_host_tools::Error>> {
        Box::pin(async move {
            self.started
                .send(())
                .map_err(|_| crate::managed_host_tools::Error::Unavailable("test/gate"))?;
            let mut release = self.release.lock().await;
            (&mut *release)
                .await
                .map_err(|_| crate::managed_host_tools::Error::Unavailable("test/gate"))?;

            let scope = self.session.scope().clone();
            let request = super::retained::SessionRequest {
                scope: scope.clone(),
                expected_scope: Some(scope),
                operation: "managed-callback-same-account-provider-request".into(),
                actor: Some(super::retained::RequestActor {
                    agent: self.actor.clone(),
                    epoch: self.epoch,
                }),
                transfer: None,
                deleted_actor_session: None,
                method: Method::ThreadRead,
                params: json!({"threadId":"fake-thread-0"}),
                deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
            };
            let result = self.session.request(request).await;
            let _sent = self.result.send(
                result
                    .as_ref()
                    .map(|receipt| receipt.result.clone())
                    .map_err(Clone::clone),
            );
            let receipt = result.map_err(|_| {
                crate::managed_host_tools::Error::Unavailable("test/provider-request")
            })?;
            match receipt.result {
                super::retained::RequestResult::Reply(_) => {
                    Ok(json!({"providerRequest":"written"}))
                }
                super::retained::RequestResult::Rejected(_) => Err(
                    crate::managed_host_tools::Error::Unavailable("test/provider-rejected"),
                ),
                super::retained::RequestResult::NotSent => Err(
                    crate::managed_host_tools::Error::Unavailable("test/provider-not-sent"),
                ),
                super::retained::RequestResult::Unknown => Err(
                    crate::managed_host_tools::Error::Unavailable("test/provider-unknown"),
                ),
            }
        })
    }

    fn rules<'a>(
        &'a self,
        _store: &'a RuntimeStore,
        _actor: &'a crate::managed_host_tools::Actor,
        _args: &'a Value,
        _request_id: &'a str,
        _now: f64,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<
            crate::provider_callbacks::tools::RulesActionResult,
            crate::managed_host_tools::Error,
        >,
    > {
        Box::pin(async move {
            Ok(crate::provider_callbacks::tools::RulesActionResult {
                value: Value::Null,
                cancel_monitor_ids: Vec::new(),
            })
        })
    }
}

#[tokio::test]
async fn managed_callback_same_account_provider_request_is_written_without_blocking_worker() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store.transaction_with::<_, StoreError>(move |tx| {
                tx.execute(
                    "UPDATE runtime_agents SET record=json_set(record,'$.startAttempt.connectionId','1') WHERE id=?1",
                    [agent],
                )?;
                Ok(())
            }).await?;
            Ok(())
        })
        .await
        .unwrap();

    let (started, mut callback_started) = tokio::sync::mpsc::unbounded_channel();
    let (provider_result, mut provider_results) = tokio::sync::mpsc::unbounded_channel();
    let (release, callback_release) = oneshot::channel();
    state.bridge.set_managed_tool_runtime(
        crate::provider_callbacks::ManagedToolRuntime::from_ports_for_test(
            crate::provider_callbacks::ManagedToolRuntimePorts {
                status_watch: Some(Arc::new(SameAccountProviderStatus {
                    session: worker.retained_session(),
                    actor: state.agent.clone(),
                    epoch: state.epoch,
                    started,
                    result: provider_result,
                    release: tokio::sync::Mutex::new(callback_release),
                })),
                child_creation: None,
                monitor_create: None,
                interrupt: None,
                monitor_input: None,
                monitor_cancel: None,
            },
        ),
    );

    let cursor = peer
        .frame(json!({
            "id":"managed-same-account-callback",
            "method":"item/tool/call",
            "params":{
                "threadId":"fake-thread-0",
                "turnId":"turn",
                "callId":"same-account-provider-request",
                "tool":"orchestration_status",
                "arguments":{}
            }
        }))
        .await;
    callback_started
        .recv()
        .await
        .expect("managed callback reaches its gated provider effect");
    release.send(()).expect("release provider effect gate");

    let provider = peer.write("thread/read").await;
    let provider_reply_cursor = peer
        .frame(json!({
            "id":provider["id"],
            "result":{"thread":{"id":"fake-thread-0"}}
        }))
        .await;
    assert!(provider_reply_cursor > cursor);
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), provider_results.recv())
        .await
        .expect("managed callback's retained request resolves after its committed provider reply")
        .expect("retained request result witness");
    assert_eq!(
        result,
        Ok(super::retained::RequestResult::Reply(
            json!({"thread":{"id":"fake-thread-0"}})
        ))
    );
    let callback = peer.next_write().await;
    assert_eq!(callback.value["id"], "managed-same-account-callback");
    let tool_result_text = callback.value["result"]["contentItems"][0]["text"]
        .as_str()
        .expect("managed tool result text");
    let tool_result: Value = serde_json::from_str(tool_result_text)
        .unwrap_or_else(|error| panic!("managed tool result JSON {tool_result_text:?}: {error}"));
    assert_eq!(tool_result["providerRequest"], "written");
    // The finalization time is wall-clock, not the session-relative journal clock.
    let awareness = callback.value["result"]["contentItems"][1]["text"]
        .as_str()
        .and_then(|text| text.strip_prefix("[Time awareness] Tool result finalized at "))
        .expect("time-awareness text");
    assert!(
        awareness[..4].parse::<u32>().unwrap() >= 2026,
        "{awareness}"
    );
    let (reply_journal_committed, checkpoint) = state
        .bridge
        .run(move |runtime_store| async move {
            let state = runtime_store
                .transaction_with::<_, WorkerError>(move |tx| {
                    Ok((
                        store::journal_committed(tx, "default", 1, provider_reply_cursor)?,
                        store::checkpoint(tx, "default", 1)?,
                    ))
                })
                .await?;
            Ok(state)
        })
        .await
        .expect("read provider reply durability state");
    assert!(
        reply_journal_committed || checkpoint >= provider_reply_cursor,
        "live callback reply follows a committed provider reply journal; cursor={provider_reply_cursor}, committed={reply_journal_committed}, checkpoint={checkpoint}"
    );
    callback
        .receipt
        .send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        })
        .ok()
        .expect("callback write receipt receiver");
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_drains_completed_managed_callback_and_writes_response() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store.transaction_with::<_, StoreError>(move |tx| {
                tx.execute(
                    "UPDATE runtime_agents SET record=json_set(record,'$.startAttempt.connectionId','1') WHERE id=?1",
                    [agent],
                )?;
                Ok(())
            }).await?;
            Ok(())
        })
        .await
        .unwrap();

    let (started, mut callback_started) = tokio::sync::mpsc::unbounded_channel();
    let (release, callback_release) = oneshot::channel();
    state.bridge.set_managed_tool_runtime(
        crate::provider_callbacks::ManagedToolRuntime::from_ports_for_test(
            crate::provider_callbacks::ManagedToolRuntimePorts {
                status_watch: Some(Arc::new(GatedShutdownStatus {
                    started,
                    release: tokio::sync::Mutex::new(callback_release),
                })),
                child_creation: None,
                monitor_create: None,
                interrupt: None,
                monitor_input: None,
                monitor_cancel: None,
            },
        ),
    );

    peer.frame(json!({
        "id":"managed-callback-during-shutdown",
        "method":"item/tool/call",
        "params":{
            "threadId":"fake-thread-0",
            "turnId":"turn",
            "callId":"callback-shutdown",
            "tool":"orchestration_status",
            "arguments":{}
        }
    }))
    .await;
    callback_started
        .recv()
        .await
        .expect("managed callback is held at the channel gate");

    worker.cancel();
    state
        .bridge
        .faults
        .shutdown_callbacks_draining
        .notified()
        .await;
    release
        .send(())
        .expect("release callback while shutdown drains");

    let callback = peer.next_write().await;
    assert_eq!(callback.value["id"], "managed-callback-during-shutdown");
    let result: Value = serde_json::from_str(
        callback.value["result"]["contentItems"][0]["text"]
            .as_str()
            .expect("managed tool result text"),
    )
    .expect("managed tool result JSON");
    assert_eq!(result["shutdownResponse"], "written");
    callback
        .receipt
        .send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        })
        .ok()
        .expect("shutdown callback write receipt receiver");
    worker.completion().await.unwrap();
}

async fn scenario_foreign_account_stop_cannot_revoke_agent() {
    let state = State::new().await;
    let before = state.record().await;
    let peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    worker_barrier(&worker).await;
    let agent = state.agent.clone();
    state
        .bridge
        .worker_run("foreign_stop", move |store| async move {
            assert!(
                store::stop(&store, "another-account", &agent)
                    .await?
                    .is_none()
            );
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(state.record().await, before);
    worker.shutdown().await.unwrap();
}

async fn scenario_callback_unknown_write_is_not_resent_on_replay() {
    let state = State::new().await;
    let frame = json!({"id":"callback", "method":"item/tool/call", "params":{}});
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    peer.frame(frame.clone()).await;
    let wire = peer.next_write().await;
    wire.receipt.send(CommandReceipt::Unknown).ok().unwrap();
    let completion = tokio::time::timeout(std::time::Duration::from_secs(120), worker.completion())
        .await
        .expect("callback completion deadline");
    if MATRIX
        .try_with(|context| {
            context
                .faults
                .fired
                .load(std::sync::atomic::Ordering::SeqCst)
        })
        .unwrap_or(false)
    {
        return;
    }
    assert!(matches!(completion, Err(WorkerError::CallbackUnknown)));
    assert!(peer.acks.try_recv().is_err());
    if MATRIX.try_with(|_| ()).is_ok() {
        // Crash-matrix recovery owns the replay phases and records their exact
        // request frame/cursor; do not run an uninstrumented fixture recovery.
        return;
    }
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    peer.frame(frame).await;
    let completion = tokio::time::timeout(std::time::Duration::from_secs(120), worker.completion())
        .await
        .expect("callback completion deadline");
    if MATRIX
        .try_with(|context| {
            context
                .faults
                .fired
                .load(std::sync::atomic::Ordering::SeqCst)
        })
        .unwrap_or(false)
    {
        return;
    }
    assert!(matches!(completion, Err(WorkerError::CallbackUnknown)));
    assert!(peer.writes.try_recv().is_err());
    assert!(peer.acks.try_recv().is_err());
}

async fn scenario_a_new_stop_can_retry_a_definitely_rejected_interrupt() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    worker
        .send(AccountCommand::Stop {
            agent_id: state.agent.clone(),
        })
        .await
        .unwrap();
    let first = peer.write("turn/interrupt").await;
    let rejected = peer
        .frame(json!({"id":first["id"],"error":{"code":-1,"message":"rejected"}}))
        .await;
    if state
        .bridge
        .faults
        .fired
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    peer.acknowledged(rejected).await;
    worker
        .send(AccountCommand::Stop {
            agent_id: state.agent.clone(),
        })
        .await
        .unwrap();
    let second = peer.write("turn/interrupt").await;
    assert_ne!(first["id"], second["id"]);
    peer.reply(&second, json!({})).await;
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn normal_turn_commits_transcript_before_ack_and_pending_event_waits_for_settlement() {
    scenario_normal_turn_commits_transcript_before_ack_and_pending_event_waits_for_settlement()
        .await
}

#[tokio::test]
async fn interrupt_before_start_reply_uses_observed_turn_and_revokes_epoch() {
    scenario_interrupt_before_start_reply_uses_observed_turn_and_revokes_epoch().await
}

#[tokio::test]
async fn shutdown_full_queue_and_silent_provider_is_bounded() {
    scenario_shutdown_full_queue_and_silent_provider_is_bounded().await
}

#[tokio::test]
async fn deadline_unknown_is_not_resent_and_late_reply_refines_it() {
    scenario_deadline_unknown_is_not_resent_and_late_reply_refines_it().await
}

#[tokio::test]
async fn provider_exit_mid_turn_retains_unknown_without_retry() {
    scenario_provider_exit_mid_turn_retains_unknown_without_retry().await
}

#[tokio::test]
async fn duplicate_stale_and_completion_before_reply_preserve_order() {
    scenario_duplicate_stale_and_completion_before_reply_preserve_order().await
}

#[tokio::test]
async fn cancel_queued_input_prevents_provider_admission() {
    scenario_cancel_queued_input_prevents_provider_admission().await
}

#[tokio::test]
async fn generation_closed_holds_the_old_submission() {
    scenario_generation_closed_holds_the_old_submission().await
}

#[tokio::test]
async fn restart_after_submitted_before_written_recovers_unknown_without_resend() {
    scenario_restart_after_submitted_before_written_recovers_unknown_without_resend().await
}

#[tokio::test]
async fn unsupported_approval_is_answered_after_commit() {
    scenario_unsupported_approval_is_answered_after_commit().await
}

#[tokio::test]
async fn unsupported_question_is_answered_after_commit() {
    scenario_unsupported_question_is_answered_after_commit().await
}

#[tokio::test]
async fn unsupported_dynamic_tool_is_answered_after_commit() {
    scenario_unsupported_dynamic_tool_is_answered_after_commit().await
}

#[tokio::test]
async fn unknown_provider_method_is_answered_after_commit() {
    scenario_unknown_provider_method_is_answered_after_commit().await
}

#[tokio::test]
async fn foreign_account_stop_cannot_revoke_agent() {
    scenario_foreign_account_stop_cannot_revoke_agent().await
}

#[tokio::test]
async fn callback_unknown_write_is_not_resent_on_replay() {
    scenario_callback_unknown_write_is_not_resent_on_replay().await
}

#[tokio::test]
async fn a_new_stop_can_retry_a_definitely_rejected_interrupt() {
    scenario_a_new_stop_can_retry_a_definitely_rejected_interrupt().await
}

mod closure;
mod crash_matrix;
use crash_matrix::MATRIX;

async fn wait_for_cursor_or_injection(peer: &mut Peer, cursor: u64) -> bool {
    let Ok(context) = MATRIX.try_with(Arc::clone) else {
        peer.acknowledged(cursor).await;
        return false;
    };
    let notified = context.faults.fired_notify.notified();
    if context
        .faults
        .fired
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return true;
    }
    tokio::select! {
        _ = notified => true,
        acknowledgement = peer.acks.recv() => {
            let acknowledgement = acknowledgement.expect("session acknowledges completion after its start reply");
            assert_eq!(acknowledgement.0, cursor, "completion is held until its start reply");
            false
        }
    }
}

async fn worker_barrier(worker: &AccountHandle) {
    while !worker
        .faults
        .startup_recovery_done
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        let notified = worker.faults.startup_recovery.notified();
        tokio::pin!(notified);
        if worker
            .faults
            .startup_recovery_done
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            break;
        }
        notified.await;
    }
    let (reply, done) = oneshot::channel();
    worker.send(AccountCommand::Barrier(reply)).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(120), done)
        .await
        .expect("worker command deadline")
        .expect("worker command barrier");
}

#[tokio::test]
async fn shutdown_releases_account_while_bridge_permit_is_blocked() {
    let state = State::new().await;
    let peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let permit = state
        .bridge
        .operation_slot
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    let before = state
        .bridge
        .faults
        .count
        .load(std::sync::atomic::Ordering::SeqCst);
    state.wake(&worker).await;
    while state
        .bridge
        .faults
        .count
        .load(std::sync::atomic::Ordering::SeqCst)
        == before
    {
        tokio::time::timeout(
            std::time::Duration::from_secs(120),
            state.bridge.faults.entered.notified(),
        )
        .await
        .expect("setup barrier deadline");
    }
    worker.cancel();
    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        state.bridge.faults.shutdown_wait.notified(),
    )
    .await
    .expect("setup barrier deadline");
    tokio::time::pause();
    tokio::time::advance(SHUTDOWN_GRACE * 2).await;
    assert!(matches!(
        worker.completion().await,
        Err(WorkerError::ShutdownTimeout)
    ));
    tokio::time::resume();
    assert!(!state.bridge.accounts.lock().unwrap().contains("default"));
    drop(permit);
    assert_eq!(
        state.event_status("input").await,
        "pending",
        "the wake was blocked before its claim transaction committed"
    );
    let mut replacement_peer = Peer::new();
    let mut replacement = replacement_peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut replacement_peer, &replacement).await;
    replacement_peer.accepted(&start).await;
    replacement_peer.completed().await;
    assert_eq!(state.event_status("input").await, "delivered");
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn accepted_turn_reattaches_and_keeps_notification_effects() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    worker.shutdown().await.unwrap();
    let mut peer = Peer::new();
    peer.cursor = 2;
    let mut worker = peer.worker(state.bridge.clone()).await;
    let cursor = peer.frame(json!({"method":"item/completed","params":{"threadId":"fake-thread-0","turnId":"turn","item":{"id":"reattached","type":"agentMessage","text":"after restart"}}})).await;
    peer.acknowledged(cursor).await;
    let count = state.bridge.run(move |store| async move { Ok(store.read()?.query_row("SELECT COUNT(*) FROM runtime_items WHERE json_extract(record,'$.text')='after restart'", [], |r|r.get::<_,i64>(0))?) }).await.unwrap();
    assert_eq!(count, 1);
    peer.completed().await;
    assert_eq!(state.record().await["status"], "completed");
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_start_restarts_without_unresolved_transport_receipts() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    let cursor = peer
        .frame(json!({"id":start["id"],"error":{"code":-1,"message":"rejected"}}))
        .await;
    peer.acknowledged(cursor).await;
    worker.shutdown().await.unwrap();
    let peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let unresolved = state.bridge.run(move |store| async move { Ok(store.read()?.query_row("SELECT COUNT(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:account:%:submitted'", [], |r|r.get::<_,i64>(0))?) }).await.unwrap();
    assert_eq!(unresolved, 0);
    assert_ne!(state.event_status("input").await, "uncertain");
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn committed_stop_survives_crash_before_interrupt_admission() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    worker_barrier(&worker).await;
    state.bridge.faults.arm(Some((0, true)));
    worker
        .send(AccountCommand::Stop {
            agent_id: state.agent.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(
        worker.completion().await,
        Err(WorkerError::TaskFailed)
    ));
    state.bridge.faults.arm(None);
    let mut peer = Peer::new();
    peer.cursor = 2;
    let mut worker = peer.worker(state.bridge.clone()).await;
    let interrupt = peer.write("turn/interrupt").await;
    assert_eq!(interrupt["params"]["turnId"], "turn");
    peer.reply(&interrupt, json!({})).await;
    worker.shutdown().await.unwrap();
}

#[test]
fn only_explicit_admission_rejection_can_mark_not_sent() {
    assert!(definite_rejection(&WorkerError::Session(
        SessionTaskError::Capacity
    )));
    for error in [
        WorkerError::TaskFailed,
        WorkerError::Closed,
        WorkerError::Session(SessionTaskError::AdmissionUnknown),
        WorkerError::Session(SessionTaskError::Closed),
        WorkerError::Store(StoreError::SchemaMismatch("unrelated journal".into())),
    ] {
        assert!(!definite_rejection(&error));
    }
}

#[tokio::test]
async fn transfer_between_resume_and_start_never_writes_to_old_account() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    let resume = peer.write("thread/resume").await;
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let mut row =
                        AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                    row.extra.insert("accountKey".into(), json!("transferred"));
                    AgentRepository::put(tx, &row)?;
                    Ok(())
                })
                .await
        })
        .await
        .unwrap();
    peer.reply(&resume, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    worker_barrier(&worker).await;
    assert!(peer.writes.try_recv().is_err());
    assert_eq!(state.record().await["accountKey"], "transferred");
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn acknowledged_journal_prefix_is_compacted() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    for _ in 0..128 {
        let cursor = peer
            .frame(json!({"method":"test/barrier","params":{}}))
            .await;
        peer.acknowledged(cursor).await;
    }
    let rows = state.bridge.run(move |store| async move { Ok(store.read()?.query_row("SELECT COUNT(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:journal:%'", [], |r|r.get::<_,i64>(0))?) }).await.unwrap();
    assert_eq!(rows, 0);
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn consecutive_journal_events_share_one_store_transaction() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    // Native initialization does not finish account restart recovery. Its queued
    // commands deliberately end a journal batch, so settle them before measuring
    // consecutive provider events with no intervening account commands.
    worker_barrier(&worker).await;
    state
        .bridge
        .faults
        .hold_journal_batch
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let transactions_before = state
        .bridge
        .faults
        .journal_transactions
        .load(std::sync::atomic::Ordering::SeqCst);
    let committed_before = state
        .bridge
        .faults
        .journal_count
        .load(std::sync::atomic::Ordering::SeqCst);
    let first = peer
        .frame(json!({"method":"test/batch","params":{"n":0}}))
        .await;
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        state.bridge.faults.journal_batch_ready.notified(),
    )
    .await
    .expect("first journal pauses before collecting the available batch");
    let mut last = first;
    for n in 1..4 {
        last = peer
            .frame(json!({"method":"test/batch","params":{"n":n}}))
            .await;
    }
    let delivered = worker
        .session_journal_barrier()
        .await
        .expect("provider journal prefix is forwarded to the account event queue");
    assert_eq!(
        delivered.0, last,
        "all four events must be queued before the first event is released"
    );
    state
        .bridge
        .faults
        .hold_journal_batch
        .store(false, std::sync::atomic::Ordering::SeqCst);
    state.bridge.faults.release_journal_batch.notify_one();
    peer.acknowledged(last).await;
    let transactions_after = state
        .bridge
        .faults
        .journal_transactions
        .load(std::sync::atomic::Ordering::SeqCst);
    let committed_after = state
        .bridge
        .faults
        .journal_count
        .load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(transactions_after - transactions_before, 1);
    assert_eq!(committed_after - committed_before, 4);
    eprintln!(
        "journal batch evidence: 4 events, {} transaction(s), {} committed entries",
        transactions_after - transactions_before,
        committed_after - committed_before
    );
    assert_eq!(
        peer.acks.try_recv().map(|ack| ack.0),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    );
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn committed_batch_replay_after_mid_batch_ack_loss_is_exactly_once() {
    async fn start(state: &State, peer: &mut Peer) -> AccountHandle {
        let worker = peer.worker(state.bridge.clone()).await;
        let request = state.start(peer, &worker).await;
        peer.accepted(&request).await;
        worker
    }
    fn delta(index: usize) -> Value {
        json!({
            "method":"item/agentMessage/delta",
            "params":{
                "threadId":"fake-thread-0",
                "turnId":"turn",
                "itemId":"batch-replay",
                "delta":format!("part-{index};")
            }
        })
    }
    async fn streams(state: &State) -> Value {
        state
            .bridge
            .run(move |store| async move {
                store.transaction_with(move |tx| {
                    let metadata = store::receipt(tx, "streams:default", "metadata")?
                        .unwrap_or_else(|| json!({"entries": [], "closed": []}));
                    Ok(json!({
                        "entries": metadata["entries"],
                        "closed": metadata["closed"],
                        "pending": store::receipt(tx, "streams:default", "pending")?,
                        "delta": metadata["post_commit"][0]["AnalyticsStream"]["observation"]["params"]["delta"],
                    }))
                }).await
            })
            .await
            .unwrap()
    }

    let recovered_state = State::new().await;
    let mut first_peer = Peer::new();
    let mut worker = start(&recovered_state, &mut first_peer).await;
    let first_batch_cursor = first_peer.cursor + 1;
    recovered_state
        .bridge
        .faults
        .hold_journal_batch
        .store(true, std::sync::atomic::Ordering::SeqCst);
    *recovered_state
        .bridge
        .faults
        .fail_journal_ack_at
        .lock()
        .unwrap() = Some(2);
    let first = first_peer.frame(delta(0)).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        recovered_state.bridge.faults.journal_batch_ready.notified(),
    )
    .await
    .expect("batch pauses before collecting events");
    for index in 1..4 {
        first_peer.frame(delta(index)).await;
    }
    recovered_state
        .bridge
        .faults
        .hold_journal_batch
        .store(false, std::sync::atomic::Ordering::SeqCst);
    recovered_state
        .bridge
        .faults
        .release_journal_batch
        .notify_one();
    first_peer.acknowledged(first + 1).await;
    assert!(matches!(
        worker.completion().await,
        Err(WorkerError::TaskFailed)
    ));
    let committed_snapshot = streams(&recovered_state).await;
    assert_eq!(committed_snapshot["delta"], "part-0;part-1;part-2;part-3;");
    assert_eq!(committed_snapshot["closed"].as_array().unwrap().len(), 1);

    let mut replay_peer = Peer::new();
    replay_peer.cursor = first_batch_cursor - 1;
    let mut replay_worker = replay_peer.worker(recovered_state.bridge.clone()).await;
    for index in 0..4 {
        replay_peer.frame(delta(index)).await;
    }
    replay_peer.acknowledged(first_batch_cursor + 3).await;
    assert!(
        replay_peer.acks.try_recv().is_err(),
        "each replayed event ACKs once"
    );
    assert_eq!(streams(&recovered_state).await, committed_snapshot);
    replay_worker.shutdown().await.unwrap();

    let baseline_state = State::new().await;
    let mut baseline_peer = Peer::new();
    let mut baseline_worker = start(&baseline_state, &mut baseline_peer).await;
    for index in 0..4 {
        let cursor = baseline_peer.frame(delta(index)).await;
        baseline_peer.acknowledged(cursor).await;
    }
    baseline_worker.shutdown().await.unwrap();
    assert_eq!(streams(&baseline_state).await, committed_snapshot);
}

#[tokio::test]
async fn callback_replay_waits_for_initialization_before_admission() {
    let state = State::new().await;
    let (release, gate) = oneshot::channel();
    let mut peer = Peer::gated(Some(gate));
    let mut worker = peer.open_worker(state.bridge.clone()).await;
    state.bridge.faults.after_cycle(0).await;
    let before = state
        .bridge
        .faults
        .cycles
        .load(std::sync::atomic::Ordering::SeqCst);
    let cursor = peer.frame(json!({"id":"replayed-callback","method":"item/tool/call","params":{"threadId":"fake-thread-0","turnId":"turn","callId":"call","tool":"unknown","arguments":{}}})).await;
    state.bridge.faults.after_cycle(before).await;
    assert!(peer.writes.try_recv().is_err());
    release.send(()).unwrap();
    let answer = peer.next_write().await;
    assert_eq!(answer.value["id"], "replayed-callback");
    answer
        .receipt
        .send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        })
        .ok()
        .unwrap();
    peer.acknowledged(cursor).await;
    worker.shutdown().await.unwrap();
}

async fn scenario_durable_stream_batch_recovers_before_flush() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let delta = peer.frame(json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-0","turnId":"turn","itemId":"stream","delta":"durable delta"}})).await;
    peer.acknowledged(delta).await;
    let pending = state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    Ok(store::receipt(tx, "streams:default", "pending")?.is_some())
                })
                .await
        })
        .await
        .unwrap();
    assert!(pending, "ACK requires durable pending text");
    // End the task without finish(), which would flush the batch.
    state.bridge.faults.arm(Some((0, false)));
    let _ = peer
        .frame(json!({"method":"test/barrier","params":{}}))
        .await;
    assert!(matches!(
        worker.completion().await,
        Err(WorkerError::TaskFailed)
    ));
    state.bridge.faults.arm(None);
    let mut peer = Peer::new();
    peer.cursor = delta;
    let mut worker = peer.worker(state.bridge.clone()).await;
    let complete = peer.frame(json!({"method":"item/completed","params":{"threadId":"fake-thread-0","turnId":"turn","item":{"id":"stream","type":"agentMessage","text":""}}})).await;
    peer.acknowledged(complete).await;
    let (text,pending,bases) = state.bridge.run(move |store| async move { store.transaction_with(move |tx| {
        let text:String = tx.query_row("SELECT json_extract(record,'$.text') FROM runtime_items WHERE json_extract(record,'$.id') LIKE '%:stream'", [], |row| row.get(0))?;
        Ok((text,store::receipt(tx,"streams:default","pending")?.is_some(),tx.query_row("SELECT COUNT(*) FROM runtime_operation_receipts WHERE signature='stream-base'",[],|row| row.get::<_,i64>(0))?))
    }).await }).await.unwrap();
    assert_eq!(text, "durable delta");
    assert!(!pending);
    assert_eq!(bases, 0);
    worker.shutdown().await.unwrap();
}
#[tokio::test]
async fn durable_stream_batch_recovers_before_flush() {
    scenario_durable_stream_batch_recovers_before_flush().await;
}

#[tokio::test]
async fn unrelated_store_failure_after_enqueue_keeps_write_unknown() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state
        .bridge
        .faults
        .hold_admission
        .store(true, std::sync::atomic::Ordering::SeqCst);
    state.wake(&worker).await;
    let resume = peer.write("thread/resume").await;
    peer.reply(&resume, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    let held = peer.next_write().await;
    assert_eq!(held.value["method"], "turn/start");
    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        state.bridge.faults.admission_held.notified(),
    )
    .await
    .expect("admitted command receipt remains withheld");
    state.bridge.run(move |store| async move {
        store.transaction_with(move |tx| {
            tx.execute_batch("CREATE TRIGGER fail_journal BEFORE INSERT ON runtime_operation_receipts WHEN NEW.id LIKE 'account-worker:journal:%' BEGIN SELECT RAISE(ABORT,'unrelated journal failure'); END;")?;
            Ok(())
        }).await
    }).await.unwrap();
    peer.frame(json!({"method":"test/barrier","params":{}}))
        .await;
    worker_barrier(&worker).await;
    assert_eq!(state.event_status("input").await, "uncertain");
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    tx.execute_batch("DROP TRIGGER fail_journal")?;
                    Ok(())
                })
                .await
        })
        .await
        .unwrap();
    drop(held);
    worker.shutdown().await.unwrap();
}

async fn scenario_stream_base_survives_replaced_visible_item() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let delta = peer.frame(json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-0","turnId":"turn","itemId":"base","delta":"a".repeat(64 * 1024)}})).await;
    peer.acknowledged(delta).await;
    let full_size = state
        .bridge
        .run(move |store| async move {
            Ok(store.read()?.query_row(
                "SELECT length(body) FROM runtime_item_fulltext WHERE id LIKE '%:base'",
                [],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(
        full_size,
        64 * 1024,
        "fixed-size delta crosses the flush threshold before completion"
    );
    let started = peer.frame(json!({"method":"item/started","params":{"threadId":"fake-thread-0","turnId":"turn","item":{"id":"base","type":"agentMessage","text":"replacement"}}})).await;
    peer.acknowledged(started).await;
    let delta = peer.frame(json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-0","turnId":"turn","itemId":"base","delta":"tail"}})).await;
    peer.acknowledged(delta).await;
    let completed = peer.frame(json!({"method":"item/completed","params":{"threadId":"fake-thread-0","turnId":"turn","item":{"id":"base","type":"agentMessage","text":""}}})).await;
    peer.acknowledged(completed).await;
    let (text, bases) = state
        .bridge
        .run(move |store| async move {
            let db = store.read()?;
            let text: String = db.query_row(
                "SELECT body FROM runtime_item_fulltext WHERE id LIKE '%:base'",
                [],
                |r| r.get(0),
            )?;
            let bases: i64 = db.query_row(
                "SELECT COUNT(*) FROM runtime_operation_receipts WHERE signature='stream-base'",
                [],
                |r| r.get(0),
            )?;
            Ok((text, bases))
        })
        .await
        .unwrap();
    assert_eq!(text, format!("{}tail", "a".repeat(64 * 1024)));
    assert_eq!(bases, 0);
    worker.shutdown().await.unwrap();
}
#[tokio::test]
async fn stream_base_survives_replaced_visible_item() {
    scenario_stream_base_survives_replaced_visible_item().await;
}

#[tokio::test]
async fn stop_and_wake_progress_with_a_replenished_event_lane() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let permit = state
        .bridge
        .operation_slot
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    let before = state
        .bridge
        .faults
        .count
        .load(std::sync::atomic::Ordering::SeqCst);
    peer.frame(json!({"method":"test/barrier","params":{}}))
        .await;
    while state
        .bridge
        .faults
        .count
        .load(std::sync::atomic::Ordering::SeqCst)
        == before
    {
        tokio::time::timeout(
            std::time::Duration::from_secs(120),
            state.bridge.faults.entered.notified(),
        )
        .await
        .expect("setup barrier deadline");
    }
    // With the worker blocked in its first transaction, 32 admitted entries fill
    // both bounded 16-entry lanes. More are replenished while commands run.
    for _ in 0..32 {
        peer.frame(json!({"method":"test/barrier","params":{}}))
            .await;
    }
    let entries = peer.entries.clone();
    let clock = peer.clock.clone();
    let producer = tokio::spawn(async move {
        for cursor in 34..66 {
            if entries
                .send(JournalEntry {
                    cursor: EventCursor(cursor),
                    generation: Generation(1),
                    received_at_ms: clock.now_ms(),
                    payload: JournalPayload::Frame(
                        studio_runtime_providers::LineDecoder::new()
                            .push(b"{\"method\":\"test/barrier\",\"params\":{}}\n")
                            .unwrap()
                            .remove(0),
                    ),
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });
    worker
        .send(AccountCommand::Stop {
            agent_id: state.agent.clone(),
        })
        .await
        .unwrap();
    state.wake(&worker).await;
    let (done, observed) = oneshot::channel();
    worker.send(AccountCommand::Barrier(done)).await.unwrap();
    drop(permit);
    let processed = tokio::time::timeout(std::time::Duration::from_secs(10), observed)
        .await
        .expect("commands cannot wait for the journal to empty")
        .unwrap();
    assert!(
        processed <= 5,
        "three queued commands waited for {processed} journal entries"
    );
    assert!(state.record().await["epoch"].as_i64().unwrap() > state.epoch);
    assert!(peer.writes.try_recv().is_err());
    worker.cancel();
    producer.abort();
    let _cancelled = producer.await;
    let _completed = worker.completion().await;
}

#[tokio::test]
async fn bookkeeping_rows_stay_bounded_across_completed_turns() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    for turn in 0..24 {
        if turn > 0 {
            state.enqueue(&format!("input-{turn}")).await;
        }
        let start = state.start(&mut peer, &worker).await;
        peer.reply(&start, json!({"turn":{"id":format!("turn-{turn}")}}))
            .await;
        let cursor=peer.frame(json!({"method":"turn/completed","params":{"threadId":"fake-thread-0","turn":{"id":format!("turn-{turn}"),"status":"completed"}}})).await;
        peer.acknowledged(cursor).await;
        worker_barrier(&worker).await;
        let count=state.bridge.run(move |store| async move { Ok(store.read()?.query_row("SELECT COUNT(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:%'",[],|r|r.get::<_,i64>(0))?) }).await.unwrap();
        assert!(count <= 4, "turn {turn}: retained {count} worker rows");
    }
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_releases_account_while_store_writer_is_blocked() {
    let state = State::new().await;
    let peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let (entered, observed) = oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let entered = Arc::new(std::sync::Mutex::new(Some(entered)));
    let blocked = Arc::new(std::sync::Mutex::new(blocked));
    let agent = state.agent.clone();
    let store = state.bridge.store.clone();
    store
        .transaction(move |tx| {
            tx.create_scalar_function(
                "account_worker_claim_gate",
                0,
                rusqlite::functions::FunctionFlags::SQLITE_UTF8,
                move |_| {
                    if let Some(entered) = entered
                        .lock()
                        .map_err(|_| rusqlite::Error::InvalidQuery)?
                        .take()
                    {
                        entered
                            .send(())
                            .map_err(|_| rusqlite::Error::InvalidQuery)?;
                    }
                    blocked
                        .lock()
                        .map_err(|_| rusqlite::Error::InvalidQuery)?
                        .recv()
                        .map_err(|_| rusqlite::Error::InvalidQuery)?;
                    Ok(0_i64)
                },
            )?;
            tx.execute_batch(&format!(
                "CREATE TEMP TRIGGER account_worker_claim_gate BEFORE UPDATE ON runtime_events \
                 WHEN OLD.agent='{agent}' AND OLD.status='pending' AND NEW.status='reserved' \
                 BEGIN SELECT account_worker_claim_gate(); END;"
            ))?;
            Ok(())
        })
        .await
        .expect("claim gate is installed on the store writer");
    state.wake(&worker).await;
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        observed.await.expect("claim gate reports entry")
    })
    .await
    .expect("claim transaction reaches its admitted SQL update");
    assert_eq!(
        state.bridge.operation_slot.available_permits(),
        0,
        "the identified claim job holds the account slot while its writer is blocked"
    );
    worker.cancel();
    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        state.bridge.faults.shutdown_wait.notified(),
    )
    .await
    .expect("setup barrier deadline");
    tokio::time::pause();
    tokio::time::advance(SHUTDOWN_GRACE * 2).await;
    assert!(matches!(
        worker.completion().await,
        Err(WorkerError::ShutdownTimeout)
    ));
    tokio::time::resume();
    assert!(!state.bridge.accounts.lock().unwrap().contains("default"));
    assert_eq!(
        state.bridge.operation_slot.available_permits(),
        0,
        "started job retains permit"
    );
    let second_operation_calls = Arc::new(AtomicUsize::new(0));
    let calls_in_operation = Arc::clone(&second_operation_calls);
    let mut second_operation = Box::pin(state.bridge.run(move |_store| async move {
        calls_in_operation.fetch_add(1, std::sync::atomic::Ordering::Release);
        Ok::<(), WorkerError>(())
    }));
    let second_operation_poll = std::future::poll_fn(|context| {
        std::task::Poll::Ready(second_operation.as_mut().poll(context))
    })
    .await;
    assert!(
        second_operation_poll.is_pending(),
        "a later bridge operation waits for the admitted writer callback"
    );
    assert_eq!(
        second_operation_calls.load(std::sync::atomic::Ordering::Acquire),
        0
    );
    release.send(()).unwrap();
    second_operation
        .await
        .expect("second operation follows the admitted claim");
    assert_eq!(
        second_operation_calls.load(std::sync::atomic::Ordering::Acquire),
        1
    );
    state
        .bridge
        .store
        .transaction(|tx| {
            tx.execute_batch("DROP TRIGGER account_worker_claim_gate")?;
            Ok(())
        })
        .await
        .expect("claim gate is removed");
    let peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    assert_eq!(state.event_status("input").await, "pending");
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn stream_base_and_metadata_rollback_together() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let delta=peer.frame(json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-0","turnId":"turn","itemId":"atomic","delta":"x".repeat(store::STREAM_FLUSH_BYTES)}})).await;
    peer.acknowledged(delta).await;
    state.bridge.run(move |store| async move { store.transaction_with(move |tx| {
        tx.execute_batch("CREATE TRIGGER fail_stream_metadata BEFORE UPDATE ON runtime_operation_receipts WHEN NEW.id='account-worker:streams:default:metadata' BEGIN SELECT RAISE(ABORT,'crash after base write before metadata'); END;")?;
        Ok(())
    }).await }).await.unwrap();
    let frame = json!({"method":"item/started","params":{"threadId":"fake-thread-0","turnId":"turn","item":{"id":"atomic","type":"agentMessage","text":"replacement"}}});
    peer.frame(frame.clone()).await;
    assert!(worker.completion().await.is_err());
    state.bridge.run(move |store| async move { store.transaction_with(move |tx| {
        // The delta batch was flushed before this transaction. A failed item/started
        // write must leave that previously committed base intact; only the visible
        // snapshot update and stream metadata mutation roll back together.
        assert_eq!(tx.query_row("SELECT COUNT(*) FROM runtime_operation_receipts WHERE signature='stream-base'",[],|row|row.get::<_,i64>(0))?,1);
        let body:String=tx.query_row("SELECT body FROM runtime_item_fulltext WHERE id LIKE '%:atomic'",[],|row|row.get(0))?;
        assert_eq!(body,"x".repeat(store::STREAM_FLUSH_BYTES));
        assert_eq!(store::receipt(tx,"cursor:default:1","ack")?,Some(json!(delta)));
        tx.execute_batch("DROP TRIGGER fail_stream_metadata")?;
        Ok(())
    }).await }).await.unwrap();
    let mut peer = Peer::new();
    peer.cursor = delta;
    let mut worker = peer.worker(state.bridge.clone()).await;
    let cursor = peer.frame(frame).await;
    peer.acknowledged(cursor).await;
    let cursor=peer.frame(json!({"method":"item/completed","params":{"threadId":"fake-thread-0","turnId":"turn","item":{"id":"atomic","type":"agentMessage","text":""}}})).await;
    peer.acknowledged(cursor).await;
    worker.shutdown().await.unwrap();
}

async fn transfer_at_admission(call: usize, interrupt: bool) {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    worker_barrier(&worker).await;
    state.bridge.faults.arm(None);
    if interrupt {
        let start = state.start(&mut peer, &worker).await;
        peer.reply(&start, json!({"turn":{"id":"turn"}})).await;
        worker_barrier(&worker).await;
        state.bridge.faults.arm(None);
    }
    *state.bridge.faults.pause_at.lock().unwrap() = Some(call);
    if interrupt {
        worker
            .send(AccountCommand::Stop {
                agent_id: state.agent.clone(),
            })
            .await
            .unwrap();
    } else {
        state.wake(&worker).await;
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        state.bridge.faults.paused.notified(),
    )
    .await
    .expect("submission reaches the transfer barrier");
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let mut row =
                        AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                    row.extra.insert("accountKey".into(), json!("transferred"));
                    AgentRepository::put(tx, &row)?;
                    Ok(())
                })
                .await
        })
        .await
        .unwrap();
    state.bridge.faults.release.notify_one();
    worker_barrier(&worker).await;
    assert!(
        peer.writes.try_recv().is_err(),
        "transferred account admitted a provider write"
    );
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn transfer_after_claim_before_resume_submission_is_refused() {
    transfer_at_admission(1, false).await;
}
#[tokio::test]
async fn transfer_before_claim_transaction_leaves_input_pending() {
    transfer_at_admission(0, false).await;
}
#[tokio::test]
async fn transfer_after_stop_before_interrupt_submission_is_refused() {
    transfer_at_admission(1, true).await;
}

async fn scenario_new_thread_freezes_settings_and_accepts_context() {
    let state = State::new().await;
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store.transaction(move |tx| {
                let mut record = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                record.extra.remove("threadId");
                record.extra.remove("nativeToolCatalog");
                record.extra.insert(
                    "pendingSettings".into(),
                    json!({"model":"pending-model","effort":"high","nativeEffort":"high","fastMode":true}),
                );
                AgentRepository::put(tx, &record)
            }).await?;
            Ok(())
        })
        .await
        .unwrap();
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    let start = peer.write("thread/start").await;
    assert_eq!(start["params"]["model"], "pending-model");
    assert_eq!(start["params"]["serviceTier"], "priority");
    let record = state.record().await;
    assert!(record.get("pendingSettings").is_none());
    assert!(record["prepareAttempt"].is_string());
    assert_eq!(record["startAttempt"]["settingsFixed"], true);
    peer.reply(
        &start,
        json!({"thread":{"id":"fake-thread-0"},"model":"pending-model"}),
    )
    .await;
    let turn = peer.write("turn/start").await;
    assert_eq!(turn["params"]["model"], "pending-model");
    assert_eq!(turn["params"]["effort"], "high");
    let before = state
        .bridge
        .run(move |store| async move {
            Ok(store.read()?.query_row(
                "SELECT count(*) FROM runtime_context_manifests",
                [],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(before, 0, "compilation is not delivery");
    peer.accepted(&turn).await;
    let after = state
        .bridge
        .run(move |store| async move {
            Ok(store.read()?.query_row(
                "SELECT count(*) FROM runtime_context_manifests",
                [],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(
        after, 1,
        "only an authenticated acceptance promotes context"
    );
    assert_eq!(state.event_status("input").await, "delivered");
    worker.shutdown().await.unwrap();
}
#[tokio::test]
async fn new_thread_freezes_settings_and_accepts_context() {
    scenario_new_thread_freezes_settings_and_accepts_context().await;
}

async fn scenario_child_result_materializes_before_submission() {
    let state = State::new().await;
    state.bridge.run(move |store| async move {store.transaction(move |tx|{
        tx.execute("UPDATE runtime_events SET kind='child_result',text=?1 WHERE id='annotation:input'",[json!({"agent_id":"child","result":"artifact-body".repeat(2400)}).to_string()])?;Ok(())
    }).await?;Ok(())}).await.unwrap();
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let turn = state.start(&mut peer, &worker).await;
    let text = turn["params"]["input"][0]["text"].as_str().unwrap();
    assert!(text.contains("resultFile"));
    assert!(!text.contains(&"artifact-body".repeat(2400)));
    peer.accepted(&turn).await;
    assert_eq!(state.event_status("input").await, "delivered");
    worker.shutdown().await.unwrap();
}
#[tokio::test]
async fn child_result_materializes_before_submission() {
    scenario_child_result_materializes_before_submission().await;
}

async fn scenario_workspace_refusal_retains_queue() {
    let state = State::new().await;
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut record =
                        AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                    record.extra.insert("executionMode".into(), json!("vm"));
                    AgentRepository::put(tx, &record)
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    worker_barrier(&worker).await;
    let record = state.record().await;
    assert_ne!(record["status"], "failed");
    assert!(record["error"].is_null());
    assert_eq!(state.event_status("input").await, "reserved");
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}
#[tokio::test]
async fn workspace_refusal_retains_queue() {
    scenario_workspace_refusal_retains_queue().await;
}

struct TurnStartGuest {
    calls: std::sync::Mutex<Vec<String>>,
    project_id: std::sync::Mutex<Option<String>>,
}

#[derive(Debug, Default)]
struct VmTurnGuestState {
    calls: Vec<String>,
    writes: Vec<Value>,
    turn_start_handles: Vec<String>,
    acknowledged_by_handle: Vec<(String, u64)>,
    events: std::collections::VecDeque<Value>,
    sequence: u64,
    acknowledged: u64,
    starts: usize,
    uncertain_start_once: bool,
    unknown_write_once: bool,
    codex_refresh_on_start: bool,
    hold_primary_after_turn_start: bool,
    hold_thread_resume_reply: bool,
    held_thread_resume: Option<(String, Value)>,
    exit_primary_on_next: bool,
    hold_primary_poll: bool,
    primary_poll_ready: bool,
    stops: Vec<Value>,
    credential_documents: Vec<Value>,
}

#[derive(Default)]
struct VmTurnGuest {
    state: std::sync::Mutex<VmTurnGuestState>,
    changed: tokio::sync::Notify,
}

impl VmTurnGuest {
    fn thread_id(handle: &Value) -> &'static str {
        if handle
            .as_str()
            .is_some_and(|handle| handle.ends_with("-sibling"))
        {
            "fake-thread-sibling"
        } else {
            "fake-thread-0"
        }
    }
    fn enqueue_frame(state: &mut VmTurnGuestState, owner: &str, payload: Value) {
        state.sequence += 1;
        state.events.push_back(json!({
            "_test_owner": owner,
            "sequence": state.sequence,
            "generation": 31,
            "kind": "stdout",
            "payload": payload.to_string(),
        }));
    }

    fn release_thread_resume(&self) {
        let mut state = self.state.lock().unwrap();
        let (handle, id) = state
            .held_thread_resume
            .take()
            .expect("thread/resume reply is held");
        state.hold_thread_resume_reply = false;
        Self::enqueue_frame(
            &mut state,
            &handle,
            json!({"id":id,"result":{"thread":{"id":"fake-thread-0"}}}),
        );
        self.changed.notify_one();
    }
}

impl crate::linux_vm_guest::GuestRpc for VmTurnGuest {
    fn stream(
        &self,
        method: &str,
        params: &Value,
        _request_id: Option<&str>,
        _timeout: std::time::Duration,
        _on_frame: &mut dyn FnMut(
            Value,
        )
            -> std::result::Result<(), crate::linux_vm_guest::GuestError>,
    ) -> std::result::Result<Value, crate::linux_vm_guest::GuestError> {
        let mut state = self.state.lock().unwrap();
        state.calls.push(method.to_owned());
        match method {
            "project.ensure" => Ok(json!({"projectId":params["projectId"],"stateId":"ready"})),
            "line.bind" => Ok(json!({
                "cwd":"/home/studio/project","projectId":params["projectId"],
                "line":"main","owner":"guest-owner","home":"/home/studio",
                "stateId":"ready"
            })),
            "health" => Ok(json!({"ok":true})),
            "agent.context" => Ok(json!({"projectId":params["projectId"],"line":"main"})),
            "provider.start" => {
                state.starts += 1;
                self.changed.notify_one();
                state.events.clear();
                state.sequence = 0;
                state.acknowledged = 0;
                if state.codex_refresh_on_start {
                    state.codex_refresh_on_start = false;
                    Self::enqueue_frame(
                        &mut state,
                        params["handle"].as_str().unwrap_or_default(),
                        json!({
                            "id":"vm-auth-refresh",
                            "method":"account/chatgptAuthTokens/refresh",
                            "params":{"previousAccountId":"fixture-account"}
                        }),
                    );
                }
                if state.uncertain_start_once {
                    state.uncertain_start_once = false;
                    return Err(crate::linux_vm_guest::GuestError::Transport {
                        message: "simulated lost provider start response".into(),
                        uncertain: true,
                    });
                }
                Ok(json!({
                    "handle":params["handle"],"generation":31,"sequence":0,
                    "acknowledged":0,"pid":42,"resumed":false,
                    "initResult":{"capabilities":{}},"state":"running"
                }))
            }
            "credentials.put" => {
                state.credential_documents.push(params["files"][0].clone());
                self.changed.notify_one();
                Ok(json!({"files":[{"path":params["files"][0]["path"],"bytes":64}]}))
            }
            "provider.rpc" => match params["action"].as_str().unwrap_or("") {
                "next" => {
                    let sibling_handle = params["handle"]
                        .as_str()
                        .is_some_and(|handle| handle.ends_with("-sibling"));
                    if state.hold_primary_poll && !sibling_handle {
                        state.primary_poll_ready = true;
                        self.changed.notify_one();
                        return Ok(json!({"event":null}));
                    }
                    if state.exit_primary_on_next && !sibling_handle {
                        state.exit_primary_on_next = false;
                        self.changed.notify_one();
                        return Err(crate::linux_vm_guest::GuestError::Transport {
                            message: "simulated VM adapter exit".into(),
                            uncertain: true,
                        });
                    }
                    let owner_matches = state.events.front().is_some_and(|event| {
                        event["_test_owner"]
                            .as_str()
                            .is_some_and(|owner| owner.ends_with("-sibling") == sibling_handle)
                    });
                    if owner_matches {
                        let mut event = state.events.pop_front().expect("front event exists");
                        event
                            .as_object_mut()
                            .expect("fake event object")
                            .remove("_test_owner");
                        Ok(json!({"event":event}))
                    } else {
                        Ok(json!({"event":null}))
                    }
                }
                "write" => {
                    let message = params["message"].clone();
                    state.writes.push(message.clone());
                    if state.unknown_write_once {
                        state.unknown_write_once = false;
                        return Err(crate::linux_vm_guest::GuestError::Rejected {
                            code: Some("internal".into()),
                            detail: None,
                            uncertain: false,
                        });
                    }
                    if message["method"] == "thread/resume" && state.hold_thread_resume_reply {
                        state.held_thread_resume = Some((
                            params["handle"].as_str().unwrap_or_default().to_owned(),
                            message["id"].clone(),
                        ));
                        self.changed.notify_one();
                    } else if message["method"] == "thread/start"
                        || message["method"] == "thread/resume"
                    {
                        Self::enqueue_frame(
                            &mut state,
                            params["handle"].as_str().unwrap_or_default(),
                            json!({"id":message["id"],"result":{"thread":{"id":Self::thread_id(&params["handle"])}}}),
                        );
                    } else if message["method"] == "turn/start" {
                        let primary = params["handle"]
                            .as_str()
                            .is_some_and(|handle| !handle.ends_with("-sibling"));
                        state
                            .turn_start_handles
                            .push(params["handle"].as_str().unwrap_or_default().to_owned());
                        Self::enqueue_frame(
                            &mut state,
                            params["handle"].as_str().unwrap_or_default(),
                            json!({"id":message["id"],"result":{"turn":{"id":"vm-turn"}}}),
                        );
                        if primary && state.hold_primary_after_turn_start {
                            state.hold_primary_poll = true;
                        }
                        Self::enqueue_frame(
                            &mut state,
                            params["handle"].as_str().unwrap_or_default(),
                            json!({"method":"turn/completed","params":{"threadId":Self::thread_id(&params["handle"]),"turn":{"id":"vm-turn","status":"completed"}}}),
                        );
                    } else if message["method"] == "thread/read" {
                        Self::enqueue_frame(
                            &mut state,
                            params["handle"].as_str().unwrap_or_default(),
                            json!({"id":message["id"],"result":{"thread":{"id":"fake-thread-0","turns":[]}}}),
                        );
                    }
                    self.changed.notify_one();
                    Ok(
                        json!({"accepted":true,"remoteId":message.get("id").cloned().unwrap_or(Value::Null)}),
                    )
                }
                "ack" => {
                    state.acknowledged = params["sequence"].as_u64().unwrap_or(0);
                    let acknowledged = state.acknowledged;
                    state.acknowledged_by_handle.push((
                        params["handle"].as_str().unwrap_or_default().to_owned(),
                        acknowledged,
                    ));
                    self.changed.notify_one();
                    Ok(json!({"acknowledged":state.acknowledged}))
                }
                "operationStatus" => Ok(json!({"accepted":false})),
                "info" => Ok(json!({
                    "handle":params["handle"],"generation":31,"sequence":state.sequence,
                    "acknowledged":state.acknowledged,"pid":42,"resumed":true,
                    "initResult":{"capabilities":{}},"state":"running"
                })),
                _ => Err(crate::linux_vm_guest::GuestError::InvalidRequest),
            },
            "provider.stop" => {
                state.stops.push(params.clone());
                Ok(json!({"handle":params["handle"],"state":"exited"}))
            }
            _ => Err(crate::linux_vm_guest::GuestError::InvalidRequest),
        }
    }
}

#[derive(Default)]
struct VmTurnResolver {
    reconnect_generation: Option<Arc<std::sync::atomic::AtomicU64>>,
}
impl preparation::VmProviderLaunchResolver for VmTurnResolver {
    fn resolve(
        &self,
        request: preparation::VmProviderLaunchRequest,
    ) -> preparation::VmProviderLaunchFuture<'_> {
        Box::pin(async move {
            Ok(preparation::VmProviderLaunch {
                provider: "codex".into(),
                profile: format!("profile:{}", request.account_key),
                launch: ProviderLaunchSpec::new(
                    vec!["codex".into(), "app-server".into()],
                    BTreeMap::from([("CODEX_HOME".into(), request.guest_home + "/.codex")]),
                    PathBuf::from(request.cwd),
                ),
                bootstrap: None,
            })
        })
    }

    fn refresh_codex(
        &self,
        _account_key: String,
        previous_account_id: String,
        _actor_id: String,
        _actor_epoch: i64,
        _generation: u64,
    ) -> preparation::VmProviderAuthFuture<'_> {
        let reconnect_generation = self.reconnect_generation.clone();
        Box::pin(async move {
            if let Some(generation) = reconnect_generation {
                generation.store(2, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(json!({
                "accessToken":"fake-fresh-access-token",
                "chatgptAccountId":previous_account_id
            }))
        })
    }
}

struct FakeVmAccountSession {
    generation: Arc<std::sync::atomic::AtomicU64>,
    checks: Arc<std::sync::atomic::AtomicUsize>,
    checked: Arc<tokio::sync::Notify>,
    reconnect_on_second_check: bool,
}
impl crate::account_session_runtime::AccountSessionPort for FakeVmAccountSession {
    fn current_scope(&self, account: &str) -> Option<crate::recovery_history::ConnectionScope> {
        let check = self
            .checks
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.checked.notify_one();
        if self.reconnect_on_second_check && check >= 1 {
            self.generation
                .store(2, std::sync::atomic::Ordering::SeqCst);
        }
        (account == "default").then(|| crate::recovery_history::ConnectionScope {
            account: account.to_owned(),
            connection: "vm-test-connection".into(),
            generation: self.generation.load(std::sync::atomic::Ordering::SeqCst),
        })
    }
}

struct VmClaudeRefreshResolver {
    store: Arc<RuntimeStore>,
    guest: Arc<VmTurnGuest>,
    refreshes: std::sync::atomic::AtomicUsize,
    refreshed: tokio::sync::Notify,
    attempted: tokio::sync::Notify,
    fail_next_refresh: std::sync::atomic::AtomicBool,
}

impl VmClaudeRefreshResolver {
    fn credential_path(account_key: &str) -> String {
        use sha2::Digest as _;
        let digest = data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(account_key.as_bytes()));
        format!(
            ".claude/studio-accounts/{}/.credentials.json",
            &digest[..32]
        )
    }

    async fn copy_version(
        &self,
        request: &preparation::VmProviderLaunchRequest,
        version: usize,
    ) -> Result<(), WorkerError> {
        let document =
            format!("{{\"claudeAiOauth\":{{\"accessToken\":\"fake-claude-{version}\"}}}}");
        crate::linux_vm_provider::copy_guest_credentials(
            Arc::clone(&self.store),
            self.guest.clone(),
            crate::linux_vm_provider::GuestCredentialTarget {
                agent_id: &request.agent_id,
                actor_epoch: request.epoch,
                provider: "claude",
                account_key: &request.account_key,
                relative_path: &Self::credential_path(&request.account_key),
            },
            document.as_bytes(),
        )
        .await
        .map_err(|_| WorkerError::PreparationPending("test/vm-credentials".into()))?;
        Ok(())
    }
}

impl preparation::VmProviderLaunchResolver for VmClaudeRefreshResolver {
    fn resolve(
        &self,
        request: preparation::VmProviderLaunchRequest,
    ) -> preparation::VmProviderLaunchFuture<'_> {
        Box::pin(async move {
            self.copy_version(&request, 0).await?;
            let profile = Self::credential_path(&request.account_key)
                .trim_end_matches("/.credentials.json")
                .to_owned();
            Ok(preparation::VmProviderLaunch {
                provider: "claude".into(),
                profile: profile.clone(),
                launch: ProviderLaunchSpec::new(
                    vec!["claude".into()],
                    BTreeMap::from([
                        (
                            "CLAUDE_CONFIG_DIR".into(),
                            format!("{}/{profile}", request.guest_home),
                        ),
                        ("STUDIO_CLAUDE_BIN".into(), "claude".into()),
                    ]),
                    PathBuf::from(request.cwd),
                ),
                bootstrap: None,
            })
        })
    }

    fn refresh_claude_credentials(
        &self,
        actors: Vec<preparation::VmProviderLaunchRequest>,
    ) -> preparation::VmProviderRefreshFuture<'_> {
        Box::pin(async move {
            self.attempted.notify_one();
            if self
                .fail_next_refresh
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Err(WorkerError::PreparationPending(
                    "test/transient-credential-refresh".into(),
                ));
            }
            let version = self
                .refreshes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            for actor in &actors {
                self.copy_version(actor, version).await?;
            }
            self.refreshed.notify_one();
            Ok(())
        })
    }
}

#[tokio::test]
async fn vm_actor_turn_uses_guest_session_and_settles_complete_turn() {
    let state = State::new().await;
    let agent_id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut record = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                        .expect("fixture agent");
                    record.extra.insert("executionMode".into(), json!("vm"));
                    record.extra.insert("layrReady".into(), json!(true));
                    record
                        .extra
                        .insert("cwd".into(), json!("/home/studio/project"));
                    record.extra.insert("workspaceMode".into(), json!("layr"));
                    record.extra.insert("threadId".into(), Value::Null);
                    AgentRepository::put(tx, &record)
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();

    let guest = Arc::new(VmTurnGuest::default());
    let mut resources = test_resources().unwrap();
    resources.guest_rpc = Some(guest.clone());
    resources.vm_provider_launch = Some(Arc::new(VmTurnResolver::default()));
    let peer = Peer::new();
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    let mut ready = peer.ready.clone();
    while !*ready.borrow_and_update() {
        ready.changed().await.unwrap();
    }
    state.wake(&worker).await;

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let changed = guest.changed.notified();
            let (has_turn, completed) = {
                let state = guest.state.lock().unwrap();
                (
                    state
                        .writes
                        .iter()
                        .any(|write| write["method"] == "turn/start"),
                    state.acknowledged >= 3,
                )
            };
            if has_turn && completed {
                break;
            }
            changed.await;
        }
    })
    .await
    .expect("guest turn starts and completion evidence is acknowledged");

    {
        let guest_state = guest.state.lock().unwrap();
        assert_eq!(guest_state.starts, 1);
        assert!(
            guest_state
                .writes
                .iter()
                .any(|write| write["method"] == "thread/start")
        );
        assert!(
            guest_state
                .writes
                .iter()
                .any(|write| write["method"] == "turn/start")
        );
        assert_eq!(guest_state.acknowledged, 3);
    }
    assert_eq!(state.event_status("input").await, "delivered");
    worker.shutdown().await.unwrap();
    let guest_state = guest.state.lock().unwrap();
    assert!(
        guest_state.stops.is_empty(),
        "worker cancellation detaches the transport"
    );
}

#[tokio::test]
async fn vm_session_identity_refusal_holds_one_actor_and_keeps_sibling_usable() {
    let state = State::new().await;
    let primary_id = state.agent.clone();
    let sibling_id = format!("{}-sibling", state.agent);
    let sibling = sibling_id.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut primary =
                        AgentRepository::get(tx, &AgentId::from(primary_id.as_str()))?
                            .expect("fixture agent");
                    primary.extra.insert("executionMode".into(), json!("vm"));
                    primary.extra.insert("layrReady".into(), json!(true));
                    primary
                        .extra
                        .insert("cwd".into(), json!("/home/studio/project"));
                    primary.extra.insert("workspaceMode".into(), json!("layr"));
                    primary.extra.insert("threadId".into(), Value::Null);
                    AgentRepository::put(tx, &primary)?;

                    let mut sibling_record = primary.clone();
                    sibling_record.id = AgentId::from(sibling.as_str());
                    sibling_record.root_id = sibling_record.id.clone();
                    sibling_record.status = studio_runtime_store::AgentStatus::Queued;
                    sibling_record.extra.insert("threadId".into(), Value::Null);
                    sibling_record.extra.insert("autoWake".into(), json!(true));
                    sibling_record.extra.insert("inFlight".into(), json!(false));
                    sibling_record.extra.insert("error".into(), Value::Null);
                    sibling_record.extra.remove("startAttempt");
                    sibling_record.extra.remove("preparationWake");
                    AgentRepository::put(tx, &sibling_record)
                })
                .await?;
            Ok(())
        })
        .await
        .expect("create a second durable VM actor");

    let guest = Arc::new(VmTurnGuest::default());
    let mut resources = test_resources().unwrap();
    resources.guest_rpc = Some(guest.clone());
    resources.vm_provider_launch = Some(Arc::new(VmTurnResolver::default()));
    let peer = Peer::new();
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    let mut ready = peer.ready.clone();
    while !*ready.borrow_and_update() {
        ready.changed().await.expect("account worker ready");
    }
    state.wake(&worker).await;

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let changed = guest.changed.notified();
            let complete = {
                let guest_state = guest.state.lock().unwrap();
                guest_state.starts == 1 && guest_state.acknowledged >= 3
            };
            if complete {
                break;
            }
            changed.await;
        }
    })
    .await
    .expect("first actor attaches and finishes one turn");

    let primary = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut record = AgentRepository::get(tx, &AgentId::from(primary.as_str()))?
                        .expect("primary actor");
                    record
                        .extra
                        .insert("cwd".into(), json!("/home/studio/changed-profile"));
                    AgentRepository::put(tx, &record)
                })
                .await?;
            Ok(())
        })
        .await
        .expect("primary actor profile changes while its guest is retained");
    state.enqueue_for(&state.agent, "stale-profile-turn").await;
    state.wake(&worker).await;
    worker_barrier(&worker).await;
    let held = state.record().await;
    assert_eq!(
        held["error"], "vm-agent/session-identity-changed",
        "the stale actor turn is visibly held by the preparation refusal"
    );
    assert_eq!(guest.state.lock().unwrap().starts, 1);

    state.enqueue_for(&sibling_id, "sibling-turn").await;
    worker
        .send(AccountCommand::Wake {
            agent_id: sibling_id.clone(),
            epoch: state.epoch,
        })
        .await
        .expect("wake sibling through the same account worker");
    let sibling_progress = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let changed = guest.changed.notified();
            let sibling_complete = {
                let guest_state = guest.state.lock().unwrap();
                guest_state.starts == 2
                    && guest_state.acknowledged >= 3
                    && guest_state
                        .writes
                        .iter()
                        .any(|write| write["method"] == "turn/start")
            };
            if sibling_complete {
                break;
            }
            changed.await;
        }
    })
    .await;
    if sibling_progress.is_err() {
        worker_barrier(&worker).await;
        let sibling = sibling_id.clone();
        let sibling_record = state
            .bridge
            .run(move |store| async move {
                let connection = store.read()?;
                Ok(
                    AgentRepository::get(&connection, &AgentId::from(sibling.as_str()))?
                        .map(|record| record.value()),
                )
            })
            .await
            .expect("read sibling actor after its wake");
        let guest_state = guest.state.lock().unwrap();
        let methods = guest_state
            .writes
            .iter()
            .filter_map(|write| write["method"].as_str())
            .collect::<Vec<_>>();
        panic!(
            "sibling VM actor did not progress; starts={}; methods={methods:?}; sibling status={}, error={}, attempt={}",
            guest_state.starts,
            sibling_record
                .as_ref()
                .map_or(Value::Null, |record| record["status"].clone()),
            sibling_record
                .as_ref()
                .map_or(Value::Null, |record| record["error"].clone()),
            sibling_record
                .as_ref()
                .map_or(Value::Null, |record| record["startAttempt"].clone())
        );
    }
    worker
        .shutdown()
        .await
        .expect("account worker remains healthy");
}

#[tokio::test]
async fn vm_adapter_exit_recovers_only_its_actor_and_keeps_sibling_usable() {
    let state = State::new().await;
    let primary_id = state.agent.clone();
    let primary_handle = format!("linux-worker:{primary_id}");
    let sibling_id = format!("{}-sibling", state.agent);
    let sibling = sibling_id.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut primary =
                        AgentRepository::get(tx, &AgentId::from(primary_id.as_str()))?
                            .expect("fixture agent");
                    primary.extra.insert("executionMode".into(), json!("vm"));
                    primary.extra.insert("layrReady".into(), json!(true));
                    primary
                        .extra
                        .insert("cwd".into(), json!("/home/studio/project"));
                    primary.extra.insert("workspaceMode".into(), json!("layr"));
                    primary.extra.insert("threadId".into(), Value::Null);
                    AgentRepository::put(tx, &primary)?;

                    let mut sibling_record = primary.clone();
                    sibling_record.id = AgentId::from(sibling.as_str());
                    sibling_record.root_id = sibling_record.id.clone();
                    sibling_record.status = studio_runtime_store::AgentStatus::Queued;
                    sibling_record.extra.insert("threadId".into(), Value::Null);
                    sibling_record.extra.insert("autoWake".into(), json!(true));
                    sibling_record.extra.insert("inFlight".into(), json!(false));
                    sibling_record.extra.insert("error".into(), Value::Null);
                    sibling_record.extra.remove("startAttempt");
                    sibling_record.extra.remove("preparationWake");
                    AgentRepository::put(tx, &sibling_record)
                })
                .await?;
            Ok(())
        })
        .await
        .expect("create a second durable VM actor");

    let guest = Arc::new(VmTurnGuest::default());
    {
        let mut guest_state = guest.state.lock().unwrap();
        guest_state.hold_primary_after_turn_start = true;
    }
    let mut resources = test_resources().unwrap();
    resources.guest_rpc = Some(guest.clone());
    resources.vm_provider_launch = Some(Arc::new(VmTurnResolver::default()));
    let peer = Peer::new();
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    let mut ready = peer.ready.clone();
    while !*ready.borrow_and_update() {
        ready.changed().await.expect("account worker ready");
    }
    state.wake(&worker).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let changed = guest.changed.notified();
            let ready = {
                let guest_state = guest.state.lock().unwrap();
                guest_state.primary_poll_ready
                    && guest_state
                        .turn_start_handles
                        .iter()
                        .any(|handle| handle == &primary_handle)
            };
            if ready {
                break;
            }
            changed.await;
        }
    })
    .await
    .expect("primary VM actor has a submitted turn start while polling is held");
    worker_barrier(&worker).await;
    let primary = state.record().await;
    assert_eq!(primary["startAttempt"]["submitted"], true);
    {
        let mut guest_state = guest.state.lock().unwrap();
        guest_state.hold_primary_poll = false;
        guest_state.exit_primary_on_next = true;
        guest.changed.notify_one();
    }
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let changed = guest.changed.notified();
            let exited = {
                let guest_state = guest.state.lock().unwrap();
                !guest_state.exit_primary_on_next
            };
            if exited {
                break;
            }
            changed.await;
        }
    })
    .await
    .expect("primary VM adapter reaches its controlled exit");
    state.enqueue_for(&sibling_id, "sibling-after-exit").await;
    worker
        .send(AccountCommand::Wake {
            agent_id: sibling_id.clone(),
            epoch: state.epoch,
        })
        .await
        .expect("wake sibling through the same account worker");

    let sibling_handle = format!("linux-worker:{sibling_id}");
    let sibling_progress = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let changed = guest.changed.notified();
            let sibling_completed = {
                let guest_state = guest.state.lock().unwrap();
                guest_state
                    .turn_start_handles
                    .iter()
                    .any(|handle| handle == &sibling_handle)
                    && guest_state
                        .acknowledged_by_handle
                        .iter()
                        .any(|(handle, sequence)| handle == &sibling_handle && *sequence >= 3)
            };
            if sibling_completed {
                break;
            }
            changed.await;
        }
    })
    .await;
    if sibling_progress.is_err() {
        let _barrier = worker_barrier(&worker).await;
        let primary = state.agent.clone();
        let sibling_for_read = sibling_id.clone();
        let records = state
            .bridge
            .run(move |store| async move {
                let connection = store.read()?;
                Ok((
                    AgentRepository::get(&connection, &AgentId::from(primary.as_str()))?
                        .map(|record| record.value()),
                    AgentRepository::get(&connection, &AgentId::from(sibling_for_read.as_str()))?
                        .map(|record| record.value()),
                ))
            })
            .await
            .expect("read actors after adapter exit");
        let guest_state = guest.state.lock().unwrap();
        let methods = guest_state
            .writes
            .iter()
            .filter_map(|write| Some((write["handle"].as_str()?, write["method"].as_str()?)))
            .collect::<Vec<_>>();
        panic!(
            "sibling did not complete; starts={}; methods={methods:?}; turn_handles={:?}; ack={:?}; primary.status={}, primary.attempt={}, sibling.status={}, sibling.attempt={}",
            guest_state.starts,
            guest_state
                .writes
                .iter()
                .filter_map(|write| write["method"].as_str())
                .collect::<Vec<_>>(),
            guest_state.acknowledged_by_handle,
            records
                .0
                .as_ref()
                .map_or(Value::Null, |record| record["status"].clone()),
            records
                .0
                .as_ref()
                .map_or(Value::Null, |record| record["startAttempt"].clone()),
            records
                .1
                .as_ref()
                .map_or(Value::Null, |record| record["status"].clone()),
            records
                .1
                .as_ref()
                .map_or(Value::Null, |record| record["startAttempt"].clone()),
        );
    }
    worker_barrier(&worker).await;

    let primary = state.agent.clone();
    let sibling_for_read = sibling_id.clone();
    let (primary_record, sibling_record) = state
        .bridge
        .run(move |store| async move {
            let connection = store.read()?;
            Ok((
                AgentRepository::get(&connection, &AgentId::from(primary.as_str()))?
                    .expect("primary record")
                    .value(),
                AgentRepository::get(&connection, &AgentId::from(sibling_for_read.as_str()))?
                    .expect("sibling record")
                    .value(),
            ))
        })
        .await
        .expect("read both actor outcomes");
    assert_eq!(primary_record["status"], "interrupted", "{primary_record}");
    assert_eq!(
        primary_record["disconnectRecovery"]["startAttempt"]["submitted"],
        true
    );
    assert_eq!(
        primary_record["disconnectRecovery"]["startAttempt"]["executionOutcome"],
        "unknown"
    );
    assert_eq!(state.event_status("input").await, "uncertain");
    assert_eq!(sibling_record["status"], "completed");
    worker
        .shutdown()
        .await
        .expect("account worker remains healthy");
}

#[cfg(unix)]
#[tokio::test]
async fn vm_claude_refresh_copies_rotation_to_active_guest_without_restart() {
    let state = State::new().await;
    let agent_id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut record = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                        .expect("fixture agent");
                    record.extra.insert("executionMode".into(), json!("vm"));
                    record.extra.insert("provider".into(), json!("claude"));
                    record.extra.insert("layrReady".into(), json!(true));
                    record
                        .extra
                        .insert("cwd".into(), json!("/home/studio/project"));
                    record.extra.insert("workspaceMode".into(), json!("layr"));
                    record.extra.insert("claudeOptions".into(), json!({}));
                    record.extra.insert("threadId".into(), Value::Null);
                    AgentRepository::put(tx, &record)
                })
                .await?;
            Ok(())
        })
        .await
        .expect("fixture becomes Claude VM actor");

    let guest = Arc::new(VmTurnGuest::default());
    let resolver = Arc::new(VmClaudeRefreshResolver {
        store: Arc::clone(&state.bridge.store),
        guest: guest.clone(),
        refreshes: std::sync::atomic::AtomicUsize::new(0),
        refreshed: tokio::sync::Notify::new(),
        attempted: tokio::sync::Notify::new(),
        fail_next_refresh: std::sync::atomic::AtomicBool::new(true),
    });
    let mut resources = claude_preparation::claude_resources(&state);
    resources.guest_rpc = Some(guest.clone());
    resources.vm_provider_launch = Some(resolver.clone());
    let mut peer = Peer::new();
    peer.claude = true;
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    let mut ready = peer.ready.clone();
    while !*ready.borrow_and_update() {
        ready.changed().await.expect("account worker ready");
    }
    state.wake(&worker).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let changed = guest.changed.notified();
            let started = {
                let state = guest.state.lock().unwrap();
                state.starts == 1
                    && state
                        .writes
                        .iter()
                        .any(|write| write["method"] == "turn/start")
                    && state.acknowledged >= 3
            };
            if started {
                break;
            }
            changed.await;
        }
    })
    .await
    .expect("Claude VM turn remains attached");

    let failed_sweep = resolver.attempted.notified();
    tokio::time::pause();
    tokio::time::advance(crate::claude_auth_runtime::VM_CREDENTIAL_REFRESH_INTERVAL).await;
    tokio::task::yield_now().await;
    tokio::time::resume();
    tokio::time::timeout(std::time::Duration::from_secs(10), failed_sweep)
        .await
        .expect("first account-level Claude refresh attempt fails transiently");
    assert_eq!(
        guest.state.lock().unwrap().starts,
        1,
        "a failed profile maintenance sweep leaves the guest attached"
    );

    let refreshed = resolver.refreshed.notified();
    tokio::time::pause();
    tokio::time::advance(crate::claude_auth_runtime::VM_CREDENTIAL_REFRESH_INTERVAL).await;
    tokio::task::yield_now().await;
    tokio::time::resume();
    tokio::time::timeout(std::time::Duration::from_secs(10), refreshed)
        .await
        .expect("later account-level Claude refresh sweep recovers");
    {
        let state_after = guest.state.lock().unwrap();
        assert_eq!(
            state_after.starts, 1,
            "credential sync does not restart the guest"
        );
        let copied = state_after
            .credential_documents
            .iter()
            .map(|file| {
                let bytes = base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    file["data"].as_str().unwrap(),
                )
                .expect("credential fixture is base64");
                serde_json::from_slice::<Value>(&bytes).expect("credential fixture is JSON")
            })
            .collect::<Vec<_>>();
        assert!(
            copied
                .iter()
                .any(|document| { document["claudeAiOauth"]["accessToken"] == "fake-claude-1" })
        );
    }
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn vm_journal_cannot_settle_a_sibling_actors_attempt() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
    let vm_actor = AgentRecord::from_value(json!({
        "id":"vm-actor","rootId":"vm-actor","epoch":4,"status":"running",
        "provider":"codex","accountKey":"default","executionMode":"vm",
        "threadId":"vm-thread","autoWake":true,"inFlight":true,"turnEpoch":4,
        "startAttempt":{"id":"vm-attempt","epoch":4,"submitted":true,
            "accountKey":"default","threadId":"vm-thread","turnId":"vm-turn",
            "connectionGeneration":31,"requestTicket":"vm-ticket",
            "executionOutcome":"unknown","events":[]}
    }))
    .unwrap();
    let sibling = AgentRecord::from_value(json!({
        "id":"sibling","rootId":"sibling","epoch":7,"status":"running",
        "provider":"codex","accountKey":"default","executionMode":"host",
        "threadId":"sibling-thread","autoWake":true,"inFlight":true,"turnEpoch":7,
        "startAttempt":{"id":"sibling-attempt","epoch":7,"submitted":true,
            "accountKey":"default","threadId":"sibling-thread","turnId":"sibling-turn",
            "connectionGeneration":31,"requestTicket":"sibling-ticket",
            "executionOutcome":"unknown","events":[]}
    }))
    .unwrap();
    let vm_sibling = AgentRecord::from_value(json!({
        "id":"vm-sibling","rootId":"vm-sibling","epoch":6,"status":"running",
        "provider":"codex","accountKey":"default","executionMode":"vm",
        "threadId":"vm-sibling-thread","autoWake":true,"inFlight":true,"turnEpoch":6,
        "startAttempt":{"id":"vm-sibling-attempt","epoch":6,"submitted":true,
            "accountKey":"default","threadId":"vm-sibling-thread","turnId":"vm-sibling-turn",
            "connectionGeneration":31,"requestTicket":"vm-sibling-ticket",
            "executionOutcome":"unknown","events":[]}
    }))
    .unwrap();
    let completed_vm = AgentRecord::from_value(json!({
        "id":"vm-completed","rootId":"vm-completed","epoch":8,"status":"completed",
        "provider":"codex","accountKey":"default","executionMode":"vm",
        "threadId":"completed-thread","autoWake":false,"inFlight":false,"turnEpoch":8
    }))
    .unwrap();
    store
        .transaction(move |tx| {
            AgentRepository::put(tx, &vm_actor)?;
            AgentRepository::put(tx, &sibling)?;
            AgentRepository::put(tx, &vm_sibling)?;
            AgentRepository::put(tx, &completed_vm)?;
            Ok(())
        })
        .await
        .unwrap();
    let frame = Frame::Notification(studio_runtime_providers::NotificationFrame {
        method: Method::TurnCompleted,
        params: Some(json!({
            "threadId":"sibling-thread",
            "turn":{"id":"sibling-turn","status":"completed"}
        })),
        fields: Default::default(),
    });
    let entry = JournalEntry {
        received_at_ms: 1,
        cursor: EventCursor(1),
        generation: Generation(31),
        payload: JournalPayload::Frame(frame),
    };
    store::journal_batch(
        Arc::clone(&store),
        "default",
        "default:vm:vm-actor",
        &NotificationStreamBuffer::default(),
        vec![entry],
        &Value::Null,
        Some(("vm-actor", 4)),
        None,
    )
    .await
    .unwrap()
    .expect("VM actor journal entry is durable");
    let saved = AgentRepository::get(&store.read().unwrap(), &AgentId::from("sibling"))
        .unwrap()
        .unwrap();
    assert_eq!(saved.status, studio_runtime_store::AgentStatus::Running);
    assert_eq!(saved.extra["startAttempt"]["executionOutcome"], "unknown");
    assert_eq!(saved.extra["inFlight"], true);

    let vm_entry = JournalEntry {
        received_at_ms: 2,
        cursor: EventCursor(1),
        generation: Generation(31),
        payload: JournalPayload::Frame(Frame::Notification(
            studio_runtime_providers::NotificationFrame {
                method: Method::TurnStarted,
                params: Some(json!({"threadId":"unmatched-thread","turnId":"unmatched-turn"})),
                fields: Default::default(),
            },
        )),
    };
    store::journal_batch(
        Arc::clone(&store),
        "default",
        "default:vm:vm-sibling",
        &NotificationStreamBuffer::default(),
        vec![vm_entry],
        &Value::Null,
        Some(("vm-sibling", 6)),
        None,
    )
    .await
    .unwrap()
    .expect("sibling VM actor journal entry is durable");
    store
        .transaction_with(move |tx| -> Result<(), WorkerError> {
            assert_eq!(store::checkpoint(tx, "default:vm:vm-actor", 31)?, 1);
            assert_eq!(store::checkpoint(tx, "default:vm:vm-sibling", 31)?, 1);
            Ok(())
        })
        .await
        .unwrap();

    assert!(
        store::disconnect_vm_actor(&store, "vm-actor", 4, "default", 31)
            .await
            .unwrap()
    );
    let (retired, surviving) = store
        .read()
        .map(|connection| {
            (
                AgentRepository::get(&connection, &AgentId::from("vm-actor"))
                    .unwrap()
                    .unwrap(),
                AgentRepository::get(&connection, &AgentId::from("vm-sibling"))
                    .unwrap()
                    .unwrap(),
            )
        })
        .unwrap();
    assert_eq!(
        retired.status,
        studio_runtime_store::AgentStatus::Interrupted
    );
    assert_eq!(retired.extra["inFlight"], false);
    assert!(retired.extra.contains_key("disconnectRecovery"));
    assert_eq!(surviving.status, studio_runtime_store::AgentStatus::Running);
    assert_eq!(surviving.extra["inFlight"], true);
    assert!(
        !store::disconnect_vm_actor(&store, "vm-completed", 8, "default", 31)
            .await
            .unwrap()
    );
    let connection = store.read().unwrap();
    assert_eq!(
        AgentRepository::get(&connection, &AgentId::from("vm-completed"))
            .unwrap()
            .unwrap()
            .status,
        studio_runtime_store::AgentStatus::Completed
    );
}

#[tokio::test]
async fn retained_vm_notification_is_epoch_fenced_and_committed_before_delivery() {
    let state = State::new().await;
    let agent_id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut record = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                        .expect("fixture agent");
                    record.extra.insert("executionMode".into(), json!("vm"));
                    record.extra.insert("layrReady".into(), json!(true));
                    record
                        .extra
                        .insert("cwd".into(), json!("/home/studio/project"));
                    record.extra.insert("workspaceMode".into(), json!("layr"));
                    record.extra.insert("threadId".into(), Value::Null);
                    AgentRepository::put(tx, &record)
                })
                .await?;
            Ok(())
        })
        .await
        .expect("fixture becomes VM actor");
    let guest = Arc::new(VmTurnGuest::default());
    let mut resources = test_resources().unwrap();
    resources.guest_rpc = Some(guest.clone());
    resources.vm_provider_launch = Some(Arc::new(VmTurnResolver::default()));
    let peer = Peer::new();
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    let mut ready = peer.ready.clone();
    while !*ready.borrow_and_update() {
        ready.changed().await.unwrap();
    }
    state.wake(&worker).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let changed = guest.changed.notified();
            let (started, acknowledged) = {
                let guest_state = guest.state.lock().unwrap();
                (guest_state.starts == 1, guest_state.acknowledged >= 3)
            };
            if started && acknowledged {
                break;
            }
            changed.await;
        }
    })
    .await
    .expect("fake VM actor is attached and its initial journal is acknowledged");
    let session = worker.retained_session();
    let frame = Frame::Notification(studio_runtime_providers::NotificationFrame {
        method: Method::TurnStarted,
        params: Some(json!({"threadId":"unmatched-thread","turnId":"unmatched-turn"})),
        fields: Default::default(),
    });
    session
        .deliver_vm_notification("default", &state.agent, state.epoch, 4, frame.clone())
        .await
        .expect("notification has a durable actor-scoped receipt");

    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    // Committed journal receipts are compacted after their checkpoint
                    // advances, so the actor-scoped durable checkpoint is the proof.
                    assert_eq!(store::checkpoint(tx, &format!("default:vm:{agent}"), 1)?, 4);
                    Ok(())
                })
                .await
        })
        .await
        .expect("durable notification state is readable");

    let stale = session
        .deliver_vm_notification("default", &state.agent, state.epoch + 1, 5, frame)
        .await;
    assert!(matches!(stale, Err(super::retained::PortError::Unknown)));
    worker.shutdown().await.expect("worker shuts down");
}

#[tokio::test]
async fn retained_vm_auth_callback_replies_through_the_account_session() {
    let state = State::new().await;
    let agent_id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut record = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                        .expect("fixture agent");
                    record.extra.insert("executionMode".into(), json!("vm"));
                    record.extra.insert("layrReady".into(), json!(true));
                    record
                        .extra
                        .insert("cwd".into(), json!("/home/studio/project"));
                    record.extra.insert("workspaceMode".into(), json!("layr"));
                    record.extra.insert("threadId".into(), Value::Null);
                    AgentRepository::put(tx, &record)
                })
                .await?;
            Ok(())
        })
        .await
        .expect("fixture becomes VM actor");

    let guest = Arc::new(VmTurnGuest::default());
    guest.state.lock().unwrap().codex_refresh_on_start = true;
    let mut resources = test_resources().unwrap();
    resources.guest_rpc = Some(guest.clone());
    let account_generation = Arc::new(std::sync::atomic::AtomicU64::new(1));
    resources.account_session = Some(Arc::new(FakeVmAccountSession {
        generation: Arc::clone(&account_generation),
        checks: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        checked: Arc::new(tokio::sync::Notify::new()),
        reconnect_on_second_check: false,
    }));
    resources.vm_provider_launch = Some(Arc::new(VmTurnResolver::default()));
    let peer = Peer::new();
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    let mut ready = peer.ready.clone();
    while !*ready.borrow_and_update() {
        ready.changed().await.unwrap();
    }
    state.wake(&worker).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let changed = guest.changed.notified();
            let response_written_and_acked = {
                let state = guest.state.lock().unwrap();
                let response = state
                    .writes
                    .iter()
                    .find(|write| write["id"] == "vm-auth-refresh");
                if let Some(response) = response.filter(|_| state.acknowledged >= 1) {
                    assert_eq!(response["result"]["accessToken"], "fake-fresh-access-token");
                    assert_eq!(response["result"]["chatgptAccountId"], "fixture-account");
                    assert!(response["result"].get("refreshToken").is_none());
                    true
                } else {
                    false
                }
            };
            if response_written_and_acked {
                break;
            }
            changed.await;
        }
    })
    .await
    .expect("retained account callback replies through the VM actor session");

    let _shutdown = worker.shutdown().await;
}

#[tokio::test]
async fn retained_vm_reply_uses_its_actor_cursor_when_host_cursor_is_zero() {
    let state = State::new().await;
    let agent_id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut record = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                        .expect("fixture agent");
                    record.extra.insert("executionMode".into(), json!("vm"));
                    record.extra.insert("layrReady".into(), json!(true));
                    record
                        .extra
                        .insert("cwd".into(), json!("/home/studio/project"));
                    record.extra.insert("workspaceMode".into(), json!("layr"));
                    record
                        .extra
                        .insert("threadId".into(), json!("fake-thread-0"));
                    AgentRepository::put(tx, &record)
                })
                .await?;
            Ok(())
        })
        .await
        .expect("fixture becomes VM actor with a retained thread");

    let guest = Arc::new(VmTurnGuest::default());
    guest.state.lock().unwrap().hold_thread_resume_reply = true;
    let mut resources = test_resources().unwrap();
    resources.guest_rpc = Some(guest.clone());
    resources.vm_provider_launch = Some(Arc::new(VmTurnResolver::default()));
    let peer = Peer::new();
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    let mut ready = peer.ready.clone();
    while !*ready.borrow_and_update() {
        ready.changed().await.expect("account worker ready");
    }
    state.wake(&worker).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let changed = guest.changed.notified();
            let held = guest.state.lock().unwrap().held_thread_resume.is_some();
            if held {
                break;
            }
            changed.await;
        }
    })
    .await
    .expect("VM thread/resume write is accepted and awaiting its first journal frame");

    let retained = worker.retained_session();
    let receipt = retained
        .request(super::retained::SessionRequest {
            scope: retained.scope().clone(),
            expected_scope: None,
            operation: "vm-retained-thread-read-cursor-one".into(),
            actor: Some(super::retained::RequestActor {
                agent: state.agent.clone(),
                epoch: state.epoch,
            }),
            transfer: None,
            deleted_actor_session: None,
            method: Method::ThreadRead,
            params: json!({"threadId":"fake-thread-0"}),
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(10),
        })
        .await
        .expect("retained VM read resolves from its own journal cursor");
    assert!(matches!(
        receipt.result,
        super::retained::RequestResult::Reply(_)
    ));
    let agent = state.agent.clone();
    let (host_cursor, actor_cursor) = state
        .bridge
        .run(move |store| async move {
            let scope = format!("default:vm:{agent}");
            store
                .transaction_with(move |tx| {
                    Ok((
                        store::checkpoint(tx, "default", 1)?,
                        store::checkpoint(tx, &scope, 1)?,
                    ))
                })
                .await
        })
        .await
        .expect("read committed host and actor checkpoint receipts");
    assert_eq!(host_cursor, 0, "host journal has no checkpoint");
    assert_eq!(
        actor_cursor, 1,
        "the retained reply committed at VM actor cursor one"
    );

    guest.release_thread_resume();
    worker.shutdown().await.expect("worker drains and closes");
}

#[tokio::test]
async fn retained_unknown_replay_precedes_changed_vm_session_fence() {
    let state = State::new().await;
    let agent_id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut record = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                        .expect("fixture agent");
                    record.extra.insert("executionMode".into(), json!("vm"));
                    record.extra.insert("layrReady".into(), json!(true));
                    record
                        .extra
                        .insert("cwd".into(), json!("/home/studio/project"));
                    record.extra.insert("workspaceMode".into(), json!("layr"));
                    record.extra.insert("threadId".into(), Value::Null);
                    AgentRepository::put(tx, &record)
                })
                .await?;
            Ok(())
        })
        .await
        .expect("fixture becomes VM actor");

    let guest = Arc::new(VmTurnGuest::default());
    let mut resources = test_resources().unwrap();
    resources.guest_rpc = Some(guest.clone());
    resources.vm_provider_launch = Some(Arc::new(VmTurnResolver::default()));
    let peer = Peer::new();
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    let mut ready = peer.ready.clone();
    while !*ready.borrow_and_update() {
        ready.changed().await.expect("account worker ready");
    }
    state.wake(&worker).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let changed = guest.changed.notified();
            let ready_for_request = {
                let guest_state = guest.state.lock().unwrap();
                guest_state.starts == 1
                    && guest_state
                        .writes
                        .iter()
                        .any(|write| write["method"] == "turn/start")
            };
            if ready_for_request {
                break;
            }
            changed.await;
        }
    })
    .await
    .expect("VM session is ready to accept a retained request");

    guest.state.lock().unwrap().unknown_write_once = true;
    let retained = worker.retained_session();
    let make_request =
        |scope: crate::recovery_history::ConnectionScope| super::retained::SessionRequest {
            scope,
            expected_scope: None,
            operation: "vm-unknown-profile-change".into(),
            actor: Some(super::retained::RequestActor {
                agent: state.agent.clone(),
                epoch: state.epoch,
            }),
            transfer: None,
            deleted_actor_session: None,
            method: Method::ThreadRead,
            params: json!({"threadId":"fake-thread-0"}),
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(10),
        };
    let writes_before_unknown = guest.state.lock().unwrap().writes.len();
    let first = retained
        .request(make_request(retained.scope().clone()))
        .await;
    assert!(
        matches!(
            &first,
            Ok(super::retained::SessionReceipt {
                result: super::retained::RequestResult::Unknown,
                ..
            })
        ) || matches!(&first, Err(super::retained::PortError::Unknown)),
        "first write must preserve an Unknown outcome: {first:?}"
    );
    assert_eq!(
        guest.state.lock().unwrap().writes.len(),
        writes_before_unknown + 1
    );

    let agent_id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut record = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                        .expect("fixture agent");
                    record
                        .extra
                        .insert("cwd".into(), json!("/home/studio/changed-profile"));
                    AgentRepository::put(tx, &record)
                })
                .await?;
            Ok(())
        })
        .await
        .expect("fixture profile changes while operation identity is frozen");
    let _first_shutdown = worker.shutdown().await;
    let replay_peer = Peer::new();
    let mut replay_resources = test_resources().unwrap();
    replay_resources.guest_rpc = Some(guest.clone());
    replay_resources.vm_provider_launch = Some(Arc::new(VmTurnResolver::default()));
    let mut replay_worker = replay_peer
        .open_worker_with_resources(state.bridge.clone(), replay_resources)
        .await;
    let mut replay_ready = replay_peer.ready.clone();
    while !*replay_ready.borrow_and_update() {
        replay_ready
            .changed()
            .await
            .expect("replacement account worker ready");
    }
    let replay_session = replay_worker.retained_session();
    let replay = replay_session
        .request(make_request(replay_session.scope().clone()))
        .await;
    assert!(
        matches!(
            replay,
            Ok(super::retained::SessionReceipt {
                result: super::retained::RequestResult::Unknown,
                replayed: true,
                ..
            })
        ),
        "same-operation replay preserves its recorded Unknown outcome: {replay:?}"
    );
    assert_eq!(
        guest.state.lock().unwrap().writes.len(),
        writes_before_unknown + 1,
        "profile change cannot cause a second dispatch or downgrade Unknown to NotSent"
    );
    replay_worker
        .shutdown()
        .await
        .expect("replay worker closes cleanly");
}

#[tokio::test]
async fn vm_auth_callback_is_not_sent_after_account_reconnect_at_final_fence() {
    let state = State::new().await;
    let agent_id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut record = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                        .expect("fixture agent");
                    record.extra.insert("executionMode".into(), json!("vm"));
                    record.extra.insert("layrReady".into(), json!(true));
                    record
                        .extra
                        .insert("cwd".into(), json!("/home/studio/project"));
                    record.extra.insert("workspaceMode".into(), json!("layr"));
                    record.extra.insert("threadId".into(), Value::Null);
                    AgentRepository::put(tx, &record)
                })
                .await?;
            Ok(())
        })
        .await
        .expect("fixture becomes VM actor");

    let guest = Arc::new(VmTurnGuest::default());
    guest.state.lock().unwrap().codex_refresh_on_start = true;
    let account_generation = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let account_checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let account_checked = Arc::new(tokio::sync::Notify::new());
    let mut resources = test_resources().unwrap();
    resources.guest_rpc = Some(guest.clone());
    resources.account_session = Some(Arc::new(FakeVmAccountSession {
        generation: account_generation,
        checks: account_checks.clone(),
        checked: account_checked.clone(),
        reconnect_on_second_check: true,
    }));
    resources.vm_provider_launch = Some(Arc::new(VmTurnResolver::default()));
    let peer = Peer::new();
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    let mut ready = peer.ready.clone();
    while !*ready.borrow_and_update() {
        ready.changed().await.expect("account worker ready");
    }
    state.wake(&worker).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let checked = account_checked.notified();
            if account_checks.load(std::sync::atomic::Ordering::SeqCst) >= 2 {
                break;
            }
            checked.await;
        }
    })
    .await
    .expect("stale callback reaches its account-generation fence");
    worker_barrier(&worker).await;
    assert_eq!(
        worker.faults.current_callback_phase(),
        "stale callback actor retired",
        "the worker barrier must follow stale callback retirement"
    );
    assert!(
        !guest
            .state
            .lock()
            .unwrap()
            .writes
            .iter()
            .any(|write| write["id"] == "vm-auth-refresh"),
        "a reconnect after callback resolution must prevent the token reply"
    );
    let shutdown = worker.shutdown().await;
    assert!(
        shutdown.is_ok(),
        "stale actor callback does not terminate its account worker; last phase: {}",
        worker.faults.current_shutdown_phase()
    );
}

#[tokio::test]
async fn vm_auth_callback_shutdown_waits_for_final_generation_fence() {
    let state = State::new().await;
    let agent_id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut record = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                        .expect("fixture agent");
                    record.extra.insert("executionMode".into(), json!("vm"));
                    record.extra.insert("layrReady".into(), json!(true));
                    record
                        .extra
                        .insert("cwd".into(), json!("/home/studio/project"));
                    record.extra.insert("workspaceMode".into(), json!("layr"));
                    record.extra.insert("threadId".into(), Value::Null);
                    AgentRepository::put(tx, &record)
                })
                .await?;
            Ok(())
        })
        .await
        .expect("fixture becomes VM actor");

    let guest = Arc::new(VmTurnGuest::default());
    guest.state.lock().unwrap().codex_refresh_on_start = true;
    let mut resources = test_resources().unwrap();
    resources.guest_rpc = Some(guest.clone());
    resources.account_session = Some(Arc::new(FakeVmAccountSession {
        generation: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        checks: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        checked: Arc::new(tokio::sync::Notify::new()),
        reconnect_on_second_check: true,
    }));
    resources.vm_provider_launch = Some(Arc::new(VmTurnResolver::default()));
    let peer = Peer::new();
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    let mut ready = peer.ready.clone();
    while !*ready.borrow_and_update() {
        ready.changed().await.expect("account worker ready");
    }

    *worker.faults.pause_operation.lock().unwrap() = Some("callback_admission".into());
    state.wake(&worker).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        worker.faults.paused.notified(),
    )
    .await
    .expect("callback reaches admission immediately before the final generation fence");
    worker.cancel();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        worker.faults.shutdown_wait.notified(),
    )
    .await
    .expect("account worker enters shutdown while the callback fence is held");
    worker.faults.release.notify_one();

    let shutdown = worker.completion().await;
    assert!(
        shutdown.is_ok(),
        "stale actor callback does not terminate its account worker; last phase: {}",
        worker.faults.current_shutdown_phase()
    );
    assert_eq!(
        worker.faults.current_callback_phase(),
        "stale callback actor retired",
        "shutdown must let the stale callback retire before cleanup completes"
    );
    assert!(
        !guest
            .state
            .lock()
            .unwrap()
            .writes
            .iter()
            .any(|write| write["id"] == "vm-auth-refresh"),
        "a reconnect at the final fence must prevent the token reply"
    );
}

#[tokio::test]
async fn vm_worker_restart_reconciles_uncertain_provider_start_without_resending() {
    let state = State::new().await;
    let agent_id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut record = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                        .expect("fixture agent");
                    record.extra.insert("executionMode".into(), json!("vm"));
                    record.extra.insert("layrReady".into(), json!(true));
                    record
                        .extra
                        .insert("cwd".into(), json!("/home/studio/project"));
                    record.extra.insert("workspaceMode".into(), json!("layr"));
                    AgentRepository::put(tx, &record)
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();

    let guest = Arc::new(VmTurnGuest::default());
    guest.state.lock().unwrap().uncertain_start_once = true;
    let make_resources = || {
        let mut resources = test_resources().unwrap();
        resources.guest_rpc = Some(guest.clone());
        resources.vm_provider_launch = Some(Arc::new(VmTurnResolver::default()));
        resources
    };
    let first_peer = Peer::new();
    let mut first_worker = first_peer
        .open_worker_with_resources(state.bridge.clone(), make_resources())
        .await;
    let mut first_ready = first_peer.ready.clone();
    while !*first_ready.borrow_and_update() {
        first_ready.changed().await.unwrap();
    }
    state.wake(&first_worker).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let started = guest.changed.notified();
            if guest.state.lock().unwrap().starts == 1 {
                break;
            }
            started.await;
        }
    })
    .await
    .expect("uncertain provider start is attempted");
    worker_barrier(&first_worker).await;
    let first_recovery = state.record().await;
    assert_eq!(first_recovery["status"], "starting", "{first_recovery}");
    let agent_id = state.agent.clone();
    let receipt_id = format!(
        "linux-vm-provider-start:{agent_id}:{}",
        first_recovery["epoch"].as_i64().expect("VM actor epoch")
    );
    let receipt = state
        .bridge
        .run(move |store| async move {
            let connection = store.read()?;
            Ok(EventWriteRepository::receipt(&connection, &receipt_id)?
                .expect("uncertain start keeps its durable reservation"))
        })
        .await
        .expect("read provider start receipt");
    let receipt_prefix = format!(
        "agent={agent_id};epoch={};launch=",
        first_recovery["epoch"].as_i64().expect("VM actor epoch")
    );
    assert!(
        receipt
            .signature
            .strip_prefix(&receipt_prefix)
            .is_some_and(|fingerprint| fingerprint.len() == 64)
    );
    assert_eq!(
        serde_json::from_str::<Value>(&receipt.result).unwrap()["phase"],
        "reserved"
    );
    {
        let guest_state = guest.state.lock().unwrap();
        assert_eq!(guest_state.starts, 1);
        assert!(
            !guest_state
                .writes
                .iter()
                .any(|write| write["method"] == "turn/start")
        );
    }
    let _first_shutdown = first_worker.shutdown().await;

    let second_peer = Peer::new();
    let mut second_worker = second_peer
        .open_worker_with_resources(state.bridge.clone(), make_resources())
        .await;
    let mut second_ready = second_peer.ready.clone();
    while !*second_ready.borrow_and_update() {
        second_ready.changed().await.unwrap();
    }
    state.wake(&second_worker).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let changed = guest.changed.notified();
            let (has_turn, completed) = {
                let state = guest.state.lock().unwrap();
                (
                    state
                        .writes
                        .iter()
                        .any(|write| write["method"] == "turn/start"),
                    state.acknowledged >= 3,
                )
            };
            if has_turn && completed {
                break;
            }
            changed.await;
        }
    })
    .await
    .expect("recovered guest session completes the queued actor turn");
    {
        let guest_state = guest.state.lock().unwrap();
        assert_eq!(guest_state.starts, 1, "provider.start must never be resent");
        assert!(guest_state.calls.contains(&"provider.rpc".to_owned()));
        assert!(
            guest_state
                .writes
                .iter()
                .any(|write| write["method"] == "turn/start")
        );
    }
    assert_eq!(state.event_status("input").await, "delivered");
    second_worker.shutdown().await.unwrap();
}

impl crate::linux_vm_guest::GuestRpc for TurnStartGuest {
    fn stream(
        &self,
        method: &str,
        params: &Value,
        _request_id: Option<&str>,
        _timeout: std::time::Duration,
        _on_frame: &mut dyn FnMut(
            Value,
        )
            -> std::result::Result<(), crate::linux_vm_guest::GuestError>,
    ) -> std::result::Result<Value, crate::linux_vm_guest::GuestError> {
        self.calls.lock().unwrap().push(method.to_owned());
        let project_id = params["projectId"].as_str().unwrap_or("existing-project");
        match method {
            "project.ensure" => Ok(json!({"projectId":project_id,"stateId":"existing-state"})),
            "line.bind" => {
                *self.project_id.lock().unwrap() = Some(project_id.to_owned());
                Ok(json!({
                    "cwd":"/home/studio/project", "projectId":project_id,
                    "line":"main", "owner":"guest-owner", "home":"/home/studio",
                    "stateId":"bound-state"
                }))
            }
            "health" => Ok(json!({"ok":true})),
            "agent.context" => Ok(json!({
                "projectId":self.project_id.lock().unwrap().clone().unwrap(),
                "line":"main"
            })),
            _ => Err(crate::linux_vm_guest::GuestError::InvalidRequest),
        }
    }
}

#[tokio::test]
async fn cold_vm_lead_prepares_with_fake_guest_and_returns_guest_workspace_token() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../..")
        .canonicalize()
        .unwrap();
    let agent_id = "turn-start-vm-lead";
    let attempt_id = "turn-start-attempt";
    let record = AgentRecord::from_value(json!({
        "id":agent_id,"rootId":agent_id,"epoch":3,"status":"starting",
        "provider":"codex","accountKey":"default","autoWake":true,
        "executionMode":"vm","isLead":true,"workspaceMode":"layr",
        "environment":"linux","cwd":source,"hostProjectPath":source,
        "projectId":"existing-project","layrReady":false,
        "startAttempt":{"id":attempt_id,"epoch":3,"submitted":false}
    }))
    .unwrap();
    store
        .transaction(move |tx| AgentRepository::put(tx, &record))
        .await
        .unwrap();
    let guest = Arc::new(TurnStartGuest {
        calls: std::sync::Mutex::new(Vec::new()),
        project_id: std::sync::Mutex::new(None),
    });
    let mut resources = test_resources().unwrap();
    resources.guest_rpc = Some(guest.clone());
    let intent = TurnIntent {
        agent_id: agent_id.into(),
        epoch: 3,
        attempt_id: attempt_id.into(),
        account_key: "default".into(),
        provider: "codex".into(),
        thread_id: Some("guest-thread".into()),
        cwd: Some(source.to_string_lossy().into_owned()),
        model: Some("fixture-model".into()),
        events: Vec::new(),
        radio_authorized: false,
    };

    let _workspace = preparation::workspace(Arc::clone(&store), &resources, &intent)
        .await
        .unwrap();

    let saved = AgentRepository::get(&store.read().unwrap(), &AgentId::from(agent_id))
        .unwrap()
        .unwrap()
        .value();
    assert_eq!(saved["layrReady"], true);
    assert_eq!(saved["cwd"], "/home/studio/project");
    assert_eq!(
        saved["startAttempt"]["vmWorkspacePreparation"]["phase"],
        "completed"
    );
    assert_eq!(
        guest.calls.lock().unwrap().as_slice(),
        ["project.ensure", "line.bind", "health", "agent.context"]
    );
    let _repeated = preparation::workspace(Arc::clone(&store), &resources, &intent)
        .await
        .unwrap();
    assert_eq!(
        guest.calls.lock().unwrap().len(),
        4,
        "ready VM turns reuse completed guest workspace preparation"
    );
}

async fn scenario_workspace_pending_reuses_reserved_attempt() {
    let state = State::new().await;
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut row =
                        AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                    row.id = AgentId::from("workspace-owner");
                    row.status = studio_runtime_store::AgentStatus::Waiting;
                    row.extra.insert("autoWake".into(), json!(false));
                    row.extra
                        .insert("workspaceOperation".into(), json!("capture"));
                    AgentRepository::put(tx, &row)
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    worker_barrier(&worker).await;
    let pending = state.record().await;
    assert_eq!(pending["status"], "starting");
    assert_eq!(
        pending["startAttempt"]["prepareError"],
        "workspace-readiness/pending"
    );
    assert_eq!(state.event_status("input").await, "reserved");
    assert!(peer.writes.try_recv().is_err());
    let agent = "workspace-owner".to_owned();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut row =
                        AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                    row.extra.remove("workspaceOperation");
                    AgentRepository::put(tx, &row)
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    let turn = state.start(&mut peer, &worker).await;
    let ready = state.record().await;
    assert_eq!(ready["startAttempt"]["id"], pending["startAttempt"]["id"]);
    assert!(ready["startAttempt"].get("prepareError").is_none());
    peer.accepted(&turn).await;
    assert_eq!(state.event_status("input").await, "delivered");
    worker.shutdown().await.unwrap();
}
#[tokio::test]
async fn workspace_pending_reuses_reserved_attempt() {
    scenario_workspace_pending_reuses_reserved_attempt().await;
}

#[tokio::test]
async fn preparation_commit_rechecks_workspace_settings_and_input() {
    for mutation in ["workspace", "settings", "input", "retired", "stopped"] {
        let state = State::new().await;
        let agent = state.agent.clone();
        let epoch = state.epoch;
        state.bridge.run(move |store| async move {
            if mutation == "retired" {
                let mutation_agent = agent.clone();
                store.transaction_with(move |tx| {
                    tx.execute("UPDATE runtime_events SET kind='complaint',text=?1 WHERE id='annotation:input'", [json!({"complaints":[{"complaint_id":"resolved"}]}).to_string()])?;
                    tx.execute("INSERT INTO runtime_complaints(id,record) VALUES('resolved',?1)", [json!({"id":"resolved","leadId":mutation_agent,"recipient":"lead","status":"open","responses":[],"author":mutation_agent,"text":"A response is needed","created":1.0,"version":1}).to_string()])?;
                    Ok::<(), StoreError>(())
                }).await?;
            }
            let intent = store::claim(Arc::clone(&store), "default", &agent, epoch).await?.unwrap();
            let prepared = preparation::prepare(Arc::clone(&store),
                &test_resources()?,
                &intent, Method::TurnStart, 1).await?;
            let mutation_agent = agent.clone();
            store.transaction_with(move |tx| {
                if mutation == "input" {
                    tx.execute("UPDATE runtime_events SET text='changed after preparation' WHERE id='annotation:input'", [])?;
                } else if mutation == "retired" {
                    tx.execute("UPDATE runtime_complaints SET record=json_set(record,'$.status','resolved') WHERE id='resolved'", [])?;
                } else if mutation == "stopped" {
                    let mut row = AgentRepository::get(tx, &AgentId::from(mutation_agent.as_str()))?.unwrap();
                    row.status = studio_runtime_store::AgentStatus::Interrupted;
                    row.epoch += 1;
                    row.extra.insert("autoWake".into(), json!(false));
                    AgentRepository::put(tx, &row)?;
                } else {
                    let mut row = AgentRepository::get(tx, &AgentId::from(mutation_agent.as_str()))?.unwrap();
                    if mutation == "workspace" {
                        row.extra.insert("workspaceMode".into(), json!("image"));
                    } else {
                        row.extra.insert("pendingSettings".into(), json!({"model":"changed-model"}));
                    }
                    AgentRepository::put(tx, &row)?;
                }
                Ok::<(), StoreError>(())
            }).await?;
            let operation = format!("account:default:{}:turn/start", intent.attempt_id);
            let request = RecoveredRequest {
                ticket: studio_runtime_providers::RequestTicket {
                    id: RequestId::String(operation.clone()), generation: 1, method: Method::TurnStart,
                },
                operation_id: OperationId(operation.clone()), thread_id: intent.thread_id.clone(),
                lightweight: false, deadline_ms: 60_000, unknown: false, remote_id: None,
            };
            let context = Pending { thread: prepared.thread, compiled: prepared.compiled,
                intent: prepared.intent, method: "turn/start".into(), operation: operation.clone() };
            assert!(!store::submitted(
                &store,
                context,
                "default",
                request,
                store::SubmissionEvidence {
                    turn: None,
                    submission: prepared.submission,
                    guard: Some(prepared.guard),
                    connection_id: "test-connection".into(),
                },
            ).await?, "{mutation}");
            scheduler::lifecycle_transaction_with(&store, move |tx| {
                assert!(store::receipt(tx, &operation, "submitted")?.is_none());
                let row = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap().value();
                assert_ne!(row["startAttempt"]["settingsFixed"], true, "freeze must roll back: {mutation}");
                let meta = studio_runtime_store::NotificationDispatchRepository::event_metadata(tx, "annotation:input")?;
                assert!(meta.as_ref().and_then(|v| v.get("contextManifest")).is_none());
                Ok::<_, WorkerError>(())
            }).await
        }).await.unwrap_or_else(|error| panic!("{mutation}: {error}"));
        assert_eq!(
            state.event_status("input").await,
            if mutation == "retired" {
                "stored_only"
            } else if mutation == "stopped" {
                "reserved"
            } else {
                "pending"
            },
            "{mutation}"
        );
    }
}

mod preparation_review;

#[tokio::test]
async fn move_import_pending_keeps_input_queued_without_worker_writes() {
    let state = State::new().await;
    let agent_id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut agent = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                        .expect("fixture agent exists");
                    agent.extra.insert("moveImportPending".into(), json!(true));
                    AgentRepository::put(tx, &agent)
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();

    let before = state.record().await;
    let agent_id = state.agent.clone();
    let epoch = state.epoch;
    let claim = state
        .bridge
        .run(move |store| async move { store::claim(store, "default", &agent_id, epoch).await })
        .await
        .unwrap();

    assert!(claim.is_none(), "migration-pending input is not admitted");
    assert_eq!(
        state.record().await,
        before,
        "claim leaves the actor untouched"
    );
    assert_eq!(state.event_status("input").await, "pending");
}

/// Give the fixture agent a stored parent so stop notices have a recipient.
async fn adopt_parent(state: &State) -> String {
    let child = state.agent.clone();
    let parent = format!("{child}-parent");
    let parent_id = parent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut row = AgentRepository::get(tx, &AgentId::from(child.as_str()))?
                        .expect("fixture agent exists");
                    let mut lead = row.clone();
                    lead.id = AgentId::from(parent_id.as_str());
                    lead.extra.insert("isLead".into(), json!(true));
                    lead.extra
                        .insert("threadId".into(), json!("fake-parent-thread"));
                    AgentRepository::put(tx, &lead)?;
                    row.extra.insert("parentId".into(), json!(parent_id));
                    row.extra.insert("isLead".into(), json!(false));
                    AgentRepository::put(tx, &row)
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    parent
}

async fn child_stop_notices(state: &State, parent: &str) -> i64 {
    let parent = parent.to_owned();
    state
        .bridge
        .run(move |store| async move {
            Ok(store.read()?.query_row(
                "SELECT COUNT(*) FROM runtime_events WHERE agent=?1 AND kind='child_result' AND id LIKE 'child-stop:%'",
                [parent],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap()
}

/// The fake provider ends its session the way a provider process exit does.
async fn provider_exits(peer: &Peer, worker: &mut AccountHandle) {
    peer.stop.cancel();
    let _ended = worker.completion().await;
}

#[tokio::test]
async fn provider_exit_while_the_server_runs_is_a_disconnect_reported_once() {
    let state = State::new().await;
    let parent = adopt_parent(&state).await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    provider_exits(&peer, &mut worker).await;

    let record = state.record().await;
    assert_eq!(record["status"], "interrupted");
    assert_eq!(
        record["error"],
        studio_runtime_store::PROVIDER_DISCONNECTED_TURN_ERROR
    );
    assert_ne!(record["disconnectRecovery"]["source"], "restart");
    assert_eq!(child_stop_notices(&state, &parent).await, 1);

    // The registry starts the account's next provider in the same backend.
    let mut next = Peer::identity("default", 2, None);
    let mut replacement = next.worker(state.bridge.clone()).await;
    worker_barrier(&replacement).await;
    let record = state.record().await;
    assert_eq!(
        record["error"],
        studio_runtime_store::PROVIDER_DISCONNECTED_TURN_ERROR
    );
    assert_ne!(record["disconnectRecovery"]["source"], "restart");
    assert!(record["restartRecovery"].is_null());
    assert_eq!(child_stop_notices(&state, &parent).await, 1);
    while let Ok(wire) = next.writes.try_recv() {
        assert_ne!(
            wire.value["method"], "turn/start",
            "unknown input is not resent"
        );
    }
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn disconnect_parent_event_local_enqueue_failure_is_propagated() {
    let state = State::new().await;
    let parent = adopt_parent(&state).await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let before = state.record().await;

    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    tx.execute_batch(
                        "CREATE TRIGGER reject_child_notice BEFORE INSERT ON runtime_events \
                         WHEN NEW.kind='child_result' \
                         BEGIN SELECT RAISE(ABORT,'injected local enqueue failure'); END;",
                    )?;
                    Ok(())
                })
                .await?;
            Ok(())
        })
        .await
        .expect("install local event failure trigger");

    peer.stop.cancel();
    assert!(worker.completion().await.is_err());
    assert_eq!(state.record().await, before, "disconnect writes roll back");
    assert_eq!(child_stop_notices(&state, &parent).await, 0);

    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    tx.execute_batch("DROP TRIGGER reject_child_notice")?;
                    Ok(())
                })
                .await?;
            Ok(())
        })
        .await
        .expect("remove local event failure trigger");

    let mut next = Peer::identity("default", 2, None);
    let mut replacement = next.worker(state.bridge.clone()).await;
    worker_barrier(&replacement).await;
    let record = state.record().await;
    assert_eq!(record["status"], "interrupted");
    assert_eq!(
        record["error"],
        studio_runtime_store::PROVIDER_DISCONNECTED_TURN_ERROR
    );
    assert_ne!(record["disconnectRecovery"]["source"], "restart");
    assert_eq!(child_stop_notices(&state, &parent).await, 1);
    while let Ok(wire) = next.writes.try_recv() {
        assert_ne!(
            wire.value["method"], "turn/start",
            "replacement does not resubmit the previously accepted turn"
        );
    }
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_new_backend_process_still_classifies_the_old_generation_as_a_restart() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    provider_exits(&peer, &mut worker).await;

    let mut bridge = StoreBridge::new(state.bridge.store.clone()).await.unwrap();
    bridge.faults = state.bridge.faults.clone();
    let next = Peer::identity("default", 2, None);
    let mut replacement = next.worker(bridge).await;
    worker_barrier(&replacement).await;
    let record = state.record().await;
    assert_eq!(
        record["error"],
        studio_runtime_store::BACKEND_RESTARTED_TURN_ERROR
    );
    assert_eq!(record["disconnectRecovery"]["source"], "restart");
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn thread_request_lost_with_its_provider_retries_without_duplicate_input() {
    let state = State::new().await;
    let parent = adopt_parent(&state).await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    let _resume = peer.write("thread/resume").await;
    provider_exits(&peer, &mut worker).await;

    let record = state.record().await;
    assert_eq!(record["status"], "queued");
    assert!(record["startAttempt"].is_null());
    assert_eq!(state.event_status("input").await, "pending");
    assert_eq!(child_stop_notices(&state, &parent).await, 0);
    assert!(
        peer.writes.try_recv().is_err(),
        "turn/start was not submitted before thread creation completed"
    );

    let mut next = Peer::identity("default", 2, None);
    let mut replacement = next.worker(state.bridge.clone()).await;
    state.wake(&replacement).await;
    let resume = next.write("thread/resume").await;
    next.reply(&resume, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    let start = next.write("turn/start").await;
    next.accepted(&start).await;
    assert_eq!(state.event_status("input").await, "delivered");
    worker_barrier(&replacement).await;
    assert!(
        next.writes.try_recv().is_err(),
        "the input is submitted once"
    );
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn thread_start_lost_before_turn_submission_retries_attempt_once() {
    let state = State::new().await;
    let parent = adopt_parent(&state).await;
    let agent_id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut agent = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                        .expect("fixture agent exists");
                    agent.extra.remove("threadId");
                    AgentRepository::put(tx, &agent)?;
                    Ok(())
                })
                .await?;
            Ok(())
        })
        .await
        .expect("fixture requires a new native thread");

    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    let _thread_start = peer.write("thread/start").await;
    provider_exits(&peer, &mut worker).await;

    let record = state.record().await;
    assert_eq!(record["status"], "queued");
    assert!(record["startAttempt"].is_null());
    assert_eq!(state.event_status("input").await, "pending");
    assert_eq!(child_stop_notices(&state, &parent).await, 0);

    let mut next = Peer::identity("default", 2, None);
    let mut replacement = next.worker(state.bridge.clone()).await;
    state.wake(&replacement).await;
    let thread_start = next.write("thread/start").await;
    next.reply(&thread_start, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    let turn_start = next.write("turn/start").await;
    next.accepted(&turn_start).await;
    assert_eq!(state.event_status("input").await, "delivered");
    worker_barrier(&replacement).await;
    assert!(
        next.writes.try_recv().is_err(),
        "the unsubmitted input is delivered once after thread creation"
    );
    replacement.shutdown().await.unwrap();
}
