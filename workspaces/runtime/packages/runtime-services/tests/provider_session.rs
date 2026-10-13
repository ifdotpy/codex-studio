use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::json;
use studio_runtime_providers::{Method, ResponseBody};
use studio_runtime_services::provider_session::{
    ProviderProcess, ProviderSession, ProviderSessionConfig, ProviderSessionEvent,
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
    fn split(&mut self) -> io::Result<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
        let reader = self
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("fake stdout already taken"))?;
        let writer = self
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("fake stdin already taken"))?;
        Ok((Box::new(reader), Box::new(writer)))
    }

    fn stop(&mut self) -> io::Result<()> {
        let _ignored = self.child.kill();
        self.child.wait().map(|_| ())
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
    assert_eq!(session.generation(), 1);
    let ticket = session
        .send_request(Method::ModelList, json!({"limit": 1}), None, false, 10_000)
        .expect("request should be submitted");
    let mut got_reply = false;
    let mut got_notification = false;
    while !got_reply || !got_notification {
        match session.receive_event().expect("provider event") {
            ProviderSessionEvent::Reply {
                ticket: received,
                body: ResponseBody::Result(result),
                late: false,
            } => {
                assert_eq!(received, ticket);
                assert_eq!(result, json!({"models": []}));
                got_reply = true;
            }
            ProviderSessionEvent::Inbound {
                inbound: studio_runtime_providers::Inbound::Notification(_),
                cursor: 1,
            } => {
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
        session.receive_event().expect("unknown reply event"),
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
        session.receive_event().expect("unknown event at deadline"),
        ProviderSessionEvent::UnknownOutcome {
            ticket: ticket.clone()
        }
    );
    assert_eq!(
        session.receive_event().expect("late evidence"),
        ProviderSessionEvent::Reply {
            ticket,
            body: ResponseBody::Result(json!({"late": true})),
            late: true
        }
    );
    session.close().expect("provider shutdown");
}
