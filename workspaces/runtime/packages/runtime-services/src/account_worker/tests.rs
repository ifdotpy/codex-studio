#![allow(clippy::unwrap_used)]
use super::*;
use crate::event_write::{AnnotationWrite, annotate};
use std::{path::PathBuf, process::Command};
use studio_runtime_process::{EventCursor, ProviderLaunchSpec, provider_seam::*};
use tokio::sync::oneshot;
fn test_resources() -> Result<preparation::PreparationResources, WorkerError> {
    preparation::PreparationResources::bundled(
        String::new(),
        crate::model_catalog::ModelCatalogService::new(None),
        crate::agents_create::prepared_python(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
        ),
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
    count: std::sync::atomic::AtomicUsize,
    pub(super) journal_count: std::sync::atomic::AtomicUsize,
    cycles: std::sync::atomic::AtomicUsize,
    cycled: tokio::sync::Notify,
    pause_at: std::sync::Mutex<Option<usize>>,
    paused: tokio::sync::Notify,
    release: tokio::sync::Notify,
    fired: std::sync::atomic::AtomicBool,
    target: std::sync::Mutex<Option<(usize, bool)>>,
    entered: tokio::sync::Notify,
    pub(super) shutdown_wait: tokio::sync::Notify,
}
impl Faults {
    pub(super) async fn pause(&self, index: usize) {
        let pause = *self.pause_at.lock().unwrap() == Some(index);
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
    }
    pub(super) fn before(&self) -> Result<usize, WorkerError> {
        let index = self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.entered.notify_one();
        self.check(index, false)?;
        Ok(index)
    }
    pub(super) fn after(&self, index: usize) -> Result<(), WorkerError> {
        self.check(index, true)
    }
    fn check(&self, index: usize, after: bool) -> Result<(), WorkerError> {
        if *self.target.lock().unwrap() == Some((index, after)) {
            self.fired.store(true, std::sync::atomic::Ordering::SeqCst);
            Err(WorkerError::InjectedCrash)
        } else {
            Ok(())
        }
    }
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
fn template() -> (PathBuf, Value) {
    let parent = std::env::temp_dir().join("cs-rust-account-worker");
    std::fs::create_dir_all(&parent).unwrap();
    let digest = studio_runtime_store::sha256_hex(CREATE_AGENTS.as_bytes());
    let root = parent.join(format!("template-{}", &digest[..16]));
    let lock = std::fs::File::create(parent.join("template.lock")).unwrap();
    lock.lock().unwrap();
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
    fn gated(mut gate: Option<oneshot::Receiver<()>>) -> Self {
        let stop = Shutdown::default();
        let mut stopped = stop.subscribe();
        let (commands, mut inbox) = mpsc::channel::<AdapterCommand>(16);
        let (entries, journal) = mpsc::channel(16);
        let (out, writes) = mpsc::channel(16);
        let (ack, acks) = mpsc::channel(16);
        let (status, observed) = watch::channel(AttachmentStatus {
            handle: ProviderHandle::for_account("default"),
            open_attempted: true,
            info: Some(AttachmentInfo {
                handle: ProviderHandle::for_account("default"),
                generation: Generation(1),
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
                        let _ignored = reply.send(CommandReceipt::Acknowledged(cursor));
                        tokio::select! {biased; _=Shutdown::cancelled(&mut stopped)=>break, sent=ack.send(cursor)=>{sent.unwrap();}}
                    }
                }
            }
            TaskExit {
                result: Ok(()),
                recovery_needed: None,
            }
        });
        let factory = Arc::new(Factory(std::sync::Mutex::new(Some(
            ProviderAttachment::from_task(commands, journal, observed, stop.clone(), task),
        ))));
        Self {
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
        start_account_worker(
            bridge,
            "default".into(),
            self.factory.clone(),
            OpenOptions {
                clock: self.clock.clone(),
                launch: ProviderLaunchSpec::new(
                    vec!["fake".into()],
                    BTreeMap::new(),
                    "/var/tmp".into(),
                ),
                expected_generation: Some(Generation(1)),
            },
            SessionTaskConfig {
                initialize_params: json!({}),
                generation: Generation(1),
                claude: false,
                timeout_ms: 60_000,
                initialize_operation: OperationId("init".into()),
                request_id_prefix: None,
                recovered: vec![],
            },
            resources,
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
        MATRIX
            .try_with(|context| context.frames.lock().unwrap().push(value.clone()))
            .ok();
        self.cursor += 1;
        self.entries
            .send(JournalEntry {
                received_at_ms: self.clock.now_ms(),
                cursor: EventCursor(self.cursor),
                generation: Generation(1),
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
            if let Ok(context) = MATRIX.try_with(Arc::clone) {
                let bridge = context.state.lock().unwrap().as_ref().unwrap().0.clone();
                let items=bridge.run(move |store| store.transaction_with(|tx| {
                    let checkpoint=store::receipt(tx,"cursor:default:1","ack")?.unwrap_or(Value::Null);
                    assert!(checkpoint.as_u64().unwrap_or(0)>=got,"ACK before committed cursor");
                    let mut query=tx.prepare("SELECT id FROM runtime_items WHERE json_extract(record,'$.role')='assistant'")?;
                    Ok(query.query_map([],|row|row.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?)
                })).await.unwrap();
                *context.acknowledged_items.lock().unwrap() = items;
            }
            if got >= cursor {
                assert_eq!(got, cursor);
                break;
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
        let (store, agent, epoch, root) = tokio::task::spawn_blocking(|| {
            let root = std::env::temp_dir()
                .join("cs-rust-account-worker")
                .join(format!(
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
                Arc::new(RuntimeStore::open(Some(&root)).unwrap()),
                agents[0]["id"].as_str().unwrap().to_owned(),
                agents[0]["epoch"].as_i64().unwrap(),
                root,
            )
        })
        .await
        .unwrap();
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
        let agent = self.agent.clone();
        let id = id.to_owned();
        self.bridge
            .run(move |store| {
                store.transaction_with(|tx| {
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
                })?;
                Ok(())
            })
            .await
            .unwrap();
    }
    async fn record(&self) -> Value {
        let agent = self.agent.clone();
        self.bridge
            .run(move |store| {
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
            .run(move |store| {
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
            .run(move |store| {
                store.transaction_with(|tx| {
                    assert!(
                        store::receipt(tx, &operation, "submitted")?.is_some(),
                        "Submitted must commit before transport admission"
                    );
                    Ok::<_, WorkerError>(())
                })?;
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
        .run(|store| {
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
async fn scenario_shutdown_full_queue_and_silent_provider_is_bounded() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
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
    tokio::time::advance(std::time::Duration::from_secs(61)).await;
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
    // A command-lane barrier proves the completion has been processed, while
    // the missing start reply still prevents acknowledgement of its cursor.
    worker_barrier(&worker).await;
    assert!(peer.acks.try_recv().is_err());
    let reply = peer
        .frame(json!({"id":start["id"],"result":{"turn":{"id":"turn"}}}))
        .await;
    peer.acknowledged(reply).await;
    assert!(reply > cursor);
    assert_eq!(state.record().await["status"], "completed");
    let duplicate = peer.frame(completed).await;
    peer.acknowledged(duplicate).await;
    let stale=peer.frame(json!({"method":"item/completed","params":{"threadId":"fake-thread-0","turnId":"stale","item":{"id":"stale","type":"agentMessage","text":"wrong"}}})).await;
    peer.acknowledged(stale).await;
    let count=state.bridge.run(|store|Ok(store.read()?.query_row("SELECT COUNT(*) FROM runtime_items WHERE json_extract(record,'$.role')='assistant'",[],|r|r.get::<_,i64>(0))?)).await.unwrap();
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
    assert_eq!(state.event_status("input").await, "uncertain");
    assert!(peer.writes.try_recv().is_err());
}
async fn scenario_restart_after_submitted_before_written_recovers_unknown_without_resend() {
    let state = State::new().await;
    let agent = state.agent.clone();
    let epoch = state.epoch;
    let id = state
        .bridge
        .run(move |store| {
            let intent = store::claim(store, "default", &agent, epoch)?.unwrap();
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
            let prepared =
                preparation::prepare(store, &test_resources()?, &intent, Method::TurnStart, 1)?;
            assert!(store::submitted(
                store,
                &Pending {
                    thread: prepared.thread,
                    compiled: prepared.compiled,
                    intent: prepared.intent,
                    method: "turn/start".into(),
                    operation
                },
                "default",
                &request,
                None,
                prepared.submission.as_ref(),
                Some(&prepared.guard)
            )?);
            Ok(request.ticket.id)
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
    assert_eq!(state.event_status("input").await, "uncertain");
    assert!(
        peer.writes.try_recv().is_err(),
        "unknown input must never be resent"
    );
    let cursor = peer
        .frame(json!({"id":id,"result":{"turn":{"id":"turn"}}}))
        .await;
    peer.acknowledged(cursor).await;
    assert_eq!(state.event_status("input").await, "delivered");
    let interrupt = peer.write("turn/interrupt").await;
    peer.reply(&interrupt, json!({})).await;
    peer.completed().await;
    assert_eq!(state.record().await["status"], "completed");
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
    assert_eq!(wire.value["error"]["code"], -32601);
    assert_eq!(wire.value["error"]["data"]["kind"], "unsupported_callback");
    state
        .bridge
        .run(move |store| {
            store.transaction_with(|tx| {
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

async fn scenario_foreign_account_stop_cannot_revoke_agent() {
    let state = State::new().await;
    let before = state.record().await;
    let peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    worker_barrier(&worker).await;
    let agent = state.agent.clone();
    state
        .bridge
        .worker_run(move |store| {
            assert!(store::stop(store, "another-account", &agent)?.is_none());
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
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(120), worker.completion())
            .await
            .expect("callback completion deadline"),
        Err(WorkerError::CallbackUnknown)
    ));
    assert!(peer.acks.try_recv().is_err());
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    peer.frame(frame).await;
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(120), worker.completion())
            .await
            .expect("callback completion deadline"),
        Err(WorkerError::CallbackUnknown)
    ));
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

#[tokio::test(start_paused = true)]
async fn normal_turn_commits_transcript_before_ack_and_pending_event_waits_for_settlement() {
    scenario_normal_turn_commits_transcript_before_ack_and_pending_event_waits_for_settlement()
        .await
}

#[tokio::test(start_paused = true)]
async fn interrupt_before_start_reply_uses_observed_turn_and_revokes_epoch() {
    scenario_interrupt_before_start_reply_uses_observed_turn_and_revokes_epoch().await
}

#[tokio::test(start_paused = true)]
async fn shutdown_full_queue_and_silent_provider_is_bounded() {
    scenario_shutdown_full_queue_and_silent_provider_is_bounded().await
}

#[tokio::test(start_paused = true)]
async fn deadline_unknown_is_not_resent_and_late_reply_refines_it() {
    scenario_deadline_unknown_is_not_resent_and_late_reply_refines_it().await
}

#[tokio::test(start_paused = true)]
async fn provider_exit_mid_turn_retains_unknown_without_retry() {
    scenario_provider_exit_mid_turn_retains_unknown_without_retry().await
}

#[tokio::test(start_paused = true)]
async fn duplicate_stale_and_completion_before_reply_preserve_order() {
    scenario_duplicate_stale_and_completion_before_reply_preserve_order().await
}

#[tokio::test(start_paused = true)]
async fn cancel_queued_input_prevents_provider_admission() {
    scenario_cancel_queued_input_prevents_provider_admission().await
}

#[tokio::test(start_paused = true)]
async fn generation_closed_holds_the_old_submission() {
    scenario_generation_closed_holds_the_old_submission().await
}

#[tokio::test(start_paused = true)]
async fn restart_after_submitted_before_written_recovers_unknown_without_resend() {
    scenario_restart_after_submitted_before_written_recovers_unknown_without_resend().await
}

#[tokio::test(start_paused = true)]
async fn unsupported_approval_is_answered_after_commit() {
    scenario_unsupported_approval_is_answered_after_commit().await
}

#[tokio::test(start_paused = true)]
async fn unsupported_question_is_answered_after_commit() {
    scenario_unsupported_question_is_answered_after_commit().await
}

#[tokio::test(start_paused = true)]
async fn unsupported_dynamic_tool_is_answered_after_commit() {
    scenario_unsupported_dynamic_tool_is_answered_after_commit().await
}

#[tokio::test(start_paused = true)]
async fn foreign_account_stop_cannot_revoke_agent() {
    scenario_foreign_account_stop_cannot_revoke_agent().await
}

#[tokio::test(start_paused = true)]
async fn callback_unknown_write_is_not_resent_on_replay() {
    scenario_callback_unknown_write_is_not_resent_on_replay().await
}

#[tokio::test(start_paused = true)]
async fn a_new_stop_can_retry_a_definitely_rejected_interrupt() {
    scenario_a_new_stop_can_retry_a_definitely_rejected_interrupt().await
}

mod crash_matrix;
use crash_matrix::MATRIX;

async fn worker_barrier(worker: &AccountHandle) {
    let (reply, done) = oneshot::channel();
    worker.send(AccountCommand::Barrier(reply)).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(120), done)
        .await
        .expect("worker command deadline")
        .expect("worker command barrier");
}

#[tokio::test(start_paused = true)]
async fn shutdown_releases_account_while_bridge_permit_is_blocked() {
    let state = State::new().await;
    let peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let permit = state.bridge.slot.clone().acquire_owned().await.unwrap();
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
        state.bridge.faults.entered.notified().await;
    }
    worker.cancel();
    state.bridge.faults.shutdown_wait.notified().await;
    tokio::time::advance(SHUTDOWN_GRACE * 2).await;
    assert!(matches!(
        worker.completion().await,
        Err(WorkerError::ShutdownTimeout)
    ));
    assert!(!state.bridge.accounts.lock().unwrap().contains("default"));
    drop(permit);
    let peer = Peer::new();
    let mut replacement = peer.worker(state.bridge.clone()).await;
    replacement.shutdown().await.unwrap();
    assert_eq!(state.event_status("input").await, "pending");
}

#[tokio::test(start_paused = true)]
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
    let count = state.bridge.run(|store| Ok(store.read()?.query_row("SELECT COUNT(*) FROM runtime_items WHERE json_extract(record,'$.text')='after restart'", [], |r|r.get::<_,i64>(0))?)).await.unwrap();
    assert_eq!(count, 1);
    peer.completed().await;
    assert_eq!(state.record().await["status"], "completed");
    worker.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
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
    let unresolved = state.bridge.run(|store| Ok(store.read()?.query_row("SELECT COUNT(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:account:%:submitted'", [], |r|r.get::<_,i64>(0))?)).await.unwrap();
    assert_eq!(unresolved, 0);
    assert_ne!(state.event_status("input").await, "uncertain");
    worker.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
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
        Err(WorkerError::InjectedCrash)
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

#[tokio::test(start_paused = true)]
async fn transfer_between_resume_and_start_never_writes_to_old_account() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    let resume = peer.write("thread/resume").await;
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| {
            store.transaction_with(|tx| {
                let mut row = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                row.extra.insert("accountKey".into(), json!("transferred"));
                AgentRepository::put(tx, &row)?;
                Ok(())
            })
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

#[tokio::test(start_paused = true)]
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
    let rows = state.bridge.run(|store| Ok(store.read()?.query_row("SELECT COUNT(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:journal:%'", [], |r|r.get::<_,i64>(0))?)).await.unwrap();
    assert_eq!(rows, 0);
    worker.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
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
        .run(|store| {
            store.transaction_with(|tx| {
                Ok(store::receipt(tx, "streams:default", "pending")?.is_some())
            })
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
        Err(WorkerError::InjectedCrash)
    ));
    state.bridge.faults.arm(None);
    let mut peer = Peer::new();
    peer.cursor = delta;
    let mut worker = peer.worker(state.bridge.clone()).await;
    let complete = peer.frame(json!({"method":"item/completed","params":{"threadId":"fake-thread-0","turnId":"turn","item":{"id":"stream","type":"agentMessage","text":""}}})).await;
    peer.acknowledged(complete).await;
    let (text,pending,bases) = state.bridge.run(|store| store.transaction_with(|tx| {
        let text:String = tx.query_row("SELECT json_extract(record,'$.text') FROM runtime_items WHERE json_extract(record,'$.id') LIKE '%:stream'", [], |row| row.get(0))?;
        Ok((text,store::receipt(tx,"streams:default","pending")?.is_some(),tx.query_row("SELECT COUNT(*) FROM runtime_operation_receipts WHERE signature='stream-base'",[],|row| row.get::<_,i64>(0))?))
    })).await.unwrap();
    assert_eq!(text, "durable delta");
    assert!(!pending);
    assert_eq!(bases, 0);
    worker.shutdown().await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn durable_stream_batch_recovers_before_flush() {
    scenario_durable_stream_batch_recovers_before_flush().await;
}

#[tokio::test(start_paused = true)]
async fn unrelated_store_failure_after_enqueue_keeps_write_unknown() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    let resume = peer.write("thread/resume").await;
    peer.reply(&resume, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    let held = peer.next_write().await;
    assert_eq!(held.value["method"], "turn/start");
    state.bridge.run(|store| {
        store.transaction_with(|tx| {
            tx.execute_batch("CREATE TRIGGER fail_journal BEFORE INSERT ON runtime_operation_receipts WHEN NEW.id LIKE 'account-worker:journal:%' BEGIN SELECT RAISE(ABORT,'unrelated journal failure'); END;")?;
            Ok(())
        })
    }).await.unwrap();
    peer.frame(json!({"method":"test/barrier","params":{}}))
        .await;
    assert!(worker.completion().await.is_err());
    drop(held);
    assert_eq!(state.event_status("input").await, "uncertain");
}

async fn scenario_stream_base_survives_replaced_visible_item() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let delta = peer.frame(json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-0","turnId":"turn","itemId":"base","delta":"a".repeat(store::STREAM_FLUSH_BYTES)}})).await;
    peer.acknowledged(delta).await;
    let started = peer.frame(json!({"method":"item/started","params":{"threadId":"fake-thread-0","turnId":"turn","item":{"id":"base","type":"agentMessage","text":"replacement"}}})).await;
    peer.acknowledged(started).await;
    let delta = peer.frame(json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-0","turnId":"turn","itemId":"base","delta":"tail"}})).await;
    peer.acknowledged(delta).await;
    let completed = peer.frame(json!({"method":"item/completed","params":{"threadId":"fake-thread-0","turnId":"turn","item":{"id":"base","type":"agentMessage","text":""}}})).await;
    peer.acknowledged(completed).await;
    let (text, bases) = state
        .bridge
        .run(|store| {
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
    assert_eq!(
        text,
        format!("{}tail", "a".repeat(store::STREAM_FLUSH_BYTES))
    );
    assert_eq!(bases, 0);
    worker.shutdown().await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn stream_base_survives_replaced_visible_item() {
    scenario_stream_base_survives_replaced_visible_item().await;
}

#[tokio::test(start_paused = true)]
async fn stop_and_wake_progress_with_a_replenished_event_lane() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let permit = state.bridge.slot.clone().acquire_owned().await.unwrap();
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
        state.bridge.faults.entered.notified().await;
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

#[tokio::test(start_paused = true)]
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
        let count=state.bridge.run(|store| Ok(store.read()?.query_row("SELECT COUNT(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:%'",[],|r|r.get::<_,i64>(0))?)).await.unwrap();
        assert!(count <= 4, "turn {turn}: retained {count} worker rows");
    }
    worker.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn shutdown_releases_account_while_store_writer_is_blocked() {
    let state = State::new().await;
    let peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let (entered, observed) = oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let store = state.bridge.store.clone();
    let lock = tokio::task::spawn_blocking(move || {
        store.transaction_with(|_| {
            entered.send(()).unwrap();
            blocked.recv().unwrap();
            Ok::<_, WorkerError>(())
        })
    });
    observed.await.unwrap();
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
        state.bridge.faults.entered.notified().await;
    }
    worker.cancel();
    state.bridge.faults.shutdown_wait.notified().await;
    tokio::time::advance(SHUTDOWN_GRACE * 2).await;
    assert!(matches!(
        worker.completion().await,
        Err(WorkerError::ShutdownTimeout)
    ));
    assert!(!state.bridge.accounts.lock().unwrap().contains("default"));
    assert_eq!(
        state.bridge.slot.available_permits(),
        0,
        "started job retains permit"
    );
    release.send(()).unwrap();
    lock.await.unwrap().unwrap();
    let peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    assert_eq!(state.event_status("input").await, "pending");
    worker.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn stream_base_and_metadata_rollback_together() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let delta=peer.frame(json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-0","turnId":"turn","itemId":"atomic","delta":"x".repeat(store::STREAM_FLUSH_BYTES)}})).await;
    peer.acknowledged(delta).await;
    state.bridge.run(|store| store.transaction_with(|tx| {
        tx.execute_batch("CREATE TRIGGER fail_stream_metadata BEFORE UPDATE ON runtime_operation_receipts WHEN NEW.id='account-worker:streams:default:metadata' BEGIN SELECT RAISE(ABORT,'crash after base write before metadata'); END;")?;
        Ok(())
    })).await.unwrap();
    let frame = json!({"method":"item/started","params":{"threadId":"fake-thread-0","turnId":"turn","item":{"id":"atomic","type":"agentMessage","text":"replacement"}}});
    peer.frame(frame.clone()).await;
    assert!(worker.completion().await.is_err());
    state.bridge.run(move |store| store.transaction_with(|tx| {
        assert_eq!(tx.query_row("SELECT COUNT(*) FROM runtime_operation_receipts WHERE signature='stream-base'",[],|row|row.get::<_,i64>(0))?,0);
        let body:String=tx.query_row("SELECT body FROM runtime_item_fulltext WHERE id LIKE '%:atomic'",[],|row|row.get(0))?;
        assert_eq!(body,"x".repeat(store::STREAM_FLUSH_BYTES));
        assert_eq!(store::receipt(tx,"cursor:default:1","ack")?,Some(json!(delta)));
        tx.execute_batch("DROP TRIGGER fail_stream_metadata")?;
        Ok(())
    })).await.unwrap();
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
        .run(move |store| {
            store.transaction_with(|tx| {
                let mut row = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                row.extra.insert("accountKey".into(), json!("transferred"));
                AgentRepository::put(tx, &row)?;
                Ok(())
            })
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

#[tokio::test(start_paused = true)]
async fn transfer_after_claim_before_resume_submission_is_refused() {
    transfer_at_admission(1, false).await;
}
#[tokio::test(start_paused = true)]
async fn transfer_before_claim_transaction_leaves_input_pending() {
    transfer_at_admission(0, false).await;
}
#[tokio::test(start_paused = true)]
async fn transfer_after_stop_before_interrupt_submission_is_refused() {
    transfer_at_admission(1, true).await;
}

async fn scenario_new_thread_freezes_settings_and_accepts_context() {
    let state = State::new().await;
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| {
            store.transaction(|tx| {
                let mut record = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                record.extra.remove("threadId");
                record.extra.remove("nativeToolCatalog");
                record.extra.insert(
                    "pendingSettings".into(),
                    json!({"model":"pending-model","effort":"high","nativeEffort":"high","fastMode":true}),
                );
                AgentRepository::put(tx, &record)
            })?;
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
        .run(|store| {
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
        .run(|store| {
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
#[tokio::test(start_paused = true)]
async fn new_thread_freezes_settings_and_accepts_context() {
    scenario_new_thread_freezes_settings_and_accepts_context().await;
}

async fn scenario_child_result_materializes_before_submission() {
    let state = State::new().await;
    state.bridge.run(|store|{store.transaction(|tx|{
        tx.execute("UPDATE runtime_events SET kind='child_result',text=?1 WHERE id='annotation:input'",[json!({"agent_id":"child","result":"artifact-body".repeat(2400)}).to_string()])?;Ok(())
    })?;Ok(())}).await.unwrap();
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
#[tokio::test(start_paused = true)]
async fn child_result_materializes_before_submission() {
    scenario_child_result_materializes_before_submission().await;
}

async fn scenario_workspace_refusal_retains_queue() {
    let state = State::new().await;
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| {
            store.transaction(|tx| {
                let mut record = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                record.extra.insert("executionMode".into(), json!("vm"));
                AgentRepository::put(tx, &record)
            })?;
            Ok(())
        })
        .await
        .unwrap();
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    worker_barrier(&worker).await;
    let record = state.record().await;
    assert_eq!(record["status"], "failed");
    assert_eq!(record["error"], "VmWorkspaceUnsupported");
    assert_eq!(state.event_status("input").await, "pending");
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn workspace_refusal_retains_queue() {
    scenario_workspace_refusal_retains_queue().await;
}

async fn scenario_workspace_pending_reuses_reserved_attempt() {
    let state = State::new().await;
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| {
            store.transaction(|tx| {
                let mut row = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                row.id = AgentId::from("workspace-owner");
                row.status = studio_runtime_store::AgentStatus::Waiting;
                row.extra.insert("autoWake".into(), json!(false));
                row.extra
                    .insert("workspaceOperation".into(), json!("capture"));
                AgentRepository::put(tx, &row)
            })?;
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
        .run(move |store| {
            store.transaction(|tx| {
                let mut row = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                row.extra.remove("workspaceOperation");
                AgentRepository::put(tx, &row)
            })?;
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
#[tokio::test(start_paused = true)]
async fn workspace_pending_reuses_reserved_attempt() {
    scenario_workspace_pending_reuses_reserved_attempt().await;
}

#[tokio::test]
async fn preparation_commit_rechecks_workspace_settings_and_input() {
    for mutation in ["workspace", "settings", "input", "retired"] {
        let state = State::new().await;
        let agent = state.agent.clone();
        let epoch = state.epoch;
        state.bridge.run(move |store| {
            if mutation == "retired" {
                store.transaction(|tx| {
                    tx.execute("UPDATE runtime_events SET kind='complaint',text=?1 WHERE id='annotation:input'", [json!({"complaints":[{"complaint_id":"resolved"}]}).to_string()])?;
                    tx.execute("INSERT INTO runtime_complaints(id,record) VALUES('resolved',?1)", [json!({"id":"resolved","leadId":agent,"recipient":"lead","status":"open","responses":[],"author":agent,"text":"A response is needed","created":1.0,"version":1}).to_string()])?;
                    Ok(())
                })?;
            }
            let intent = store::claim(store, "default", &agent, epoch)?.unwrap();
            let prepared = preparation::prepare(store,
                &test_resources()?,
                &intent, Method::TurnStart, 1)?;
            store.transaction(|tx| {
                if mutation == "input" {
                    tx.execute("UPDATE runtime_events SET text='changed after preparation' WHERE id='annotation:input'", [])?;
                } else if mutation == "retired" {
                    tx.execute("UPDATE runtime_complaints SET record=json_set(record,'$.status','resolved') WHERE id='resolved'", [])?;
                } else {
                    let mut row = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                    if mutation == "workspace" {
                        row.extra.insert("workspaceMode".into(), json!("image"));
                    } else {
                        row.extra.insert("pendingSettings".into(), json!({"model":"changed-model"}));
                    }
                    AgentRepository::put(tx, &row)?;
                }
                Ok(())
            })?;
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
            assert!(!store::submitted(store, &context, "default", &request, None,
                prepared.submission.as_ref(), Some(&prepared.guard))?, "{mutation}");
            scheduler::lifecycle_transaction_with(store, |tx| {
                assert!(store::receipt(tx, &operation, "submitted")?.is_none());
                let row = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap().value();
                assert_ne!(row["startAttempt"]["settingsFixed"], true, "freeze must roll back: {mutation}");
                let meta = studio_runtime_store::NotificationDispatchRepository::event_metadata(tx, "annotation:input")?;
                assert!(meta.as_ref().and_then(|v| v.get("contextManifest")).is_none());
                Ok::<_, WorkerError>(())
            })
        }).await.unwrap_or_else(|error| panic!("{mutation}: {error}"));
        assert_eq!(
            state.event_status("input").await,
            if mutation == "retired" {
                "stored_only"
            } else {
                "pending"
            },
            "{mutation}"
        );
    }
}

mod preparation_review;
