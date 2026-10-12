use std::io::{self, Read, Write};
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

#[derive(Default)]
struct Noop;
impl Handler for Noop {
    type Error = ();
    fn handle(&mut self, _: Inbound) -> Result<(), Self::Error> {
        Ok(())
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
