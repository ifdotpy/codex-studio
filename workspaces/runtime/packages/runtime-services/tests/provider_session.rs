#![allow(clippy::unwrap_used)]

use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;
use studio_runtime_process::{ProviderProcess, ProviderProcessReader};
use studio_runtime_providers::{Method, ResponseBody};
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

struct CursorReader {
    bytes: io::Cursor<Vec<u8>>,
    cursors: Vec<u64>,
}

impl Read for CursorReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.bytes.read(output)
    }
}

impl ProviderProcessReader for CursorReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.bytes.read(output)
    }

    fn take_cursors(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.cursors)
    }
}

struct CursorProcess(Arc<AtomicU64>);

impl ProviderProcess for CursorProcess {
    fn split(&mut self) -> io::Result<(Box<dyn ProviderProcessReader>, Box<dyn Write + Send>)> {
        let bytes =
            b"{\"id\":1,\"result\":{\"ok\":true}}\n{\"method\":\"test/duringInitialize\"}\n";
        let handled = self.0.load(Ordering::Acquire);
        Ok((
            Box::new(CursorReader {
                bytes: io::Cursor::new(bytes.to_vec()),
                cursors: if handled == 0 { vec![10, 11] } else { vec![11] },
            }),
            Box::new(Vec::<u8>::new()),
        ))
    }

    fn acknowledged_cursor(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }

    fn generation(&self) -> u64 {
        2
    }

    fn acknowledge(&mut self, cursor: u64) -> io::Result<()> {
        self.0.store(cursor, Ordering::Release);
        Ok(())
    }

    fn stop(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct DuplicateFrameProcess(Arc<AtomicU64>);

impl ProviderProcess for DuplicateFrameProcess {
    fn split(&mut self) -> io::Result<(Box<dyn ProviderProcessReader>, Box<dyn Write + Send>)> {
        Ok((
            Box::new(CursorReader {
                bytes: io::Cursor::new(
                    b"{\"id\":1,\"result\":{}}\n{\"method\":\"test/once\"}\n{\"method\":\"test/once\"}\n{\"id\":1,\"result\":{}}\n".to_vec(),
                ),
                cursors: vec![10, 11, 11, 12],
            }),
            Box::new(Vec::<u8>::new()),
        ))
    }

    fn acknowledged_cursor(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }

    fn generation(&self) -> u64 {
        2
    }

    fn acknowledge(&mut self, cursor: u64) -> io::Result<()> {
        self.0.store(cursor, Ordering::Release);
        Ok(())
    }

    fn stop(&mut self) -> io::Result<()> {
        Ok(())
    }
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
    let mut session = connect("late");
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

#[test]
fn consumer_acknowledges_cursor_only_after_durable_handling() {
    let acknowledged = Arc::new(AtomicU64::new(0));
    let mut session = ProviderSession::connect(
        Box::new(CursorProcess(Arc::clone(&acknowledged))),
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
fn unhandled_frame_is_redelivered_after_reader_session_restarts() {
    let acknowledged = Arc::new(AtomicU64::new(0));
    let first = ProviderSession::connect(
        Box::new(CursorProcess(Arc::clone(&acknowledged))),
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
        Box::new(CursorProcess(Arc::clone(&acknowledged))),
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
    let acknowledged = Arc::new(AtomicU64::new(0));
    let mut session = ProviderSession::connect(
        Box::new(DuplicateFrameProcess(Arc::clone(&acknowledged))),
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
