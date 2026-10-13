//! Live terminal sessions backed by the local PTY process port.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use base64::Engine as _;
use serde_json::{Value, json};
use studio_runtime_process::{LocalPtyHost, PtyHost, PtySession};

use crate::terminal_driver::{
    SessionError, TerminalEvent, TerminalNotification, TerminalOutcome, TerminalSession,
};

const READ_CHUNK: usize = 16 * 1024;

pub type NotificationCallback = Arc<dyn Fn(TerminalNotification) + Send + Sync>;

struct LiveTerminal {
    session: PtySession,
    reader: Option<JoinHandle<()>>,
}

/// A blocking local PTY adapter implementing the service's AppServer-shaped port.
#[derive(Clone, Default)]
pub struct PtyTerminalSession {
    host: LocalPtyHost,
    sessions: Arc<Mutex<HashMap<String, LiveTerminal>>>,
    callback: Arc<Mutex<Option<NotificationCallback>>>,
}

impl PtyTerminalSession {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_notification_callback(&self, callback: NotificationCallback) {
        if let Ok(mut current) = self.callback.lock() {
            *current = Some(callback);
        }
    }

    fn callback(&self) -> Result<NotificationCallback, SessionError> {
        self.callback
            .lock()
            .map_err(|_| SessionError::Disconnected)?
            .clone()
            .ok_or(SessionError::Disconnected)
    }

    fn spawn(&self, params: Value) -> Result<Value, SessionError> {
        let id = field_str(&params, "processHandle")?.to_owned();
        let command = params
            .get("command")
            .and_then(Value::as_array)
            .ok_or(SessionError::Rejected)?
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or(SessionError::Rejected)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .ok_or(SessionError::Rejected)?;
        let environment = params
            .get("env")
            .and_then(Value::as_object)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|(key, value)| {
                        value.as_str().map(|value| (key.clone(), value.to_owned()))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let size = params.get("size").ok_or(SessionError::Rejected)?;
        let cols = field_u16(size, "cols")?;
        let rows = field_u16(size, "rows")?;
        let mut session = self
            .host
            .open(&command, cwd.as_ref(), &environment)
            .map_err(|_| SessionError::Rejected)?;
        self.host
            .resize(&mut session, cols, rows)
            .map_err(|_| SessionError::Rejected)?;

        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SessionError::Disconnected)?;
        if sessions.contains_key(&id) {
            let _close_result = self.host.close(session);
            return Err(SessionError::Rejected);
        }
        let reader_session = session.clone();
        let callback = self.callback()?;
        let sequence = Arc::new(AtomicU64::new(0));
        let reader_id = id.clone();
        let reader = thread::Builder::new()
            .name(format!("terminal-pty-{id}"))
            .spawn(move || {
                read_session(LocalPtyHost, reader_session, reader_id, callback, sequence);
            })
            .map_err(|_| SessionError::Rejected)?;
        sessions.insert(
            id,
            LiveTerminal {
                session,
                reader: Some(reader),
            },
        );
        Ok(json!({"ok":true}))
    }

    fn with_session(
        &self,
        id: &str,
        action: impl FnOnce(&mut PtySession) -> Result<(), SessionError>,
    ) -> Result<(), SessionError> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| SessionError::Disconnected)?;
        let live = sessions.get(id).ok_or(SessionError::Rejected)?;
        let mut session = live.session.clone();
        drop(sessions);
        action(&mut session)
    }

    fn close_one(&self, id: &str) -> Result<(), SessionError> {
        let live = self
            .sessions
            .lock()
            .map_err(|_| SessionError::Disconnected)?
            .remove(id);
        if let Some(mut live) = live {
            let close_failed = self.host.close(live.session).is_err();
            let reader_failed = live
                .reader
                .take()
                .is_some_and(|reader| reader.join().is_err());
            if close_failed || reader_failed {
                return Err(SessionError::Disconnected);
            }
        }
        Ok(())
    }
}

impl Drop for PtyTerminalSession {
    fn drop(&mut self) {
        if Arc::strong_count(&self.sessions) == 1 {
            let _shutdown_result = self.close();
        }
    }
}

impl TerminalSession for PtyTerminalSession {
    fn send_request(
        &self,
        _operation_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, SessionError> {
        match method {
            "process/spawn" => self.spawn(params),
            "process/writeStdin" => {
                let id = field_str(&params, "processHandle")?;
                let input = params
                    .get("deltaBase64")
                    .and_then(Value::as_str)
                    .ok_or(SessionError::Rejected)?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(input)
                    .map_err(|_| SessionError::Rejected)?;
                self.with_session(id, |session| {
                    self.host
                        .write(session, &bytes)
                        .map_err(|_| SessionError::Disconnected)
                })?;
                Ok(json!({"ok":true}))
            }
            "process/resizePty" => {
                let id = field_str(&params, "processHandle")?;
                let size = params.get("size").ok_or(SessionError::Rejected)?;
                let cols = field_u16(size, "cols")?;
                let rows = field_u16(size, "rows")?;
                self.with_session(id, |session| {
                    self.host
                        .resize(session, cols, rows)
                        .map_err(|_| SessionError::Disconnected)
                })?;
                Ok(json!({"ok":true}))
            }
            "process/kill" => {
                self.close_one(field_str(&params, "processHandle")?)?;
                Ok(json!({"ok":true}))
            }
            _ => Err(SessionError::Rejected),
        }
    }

    fn receive_notification(&self) -> Result<Option<TerminalNotification>, SessionError> {
        Ok(None)
    }

    fn receive_outcome(&self) -> Result<Option<TerminalOutcome>, SessionError> {
        Ok(None)
    }

    fn close(&self) -> Result<(), SessionError> {
        let ids = self
            .sessions
            .lock()
            .map_err(|_| SessionError::Disconnected)?
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut failed = false;
        for id in ids {
            failed |= self.close_one(&id).is_err();
        }
        if failed {
            Err(SessionError::Disconnected)
        } else {
            Ok(())
        }
    }
}

fn read_session(
    host: LocalPtyHost,
    mut session: PtySession,
    terminal_id: String,
    callback: NotificationCallback,
    sequence: Arc<AtomicU64>,
) {
    let mut buffer = [0; READ_CHUNK];
    loop {
        match host.read(&mut session, &mut buffer) {
            Ok(0) => break,
            Ok(count) => callback(TerminalNotification {
                terminal_id: terminal_id.clone(),
                sequence: sequence.fetch_add(1, Ordering::Relaxed) + 1,
                event: TerminalEvent::Output(buffer[..count].to_vec()),
            }),
            Err(_) => break,
        }
    }
    let _close_result = host.close(session.clone());
    let code = host.wait(&mut session).ok().flatten().map(i64::from);
    callback(TerminalNotification {
        terminal_id,
        sequence: sequence.fetch_add(1, Ordering::Relaxed) + 1,
        event: TerminalEvent::Exit(code),
    });
}

fn field_str<'a>(value: &'a Value, name: &str) -> Result<&'a str, SessionError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or(SessionError::Rejected)
}

fn field_u16(value: &Value, name: &str) -> Result<u16, SessionError> {
    value
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|number| u16::try_from(number).ok())
        .ok_or(SessionError::Rejected)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use serde_json::json;

    use super::PtyTerminalSession;
    use crate::terminal_driver::{TerminalEvent, TerminalNotification, TerminalSession};

    #[test]
    fn printf_output_is_delivered_losslessly() {
        let cwd = tempfile::tempdir().expect("temporary terminal cwd");
        let adapter = PtyTerminalSession::new();
        let (sender, receiver) = mpsc::channel::<TerminalNotification>();
        adapter.set_notification_callback(std::sync::Arc::new(move |event| {
            let _send_result = sender.send(event);
        }));
        adapter
            .send_request(
                "spawn-printf",
                "process/spawn",
                json!({
                    "processHandle":"printf-session",
                    "cwd":cwd.path(),
                    "command":["/usr/bin/printf","terminal-output-Ж🙂"],
                    "env":{},
                    "size":{"cols":100,"rows":28}
                }),
            )
            .expect("spawn printf");
        let mut output = Vec::new();
        while let TerminalEvent::Output(bytes) = receiver.recv().expect("PTY process event").event {
            output.extend(bytes);
        }
        assert_eq!(output, "terminal-output-Ж🙂".as_bytes());
        adapter
            .send_request(
                "close-printf",
                "process/kill",
                json!({"processHandle":"printf-session"}),
            )
            .expect("reap printf");
    }

    #[test]
    fn cat_accepts_input_resizes_and_closes_through_the_session_port() {
        let cwd = tempfile::tempdir().expect("temporary terminal cwd");
        let adapter = PtyTerminalSession::new();
        let (sender, receiver) = mpsc::channel::<TerminalNotification>();
        adapter.set_notification_callback(std::sync::Arc::new(move |event| {
            let _send_result = sender.send(event);
        }));
        adapter
            .send_request(
                "spawn-test",
                "process/spawn",
                json!({
                    "processHandle":"test-session",
                    "cwd":cwd.path(),
                    "command":["/bin/cat"],
                    "env":{},
                    "size":{"cols":96,"rows":31}
                }),
            )
            .expect("spawn cat");
        adapter
            .send_request(
                "resize-test",
                "process/resizePty",
                json!({"processHandle":"test-session","size":{"cols":88,"rows":27}}),
            )
            .expect("resize cat");
        adapter
            .send_request(
                "input-test",
                "process/writeStdin",
                json!({"processHandle":"test-session","deltaBase64":"cnVzdC10ZXJtaW5hbAo="}),
            )
            .expect("write cat input");

        let mut output = Vec::new();
        while !output
            .windows(b"rust-terminal".len())
            .any(|window| window == b"rust-terminal")
        {
            let event = receiver.recv().expect("PTY output event");
            if let TerminalEvent::Output(bytes) = event.event {
                output.extend(bytes);
            }
        }
        adapter
            .send_request(
                "close-test",
                "process/kill",
                json!({"processHandle":"test-session"}),
            )
            .expect("close and reap cat");
        loop {
            if matches!(
                receiver.recv().expect("PTY exit event").event,
                TerminalEvent::Exit(_)
            ) {
                break;
            }
        }
    }
}
