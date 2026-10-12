use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;

use serde_json::json;
use studio_runtime_providers::{ConnectionDriver, DriverConfig, Handler, Inbound, Method};

struct ChunkReader {
    chunks: mpsc::Receiver<Option<Vec<u8>>>,
    current: Vec<u8>,
    closed: bool,
}

impl Read for ChunkReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        while self.current.is_empty() && !self.closed {
            match self.chunks.recv() {
                Ok(Some(chunk)) => self.current = chunk,
                Ok(None) | Err(_) => self.closed = true,
            }
        }
        if self.current.is_empty() {
            return Ok(0);
        }
        let count = output.len().min(self.current.len());
        output[..count].copy_from_slice(&self.current[..count]);
        self.current.drain(..count);
        Ok(count)
    }
}

struct AcknowledgedReader {
    chunks: mpsc::Receiver<Option<Vec<u8>>>,
    read_ack: mpsc::Sender<()>,
}

struct ObservedReader {
    chunks: mpsc::Receiver<Option<Vec<u8>>>,
    read_started: mpsc::Sender<()>,
}

impl Read for ObservedReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.read_started
            .send(())
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        match self.chunks.recv() {
            Ok(Some(chunk)) => {
                let count = output.len().min(chunk.len());
                output[..count].copy_from_slice(&chunk[..count]);
                Ok(count)
            }
            Ok(None) | Err(_) => Ok(0),
        }
    }
}
impl Read for AcknowledgedReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        match self.chunks.recv() {
            Ok(Some(chunk)) => {
                let count = output.len().min(chunk.len());
                output[..count].copy_from_slice(&chunk[..count]);
                self.read_ack
                    .send(())
                    .map_err(|_| io::ErrorKind::BrokenPipe)?;
                Ok(count)
            }
            Ok(None) | Err(_) => {
                self.read_ack
                    .send(())
                    .map_err(|_| io::ErrorKind::BrokenPipe)?;
                Ok(0)
            }
        }
    }
}

#[derive(Default)]
struct Sink;
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct FailedWriter;
impl Write for FailedWriter {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "scripted failure",
        ))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct ControlledWriter(Arc<AtomicBool>);
impl Write for ControlledWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.load(Ordering::SeqCst) {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "scripted failure",
            ))
        } else {
            Ok(bytes.len())
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct ManualClock(AtomicU64);
impl ManualClock {
    fn set(&self, now_ms: u64) {
        self.0.store(now_ms, Ordering::SeqCst);
    }
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
struct Noop;
impl Handler for Noop {
    type Error = ();
    fn handle(&mut self, _: Inbound) -> Result<(), Self::Error> {
        Ok(())
    }
}

struct FailingHandler {
    dropped: mpsc::Sender<()>,
}

impl Handler for FailingHandler {
    type Error = ();

    fn handle(&mut self, _: Inbound) -> Result<(), Self::Error> {
        Err(())
    }
}

impl Drop for FailingHandler {
    fn drop(&mut self) {
        assert!(self.dropped.send(()).is_ok());
    }
}

struct BlockingHandler {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
    delivered: mpsc::Sender<String>,
}
impl Handler for BlockingHandler {
    type Error = ();
    fn handle(&mut self, message: Inbound) -> Result<(), Self::Error> {
        let method = match message {
            Inbound::Request(request) => request.method.as_str().to_owned(),
            Inbound::Notification(studio_runtime_providers::Frame::Notification(note)) => {
                note.method.as_str().to_owned()
            }
            Inbound::Notification(studio_runtime_providers::Frame::UnknownMethod(unknown)) => {
                unknown.method
            }
            _ => String::from("other"),
        };
        self.delivered.send(method).map_err(|_| ())?;
        self.entered.send(()).map_err(|_| ())?;
        self.release.recv().map_err(|_| ())
    }
}

fn json_line(value: serde_json::Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&value).expect("scripted test operation");
    bytes.push(b'\n');
    bytes
}

#[test]
fn blocked_callback_does_not_block_reader_and_dispatch_stays_fifo_through_close() {
    let (chunks_tx, chunks_rx) = mpsc::channel();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (delivered_tx, delivered_rx) = mpsc::channel();
    let reader = ChunkReader {
        chunks: chunks_rx,
        current: Vec::new(),
        closed: false,
    };
    let handler = BlockingHandler {
        entered: entered_tx,
        release: release_rx,
        delivered: delivered_tx,
    };
    let mut driver = ConnectionDriver::start(
        reader,
        Sink,
        handler,
        DriverConfig { generation: 3, claude: false, now_ms: 0, timeout_ms: 10_000, initialize_params: json!({"clientInfo":{"name":"codex_agents_canvas","version":"1.0.0"},"capabilities":{"experimentalApi":true}}) },
    ).expect("scripted test operation");

    chunks_tx
        .send(Some(json_line(json!({"id":1,"result":{}}))))
        .expect("scripted test operation");
    driver.pump_next(0).expect("scripted test operation");
    chunks_tx
        .send(Some(json_line(
            json!({"method":"turn/started","params":{"threadId":"a"}}),
        )))
        .expect("scripted test operation");
    driver.pump_next(0).expect("scripted test operation");
    entered_rx.recv().expect("scripted test operation");
    assert_eq!(
        delivered_rx.recv().expect("scripted test operation"),
        "turn/started"
    );

    chunks_tx
        .send(Some(json_line(
            json!({"method":"turn/completed","params":{"threadId":"a"}}),
        )))
        .expect("scripted test operation");
    let dispatched = driver.pump_next(0).expect("scripted test operation");
    assert!(dispatched.is_empty());
    chunks_tx.send(None).expect("scripted test operation");
    driver.pump_next(0).expect("scripted test operation");

    release_tx.send(()).expect("scripted test operation");
    assert_eq!(
        delivered_rx.recv().expect("scripted test operation"),
        "turn/completed"
    );
    entered_rx.recv().expect("scripted test operation");
    let (closing_tx, closing_rx) = mpsc::channel();
    let closer = std::thread::spawn(move || {
        closing_tx.send(()).expect("scripted test operation");
        driver.close().expect("scripted test operation");
    });
    closing_rx.recv().expect("scripted test operation");
    release_tx.send(()).expect("scripted test operation");
    closer.join().expect("scripted test operation");
    assert_eq!(Method::TurnStarted.as_str(), "turn/started");
}

#[test]
fn eof_mid_frame_marks_waiters_unknown() {
    let (chunks_tx, chunks_rx) = mpsc::channel();
    let reader = ChunkReader {
        chunks: chunks_rx,
        current: Vec::new(),
        closed: false,
    };
    let mut driver = ConnectionDriver::start(
        reader, Sink, Noop, DriverConfig { generation: 4, claude: false, now_ms: 0, timeout_ms: 10_000, initialize_params: json!({"clientInfo":{"name":"codex_agents_canvas","version":"1.0.0"},"capabilities":{"experimentalApi":true}}) },
    ).expect("scripted test operation");
    chunks_tx
        .send(Some(b"{\"id\":1,\"result\":".to_vec()))
        .expect("scripted test operation");
    chunks_tx.send(None).expect("scripted test operation");
    let actions = driver.pump_next(0).expect("scripted test operation");
    assert!(actions.iter().any(|action| matches!(
        action,
        studio_runtime_providers::Action::Resolve {
            outcome: studio_runtime_providers::RequestOutcome::Unknown,
            ..
        }
    )));
    driver.close().expect("scripted test operation");
}

struct MidFrameFailure(bool);
impl Read for MidFrameFailure {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if !self.0 {
            self.0 = true;
            let partial = b"{\"id\":1,\"result\":";
            output[..partial.len()].copy_from_slice(partial);
            Ok(partial.len())
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "scripted failure",
            ))
        }
    }
}

#[test]
fn transport_error_mid_frame_marks_waiters_unknown() {
    let mut driver = ConnectionDriver::start(
        MidFrameFailure(false),
        Sink,
        Noop,
        DriverConfig {
            generation: 5,
            claude: false,
            now_ms: 0,
            timeout_ms: 10_000,
            initialize_params: json!({"clientInfo":{"name":"codex_agents_canvas","version":"1.0.0"},"capabilities":{"experimentalApi":true}}),
        },
    )
    .expect("scripted test operation");
    let actions = driver.pump_next(0).expect("scripted test operation");
    assert!(actions.iter().any(|action| matches!(
        action,
        studio_runtime_providers::Action::Resolve {
            outcome: studio_runtime_providers::RequestOutcome::Unknown,
            ..
        }
    )));
    driver.close().expect("scripted test operation");
}

#[test]
fn write_failure_from_submit_returns_unknown_request_identity_and_loses_session() {
    let (chunks_tx, chunks_rx) = mpsc::channel();
    let fail_write = Arc::new(AtomicBool::new(false));
    let mut driver = ConnectionDriver::start(
        ChunkReader { chunks: chunks_rx, current: Vec::new(), closed: false },
        ControlledWriter(fail_write.clone()),
        Noop,
        DriverConfig {
            generation: 7,
            claude: false,
            now_ms: 0,
            timeout_ms: 100,
            initialize_params: json!({"clientInfo":{"name":"codex_agents_canvas","version":"1.0.0"},"capabilities":{"experimentalApi":true}}),
        },
    ).expect("scripted test operation");
    chunks_tx
        .send(Some(json_line(json!({"id":1,"result":{}}))))
        .expect("scripted test operation");
    driver.pump_next(0).expect("scripted test operation");
    fail_write.store(true, Ordering::SeqCst);
    let error = driver
        .submit(Method::ModelList, json!({}), None, false, 1, 50)
        .expect_err("scripted writer failure");
    match error {
        studio_runtime_providers::DriverError::Io { unresolved, .. } => assert!(
            unresolved.iter().any(|action| matches!(
                action,
                studio_runtime_providers::Action::Resolve {
                    ticket: studio_runtime_providers::RequestTicket {
                        id: studio_runtime_providers::RequestId::Number(2),
                        ..
                    },
                    outcome: studio_runtime_providers::RequestOutcome::Unknown,
                }
            )),
            "write error must expose request ID and unknown outcome: {unresolved:?}"
        ),
        other => panic!("expected I/O error carrying settlement actions, got {other:?}"),
    }
    assert!(matches!(
        driver.submit(Method::AccountRead, json!({}), None, false, 2, 50),
        Err(studio_runtime_providers::DriverError::Session(
            studio_runtime_providers::SessionError::NotReady
        ))
    ));
    chunks_tx.send(None).expect("scripted test operation");
    driver.close().expect("scripted test operation");
}

#[test]
fn pump_one_returns_deadline_unknown_actions_when_queue_is_empty() {
    let (chunks_tx, chunks_rx) = mpsc::channel();
    let mut driver = ConnectionDriver::start(
        ChunkReader {
            chunks: chunks_rx,
            current: Vec::new(),
            closed: false,
        },
        Sink,
        Noop,
        DriverConfig {
            generation: 8,
            claude: false,
            now_ms: 0,
            timeout_ms: 100,
            initialize_params: json!({}),
        },
    )
    .expect("scripted test operation");
    chunks_tx
        .send(Some(json_line(json!({"id":1,"result":{}}))))
        .expect("scripted test operation");
    driver.pump_next(0).expect("scripted test operation");
    let (ticket, _) = driver
        .submit(Method::ModelList, json!({}), None, false, 1, 5)
        .expect("scripted test operation");
    let error = driver.pump_one(6).expect_err("scripted deadline");
    let detail = format!("{error:?}");
    assert!(
        detail.contains(&format!("{:?}", ticket.id)) && detail.contains("Unknown"),
        "{detail}"
    );
    chunks_tx.send(None).expect("scripted test operation");
    driver.close().expect("scripted test operation");
}

#[test]
fn pump_next_preserves_deadline_actions_after_receiving_a_frame() {
    let (chunks_tx, chunks_rx) = mpsc::channel();
    let mut driver = ConnectionDriver::start(
        ChunkReader {
            chunks: chunks_rx,
            current: Vec::new(),
            closed: false,
        },
        Sink,
        Noop,
        DriverConfig {
            generation: 9,
            claude: false,
            now_ms: 0,
            timeout_ms: 100,
            initialize_params: json!({}),
        },
    )
    .expect("scripted test operation");
    chunks_tx
        .send(Some(json_line(json!({"id":1,"result":{}}))))
        .expect("scripted test operation");
    driver.pump_next(0).expect("scripted test operation");
    let (ticket, _) = driver
        .submit(Method::ModelList, json!({}), None, false, 1, 5)
        .expect("scripted test operation");
    chunks_tx
        .send(Some(json_line(
            json!({"method":"turn/started","params":{}}),
        )))
        .expect("scripted test operation");
    let error = driver.pump_next(6).expect_err("scripted deadline");
    let detail = format!("{error:?}");
    assert!(
        detail.contains(&format!("{:?}", ticket.id)) && detail.contains("Unknown"),
        "{detail}"
    );
    chunks_tx.send(None).expect("scripted test operation");
    driver.close().expect("scripted test operation");
}

#[test]
fn pump_next_with_advanced_injected_clock_expires_without_blocking() {
    let (chunks_tx, chunks_rx) = mpsc::channel();
    let mut driver = ConnectionDriver::start(
        ChunkReader {
            chunks: chunks_rx,
            current: Vec::new(),
            closed: false,
        },
        Sink,
        Noop,
        DriverConfig {
            generation: 10,
            claude: false,
            now_ms: 0,
            timeout_ms: 100,
            initialize_params: json!({}),
        },
    )
    .expect("scripted test operation");
    chunks_tx
        .send(Some(json_line(json!({"id":1,"result":{}}))))
        .expect("scripted test operation");
    driver.pump_next(0).expect("scripted test operation");
    let (ticket, _) = driver
        .submit(Method::ModelList, json!({}), None, false, 1, 5)
        .expect("scripted test operation");
    let clock = ManualClock::default();
    clock.set(6);
    let error = driver
        .pump_next_with_clock(|| clock.now_ms())
        .expect_err("advanced scripted clock expires pending request");
    let detail = format!("{error:?}");
    assert!(
        detail.contains(&format!("{:?}", ticket.id)) && detail.contains("Unknown"),
        "{detail}"
    );
    chunks_tx.send(None).expect("scripted test operation");
    driver.close().expect("scripted test operation");
}

#[test]
fn pump_one_expires_before_queued_response_at_deadline_but_accepts_response_before_it() {
    for (now_ms, expected_late) in [(4, false), (6, true)] {
        let (chunks_tx, chunks_rx) = mpsc::channel();
        let (read_ack_tx, read_ack_rx) = mpsc::channel();
        let mut driver = ConnectionDriver::start(
            AcknowledgedReader {
                chunks: chunks_rx,
                read_ack: read_ack_tx,
            },
            Sink,
            Noop,
            DriverConfig {
                generation: 11,
                claude: false,
                now_ms: 0,
                timeout_ms: 100,
                initialize_params: json!({}),
            },
        )
        .expect("scripted test operation");
        chunks_tx
            .send(Some(json_line(json!({"id":1,"result":{}}))))
            .expect("scripted test operation");
        driver.pump_next(0).expect("scripted test operation");
        read_ack_rx
            .recv()
            .expect("reader consumed initialization reply");
        let (ticket, _) = driver
            .submit(Method::ModelList, json!({}), None, false, 1, 4)
            .expect("scripted test operation");
        chunks_tx
            .send(Some(json_line(
                json!({"id":ticket.id,"result":{"ok":true}}),
            )))
            .expect("scripted test operation");
        read_ack_rx
            .recv()
            .expect("reader consumed scripted response");
        chunks_tx.send(None).expect("scripted test operation");
        read_ack_rx.recv().expect("reader consumed scripted EOF");
        let result = driver.pump_one(now_ms);
        let detail = match result {
            Ok(actions) => format!("{actions:?}"),
            Err(error) => format!("{error:?}"),
        };
        if expected_late {
            assert!(
                detail.contains("Unknown") && detail.contains("late: true"),
                "now={now_ms} result={detail}"
            );
        } else {
            assert!(
                detail.contains("late: false"),
                "now={now_ms} result={detail}"
            );
        }
        driver.close().expect("scripted test operation");
    }
}

#[test]
fn deadline_resolution_survives_callback_dispatch_failure() {
    let (chunks_tx, chunks_rx) = mpsc::channel();
    let (read_started_tx, read_started_rx) = mpsc::channel();
    let (handler_dropped_tx, handler_dropped_rx) = mpsc::channel();
    let mut driver = ConnectionDriver::start(
        ObservedReader {
            chunks: chunks_rx,
            read_started: read_started_tx,
        },
        Sink,
        FailingHandler {
            dropped: handler_dropped_tx,
        },
        DriverConfig {
            generation: 11,
            claude: false,
            now_ms: 0,
            timeout_ms: 100,
            initialize_params: json!({}),
        },
    )
    .expect("scripted test operation");
    read_started_rx.recv().expect("reader started");
    chunks_tx
        .send(Some(json_line(json!({"id":1,"result":{}}))))
        .expect("init reply");
    read_started_rx.recv().expect("init response queued");
    driver.pump_next(0).expect("initialize");
    let (ticket, _) = driver
        .submit(Method::ModelList, json!({}), None, false, 1, 5)
        .expect("submit pending request");

    chunks_tx
        .send(Some(json_line(
            json!({"method":"turn/started","params":{}}),
        )))
        .expect("first callback");
    read_started_rx.recv().expect("first callback queued");
    driver
        .pump_one(0)
        .expect("callback is handed off asynchronously");
    handler_dropped_rx
        .recv()
        .expect("failed dispatcher worker exited");

    chunks_tx
        .send(Some(json_line(
            json!({"method":"turn/completed","params":{}}),
        )))
        .expect("second callback");
    read_started_rx.recv().expect("second callback queued");
    let error = driver
        .pump_one(6)
        .expect_err("expired request and failed dispatcher");
    let detail = format!("{error:?}");
    assert!(
        detail.contains(&format!("{:?}", ticket.id)) && detail.contains("Unknown"),
        "deadline settlement was lost: {detail}"
    );
    chunks_tx.send(None).expect("close reader");
    assert!(driver.close().is_err());
}

#[test]
fn write_failure_exposes_unknown_outcome_for_registered_request() {
    let (_chunks_tx, chunks_rx) = mpsc::channel();
    let result = ConnectionDriver::start(
        ChunkReader {
            chunks: chunks_rx,
            current: Vec::new(),
            closed: false,
        },
        FailedWriter,
        Noop,
        DriverConfig {
            generation: 6,
            claude: false,
            now_ms: 0,
            timeout_ms: 10_000,
            initialize_params: json!({"clientInfo":{"name":"codex_agents_canvas","version":"1.0.0"},"capabilities":{"experimentalApi":true}}),
        },
    );
    let error = match result {
        Ok(_) => panic!("scripted writer failure should fail"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        studio_runtime_providers::DriverError::Io { unresolved, .. }
            if unresolved.iter().any(|action| matches!(action,
                studio_runtime_providers::Action::Resolve {
                    outcome: studio_runtime_providers::RequestOutcome::Unknown,
                    ..
                }
            ))
    ));
}
