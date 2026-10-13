use super::*;
use crate::event_write::{AnnotationWrite, annotate};
use crate::scheduler::HISTORY_UNAVAILABLE;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use studio_runtime_process::{ProviderFactoryError, ProviderProcess, ProviderProcessReader};
use studio_runtime_store::{AgentRecord, AgentStatus};

const CREATE_AGENTS: &str = r#"
import json, pathlib, sys
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

static TEMP_STATE_ID: AtomicUsize = AtomicUsize::new(0);

struct TemporaryState(PathBuf);

impl TemporaryState {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock follows Unix epoch")
            .as_nanos();
        let id = TEMP_STATE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "studio-turn-runtime-{}-{nonce}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("temporary state directory creates");
        Self(root)
    }
}

impl Drop for TemporaryState {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

fn fixture_agents(root: &Path, count: usize) -> Vec<(String, i64)> {
    let output = Command::new(python_executable())
        .arg("-c")
        .arg(CREATE_AGENTS)
        .arg(root)
        .arg(server_source())
        .arg(count.to_string())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("Python fixture starts");
    assert!(
        output.status.success(),
        "Python fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<Vec<Value>>(&output.stdout)
        .expect("Python fixture returns JSON")
        .into_iter()
        .map(|agent| {
            (
                agent["id"]
                    .as_str()
                    .expect("agent id is a string")
                    .to_owned(),
                agent["epoch"].as_i64().expect("agent epoch is an integer"),
            )
        })
        .collect()
}

fn write_annotation(store: &RuntimeStore, agent_id: &str, id: &str) {
    store
        .transaction(|transaction| {
            annotate(
                transaction,
                AnnotationWrite {
                    agent_id: agent_id.to_owned(),
                    id: Some(id.to_owned()),
                    path: "README.md".to_owned(),
                    line: 1,
                    text: "fake-provider annotation".to_owned(),
                    turn_id: None,
                    signature: id.to_owned(),
                },
            )
            .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
            Ok(())
        })
        .expect("annotation creates a pending scheduler event");
}

fn python_executable() -> PathBuf {
    if let Some(python) = std::env::var_os("CODEX_AGENTS_PYTHON") {
        return python.into();
    }
    let output = Command::new("python3")
        .arg(server_source().join("codex_python.py"))
        .output()
        .expect("Python resolver starts");
    assert!(output.status.success(), "Python resolver succeeds");
    String::from_utf8(output.stdout)
        .expect("resolved interpreter is UTF-8")
        .trim()
        .into()
}

fn server_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src")
}

const WATCHDOG: Duration = Duration::from_secs(20);
type FakeFrame = Option<(Vec<u8>, u64)>;
struct Fake {
    output: Mutex<Option<TestSender<FakeFrame>>>,
    requests: TestSender<Value>,
    generation: AtomicU64,
    acknowledged: AtomicU64,
    fail_write: AtomicBool,
    attaches: AtomicUsize,
    stops: AtomicUsize,
}
impl Fake {
    fn frame(&self, value: Value, cursor: u64) {
        self.output
            .lock()
            .expect("fixture operation succeeds")
            .as_ref()
            .expect("fixture operation succeeds")
            .send(Some((format!("{value}\n").into_bytes(), cursor)))
            .expect("fixture operation succeeds");
    }
}
struct SharedFake(Arc<Fake>);
impl ProviderProcessFactory for SharedFake {
    fn start_or_reattach(
        &self,
        _handle: &ProviderHandle,
    ) -> Result<Box<dyn ProviderProcess>, ProviderFactoryError> {
        let (tx, rx) = mpsc::channel();
        *self.0.output.lock().expect("fixture operation succeeds") = Some(tx);
        self.0.attaches.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(FakeProcess {
            fake: self.0.clone(),
            input: Some(rx),
        }))
    }
}
struct FakeProcess {
    fake: Arc<Fake>,
    input: Option<TestReceiver<FakeFrame>>,
}
impl ProviderProcess for FakeProcess {
    fn split(&mut self) -> io::Result<(Box<dyn ProviderProcessReader>, Box<dyn Write + Send>)> {
        Ok((
            Box::new(FakeReader {
                input: self.input.take().expect("fixture operation succeeds"),
                buffer: VecDeque::new(),
                cursors: Vec::new(),
            }),
            Box::new(FakeWriter {
                fake: self.fake.clone(),
                buffer: Vec::new(),
            }),
        ))
    }
    fn generation(&self) -> u64 {
        self.fake.generation.load(Ordering::Relaxed)
    }
    fn resumed(&self) -> bool {
        self.fake.attaches.load(Ordering::Relaxed) > 1
    }
    fn acknowledged_cursor(&self) -> u64 {
        self.fake.acknowledged.load(Ordering::Relaxed)
    }
    fn acknowledge(&mut self, cursor: u64) -> io::Result<()> {
        self.fake.acknowledged.store(cursor, Ordering::Relaxed);
        Ok(())
    }
    fn stop(&mut self) -> io::Result<()> {
        self.fake.stops.fetch_add(1, Ordering::Relaxed);
        if let Some(output) = self
            .fake
            .output
            .lock()
            .expect("fixture operation succeeds")
            .take()
        {
            let _ignored = output.send(None);
        }
        Ok(())
    }
}
struct FakeReader {
    input: TestReceiver<FakeFrame>,
    buffer: VecDeque<u8>,
    cursors: Vec<u64>,
}
impl ProviderProcessReader for FakeReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.buffer.is_empty() {
            match self.input.recv() {
                Ok(Some((bytes, cursor))) => {
                    self.buffer.extend(bytes);
                    self.cursors.push(cursor);
                }
                _ => return Ok(0),
            }
        }
        let n = output.len().min(self.buffer.len());
        for byte in &mut output[..n] {
            *byte = self.buffer.pop_front().expect("fixture operation succeeds");
        }
        Ok(n)
    }
    fn take_cursors(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.cursors)
    }
}
struct FakeWriter {
    fake: Arc<Fake>,
    buffer: Vec<u8>,
}
impl Write for FakeWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.fake.fail_write.swap(false, Ordering::Relaxed) {
            return Err(io::Error::other("injected write failure"));
        }
        self.buffer.extend_from_slice(bytes);
        while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line = self.buffer.drain(..=end).collect::<Vec<_>>();
            let frame: Value = serde_json::from_slice(&line)?;
            self.fake
                .requests
                .send(frame)
                .map_err(|_| io::Error::other("test request receiver closed"))?;
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct Harness {
    _state: TemporaryState,
    store: Arc<RuntimeStore>,
    agents: Vec<(String, i64)>,
    fake: Arc<Fake>,
    worker: AccountWorker,
    input: Receiver,
    requests: TestReceiver<Value>,
    cursor: u64,
}
impl Harness {
    fn new(count: usize) -> Self {
        Self::connecting(count, true)
    }
    fn connecting(count: usize, initialize: bool) -> Self {
        let state = TemporaryState::new();
        let agents = fixture_agents(&state.0, count);
        let store =
            Arc::new(RuntimeStore::open(Some(&state.0)).expect("fixture operation succeeds"));
        let (request_tx, requests) = mpsc::channel();
        let (tx, input) = mailbox::channel();
        let wake = TurnWakeHandle(Weak::new());
        let fake = Arc::new(Fake {
            output: Mutex::new(None),
            requests: request_tx,
            generation: AtomicU64::new(1),
            acknowledged: AtomicU64::new(0),
            fail_write: AtomicBool::new(false),
            attaches: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
        });
        let worker = AccountWorker::new(
            store.clone(),
            Arc::new(SharedFake(fake.clone())),
            "default".into(),
            tx,
            wake,
            Arc::new(AtomicU64::new(0)),
            60_000,
        );
        let mut h = Self {
            _state: state,
            store,
            agents,
            fake,
            worker,
            input,
            requests,
            cursor: 0,
        };
        h.worker.connect().expect("fixture operation succeeds");
        if initialize {
            h.initialize();
        }
        h
    }
    fn request(&self, method: &str) -> Value {
        loop {
            let value = self
                .requests
                .recv_timeout(WATCHDOG)
                .expect("provider request arrives");
            if value["method"] == "initialized" {
                continue;
            }
            assert_eq!(value["method"], method, "unexpected request: {value}");
            return value;
        }
    }
    fn initialize(&mut self) {
        let request = self.request("initialize");
        self.frame(json!({"id":request["id"],"result":{"protocolVersion":1}}));
        assert!(
            self.worker
                .session
                .as_ref()
                .expect("fixture operation succeeds")
                .is_ready()
        );
    }
    fn frame(&mut self, frame: Value) {
        self.cursor += 1;
        self.fake.frame(frame, self.cursor);
        loop {
            let input = self
                .input
                .recv_timeout(WATCHDOG)
                .expect("provider frame arrives");
            let matches = matches!(&input,AccountInput::Transport(ProviderSessionInput::Frame{cursor,..}) if *cursor == Some(self.cursor));
            self.worker
                .input(input)
                .expect("fixture operation succeeds");
            if matches {
                break;
            }
        }
    }
    fn start(&mut self, index: usize) -> Value {
        let (agent, epoch) = self.agents[index].clone();
        write_annotation(&self.store, &agent, &format!("input-{index}"));
        self.worker
            .input(AccountInput::Wake(agent, epoch))
            .expect("fixture operation succeeds");
        let resume = self.request("thread/resume");
        self.frame(
            json!({"id":resume["id"],"result":{"thread":{"id":resume["params"]["threadId"]}}}),
        );
        self.request("turn/start")
    }
    fn accepted(&mut self, request: &Value, turn: &str) {
        self.frame(json!({"id":request["id"],"result":{"turn":{"id":turn}}}));
    }
    fn notice(&mut self, index: usize, method: &str, turn: &str, status: &str) {
        self.frame(json!({"method":method,"params":{"threadId":format!("fake-thread-{index}"),"turn":{"id":turn,"status":status}}}));
    }
    fn record(&self, index: usize) -> AgentRecord {
        AgentRepository::get(
            &self.store.read().expect("fixture operation succeeds"),
            &AgentId::from(self.agents[index].0.as_str()),
        )
        .expect("fixture operation succeeds")
        .expect("fixture operation succeeds")
    }
    fn status(&self, index: usize) -> String {
        self.store
            .read()
            .expect("fixture operation succeeds")
            .query_row(
                "SELECT status FROM runtime_events WHERE id=?1",
                [format!("annotation:input-{index}")],
                |row| row.get(0),
            )
            .expect("fixture operation succeeds")
    }
    fn stop(&mut self, index: usize) {
        let mut a = self.record(index);
        a.epoch += 1;
        a.status = AgentStatus::Paused;
        a.extra.insert("autoWake".into(), json!(false));
        a.extra.insert("error".into(), json!("Stopped by user"));
        self.store
            .transaction(|tx| AgentRepository::put(tx, &a))
            .expect("fixture operation succeeds");
        self.worker
            .input(AccountInput::Stop(a.id.0, a.epoch))
            .expect("fixture operation succeeds");
    }
    fn shutdown(&mut self) {
        if self.worker.session.is_none() {
            return;
        }
        if self
            .worker
            .input(AccountInput::Shutdown)
            .expect("fixture operation succeeds")
        {
            return;
        }
        loop {
            let input = self
                .input
                .recv_timeout(WATCHDOG)
                .expect("shutdown barrier arrives");
            if self
                .worker
                .input(input)
                .expect("fixture operation succeeds")
            {
                break;
            }
        }
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(session) = self.worker.session.take() {
            let _ignored = session.close();
        }
    }
}

#[test]
fn unrelated_started_after_submission_never_takes_the_input() {
    let mut h = Harness::new(1);
    let request = h.start(0);
    h.notice(0, "turn/started", "unrelated", "inProgress");
    assert_eq!(h.status(0), "dispatching");
    assert!(h.record(0).extra["startAttempt"]["turnId"].is_null());
    h.accepted(&request, "ours");
    assert_eq!(h.status(0), "delivered");
    assert_eq!(h.record(0).extra["turnId"], "ours");
}
#[test]
fn python_completed_ledger_rejects_old_notices_without_eviction() {
    let mut h = Harness::new(1);
    let request = h.start(0);
    let agent = h.agents[0].0.clone();
    h.store
        .transaction(|tx| {
            for i in 0..260 {
                tx.execute(
                    "INSERT INTO runtime_completed_turns VALUES(?1)",
                    [format!("{agent}:old-{i}")],
                )?;
            }
            Ok(())
        })
        .expect("fixture operation succeeds");
    h.notice(0, "turn/completed", "old-0", "completed");
    h.notice(0, "turn/started", "old-0", "inProgress");
    assert_eq!(h.status(0), "dispatching");
    assert!(h.worker.deferred.is_empty());
    h.accepted(&request, "ours");
    h.notice(0, "turn/completed", "ours", "completed");
    let count: i64 = h
        .store
        .read()
        .expect("fixture operation succeeds")
        .query_row("SELECT COUNT(*) FROM runtime_completed_turns", [], |r| {
            r.get(0)
        })
        .expect("fixture operation succeeds");
    assert_eq!(count, 261);
    assert_eq!(h.record(0).extra["lastAnswer"], "");
    assert_eq!(
        h.record(0).extra["lastCompletedTurnError"]["message"],
        HISTORY_UNAVAILABLE
    );
}
#[test]
fn completion_before_correlated_reply_retains_cursor_then_commits() {
    let mut h = Harness::new(1);
    let request = h.start(0);
    let before = h.fake.acknowledged.load(Ordering::Relaxed);
    h.notice(0, "turn/completed", "ours", "completed");
    assert_eq!(h.status(0), "dispatching");
    assert_eq!(h.fake.acknowledged.load(Ordering::Relaxed), before);
    h.accepted(&request, "ours");
    assert_eq!(h.status(0), "delivered");
    assert_eq!(h.record(0).status, AgentStatus::Failed);
    assert_eq!(h.fake.acknowledged.load(Ordering::Relaxed), h.cursor);
}
#[test]
fn stopped_unknown_start_is_interrupted_by_activity_without_binding_input() {
    let mut h = Harness::new(1);
    let request = h.start(0);
    h.stop(0);
    let id: RequestId =
        serde_json::from_value(request["id"].clone()).expect("fixture operation succeeds");
    h.worker.settlements.push_back(Settlement {
        event: ProviderSessionEvent::UnknownOutcome {
            ticket: RequestTicket {
                id,
                method: Method::TurnStart,
                generation: 1,
            },
        },
        cursor: None,
        failures: 0,
    });
    h.worker
        .drain_settlements()
        .expect("fixture operation succeeds");
    h.notice(0, "turn/started", "ours", "inProgress");
    let interrupt = h.request("turn/interrupt");
    assert_eq!(interrupt["params"]["turnId"], "ours");
    assert_eq!(h.record(0).status, AgentStatus::Paused);
    assert_eq!(h.status(0), "uncertain");
    h.accepted(&request, "ours");
    assert_eq!(h.record(0).status, AgentStatus::Paused);
    assert_eq!(h.status(0), "delivered");
    h.notice(0, "turn/completed", "ours", "interrupted");
    assert_eq!(h.record(0).extra["inFlight"], false);
    assert_eq!(h.record(0).extra["error"], "Stopped by user");
}
#[test]
fn interrupt_rejection_is_a_visible_account_hold() {
    let mut h = Harness::new(1);
    let request = h.start(0);
    h.accepted(&request, "ours");
    h.stop(0);
    let interrupt = h.request("turn/interrupt");
    h.frame(json!({"id":interrupt["id"],"error":{"message":"no interrupt"}}));
    assert!(h.worker.held);
    assert_eq!(h.record(0).extra["nativeFailureHold"], true);
    let error:String=h.store.read().expect("fixture operation succeeds").query_row("SELECT json_extract(payload,'$.value.error') FROM sync_entities WHERE collection='agent' AND id=?1",[&h.agents[0].0],|row|row.get(0)).expect("fixture operation succeeds");
    assert_eq!(error, SETTLEMENT_HELD);
}
#[test]
fn shutdown_drains_buffered_completion_before_disconnect_receipt() {
    let mut h = Harness::new(1);
    let request = h.start(0);
    h.accepted(&request, "ours");
    h.cursor += 1;
    h.fake.frame(json!({"method":"turn/completed","params":{"threadId":"fake-thread-0","turn":{"id":"ours","status":"completed"}}}),h.cursor);
    // Shutdown is deliberately consumed before the reader's buffered completion.
    h.shutdown();
    let a = h.record(0);
    assert_eq!(a.status, AgentStatus::Failed);
    assert_eq!(a.extra["lastCompletedTurn"], "ours");
    assert!(!a.extra.contains_key("disconnectRecovery"));
    assert_eq!(h.fake.acknowledged.load(Ordering::Relaxed), h.cursor);
}
#[test]
fn disconnect_preserves_receipt_and_new_generation_never_leaves_running() {
    let mut h = Harness::new(1);
    let request = h.start(0);
    h.accepted(&request, "ours");
    h.shutdown();
    let a = h.record(0);
    assert_eq!(a.status, AgentStatus::Interrupted);
    assert_eq!(a.extra["turnId"], "ours");
    assert_eq!(a.extra["disconnectRecovery"]["turnId"], "ours");
    assert_eq!(a.extra["restartRecovery"]["stage"], "pending");
    assert!(!a.extra.contains_key("lastCompletedTurn"));
    recover_on_startup(&h.store).expect("fixture operation succeeds");
    h.fake.generation.store(2, Ordering::Relaxed);
    h.worker.closing = None;
    h.worker.quiesced = false;
    h.worker.connect().expect("fixture operation succeeds");
    h.initialize();
    assert_eq!(h.record(0).status, AgentStatus::Interrupted);
    assert!(h.worker.pending.is_empty());
    assert_eq!(h.record(0).extra["inFlight"], false);
}
#[test]
fn not_sent_restores_pending_events_and_keeps_account_usable() {
    let mut h = Harness::new(2);
    let request = h.start(0);
    let ticket = RequestTicket {
        id: serde_json::from_value(request["id"].clone()).expect("fixture operation succeeds"),
        method: Method::TurnStart,
        generation: 1,
    };
    h.worker.settlements.push_back(Settlement {
        event: ProviderSessionEvent::NotSent { ticket },
        cursor: None,
        failures: 0,
    });
    h.worker
        .drain_settlements()
        .expect("fixture operation succeeds");
    assert_eq!(h.status(0), "pending");
    assert!(!h.record(0).extra.contains_key("startAttempt"));
    let second = h.start(1);
    h.accepted(&second, "other");
    assert_eq!(h.record(1).status, AgentStatus::Running);
}
#[test]
fn same_attempt_and_epoch_for_two_agents_produce_distinct_tickets() {
    let mut h = Harness::new(2);
    let a = h.start(0);
    let b = h.start(1);
    assert_ne!(a["id"], b["id"]);
    let mut intents = h
        .worker
        .pending
        .values()
        .map(|p| p.intent.clone())
        .collect::<Vec<_>>();
    for i in &mut intents {
        i.attempt_id = "same-clock-tick".into();
        i.epoch = 1;
    }
    assert_ne!(
        request_id(&intents[0], Method::TurnStart),
        request_id(&intents[1], Method::TurnStart)
    );
    assert_ne!(
        request_id(&intents[0], Method::ThreadResume),
        request_id(&intents[1], Method::ThreadResume)
    );
}
#[test]
fn repeated_settlement_failure_holds_cursor_until_restart_commits() {
    let mut h = Harness::new(1);
    let request = h.start(0);
    h.accepted(&request, "ours");
    h.store.transaction(|tx|{tx.execute_batch("CREATE TRIGGER fail_completion BEFORE UPDATE ON runtime_agents WHEN json_extract(NEW.record,'$.lastCompletedTurn')='ours' BEGIN SELECT RAISE(FAIL,'injected completion failure'); END;")?;Ok(())}).expect("fixture operation succeeds");
    let ack = h.fake.acknowledged.load(Ordering::Relaxed);
    h.notice(0, "turn/completed", "ours", "completed");
    for _ in 1..SETTLEMENT_RETRIES {
        h.worker.retry_at = Some(Instant::now());
        h.worker
            .input(AccountInput::Deadline)
            .expect("fixture operation succeeds");
    }
    assert!(h.worker.held);
    assert!(h.worker.retry_at.is_none());
    assert_eq!(h.fake.acknowledged.load(Ordering::Relaxed), ack);
    assert_eq!(h.record(0).extra["startOutcomeHold"]["cursor"], h.cursor);
    assert_eq!(h.record(0).extra["error"], SETTLEMENT_HELD);
    h.store
        .transaction(|tx| {
            tx.execute_batch("DROP TRIGGER fail_completion")?;
            Ok(())
        })
        .expect("fixture operation succeeds");
    h.worker
        .input(AccountInput::Restart)
        .expect("fixture operation succeeds");
    assert!(!h.worker.held);
    assert_eq!(h.record(0).status, AgentStatus::Failed);
    assert_eq!(h.fake.acknowledged.load(Ordering::Relaxed), h.cursor);
}

#[test]
fn late_rejection_after_unknown_fails_once_and_does_not_requeue() {
    let mut h = Harness::new(1);
    let request = h.start(0);
    let ticket = RequestTicket {
        id: serde_json::from_value(request["id"].clone()).expect("fixture operation succeeds"),
        method: Method::TurnStart,
        generation: 1,
    };
    h.worker.settlements.push_back(Settlement {
        event: ProviderSessionEvent::UnknownOutcome { ticket },
        cursor: None,
        failures: 0,
    });
    h.worker
        .drain_settlements()
        .expect("fixture operation succeeds");
    h.frame(json!({"id":request["id"],"error":{"message":"provider refused input"}}));
    assert_eq!(h.status(0), "failed");
    assert_eq!(h.record(0).status, AgentStatus::Failed);
    assert!(
        SchedulerRepository::pending_agents(&h.store.read().expect("fixture operation succeeds"))
            .expect("fixture operation succeeds")
            .is_empty()
    );
}

#[test]
fn oversized_request_is_refused_before_submitted_marker() {
    let mut h = Harness::new(1);
    let (agent, epoch) = h.agents[0].clone();
    write_annotation(&h.store, &agent, "input-0");
    let mut intent = claim_intent(&h.store, &agent, epoch, "oversized", 1.0)
        .expect("fixture operation succeeds")
        .expect("fixture operation succeeds");
    intent.events[0].text = "x".repeat(studio_runtime_providers::MAX_FRAME_BYTES + 1);
    h.worker.submit(intent).expect("fixture operation succeeds");
    assert_eq!(h.status(0), "pending");
    assert!(!h.record(0).extra.contains_key("startAttempt"));
    assert_eq!(h.record(0).extra["nativeFailureHold"], true);
    assert!(h.worker.pending.is_empty());
}

#[test]
fn turn_permissions_match_python_including_vm_reviewers_and_lead_roots() {
    let h = Harness::new(1);
    let (agent, epoch) = h.agents[0].clone();
    write_annotation(&h.store, &agent, "input-0");
    let intent = claim_intent(&h.store, &agent, epoch, "permissions", 1.0)
        .expect("fixture operation succeeds")
        .expect("fixture operation succeeds");
    let mut inputs = Vec::new();
    let mut actual = Vec::new();
    let local_progress = h.store.root().join("progress").join(&agent);
    fs::create_dir_all(&local_progress).expect("fixture operation succeeds");
    let vm_home = h.store.root().join("vm-home");
    let vm_progress = vm_home.join(".studio/progress").join(&agent);
    fs::create_dir_all(&vm_progress).expect("fixture operation succeeds");
    for mode in ["local", "vm"] {
        for role in ["implementer", "reviewer"] {
            for yolo in [json!(true), json!(false), Value::Null] {
                let mut record = h.record(0);
                record.extra.insert("executionMode".into(), json!(mode));
                record.extra.insert("role".into(), json!(role));
                record.extra.insert("yoloMode".into(), yolo);
                record.extra.insert("layrReady".into(), json!(true));
                record.extra.insert("isLead".into(), json!(true));
                record.extra.insert("layrHome".into(), json!(vm_home));
                h.store
                    .transaction(|tx| AgentRepository::put(tx, &record))
                    .expect("fixture operation succeeds");
                let params =
                    provider_turn_params(&h.store, &intent).expect("fixture operation succeeds");
                let policy = params
                    .as_object()
                    .expect("fixture operation succeeds")
                    .iter()
                    .filter(|(k, _)| matches!(k.as_str(), "approvalPolicy" | "sandboxPolicy"))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<serde_json::Map<_, _>>();
                actual.push(Value::Object(policy));
                let mut input = serde_json::to_value(&record).expect("fixture operation succeeds");
                input["progressDirectory"] = json!(if mode == "vm" {
                    vm_progress.clone()
                } else {
                    local_progress.clone()
                });
                inputs.push(input);
            }
        }
    }
    let code = "import json,pathlib,sys,types; sys.path.insert(0,sys.argv[1]); from codex_runtime import Runtime; rows=json.loads(sys.argv[2]); print(json.dumps([Runtime.turn_permissions(types.SimpleNamespace(progress_file=lambda a:pathlib.Path(a['progressDirectory'])/'PROGRESS.md'),a) for a in rows]))";
    let output = Command::new(python_executable())
        .arg("-c")
        .arg(code)
        .arg(server_source())
        .arg(serde_json::to_string(&inputs).expect("fixture operation succeeds"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("fixture operation succeeds");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: Vec<Value> =
        serde_json::from_slice(&output.stdout).expect("fixture operation succeeds");
    assert_eq!(actual, expected);
}

#[test]
fn runtime_wake_enqueues_account_claim_and_shutdown_drains_real_channel() {
    let mut h = Harness::new(1);
    h.shutdown();
    write_annotation(&h.store, &h.agents[0].0, "input-0");
    let attached = h.fake.attaches.load(Ordering::Relaxed);
    let mut runtime = TurnRuntime::start(h.store.clone(), Arc::new(SharedFake(h.fake.clone())))
        .expect("fixture operation succeeds");
    let initialize = h.request("initialize");
    h.cursor += 1;
    h.fake
        .frame(json!({"id":initialize["id"],"result":{}}), h.cursor);
    let resume = h.request("thread/resume");
    h.cursor += 1;
    h.fake.frame(
        json!({"id":resume["id"],"result":{"thread":{"id":"fake-thread-0"}}}),
        h.cursor,
    );
    let start = h.request("turn/start");
    h.cursor += 1;
    h.fake.frame(
        json!({"id":start["id"],"result":{"turn":{"id":"runtime-turn"}}}),
        h.cursor,
    );
    h.cursor += 1;
    h.fake.frame(json!({"method":"turn/completed","params":{"threadId":"fake-thread-0","turn":{"id":"runtime-turn","status":"completed"}}}),h.cursor);
    runtime.shutdown().expect("fixture operation succeeds");
    assert_eq!(h.record(0).status, AgentStatus::Failed);
    assert_eq!(h.status(0), "delivered");
    assert_eq!(h.fake.attaches.load(Ordering::Relaxed), attached + 1);
    assert_eq!(h.fake.acknowledged.load(Ordering::Relaxed), h.cursor);
}

#[test]
fn reattach_recovers_original_ticket_and_settles_replay_before_initialize() {
    let mut h = Harness::new(1);
    let start = h.start(0);
    let generation = h
        .worker
        .session
        .as_ref()
        .expect("fixture operation succeeds")
        .generation();
    h.worker
        .begin_close(Closing::Reconnect)
        .expect("fixture operation succeeds");
    loop {
        let input = h
            .input
            .recv_timeout(WATCHDOG)
            .expect("fixture operation succeeds");
        let barrier = matches!(
            input,
            AccountInput::Transport(ProviderSessionInput::Quiesced)
        );
        h.worker.input(input).expect("fixture operation succeeds");
        if barrier {
            break;
        }
    }
    let initialize = h.request("initialize");
    assert_eq!(
        h.worker
            .session
            .as_ref()
            .expect("fixture operation succeeds")
            .generation(),
        generation
    );
    h.accepted(&start, "replayed");
    assert_eq!(h.status(0), "delivered");
    h.frame(json!({"id":initialize["id"],"result":{}}));
    assert_eq!(h.fake.acknowledged.load(Ordering::Relaxed), h.cursor);
    assert!(h.worker.pending.is_empty());
    h.notice(0, "turn/completed", "replayed", "completed");
    assert_eq!(h.record(0).extra["lastCompletedTurn"], "replayed");
}

#[test]
fn held_acceptance_is_retried_on_stop_without_replaying_turn_start() {
    let mut h = Harness::new(1);
    let start = h.start(0);
    h.store.transaction(|tx|{tx.execute_batch("CREATE TRIGGER fail_accept BEFORE UPDATE OF status ON runtime_events WHEN NEW.status='delivered' BEGIN SELECT RAISE(FAIL,'injected acceptance failure'); END;")?;Ok(())}).expect("fixture operation succeeds");
    let ack = h.fake.acknowledged.load(Ordering::Relaxed);
    h.accepted(&start, "stopped");
    for _ in 1..SETTLEMENT_RETRIES {
        h.worker.retry_at = Some(Instant::now());
        h.worker
            .input(AccountInput::Deadline)
            .expect("fixture operation succeeds");
    }
    assert!(h.worker.held);
    assert_eq!(h.fake.acknowledged.load(Ordering::Relaxed), ack);
    h.store
        .transaction(|tx| {
            tx.execute_batch("DROP TRIGGER fail_accept")?;
            Ok(())
        })
        .expect("fixture operation succeeds");
    h.stop(0);
    assert!(!h.worker.held);
    assert_eq!(h.status(0), "delivered");
    assert_eq!(h.record(0).status, AgentStatus::Paused);
    assert_eq!(h.request("turn/interrupt")["params"]["turnId"], "stopped");
    assert_eq!(h.fake.acknowledged.load(Ordering::Relaxed), h.cursor);
}

#[test]
fn interrupt_transport_send_failure_holds_account_without_losing_stopped_input() {
    let mut h = Harness::new(1);
    let start = h.start(0);
    let ticket = RequestTicket {
        id: serde_json::from_value(start["id"].clone()).expect("fixture operation succeeds"),
        method: Method::TurnStart,
        generation: 1,
    };
    h.worker.settlements.push_back(Settlement {
        event: ProviderSessionEvent::UnknownOutcome { ticket },
        cursor: None,
        failures: 0,
    });
    h.worker
        .drain_settlements()
        .expect("fixture operation succeeds");
    h.stop(0);
    h.fake.fail_write.store(true, Ordering::Relaxed);
    h.notice(0, "turn/started", "lost-reply-turn", "inProgress");
    while !h.worker.held {
        let input = h
            .input
            .recv_timeout(WATCHDOG)
            .expect("interrupt write result arrives");
        h.worker.input(input).expect("fixture operation succeeds");
    }
    assert_eq!(h.record(0).status, AgentStatus::Paused);
    assert_eq!(h.record(0).extra["error"], SETTLEMENT_HELD);
    assert_eq!(h.status(0), "uncertain");
    assert!(h.record(0).extra["startAttempt"]["turnId"].is_null());
    assert!(
        h.worker.pending.contains_key(
            &serde_json::from_value::<RequestId>(start["id"].clone())
                .expect("fixture operation succeeds")
        )
    );
}

#[test]
fn rejected_initialize_holds_account_until_explicit_restart() {
    let mut h = Harness::connecting(1, false);
    let request = h.request("initialize");
    h.frame(json!({"id":request["id"],"error":{"code":-32000,"message":"fixture rejection"}}));
    while h.worker.session.is_some() {
        let input = h
            .input
            .recv_timeout(WATCHDOG)
            .expect("quiescence barrier arrives");
        h.worker
            .input(input)
            .expect("failed initialization settles");
    }
    assert!(h.worker.held);
    assert_eq!(h.fake.attaches.load(Ordering::Relaxed), 1);
    assert_eq!(h.record(0).extra["error"], SETTLEMENT_HELD);
    h.worker
        .input(AccountInput::Restart)
        .expect("explicit restart reopens admission");
    assert_eq!(h.fake.attaches.load(Ordering::Relaxed), 2);
    h.initialize();
}

#[test]
fn activity_before_stop_interrupts_the_observed_unbound_turn() {
    let mut h = Harness::new(1);
    let _start = h.start(0);
    h.notice(0, "turn/started", "observed-before-stop", "inProgress");
    assert_eq!(
        h.record(0).extra["startAttempt"]["observedTurnId"],
        "observed-before-stop"
    );
    h.stop(0);
    assert_eq!(
        h.request("turn/interrupt")["params"]["turnId"],
        "observed-before-stop"
    );
    assert!(h.record(0).extra["turnId"].is_null());
    assert_eq!(h.status(0), "dispatching");
}

#[test]
fn unrelated_completion_releases_cursor_after_definite_rejection() {
    let mut h = Harness::new(1);
    let start = h.start(0);
    h.notice(0, "turn/completed", "unrelated", "completed");
    h.frame(json!({"id":start["id"],"error":{"message":"definite rejection"}}));
    assert_eq!(h.status(0), "failed");
    assert!(h.worker.deferred.is_empty());
    assert_eq!(h.fake.acknowledged.load(Ordering::Relaxed), h.cursor);
    h.worker
        .input(AccountInput::Restart)
        .expect("regression fixture");
    assert!(h.worker.deferred.is_empty());
    assert_eq!(h.fake.acknowledged.load(Ordering::Relaxed), h.cursor);
}

#[test]
fn rejected_interrupt_allows_explicit_stop_retry() {
    let mut h = Harness::new(1);
    let start = h.start(0);
    h.accepted(&start, "running");
    h.stop(0);
    let interrupt = h.request("turn/interrupt");
    h.frame(json!({"id":interrupt["id"],"error":{"message":"temporary rejection"}}));
    assert!(h.worker.held);
    h.stop(0);
    assert!(!h.worker.held);
    assert_eq!(h.request("turn/interrupt")["params"]["turnId"], "running");
    assert_eq!(h.worker.interrupts.len(), 1);
    assert_eq!(h.record(0).extra["inFlight"], true);
}

#[test]
fn python_unknown_submission_gets_a_durable_recovery_hold() {
    let mut h = Harness::new(1);
    let _start = h.start(0);
    let mut a = h.record(0);
    let attempt = a
        .extra
        .get_mut("startAttempt")
        .expect("regression fixture")
        .as_object_mut()
        .expect("regression fixture");
    attempt.remove("connectionGeneration");
    attempt.remove("requestTicket");
    attempt.insert("supervisorIdentity".into(), json!({"generation":1}));
    attempt.insert("executionOutcome".into(), json!("unknown"));
    a.status = AgentStatus::Waiting;
    h.store
        .transaction(|tx| AgentRepository::put(tx, &a))
        .expect("regression fixture");
    recover_on_startup(&h.store).expect("regression fixture");
    let accounts = SchedulerRepository::accounts_with_unknown_submitted_turns(
        &h.store.read().expect("regression fixture"),
    )
    .expect("regression fixture");
    assert!(accounts.is_empty());
    assert_eq!(h.record(0).extra["inFlight"], false);
    assert_eq!(h.record(0).extra["restartRecovery"]["stage"], "held");
    assert_eq!(h.record(0).extra["nativeFailureHold"], true);
    assert_eq!(
        h.record(0).extra["disconnectRecovery"]["startAttempt"]["supervisorIdentity"]["generation"],
        1
    );
    assert!(
        h.record(0).extra["error"]
            .as_str()
            .expect("recovery error")
            .contains("history reconciliation")
    );
}

#[test]
fn turn_lifecycle_writes_execution_history_with_fallback_error() {
    let mut h = Harness::new(1);
    let before: i64 = h
        .store
        .read()
        .expect("regression fixture")
        .query_row(
            "SELECT COUNT(*) FROM runtime_execution_attempts",
            [],
            |row| row.get(0),
        )
        .expect("regression fixture");
    let start = h.start(0);
    h.accepted(&start, "ours");
    h.notice(0, "turn/completed", "ours", "failed");
    let after: i64 = h
        .store
        .read()
        .expect("regression fixture")
        .query_row(
            "SELECT COUNT(*) FROM runtime_execution_attempts",
            [],
            |row| row.get(0),
        )
        .expect("regression fixture");
    assert_eq!(after, before + 1);
    assert_eq!(h.record(0).extra["lastCompletedTurnStatus"], "failed");
    assert_eq!(
        h.record(0).extra["lastCompletedTurnError"]["message"],
        "Codex ended this turn with an error."
    );
}

#[test]
fn interrupted_turn_schedules_already_pending_input() {
    let mut h = Harness::new(1);
    let start = h.start(0);
    h.accepted(&start, "ours");
    write_annotation(&h.store, &h.agents[0].0, "second-input");
    h.notice(0, "turn/completed", "ours", "interrupted");
    let a = h.record(0);
    assert_eq!(a.status, AgentStatus::Queued);
    assert_eq!(a.extra["autoWake"], true);
    assert_ne!(a.extra.get("nativeFailureHold"), Some(&json!(true)));
    let pending: i64 = h
        .store
        .read()
        .expect("regression fixture")
        .query_row(
            "SELECT COUNT(*) FROM runtime_events WHERE agent=?1 AND status='pending'",
            [&a.id.0],
            |r| r.get(0),
        )
        .expect("regression fixture");
    assert_eq!(pending, 1);
    assert_eq!(
        SchedulerRepository::pending_agents(&h.store.read().expect("regression fixture"))
            .expect("regression fixture")
            .len(),
        1
    );
}

#[test]
fn rejected_start_preserves_previous_completion() {
    let mut h = Harness::new(1);
    let mut a = h.record(0);
    a.extra
        .insert("lastCompletedTurn".into(), json!("previous-turn"));
    a.extra
        .insert("lastCompletedTurnStatus".into(), json!("completed"));
    a.extra.insert("lastCompletedTurnError".into(), Value::Null);
    h.store
        .transaction(|tx| AgentRepository::put(tx, &a))
        .expect("regression fixture");
    let start = h.start(0);
    h.frame(json!({"id":start["id"],"error":{"message":"new-input-rejected"}}));
    let a = h.record(0);
    assert_eq!(a.extra["lastCompletedTurn"], "previous-turn");
    assert_eq!(a.extra["lastCompletedTurnStatus"], "completed");
    assert!(a.extra["lastCompletedTurnError"].is_null());
    assert_eq!(a.extra["startAttempt"]["executionOutcome"], "rejected");
}
#[test]
fn held_account_backpressures_the_producer_and_still_accepts_shutdown() {
    let mut h = Harness::new(1);
    h.shutdown();
    let mut runtime = TurnRuntime::start(h.store.clone(), Arc::new(SharedFake(h.fake.clone())))
        .expect("runtime starts");
    let sender = runtime.sessions.sender("default").expect("account mailbox");
    let initialize = h.request("initialize");
    h.cursor += 1;
    h.fake
        .frame(json!({"id":initialize["id"],"result":{}}), h.cursor);
    sender.send(AccountInput::Hold).expect("hold command");
    let (reply, receive) = mpsc::channel();
    sender
        .send(AccountInput::Snapshot(reply))
        .expect("state barrier");
    let (before, held) = receive.recv_timeout(WATCHDOG).expect("actor held barrier");
    assert!(held);
    for n in 0..mailbox::CAPACITY + 1 {
        h.cursor += 1;
        h.fake.frame(
            json!({"method":"item/started","params":{"item":{"id":format!("item-{n}")}}}),
            h.cursor,
        );
    }
    assert_eq!(sender.backpressure_barrier(), mailbox::CAPACITY);
    let (reply, receive) = mpsc::channel();
    sender
        .send(AccountInput::Snapshot(reply))
        .expect("control remains admitted");
    assert_eq!(
        receive.recv_timeout(WATCHDOG).expect("held queue snapshot"),
        (before, true)
    );
    let acknowledged = h.fake.acknowledged.load(Ordering::Relaxed);
    runtime
        .shutdown()
        .expect("full mailbox shutdown releases blocked producer");
    assert_eq!(h.fake.acknowledged.load(Ordering::Relaxed), acknowledged);
}

#[test]
fn uncertain_interrupt_receipt_stays_held_on_explicit_stop() {
    let mut h = Harness::new(1);
    let start = h.start(0);
    h.accepted(&start, "running");
    // Zero creates an already-expired request; Deadline is delivered explicitly.
    h.worker.timeout_ms = 0;
    h.stop(0);
    h.request("turn/interrupt");
    h.worker
        .input(AccountInput::Deadline)
        .expect("interrupt deadline");
    assert!(h.worker.held);
    h.stop(0);
    assert!(h.worker.held);
    assert_eq!(h.worker.interrupts.len(), 1);
    assert!(matches!(
        h.requests.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
}

#[test]
fn stale_completion_releases_cursor_after_not_sent() {
    let mut h = Harness::new(1);
    let start = h.start(0);
    h.notice(0, "turn/completed", "unrelated", "completed");
    let ticket = RequestTicket {
        id: serde_json::from_value(start["id"].clone()).expect("ticket"),
        method: Method::TurnStart,
        generation: 1,
    };
    h.worker.settlements.push_back(Settlement {
        event: ProviderSessionEvent::NotSent { ticket },
        cursor: None,
        failures: 0,
    });
    h.worker
        .input(AccountInput::Restart)
        .expect("reclassify deferred frame");
    assert!(h.worker.deferred.is_empty());
    assert_eq!(h.fake.acknowledged.load(Ordering::Relaxed), h.cursor);
    assert_eq!(h.status(0), "pending");
}

#[test]
fn execution_history_matches_python_across_submission_acceptance_and_completion() {
    let mut h = Harness::new(1);
    let start = h.start(0);
    let submitted = serde_json::to_value(h.record(0)).expect("agent snapshot");
    h.accepted(&start, "execution-turn");
    let accepted = serde_json::to_value(h.record(0)).expect("agent snapshot");
    h.notice(0, "turn/completed", "execution-turn", "completed");
    let completed = serde_json::to_value(h.record(0)).expect("agent snapshot");
    let attempt = completed["startAttempt"]["id"].as_str().expect("attempt");
    let connection = h.store.read().expect("history read");
    let raw: String = connection
        .query_row(
            "SELECT record FROM runtime_execution_attempts WHERE id=?1",
            [attempt],
            |r| r.get(0),
        )
        .expect("attempt history");
    let mut actual_attempt: Value = serde_json::from_str(&raw).expect("attempt JSON");
    actual_attempt
        .as_object_mut()
        .expect("attempt object")
        .remove("inputIdentity");
    actual_attempt
        .as_object_mut()
        .expect("attempt object")
        .remove("claimResult");
    let run = actual_attempt["runId"].as_str().expect("run id");
    let raw: String = connection
        .query_row(
            "SELECT record FROM runtime_execution_runs WHERE id=?1",
            [run],
            |r| r.get(0),
        )
        .expect("run history");
    let mut actual_run: Value = serde_json::from_str(&raw).expect("run JSON");
    actual_run
        .as_object_mut()
        .expect("run object")
        .remove("finished");
    let events = connection
        .prepare("SELECT event FROM runtime_execution_inputs WHERE run=?1 ORDER BY event")
        .expect("input query")
        .query_map([run], |r| r.get::<_, String>(0))
        .expect("inputs")
        .collect::<Result<Vec<_>, _>>()
        .expect("input rows");
    let script = r#"
import json,sqlite3,sys
sys.path.insert(0,sys.argv[1])
import codex_execution as execution
submitted,accepted,completed=json.load(sys.stdin)
db=sqlite3.connect(':memory:')
execution.ensure_tables(db)
db.execute('CREATE TABLE runtime_items(id TEXT,agent TEXT,created REAL,record TEXT)')
execution.reconcile_agent(db,submitted,None)
execution.reconcile_agent(db,accepted,submitted)
execution.observe_native(db,accepted,'turn/completed',{'threadId':accepted['threadId'],'turn':{'id':'execution-turn','status':'completed'}})
execution.reconcile_agent(db,completed,accepted)
a=json.loads(db.execute('SELECT record FROM runtime_execution_attempts WHERE id=?',(completed['startAttempt']['id'],)).fetchone()[0])
r=json.loads(db.execute('SELECT record FROM runtime_execution_runs WHERE id=?',(a['runId'],)).fetchone()[0])
r.pop('finished',None)
print(json.dumps({'attempt':a,'run':r,'events':[row[0] for row in db.execute('SELECT event FROM runtime_execution_inputs WHERE run=? ORDER BY event',(r['id'],))]}))
"#;
    let mut child = Command::new(python_executable())
        .args(["-c", script])
        .arg(server_source())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("Python oracle");
    child
        .stdin
        .take()
        .expect("oracle input")
        .write_all(
            json!([submitted, accepted, completed])
                .to_string()
                .as_bytes(),
        )
        .expect("snapshots send");
    let output = child.wait_with_output().expect("oracle exits");
    assert!(output.status.success());
    let expected: Value = serde_json::from_slice(&output.stdout).expect("oracle JSON");
    assert_eq!(
        json!({"attempt":actual_attempt,"run":actual_run,"events":events}),
        expected
    );
    assert_eq!(expected["run"]["status"], "completed");
    assert_eq!(h.record(0).status, AgentStatus::Failed); // named transcript refusal remains explicit
}

#[test]
fn same_generation_restart_restores_authority_and_finishes_receipt() {
    let mut h = Harness::new(1);
    let start = h.start(0);
    h.accepted(&start, "recovering-turn");
    h.store
        .transaction(|tx| {
            SchedulerRepository::disconnect_account(tx, "default", 1, 1.0, true)?;
            SchedulerRepository::reconcile_generation(tx, "default", 1, 2.0)
        })
        .expect("same supervisor generation reconciles");
    let agent = h.record(0);
    assert_eq!(agent.status, AgentStatus::Running);
    assert_eq!(agent.extra["autoWake"], true);
    assert_eq!(agent.extra["restartRecovery"]["stage"], "reattached");
    let before = agent.extra["events"].as_u64().expect("counter");
    h.notice(0, "turn/started", "recovering-turn", "running");
    let agent = h.record(0);
    assert_eq!(agent.extra["events"], before + 1);
    assert_eq!(agent.extra["activity"]["phase"], "thinking");
    assert!(agent.extra["activity"]["at"].is_number());
    assert!(
        agent.extra["lastEvent"]
            .as_str()
            .expect("UTC stamp")
            .ends_with('Z')
    );
    h.notice(0, "turn/completed", "recovering-turn", "interrupted");
    let agent = h.record(0);
    assert_eq!(agent.extra["events"], before + 2);
    assert_eq!(agent.extra["restartRecovery"]["stage"], "finished");
    assert_eq!(agent.extra["restartRecovery"]["outcome"], "interrupted");
    assert!(agent.extra["restartRecovery"]["reconciledAt"].is_number());
}

#[test]
fn unresolved_completion_queue_has_a_fixed_settlement_limit() {
    let mut h = Harness::new(1);
    h.start(0);
    for n in 0..MAX_SETTLEMENTS - MAX_OUTSTANDING * 2 {
        h.notice(0, "turn/completed", &format!("unbound-{n}"), "completed");
    }
    assert!(h.worker.held);
    assert!(h.worker.settlements.len() + h.worker.deferred.len() <= MAX_SETTLEMENTS);
    assert!(h.fake.acknowledged.load(Ordering::Relaxed) < h.cursor);
    assert_eq!(h.status(0), "dispatching");
}
