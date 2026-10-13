//! Live terminal sessions backed by the local PTY process port.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};
use studio_runtime_process::{LocalPtyHost, PtyHost, PtyIdentity, PtySession, cleanup_orphan};

use crate::terminal_driver::{
    SessionError, TerminalEvent, TerminalNotification, TerminalOutcome, TerminalSession,
};

const READ_CHUNK: usize = 16 * 1024;

pub type NotificationCallback = Arc<dyn Fn(TerminalNotification) + Send + Sync>;
pub type IdentityCallback =
    Arc<dyn Fn(&str, &PtyIdentity) -> Result<(), SessionError> + Send + Sync>;

struct LiveTerminal {
    session: PtySession,
    reader: Option<JoinHandle<()>>,
    reader_done: mpsc::Receiver<()>,
}

/// A blocking local PTY adapter implementing the service's AppServer-shaped port.
#[derive(Clone, Default)]
pub struct PtyTerminalSession {
    host: LocalPtyHost,
    sessions: Arc<Mutex<HashMap<String, LiveTerminal>>>,
    callback: Arc<Mutex<Option<NotificationCallback>>>,
    identity_callback: Arc<Mutex<Option<IdentityCallback>>>,
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

    pub fn set_identity_callback(&self, callback: IdentityCallback) {
        if let Ok(mut current) = self.identity_callback.lock() {
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

        let identity_callback = self
            .identity_callback
            .lock()
            .map_err(|_| SessionError::Disconnected)?
            .clone();
        if let Some(identity_callback) = identity_callback {
            identity_callback(&id, session.identity())?;
        }

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
        let (reader_done_tx, reader_done_rx) = mpsc::channel();
        let sessions_weak = Arc::downgrade(&self.sessions);
        let reader = thread::Builder::new()
            .name(format!("terminal-pty-{id}"))
            .spawn(move || {
                read_session(
                    LocalPtyHost,
                    reader_session,
                    reader_id,
                    callback,
                    sequence,
                    sessions_weak,
                    reader_done_tx,
                );
            })
            .map_err(|_| SessionError::Rejected)?;
        sessions.insert(
            id,
            LiveTerminal {
                session,
                reader: Some(reader),
                reader_done: reader_done_rx,
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
            let reader_failed = live.reader.take().is_some_and(|reader| {
                match live.reader_done.recv_timeout(Duration::from_secs(2)) {
                    Ok(()) => reader.join().is_err(),
                    Err(_) => {
                        drop(reader);
                        true
                    }
                }
            });
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

    fn cleanup_orphan(&self, identity: &Value) -> Result<bool, SessionError> {
        let identity = serde_json::from_value::<PtyIdentity>(identity.clone())
            .map_err(|_| SessionError::Rejected)?;
        cleanup_persisted_identity(&identity)
    }
}

fn read_session(
    host: LocalPtyHost,
    mut session: PtySession,
    terminal_id: String,
    callback: NotificationCallback,
    sequence: Arc<AtomicU64>,
    sessions: Weak<Mutex<HashMap<String, LiveTerminal>>>,
    done: mpsc::Sender<()>,
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
    if let Some(sessions) = sessions.upgrade()
        && let Ok(mut sessions) = sessions.lock()
    {
        sessions.remove(&terminal_id);
    }
    callback(TerminalNotification {
        terminal_id: terminal_id.clone(),
        sequence: sequence.fetch_add(1, Ordering::Relaxed) + 1,
        event: TerminalEvent::Exit(code),
    });
    let _send_result = done.send(());
}

pub fn cleanup_persisted_identity(identity: &PtyIdentity) -> Result<bool, SessionError> {
    cleanup_orphan(identity).map_err(|_| SessionError::Disconnected)
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
    use tempfile::TempDir;

    use super::PtyTerminalSession;
    use crate::terminal_driver::{TerminalEvent, TerminalNotification, TerminalSession};

    fn terminal_cwd() -> TempDir {
        let root = std::path::Path::new("/dev/shm/cs-rust-terminals-tests");
        std::fs::create_dir_all(root).expect("tmpfs test state directory");
        tempfile::Builder::new()
            .prefix("terminal-pty-")
            .tempdir_in(root)
            .expect("temporary terminal cwd")
    }

    #[test]
    fn printf_output_is_delivered_losslessly() {
        let cwd = terminal_cwd();
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
        assert!(adapter.sessions.lock().expect("session map").is_empty());
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
        use std::time::Duration;
        let cwd = terminal_cwd();
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
                    "command":["/bin/sh","-c","stty -echo; printf READY; exec /bin/cat"],
                    "env":{},
                    "size":{"cols":96,"rows":31}
                }),
            )
            .expect("spawn cat");
        let mut output = Vec::new();
        while !output
            .windows(b"READY".len())
            .any(|window| window == b"READY")
        {
            let event = receiver
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("PTY process ready");
            if let TerminalEvent::Output(bytes) = event.event {
                output.extend(bytes);
            }
        }
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

        let received_input = loop {
            let event = match receiver.recv_timeout(Duration::from_secs(2)) {
                Ok(event) => event,
                Err(_) => break false,
            };
            if let TerminalEvent::Output(bytes) = event.event {
                output.extend(bytes);
            }
            if output
                .windows(b"rust-terminal".len())
                .any(|window| window == b"rust-terminal")
            {
                break true;
            }
            if output.len() > 4096 {
                break false;
            }
        };
        adapter
            .send_request(
                "close-test",
                "process/kill",
                json!({"processHandle":"test-session"}),
            )
            .expect("close and reap cat");
        assert!(received_input, "cat did not echo the input bytes");
        loop {
            if matches!(
                receiver.recv().expect("PTY exit event").event,
                TerminalEvent::Exit(_)
            ) {
                break;
            }
        }
    }

    #[test]
    fn resize_changes_the_window_size_reported_by_the_child() {
        let cwd = terminal_cwd();
        let adapter = PtyTerminalSession::new();
        let (sender, receiver) = mpsc::channel::<TerminalNotification>();
        adapter.set_notification_callback(std::sync::Arc::new(move |event| {
            let _send_result = sender.send(event);
        }));
        adapter
            .send_request(
                "spawn-size-probe",
                "process/spawn",
                json!({
                    "processHandle":"size-probe",
                    "cwd":cwd.path(),
                    "command":["/bin/sh","-c","IFS= read -r line; stty size"],
                    "env":{"TERM":"xterm-256color","COLORTERM":"truecolor"},
                    "size":{"cols":96,"rows":31}
                }),
            )
            .expect("spawn size probe");
        adapter
            .send_request(
                "resize-size-probe",
                "process/resizePty",
                json!({"processHandle":"size-probe","size":{"cols":88,"rows":27}}),
            )
            .expect("resize before probing");
        adapter
            .send_request(
                "input-size-probe",
                "process/writeStdin",
                json!({"processHandle":"size-probe","deltaBase64":"eAo="}),
            )
            .expect("release size probe");

        let mut output = Vec::new();
        loop {
            match receiver.recv().expect("size probe output").event {
                TerminalEvent::Output(bytes) => {
                    output.extend(bytes);
                    if String::from_utf8_lossy(&output).contains("27 88") {
                        break;
                    }
                }
                TerminalEvent::Exit(_) => panic!("size probe exited before reporting dimensions"),
            }
        }
        assert!(String::from_utf8_lossy(&output).contains("27 88"));
        adapter
            .send_request(
                "close-size-probe",
                "process/kill",
                json!({"processHandle":"size-probe"}),
            )
            .expect("reap size probe");
    }

    #[test]
    fn child_environment_does_not_inherit_server_only_variables() {
        let server_only = std::env::var("STUDIO_TERMINAL_SERVER_ONLY")
            .expect("test runner sets the server-only variable");
        assert_eq!(server_only, "must-not-leak");
        let cwd = terminal_cwd();
        let adapter = PtyTerminalSession::new();
        let (sender, receiver) = mpsc::channel::<TerminalNotification>();
        adapter.set_notification_callback(std::sync::Arc::new(move |event| {
            let _send_result = sender.send(event);
        }));
        adapter
            .send_request(
                "spawn-env",
                "process/spawn",
                json!({
                    "processHandle":"env-session",
                    "cwd":cwd.path(),
                    "command":["/bin/sh","-c","/usr/bin/env; pwd"],
                    "env":{"TERM":"xterm-256color","COLORTERM":"truecolor"},
                    "size":{"cols":100,"rows":28}
                }),
            )
            .expect("spawn env");
        let mut output = Vec::new();
        while let TerminalEvent::Output(bytes) = receiver.recv().expect("env event").event {
            output.extend(bytes);
        }
        let output = String::from_utf8(output).expect("environment output is UTF-8");
        assert!(output.lines().any(|line| line == "TERM=xterm-256color"));
        assert!(output.lines().any(|line| line == "COLORTERM=truecolor"));
        assert!(!output.contains("STUDIO_TERMINAL_SERVER_ONLY"));
        assert!(
            output
                .lines()
                .any(|line| line == cwd.path().to_string_lossy())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn persisted_identity_never_signals_a_reused_pid() {
        use super::cleanup_persisted_identity;

        let cwd = terminal_cwd();
        let adapter = PtyTerminalSession::new();
        adapter.set_notification_callback(std::sync::Arc::new(|_| {}));
        let (identity_sender, identity_receiver) = mpsc::channel();
        adapter.set_identity_callback(std::sync::Arc::new(move |_, identity| {
            let _send_result = identity_sender.send(identity.clone());
            Ok(())
        }));
        adapter
            .send_request(
                "spawn-identity-check",
                "process/spawn",
                json!({
                    "processHandle":"identity-check",
                    "cwd":cwd.path(),
                    "command":["/bin/cat"],
                    "env":{"TERM":"xterm-256color","COLORTERM":"truecolor"},
                    "size":{"cols":100,"rows":28}
                }),
            )
            .expect("spawn identity target");
        let mut stale = identity_receiver.recv().expect("persisted identity");
        stale.start_time_ticks = stale.start_time_ticks.saturating_add(1);
        assert!(!cleanup_persisted_identity(&stale).expect("verify stale identity"));
        assert!(std::fs::metadata(format!("/proc/{}/stat", stale.pid)).is_ok());
        adapter
            .send_request(
                "close-identity-check",
                "process/kill",
                json!({"processHandle":"identity-check"}),
            )
            .expect("close verified test process");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn close_kills_background_jobs_in_other_process_groups() {
        let cwd = terminal_cwd();
        let adapter = PtyTerminalSession::new();
        let (sender, receiver) = mpsc::channel::<TerminalNotification>();
        let (identity_sender, identity_receiver) = mpsc::channel();
        adapter.set_notification_callback(std::sync::Arc::new(move |event| {
            let _send_result = sender.send(event);
        }));
        adapter.set_identity_callback(std::sync::Arc::new(move |_, identity| {
            let _send_result = identity_sender.send(identity.clone());
            Ok(())
        }));
        adapter
            .send_request(
                "spawn-background-job",
                "process/spawn",
                json!({
                    "processHandle":"background-session",
                    "cwd":cwd.path(),
                    "command":["/bin/sh","-c","set -m; /usr/bin/mkfifo terminal-fifo; /bin/cat terminal-fifo >/dev/null & echo CHILD_PID:$!; wait"],
                    "env":{"TERM":"xterm-256color","COLORTERM":"truecolor"},
                    "size":{"cols":100,"rows":28}
                }),
            )
            .expect("spawn background job");
        let identity = identity_receiver
            .recv()
            .expect("persisted process identity");
        let mut output = String::new();
        let child_pid = loop {
            if let Some(pid) = output
                .lines()
                .find_map(|line| line.strip_prefix("CHILD_PID:")?.trim().parse::<u32>().ok())
            {
                break pid;
            }
            if let TerminalEvent::Output(bytes) = receiver.recv().expect("process output").event {
                output.push_str(&String::from_utf8_lossy(&bytes));
            }
        };
        let stat = std::fs::read_to_string(format!("/proc/{child_pid}/stat"))
            .expect("background job remains before close");
        let (_, fields) = stat.rsplit_once(") ").expect("proc stat fields");
        let fields = fields.split_whitespace().collect::<Vec<_>>();
        assert_eq!(
            fields[3].parse::<u32>().expect("session id"),
            identity.session
        );
        assert_ne!(
            fields[2].parse::<u32>().expect("process group"),
            identity.session
        );

        let child_group = fields[2].parse::<i32>().expect("background process group");
        let close_result = adapter.send_request(
            "close-background-job",
            "process/kill",
            json!({"processHandle":"background-session"}),
        );
        let stat = std::fs::read_to_string(format!("/proc/{child_pid}/stat"));
        let remains = stat.is_err()
            || stat.is_ok_and(|stat| {
                stat.rsplit_once(") ")
                    .and_then(|(_, fields)| fields.split_whitespace().next())
                    == Some("Z")
            });
        if !remains {
            let _kill_result = std::process::Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{child_group}")])
                .status();
        }
        close_result.expect("close and reap session groups");
        assert!(remains, "background process group survived terminal close");
    }
}
