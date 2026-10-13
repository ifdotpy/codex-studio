#![allow(clippy::unwrap_used)]

use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Condvar, Mutex};

use serde_json::{Value, json};
use studio_runtime_process::{ProviderProcess, ProviderProcessReader};
use studio_runtime_providers::{Method, RequestId, RequestTicket, ResponseBody};
use studio_runtime_services::provider_session::{
    ProviderSession, ProviderSessionConfig, ProviderSessionEvent,
};

struct FakeReader(ChildStdout);

impl Read for FakeReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.0.read(bytes)
    }
}

impl ProviderProcessReader for FakeReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.0.read(bytes)
    }

    fn take_cursors(&mut self) -> Vec<u64> {
        Vec::new()
    }
}

struct ScriptChunk {
    bytes: Vec<u8>,
    cursor: Option<u64>,
}

struct ScriptReader {
    receiver: Receiver<ScriptChunk>,
    current: Option<ScriptChunk>,
    offset: usize,
    completed: Vec<u64>,
}

impl Read for ScriptReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.current.is_none() {
            self.current = self.receiver.recv().ok();
            self.offset = 0;
        }
        let Some(chunk) = self.current.as_ref() else {
            return Ok(0);
        };
        let count = (chunk.bytes.len() - self.offset).min(output.len());
        output[..count].copy_from_slice(&chunk.bytes[self.offset..self.offset + count]);
        self.offset += count;
        if self.offset == chunk.bytes.len() {
            if let Some(cursor) = chunk.cursor {
                self.completed.push(cursor);
            }
            self.current = None;
        }
        Ok(count)
    }
}

impl ProviderProcessReader for ScriptReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        Read::read(self, output)
    }

    fn take_cursors(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.completed)
    }
}

struct ScriptWriter {
    buffer: Vec<u8>,
    sender: Option<SyncSender<ScriptChunk>>,
    script: Vec<(Value, Option<u64>)>,
    request_ids: Arc<Mutex<Vec<RequestId>>>,
    progress_sender: Option<Sender<usize>>,
    sent_chunks: usize,
    sent: bool,
}

impl Write for ScriptWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let message: Value = serde_json::from_slice(&self.buffer[..end])
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            self.buffer.drain(..=end);
            if let Some(id) = message.get("id")
                && let Ok(id) = serde_json::from_value(id.clone())
            {
                self.request_ids
                    .lock()
                    .map_err(|_| io::Error::other("request id log poisoned"))?
                    .push(id);
            }
            if !self.sent && message.get("method").and_then(Value::as_str) == Some("initialize") {
                let initialize_id = message.get("id").cloned().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "initialize request has no id")
                })?;
                for (mut frame, cursor) in self.script.clone() {
                    if frame.get("id") == Some(&Value::String("__initialize__".into())) {
                        frame["id"] = initialize_id.clone();
                    }
                    let mut bytes = serde_json::to_vec(&frame)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                    bytes.push(b'\n');
                    self.sender
                        .as_ref()
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::BrokenPipe, "script writer closed")
                        })?
                        .send(ScriptChunk { bytes, cursor })
                        .map_err(|_| {
                            io::Error::new(io::ErrorKind::BrokenPipe, "script reader closed")
                        })?;
                    self.sent_chunks += 1;
                    if let Some(progress) = &self.progress_sender {
                        let _ignored = progress.send(self.sent_chunks);
                    }
                }
                self.sender.take();
                self.sent = true;
            }
        }
        Ok(())
    }
}

struct ScriptProcess {
    acknowledged: Arc<AtomicU64>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
    fail_acknowledge: bool,
    generation: u64,
    script: Vec<(Value, Option<u64>)>,
    resumed: bool,
    request_ids: Arc<Mutex<Vec<RequestId>>>,
    output_capacity: usize,
    progress_sender: Option<Sender<usize>>,
}

impl ProviderProcess for ScriptProcess {
    fn split(&mut self) -> io::Result<(Box<dyn ProviderProcessReader>, Box<dyn Write + Send>)> {
        let (sender, receiver) = mpsc::sync_channel(self.output_capacity);
        Ok((
            Box::new(ScriptReader {
                receiver,
                current: None,
                offset: 0,
                completed: Vec::new(),
            }),
            Box::new(ScriptWriter {
                buffer: Vec::new(),
                sender: Some(sender),
                script: self.script.clone(),
                request_ids: Arc::clone(&self.request_ids),
                progress_sender: self.progress_sender.clone(),
                sent_chunks: 0,
                sent: false,
            }),
        ))
    }

    fn acknowledged_cursor(&self) -> u64 {
        self.acknowledged.load(Ordering::Acquire)
    }

    fn generation(&self) -> u64 {
        self.generation
    }

    fn resumed(&self) -> bool {
        self.resumed
    }

    fn acknowledge(&mut self, cursor: u64) -> io::Result<()> {
        if self.fail_acknowledge {
            return Err(io::Error::other("deliberate acknowledgement failure"));
        }
        self.acknowledged.store(cursor, Ordering::Release);
        Ok(())
    }

    fn stop(&mut self) -> io::Result<()> {
        self.stopped.store(true, Ordering::Release);
        Ok(())
    }
}

fn scripted_process(
    acknowledged: Arc<AtomicU64>,
    script: Vec<(Value, Option<u64>)>,
) -> ScriptProcess {
    scripted_process_with_ids(acknowledged, script).0
}

fn scripted_process_with_ids(
    acknowledged: Arc<AtomicU64>,
    script: Vec<(Value, Option<u64>)>,
) -> (ScriptProcess, Arc<Mutex<Vec<RequestId>>>) {
    let request_ids = Arc::new(Mutex::new(Vec::new()));
    (
        ScriptProcess {
            acknowledged,
            stopped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            fail_acknowledge: false,
            generation: 2,
            script,
            resumed: false,
            request_ids: Arc::clone(&request_ids),
            output_capacity: 256,
            progress_sender: None,
        },
        request_ids,
    )
}

fn basic_cursor_script(init_cursor: Option<u64>) -> Vec<(Value, Option<u64>)> {
    vec![
        (
            json!({"id":"__initialize__","result":{"ok":true}}),
            init_cursor,
        ),
        (json!({"method":"test/duringInitialize"}), Some(11)),
    ]
}

struct FakeProcess {
    child: Child,
    stdout: Option<ChildStdout>,
    stdin: Option<ChildStdin>,
}

impl FakeProcess {
    fn new(mode: &str) -> Self {
        let fake = format!(
            "{}/tests/fixtures/fake_provider.py",
            env!("CARGO_MANIFEST_DIR")
        );
        let mut command = Command::new("python3");
        let mut child = command
            .arg(fake)
            .env("FAKE_PROVIDER_MODE", mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("fake provider starts");
        Self {
            stdout: child.stdout.take(),
            stdin: child.stdin.take(),
            child,
        }
    }
}

impl ProviderProcess for FakeProcess {
    fn split(&mut self) -> io::Result<(Box<dyn ProviderProcessReader>, Box<dyn Write + Send>)> {
        let reader = self
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("fake stdout already taken"))?;
        let writer = self
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("fake stdin already taken"))?;
        Ok((Box::new(FakeReader(reader)), Box::new(writer)))
    }

    fn stop(&mut self) -> io::Result<()> {
        let _ignored = self.child.kill();
        self.child.wait().map(|_| ())
    }

    fn acknowledged_cursor(&self) -> u64 {
        0
    }

    fn generation(&self) -> u64 {
        0
    }

    fn acknowledge(&mut self, _cursor: u64) -> io::Result<()> {
        Ok(())
    }
}

fn connect(mode: &str) -> ProviderSession {
    let mut config = ProviderSessionConfig::new();
    config.request_timeout_ms = 10_000;
    ProviderSession::connect(Box::new(FakeProcess::new(mode)), config)
        .expect("fake provider should initialize")
}

fn receive_result(
    session: &mut ProviderSession,
) -> Result<
    (ProviderSessionEvent, Option<u64>),
    studio_runtime_services::provider_session::ProviderSessionError,
> {
    loop {
        let event = session.receive_event()?;
        if !matches!(event.0, ProviderSessionEvent::WriteCompleted { .. }) {
            return Ok(event);
        }
    }
}

#[test]
fn fake_provider_initializes_exchanges_request_and_routes_notifications() {
    let mut session = connect("normal");
    let ticket = session
        .send_request(Method::ModelList, json!({"limit": 1}), None, false, 10_000)
        .expect("request should be submitted");
    let mut got_reply = false;
    let mut got_notification = false;
    while !got_reply || !got_notification {
        match receive_result(&mut session).expect("provider event").0 {
            ProviderSessionEvent::Reply {
                ticket: received,
                body: ResponseBody::Result(result),
                late: false,
            } => {
                assert_eq!(received, ticket);
                assert_eq!(result, json!({"models": []}));
                got_reply = true;
            }
            ProviderSessionEvent::Inbound(studio_runtime_providers::Inbound::Notification(_)) => {
                got_notification = true;
            }
            ProviderSessionEvent::OrphanReply { .. } => {}
            other => panic!("unexpected provider event: {other:?}"),
        }
    }
    session.close().expect("provider shutdown");
}

#[test]
fn lost_reply_is_unknown_and_is_not_retried() {
    let mut session = connect("exit");
    let ticket = session
        .send_request(Method::ModelList, json!({}), None, false, 10_000)
        .expect("request should be submitted once");
    assert_eq!(
        receive_result(&mut session).expect("unknown reply event").0,
        ProviderSessionEvent::UnknownOutcome { ticket }
    );
    session.close().expect("provider shutdown");
}

#[test]
fn reply_after_deadline_is_reported_as_late_evidence() {
    let mut session = connect("late");
    let ticket = session
        .send_request(Method::ModelList, json!({}), None, false, 10)
        .expect("request should be submitted once");
    assert_eq!(
        receive_result(&mut session)
            .expect("unknown event at deadline")
            .0,
        ProviderSessionEvent::UnknownOutcome {
            ticket: ticket.clone()
        }
    );
    assert_eq!(
        receive_result(&mut session).expect("late evidence").0,
        ProviderSessionEvent::Reply {
            ticket,
            body: ResponseBody::Result(json!({"late": true})),
            late: true
        }
    );
    session.close().expect("provider shutdown");
}

#[test]
fn consumer_acknowledges_cursor_only_after_durable_handling() {
    let acknowledged = Arc::new(AtomicU64::new(9));
    let mut session = ProviderSession::connect(
        Box::new(scripted_process(
            Arc::clone(&acknowledged),
            basic_cursor_script(Some(10)),
        )),
        ProviderSessionConfig::new(),
    )
    .expect("cursor provider should initialize");
    assert_eq!(session.generation(), 2);
    assert_eq!(acknowledged.load(Ordering::Acquire), 10);

    let (event, cursor) = session.receive_event().expect("deferred notification");
    assert!(matches!(event, ProviderSessionEvent::Inbound(_)));
    assert_eq!(cursor, Some(11));
    assert_eq!(
        acknowledged.load(Ordering::Acquire),
        10,
        "dispatch alone does not acknowledge the consumer event"
    );
    session.acknowledge_handled(11).unwrap();
    assert_eq!(acknowledged.load(Ordering::Acquire), 11);
    session.close().unwrap();
}

#[test]
fn initialization_acknowledgement_failure_stops_the_process() {
    let acknowledged = Arc::new(AtomicU64::new(0));
    let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut process = scripted_process(
        Arc::clone(&acknowledged),
        vec![(json!({"id":"__initialize__","result":{}}), Some(1))],
    );
    process.stopped = Arc::clone(&stopped);
    process.fail_acknowledge = true;
    let result = ProviderSession::connect(Box::new(process), ProviderSessionConfig::new());
    assert!(
        result.is_err(),
        "failed initialization ACK must fail connect"
    );
    assert!(
        stopped.load(Ordering::Acquire),
        "failed connect must stop I/O"
    );
}

#[test]
fn initialization_acknowledgement_failure_emits_one_terminal_event() {
    use studio_runtime_services::provider_session::{
        ProviderSessionInput, ProviderSessionInputSink,
    };

    let acknowledged = Arc::new(AtomicU64::new(0));
    let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut process = scripted_process(
        Arc::clone(&acknowledged),
        vec![(json!({"id":"__initialize__","result":{}}), Some(1))],
    );
    process.stopped = Arc::clone(&stopped);
    process.fail_acknowledge = true;
    let (sender, receiver) = mpsc::channel();
    let sink: ProviderSessionInputSink = Arc::new(move |input| {
        let _ignored = sender.send(input);
    });
    let mut session = ProviderSession::begin_connect_recovering(
        Box::new(process),
        ProviderSessionConfig::new(),
        Vec::new(),
        sink,
    )
    .unwrap();
    let input = loop {
        let input = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        if matches!(input, ProviderSessionInput::Frame { .. }) {
            break input;
        }
        let _events = session.handle_input(input).unwrap();
    };
    let events = session.handle_input(input).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|(event, _)| matches!(event, ProviderSessionEvent::Closed { .. }))
            .count(),
        1
    );
    session.quiesce().unwrap();
    assert!(stopped.load(Ordering::Acquire));
}

#[test]
fn unhandled_frame_is_redelivered_after_reader_session_restarts() {
    let acknowledged = Arc::new(AtomicU64::new(9));
    let first = ProviderSession::connect(
        Box::new(scripted_process(
            Arc::clone(&acknowledged),
            basic_cursor_script(Some(10)),
        )),
        ProviderSessionConfig::new(),
    )
    .expect("first provider should initialize");
    assert_eq!(acknowledged.load(Ordering::Acquire), 10);
    first.close().unwrap();
    assert_eq!(
        acknowledged.load(Ordering::Acquire),
        10,
        "closing before consumer handling must leave notification unacknowledged"
    );

    let mut reattached = ProviderSession::connect(
        Box::new(scripted_process(
            Arc::clone(&acknowledged),
            basic_cursor_script(None),
        )),
        ProviderSessionConfig::new(),
    )
    .expect("reattached provider should use cached initialization");
    let (event, cursor) = reattached.receive_event().unwrap();
    assert!(matches!(event, ProviderSessionEvent::Inbound(_)));
    assert_eq!(cursor, Some(11));
    reattached.acknowledge_handled(11).unwrap();
    assert_eq!(acknowledged.load(Ordering::Acquire), 11);
    reattached.close().unwrap();
}

#[test]
fn duplicate_reply_and_already_acknowledged_cursor_are_dropped() {
    let acknowledged = Arc::new(AtomicU64::new(9));
    let mut session = ProviderSession::connect(
        Box::new(scripted_process(
            Arc::clone(&acknowledged),
            vec![
                (json!({"id":"__initialize__","result":{}}), Some(10)),
                (json!({"method":"test/once"}), Some(11)),
                (json!({"method":"test/once"}), Some(11)),
                (json!({"id":"__initialize__","result":{}}), Some(12)),
            ],
        )),
        ProviderSessionConfig::new(),
    )
    .expect("duplicate frame provider initializes");
    let (event, cursor) = session.receive_event().expect("first notification");
    assert!(matches!(event, ProviderSessionEvent::Inbound(_)));
    assert_eq!(cursor, Some(11));
    session.acknowledge_handled(11).unwrap();
    assert!(
        matches!(
            session.receive_event(),
            Ok((ProviderSessionEvent::Closed { generation: 2 }, None))
        ),
        "duplicate frames must not reach the consumer"
    );
    assert!(matches!(
        session.receive_event(),
        Err(studio_runtime_services::provider_session::ProviderSessionError::Closed)
    ));
    assert_eq!(acknowledged.load(Ordering::Acquire), 12);
    session.close().unwrap();
}

#[test]
fn recovered_reply_waits_for_the_contiguous_handshake_ack_prefix() {
    let acknowledged = Arc::new(AtomicU64::new(0));
    let recovered_id = RequestId::String("durable-ticket".into());
    let process = scripted_process(
        Arc::clone(&acknowledged),
        vec![
            (
                json!({"id":"durable-ticket","result":{"resumed":true}}),
                Some(1),
            ),
            (json!({"id":"__initialize__","result":{}}), Some(2)),
        ],
    );
    let mut session = ProviderSession::connect_recovering(
        Box::new(process),
        ProviderSessionConfig::new(),
        vec![(recovered_id.clone(), Method::ModelList, None)],
    )
    .expect("initialize after replayed recovered reply");
    assert_eq!(acknowledged.load(Ordering::Acquire), 0);
    assert!(matches!(
        receive_result(&mut session).unwrap(),
        (
            ProviderSessionEvent::Reply {
                ticket: RequestTicket {
                    id,
                    method: Method::ModelList,
                    ..
                },
                body: ResponseBody::Result(_),
                late: false,
            },
            Some(1)
        ) if id == recovered_id
    ));
    session.acknowledge_handled(1).unwrap();
    assert_eq!(acknowledged.load(Ordering::Acquire), 2);
    session.close().unwrap();
}

#[test]
fn orphan_reply_is_visible_and_unacknowledged_until_consumer_settles_it() {
    let acknowledged = Arc::new(AtomicU64::new(0));
    let process = scripted_process(
        Arc::clone(&acknowledged),
        vec![
            (json!({"id":777,"result":{"orphan":true}}), Some(1)),
            (json!({"id":"__initialize__","result":{}}), Some(2)),
        ],
    );
    let mut session = ProviderSession::connect(Box::new(process), ProviderSessionConfig::new())
        .expect("initialize after replayed orphan");
    let (event, cursor) = receive_result(&mut session).unwrap();
    assert!(matches!(event, ProviderSessionEvent::OrphanReply { .. }));
    assert_eq!(cursor, Some(1));
    assert_eq!(acknowledged.load(Ordering::Acquire), 0);
    session.acknowledge_handled(1).unwrap();
    assert_eq!(acknowledged.load(Ordering::Acquire), 2);
    session.close().unwrap();
}

#[test]
fn reattach_namespaces_request_ids_so_old_same_generation_initialize_replies_are_orphans() {
    let acknowledged = Arc::new(AtomicU64::new(0));
    let (first_process, first_ids) = scripted_process_with_ids(
        Arc::clone(&acknowledged),
        vec![(json!({"id":"__initialize__","result":{}}), None)],
    );
    let first =
        ProviderSession::connect(Box::new(first_process), ProviderSessionConfig::new()).unwrap();
    first.close().unwrap();
    let first_initialize_id = first_ids.lock().unwrap()[0].clone();
    assert_eq!(first_initialize_id, RequestId::Number(1));

    let (mut resumed_process, resumed_ids) = scripted_process_with_ids(
        Arc::clone(&acknowledged),
        vec![
            (json!({"id":1,"result":{"old":true}}), Some(1)),
            (json!({"id":"__initialize__","result":{}}), Some(2)),
        ],
    );
    resumed_process.resumed = true;
    let mut resumed =
        ProviderSession::connect(Box::new(resumed_process), ProviderSessionConfig::new()).unwrap();
    let resumed_initialize_id = resumed_ids.lock().unwrap()[0].clone();
    assert_ne!(resumed_initialize_id, first_initialize_id);
    let (event, cursor) = receive_result(&mut resumed).unwrap();
    assert!(matches!(event, ProviderSessionEvent::OrphanReply { .. }));
    assert_eq!(cursor, Some(1));
    assert_eq!(acknowledged.load(Ordering::Acquire), 0);
    resumed.acknowledge_handled(1).unwrap();
    assert_eq!(acknowledged.load(Ordering::Acquire), 2);
    resumed.close().unwrap();
}

#[test]
fn startup_drains_replay_when_both_bounded_queues_are_full() {
    use studio_runtime_services::provider_session::{
        ProviderSessionInput, ProviderSessionInputSink,
    };

    let acknowledged = Arc::new(AtomicU64::new(0));
    let (progress_sender, progress_receiver) = mpsc::channel();
    let (attempt_sender, attempt_receiver) = mpsc::channel();
    let (input_sender, input_receiver) = mpsc::sync_channel(1);
    let mut script = vec![(json!({"id":"__initialize__","result":{}}), None)];
    script.extend((1..=100).map(|cursor| {
        (
            json!({"method":"test/replayed","params":{"cursor":cursor}}),
            Some(cursor),
        )
    }));
    let (mut process, _) = scripted_process_with_ids(Arc::clone(&acknowledged), script);
    process.output_capacity = 64;
    process.resumed = true;
    process.progress_sender = Some(progress_sender);
    let sink: ProviderSessionInputSink = Arc::new(move |input: ProviderSessionInput| {
        let _ignored = attempt_sender.send(());
        let _ignored = input_sender.send(input);
    });
    let mut session = ProviderSession::begin_connect_recovering(
        Box::new(process),
        ProviderSessionConfig::new(),
        Vec::new(),
        sink,
    )
    .unwrap();

    for _ in 0..2 {
        attempt_receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("reader should fill the bounded account input channel");
    }
    let mut output_chunks = 0;
    while output_chunks < 66 {
        output_chunks = progress_receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("provider output queue should fill behind the blocked account channel");
    }
    assert!(!session.is_ready());

    let initialize = input_receiver
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("cached initialize reply remains available ahead of the replay backlog");
    session.handle_input(initialize).unwrap();
    assert!(session.is_ready());

    let mut replayed = 0;
    while replayed < 100 {
        let input = input_receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("startup must keep draining while replay backlog is full");
        for (event, cursor) in session.handle_input(input).unwrap() {
            if matches!(event, ProviderSessionEvent::Inbound(_)) {
                replayed += 1;
                session
                    .acknowledge_handled(cursor.expect("replayed frame cursor"))
                    .unwrap();
            }
        }
    }
    assert_eq!(replayed, 100);
    assert_eq!(acknowledged.load(Ordering::Acquire), 100);
    loop {
        let input = input_receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("reader should finish after draining the replay backlog");
        let ended = matches!(input, ProviderSessionInput::End);
        let _events = session.handle_input(input).unwrap();
        if ended {
            break;
        }
    }
    session.quiesce().unwrap();
}

struct BlockGate {
    state: Mutex<(bool, bool)>,
    wake: Condvar,
}

struct QueuePressureReader {
    receiver: Receiver<ScriptChunk>,
    current: Option<ScriptChunk>,
    offset: usize,
    completed: Vec<u64>,
}

impl Read for QueuePressureReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.current.is_none() {
            self.current = self.receiver.recv().ok();
            self.offset = 0;
        }
        let Some(chunk) = self.current.as_ref() else {
            return Ok(0);
        };
        let count = (chunk.bytes.len() - self.offset).min(output.len());
        output[..count].copy_from_slice(&chunk.bytes[self.offset..self.offset + count]);
        self.offset += count;
        if self.offset == chunk.bytes.len() {
            if let Some(cursor) = chunk.cursor {
                self.completed.push(cursor);
            }
            self.current = None;
        }
        Ok(count)
    }
}

impl ProviderProcessReader for QueuePressureReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        Read::read(self, output)
    }

    fn take_cursors(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.completed)
    }
}

struct QueuePressureWriter {
    sender: Option<Sender<ScriptChunk>>,
    gate: Arc<BlockGate>,
    buffer: Vec<u8>,
    initialized: bool,
}

impl Write for QueuePressureWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let message: Value = serde_json::from_slice(&self.buffer[..end])
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            self.buffer.drain(..=end);
            let method = message.get("method").and_then(Value::as_str);
            if method == Some("initialize") {
                let mut bytes = serde_json::to_vec(&json!({"id": message["id"], "result": {}}))
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                bytes.push(b'\n');
                self.sender
                    .as_ref()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "writer closed"))?
                    .send(ScriptChunk {
                        bytes,
                        cursor: None,
                    })
                    .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "reader closed"))?;
                self.initialized = true;
            } else if self.initialized && method != Some("initialized") {
                let mut state = self
                    .gate
                    .state
                    .lock()
                    .map_err(|_| io::Error::other("write gate poisoned"))?;
                state.0 = true;
                self.gate.wake.notify_all();
                while !state.1 {
                    state = self
                        .gate
                        .wake
                        .wait(state)
                        .map_err(|_| io::Error::other("write gate poisoned"))?;
                }
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "cancelled"));
            }
        }
        Ok(())
    }
}

struct QueuePressureProcess(Arc<BlockGate>);

impl ProviderProcess for QueuePressureProcess {
    fn split(&mut self) -> io::Result<(Box<dyn ProviderProcessReader>, Box<dyn Write + Send>)> {
        let (sender, receiver) = mpsc::channel();
        Ok((
            Box::new(QueuePressureReader {
                receiver,
                current: None,
                offset: 0,
                completed: Vec::new(),
            }),
            Box::new(QueuePressureWriter {
                sender: Some(sender),
                gate: Arc::clone(&self.0),
                buffer: Vec::new(),
                initialized: false,
            }),
        ))
    }

    fn acknowledged_cursor(&self) -> u64 {
        0
    }

    fn generation(&self) -> u64 {
        4
    }

    fn acknowledge(&mut self, _cursor: u64) -> io::Result<()> {
        Ok(())
    }

    fn stop(&mut self) -> io::Result<()> {
        Err(io::Error::other("deliberate stop failure"))
    }

    fn cancel(&mut self) -> io::Result<()> {
        let mut state = self
            .0
            .state
            .lock()
            .map_err(|_| io::Error::other("write gate poisoned"))?;
        state.1 = true;
        self.0.wake.notify_all();
        Ok(())
    }
}

#[test]
fn queue_saturation_reports_not_sent_unknowns_and_one_terminal_before_quiesce_barrier() {
    use studio_runtime_services::provider_session::{ProviderSessionError, ProviderSessionInput};

    let gate = Arc::new(BlockGate {
        state: Mutex::new((false, false)),
        wake: Condvar::new(),
    });
    let (input_sender, input_receiver) = mpsc::channel();
    let sink = Arc::new(move |input| {
        let _ignored = input_sender.send(input);
    });
    let mut session = ProviderSession::begin_connect_recovering(
        Box::new(QueuePressureProcess(Arc::clone(&gate))),
        ProviderSessionConfig::new(),
        Vec::new(),
        sink,
    )
    .unwrap();
    while !session.is_ready() {
        let input = input_receiver.recv().unwrap();
        let _events = session.handle_input(input).unwrap();
    }

    let mut accepted = Vec::new();
    let first = session
        .send_request(Method::ModelList, json!({}), None, false, 10_000)
        .unwrap();
    {
        let mut state = gate.state.lock().unwrap();
        while !state.0 {
            state = gate.wake.wait(state).unwrap();
        }
    }
    accepted.push(first);
    let not_sent = loop {
        match session.send_request(Method::ModelList, json!({}), None, false, 10_000) {
            Ok(ticket) => accepted.push(ticket),
            Err(ProviderSessionError::NotSent { ticket }) => break ticket,
            Err(error) => panic!("unexpected queue failure: {error}"),
        }
    };
    assert_eq!(not_sent.method, Method::ModelList);
    let events = session.take_events();
    assert_eq!(
        events
            .iter()
            .filter(|(event, _)| matches!(event, ProviderSessionEvent::Closed { .. }))
            .count(),
        1
    );
    let unknown = events
        .iter()
        .filter_map(|(event, _)| match event {
            ProviderSessionEvent::UnknownOutcome { ticket } => Some(ticket),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(unknown.len(), accepted.len());
    assert!(accepted.iter().all(|ticket| unknown.contains(&ticket)));
    assert!(session.take_events().is_empty());

    assert!(matches!(
        session.quiesce(),
        Err(ProviderSessionError::Process(_))
    ));
    let mut saw_barrier = false;
    while !saw_barrier {
        saw_barrier = matches!(
            input_receiver.recv().unwrap(),
            ProviderSessionInput::Quiesced
        );
    }
}
