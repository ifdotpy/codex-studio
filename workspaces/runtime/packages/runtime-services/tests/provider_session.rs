use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use studio_runtime_providers::{Method, ResponseBody};
use studio_runtime_services::provider_session::{
    ProviderProcess, ProviderProcessReader, ProviderSession, ProviderSessionConfig,
    ProviderSessionEvent,
};

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
        Ok((Box::new(CursorReader(reader)), Box::new(writer)))
    }

    fn acknowledged_cursor(&self) -> u64 {
        0
    }
    fn generation(&self) -> u64 {
        1
    }
    fn acknowledge(&mut self, _cursor: u64) -> io::Result<()> {
        Ok(())
    }

    fn stop(&mut self) -> io::Result<()> {
        let _ignored = self.child.kill();
        self.child.wait().map(|_| ())
    }
}

struct ControlledProvider {
    output: Mutex<Option<Sender<Option<Vec<u8>>>>>,
}

impl ControlledProvider {
    fn send_frame(&self, frame: Value) {
        let bytes = format!(
            "{}\n",
            serde_json::to_string(&frame).expect("fake frame serializes")
        );
        self.output
            .lock()
            .expect("fake output lock is available")
            .as_ref()
            .expect("fake provider is connected")
            .send(Some(bytes.into_bytes()))
            .expect("fake reader remains connected");
    }
}

struct ControlledProcess {
    provider: Arc<ControlledProvider>,
    input: Option<Receiver<Option<Vec<u8>>>>,
    output: Sender<Option<Vec<u8>>>,
}

impl ControlledProcess {
    fn new(provider: Arc<ControlledProvider>) -> Self {
        let (output, input) = mpsc::channel();
        *provider
            .output
            .lock()
            .expect("fake output lock is available") = Some(output.clone());
        Self {
            provider,
            input: Some(input),
            output,
        }
    }
}

impl ProviderProcess for ControlledProcess {
    fn split(&mut self) -> io::Result<(Box<dyn ProviderProcessReader>, Box<dyn Write + Send>)> {
        let input = self
            .input
            .take()
            .ok_or_else(|| io::Error::other("fake reader already taken"))?;
        Ok((
            Box::new(ControlledReader {
                input,
                buffered: VecDeque::new(),
                ended: false,
            }),
            Box::new(ControlledWriter {
                output: self.output.clone(),
                buffered: Vec::new(),
            }),
        ))
    }

    fn acknowledged_cursor(&self) -> u64 {
        0
    }
    fn generation(&self) -> u64 {
        1
    }
    fn acknowledge(&mut self, _cursor: u64) -> io::Result<()> {
        Ok(())
    }

    fn stop(&mut self) -> io::Result<()> {
        if let Some(output) = self
            .provider
            .output
            .lock()
            .map_err(|_| io::Error::other("fake output lock poisoned"))?
            .take()
        {
            let _ignored = output.send(None);
        }
        Ok(())
    }
}

struct ControlledReader {
    input: Receiver<Option<Vec<u8>>>,
    buffered: VecDeque<u8>,
    ended: bool,
}

impl Read for ControlledReader {
    fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
        while self.buffered.is_empty() && !self.ended {
            match self.input.recv() {
                Ok(Some(bytes)) => self.buffered.extend(bytes),
                Ok(None) | Err(_) => self.ended = true,
            }
        }
        if self.buffered.is_empty() {
            return Ok(0);
        }
        let count = target.len().min(self.buffered.len());
        for slot in &mut target[..count] {
            *slot = self.buffered.pop_front().expect("buffered byte exists");
        }
        Ok(count)
    }
}

impl ProviderProcessReader for ControlledReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        <Self as Read>::read(self, buffer)
    }
    fn take_cursors(&mut self) -> Vec<u64> {
        Vec::new()
    }
}

struct CursorReader<R>(R);

impl<R: Read + Send> Read for CursorReader<R> {
    fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
        self.0.read(target)
    }
}

impl<R: Read + Send> ProviderProcessReader for CursorReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
    fn take_cursors(&mut self) -> Vec<u64> {
        Vec::new()
    }
}

struct ControlledWriter {
    output: Sender<Option<Vec<u8>>>,
    buffered: Vec<u8>,
}

impl Write for ControlledWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffered.extend_from_slice(bytes);
        while let Some(end) = self.buffered.iter().position(|byte| *byte == b'\n') {
            let line = self.buffered.drain(..=end).collect::<Vec<_>>();
            let request: Value = serde_json::from_slice(&line)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if request.get("method").and_then(Value::as_str) == Some("initialize") {
                let response = json!({
                    "id":request["id"],
                    "result":{"protocolVersion":1}
                });
                let bytes = format!(
                    "{}\n",
                    serde_json::to_string(&response).expect("initialize response serializes")
                );
                self.output
                    .send(Some(bytes.into_bytes()))
                    .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "fake reader closed"))?;
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn connect(mode: &str) -> ProviderSession {
    let mut config = ProviderSessionConfig::new();
    config.request_timeout_ms = 10_000;
    ProviderSession::connect(Box::new(FakeProcess::new(mode)), config)
        .expect("fake provider should initialize")
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
        match session.receive_event().expect("provider event").0 {
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
        session.receive_event().expect("unknown reply event").0,
        ProviderSessionEvent::UnknownOutcome { ticket }
    );
    session.close().expect("provider shutdown");
}

#[test]
fn reply_after_deadline_is_reported_as_late_evidence() {
    let provider = Arc::new(ControlledProvider {
        output: Mutex::new(None),
    });
    let mut session = ProviderSession::connect(
        Box::new(ControlledProcess::new(Arc::clone(&provider))),
        ProviderSessionConfig::new(),
    )
    .expect("controlled fake provider initializes");
    let ticket = session
        .send_request(Method::ModelList, json!({}), None, false, 0)
        .expect("request should be submitted once");
    assert_eq!(
        session
            .receive_event()
            .expect("unknown event at deadline")
            .0,
        ProviderSessionEvent::UnknownOutcome {
            ticket: ticket.clone()
        }
    );
    provider.send_frame(json!({
        "id":ticket.id,
        "result":{"late":true}
    }));
    assert_eq!(
        session.receive_event().expect("late evidence").0,
        ProviderSessionEvent::Reply {
            ticket,
            body: ResponseBody::Result(json!({"late": true})),
            late: true
        }
    );
    session.close().expect("provider shutdown");
}
