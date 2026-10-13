#![allow(clippy::unwrap_used)]
use super::*;
use crate::event_write::{AnnotationWrite, annotate};
use std::{path::PathBuf, process::Command};
use studio_runtime_process::{EventCursor, ProviderLaunchSpec, provider_seam::*};
use tokio::sync::oneshot;
const CREATE_AGENTS: &str = r#"
import json, pathlib, sys
sys.path.insert(0, sys.argv[4])
from oracle_guard import install_guard
_GUARD = install_guard()  # Runtime initialization owns a local Node crypto helper.
sys.path.insert(0, sys.argv[2])
from codex_runtime import Runtime
class QuietRuntime(Runtime):
    def schedule(self):
        return
root = pathlib.Path(sys.argv[1])
runtime = QuietRuntime(root, lambda *_args, **_kwargs: None)
agents = []
for index in range(int(sys.argv[3])):
    agent = runtime.create({"name":"Rust turn fixture", "cwd":str(root), "prompt":"temporary"}, defer=True)
    agent.update(autoWake=True, status="queued", accountKey="default",
                 threadId="fake-thread-" + str(index), model="fake-model")
    with runtime.lock, runtime.db() as db:
        runtime.put(db, "agents", agent)
    agents.append({"id":agent["id"], "epoch":agent["epoch"]})
print(json.dumps(agents))
runtime.close()
"#;

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
        let task = tokio::spawn(async move {
            let _status = status;
            loop {
                let command = tokio::select! { biased; _=cancelled(&mut stopped)=>break, command=inbox.recv()=>match command {Some(c)=>c,None=>break}};
                match command {
                    AdapterCommand::Write {
                        write,
                        reply,
                        observed,
                    } => {
                        let initialized = write.message()["method"] == "initialized";
                        let result = match write.message()["method"].as_str() {
                            Some("initialize") => CommandReceipt::CachedInitialize(json!({})),
                            Some("initialized") => CommandReceipt::Written {
                                remote_id: None,
                                duplicate: false,
                            },
                            _ => {
                                let (receipt, result) = oneshot::channel();
                                tokio::select! {biased; _=cancelled(&mut stopped)=>break, sent=out.send(Wire {value:write.message().clone(),receipt})=>{sent.unwrap();}}
                                tokio::select! {biased; _=cancelled(&mut stopped)=>break, r=result=>r.unwrap_or(CommandReceipt::Unknown)}
                            }
                        };
                        let _ignored = reply.send(result);
                        tokio::select! {biased; _=cancelled(&mut stopped)=>break, _=observed=>{}}
                        if initialized {
                            ready_tx.send_replace(true);
                        }
                    }
                    AdapterCommand::Ack { cursor, reply } => {
                        let _ignored = reply.send(CommandReceipt::Acknowledged(cursor));
                        tokio::select! {biased; _=cancelled(&mut stopped)=>break, sent=ack.send(cursor)=>{sent.unwrap();}}
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
        let worker = start_account_worker(
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
        )
        .await
        .unwrap();
        let mut ready = self.ready.clone();
        while !*ready.borrow_and_update() {
            ready.changed().await.unwrap();
        }
        worker
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
}
impl State {
    async fn new() -> Self {
        let (store, agent, epoch, root) = tokio::task::spawn_blocking(|| {
            let root = PathBuf::from("/var/tmp/cs-rust-account-worker").join(format!(
                "worker-{}",
                uuid::Uuid::new_v5(
                    &uuid::Uuid::NAMESPACE_OID,
                    format!("{:?}-{:?}", SystemTime::now(), std::thread::current().id()).as_bytes()
                )
            ));
            std::fs::create_dir_all(&root).unwrap();
            let output = Command::new(std::env::var_os("CODEX_AGENTS_PYTHON").unwrap())
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
            let agents: Value = serde_json::from_slice(&output.stdout).unwrap();
            (
                Arc::new(RuntimeStore::open(Some(&root)).unwrap()),
                agents[0]["id"].as_str().unwrap().to_owned(),
                agents[0]["epoch"].as_i64().unwrap(),
                root,
            )
        })
        .await
        .unwrap();
        let state = Self {
            bridge: StoreBridge::new(store).await.unwrap(),
            agent,
            epoch,
            root,
        };
        state.enqueue("input").await;
        state
    }
    async fn enqueue(&self, id: &str) {
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
        let _ignored = std::fs::remove_dir_all(&self.root);
    }
}
#[tokio::test(start_paused = true)]
async fn normal_turn_commits_transcript_before_ack_and_pending_event_waits_for_settlement() {
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
#[tokio::test(start_paused = true)]
async fn interrupt_before_start_reply_uses_observed_turn_and_revokes_epoch() {
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
#[tokio::test(start_paused = true)]
async fn shutdown_full_queue_and_silent_provider_is_bounded() {
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
    worker.completion().await.unwrap();
    drop(blocked);
    assert_ne!(state.event_status("input").await, "delivered");
}
#[tokio::test(start_paused = true)]
async fn deadline_unknown_is_not_resent_and_late_reply_refines_it() {
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
#[tokio::test(start_paused = true)]
async fn provider_exit_mid_turn_retains_unknown_without_retry() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let _start = state.start(&mut peer, &worker).await;
    peer.stop.cancel();
    let _result = worker.completion().await;
    assert_eq!(state.event_status("input").await, "uncertain");
    assert!(peer.writes.try_recv().is_err());
}
#[tokio::test(start_paused = true)]
async fn duplicate_stale_and_completion_before_reply_preserve_order() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    let completed = json!({"method":"turn/completed","params":{"threadId":"fake-thread-0","turn":{"id":"turn","status":"completed"}}});
    let cursor = peer.frame(completed.clone()).await;
    // No ACK can pass the completion until the correlated reply binds its turn.
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
#[tokio::test(start_paused = true)]
async fn cancel_queued_input_prevents_provider_admission() {
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
    // Cancellation overtakes the queued work and cannot admit provider input.
    worker.cancel();
    worker.completion().await.unwrap();
    assert!(peer.writes.try_recv().is_err());
    assert_ne!(state.event_status("input").await, "delivered");
}
#[tokio::test(start_paused = true)]
async fn generation_closed_holds_the_old_submission() {
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
#[tokio::test(start_paused = true)]
async fn restart_after_submitted_before_written_recovers_unknown_without_resend() {
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
            let (_, submission) = store::params(store, &intent, Method::TurnStart, None)?.unwrap();
            assert!(store::submitted(
                store,
                &Pending {
                    intent,
                    method: "turn/start".into(),
                    operation
                },
                &request,
                None,
                submission.as_ref()
            )?);
            Ok(request.ticket.id)
        })
        .await
        .unwrap();
    // Reconstruct the runtime boundary as after a process crash. There is no Written receipt.
    let bridge = StoreBridge::new(state.bridge.store.clone()).await.unwrap();
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
#[tokio::test(start_paused = true)]
async fn unsupported_approval_is_answered_after_commit() {
    unsupported_callback("item/commandExecution/requestApproval").await;
}
#[tokio::test(start_paused = true)]
async fn unsupported_question_is_answered_after_commit() {
    unsupported_callback("item/tool/requestUserInput").await;
}
#[tokio::test(start_paused = true)]
async fn unsupported_dynamic_tool_is_answered_after_commit() {
    unsupported_callback("item/tool/call").await;
}

#[tokio::test(start_paused = true)]
async fn foreign_account_stop_cannot_revoke_agent() {
    let state = State::new().await;
    let before = state.record().await;
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| {
            assert!(store::stop(store, "another-account", &agent)?.is_none());
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(state.record().await, before);
}

#[tokio::test(start_paused = true)]
async fn callback_unknown_write_is_not_resent_on_replay() {
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

#[tokio::test(start_paused = true)]
async fn a_new_stop_can_retry_a_definitely_rejected_interrupt() {
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
