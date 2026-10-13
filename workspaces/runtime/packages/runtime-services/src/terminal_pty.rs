//! Live terminal sessions backed by the local PTY process port.

use std::collections::HashMap;
use std::ffi::OsString;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};
use studio_runtime_process::{
    LocalPtyHost, PtyHost, PtyIdentity, PtySession, cleanup_orphan, terminal_environment,
};

use crate::terminal_driver::{
    SessionError, TerminalEvent, TerminalNotification, TerminalOutcome, TerminalSession,
};

const READ_CHUNK: usize = 16 * 1024;

pub type NotificationCallback = Arc<dyn Fn(TerminalNotification) + Send + Sync>;
pub type IdentityCallback =
    Arc<dyn Fn(&str, Option<&PtyIdentity>) -> Result<(), SessionError> + Send + Sync>;

struct LiveTerminal {
    session: PtySession,
    reader: Option<JoinHandle<()>>,
    reader_done: Option<mpsc::Receiver<()>>,
}

/// A blocking local PTY adapter implementing the service's AppServer-shaped port.
#[derive(Clone, Default)]
pub struct PtyTerminalSession {
    host: LocalPtyHost,
    sessions: Arc<Mutex<HashMap<String, LiveTerminal>>>,
    callback: Arc<Mutex<Option<NotificationCallback>>>,
    identity_callback: Arc<Mutex<Option<IdentityCallback>>>,
    #[cfg(test)]
    fail_next_close: Arc<Mutex<bool>>,
    #[cfg(test)]
    force_identity_unavailable: Arc<AtomicBool>,
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
        let overlay = params
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
        let mut environment = terminal_environment();
        for (key, value) in overlay {
            environment.insert(OsString::from(key), OsString::from(value));
        }
        let environment = environment.into_iter().collect::<Vec<_>>();
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
            #[cfg(test)]
            let identity = if self.force_identity_unavailable.load(Ordering::Acquire) {
                None
            } else {
                session.identity()
            };
            #[cfg(not(test))]
            let identity = session.identity();
            identity_callback(&id, identity)?;
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
                reader_done: Some(reader_done_rx),
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
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let live = self
            .sessions
            .lock()
            .map_err(|_| SessionError::Disconnected)?
            .remove(id);
        if let Some(mut live) = live {
            #[cfg(test)]
            let injected_failure = self
                .fail_next_close
                .lock()
                .map(|mut fail| std::mem::take(&mut *fail))
                .unwrap_or(false);
            #[cfg(not(test))]
            let injected_failure = false;
            let close_failed = injected_failure || self.host.close(live.session.clone()).is_err();
            let reader_failed = if close_failed {
                live.reader.take().is_some_and(|reader| {
                    drop(reader);
                    true
                })
            } else {
                live.reader.take().is_some_and(|reader| {
                    match live.reader_done.take().map(|done| {
                        done.recv_timeout(
                            deadline.saturating_duration_since(std::time::Instant::now()),
                        )
                    }) {
                        Some(Ok(())) => reader.join().is_err(),
                        Some(Err(_)) | None => {
                            drop(reader);
                            true
                        }
                    }
                })
            };
            if close_failed || reader_failed {
                live.reader = None;
                live.reader_done = None;
                self.sessions
                    .lock()
                    .map_err(|_| SessionError::Disconnected)?
                    .insert(id.to_owned(), live);
                return Err(SessionError::Disconnected);
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn fail_next_close_for_test(&self) {
        *self
            .fail_next_close
            .lock()
            .expect("close failure injection") = true;
    }

    #[cfg(test)]
    fn force_identity_unavailable_for_test(&self) {
        self.force_identity_unavailable
            .store(true, Ordering::Release);
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
        if identity.is_null() {
            return Ok(false);
        }
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
    if let Err(error) = host.close(session.clone()) {
        callback(TerminalNotification {
            terminal_id,
            sequence: sequence.fetch_add(1, Ordering::Relaxed) + 1,
            event: TerminalEvent::CleanupFailed(format!("Terminal teardown failed: {error}")),
        });
        let _send_result = done.send(());
        return;
    }
    let code = match host.wait(&mut session) {
        Ok(Some(code)) => i64::from(code),
        Ok(None) => {
            callback(TerminalNotification {
                terminal_id,
                sequence: sequence.fetch_add(1, Ordering::Relaxed) + 1,
                event: TerminalEvent::CleanupFailed(
                    "Terminal teardown did not reap the child".into(),
                ),
            });
            let _send_result = done.send(());
            return;
        }
        Err(error) => {
            callback(TerminalNotification {
                terminal_id,
                sequence: sequence.fetch_add(1, Ordering::Relaxed) + 1,
                event: TerminalEvent::CleanupFailed(format!(
                    "Could not reap terminal child: {error}"
                )),
            });
            let _send_result = done.send(());
            return;
        }
    };
    if let Some(sessions) = sessions.upgrade()
        && let Ok(mut sessions) = sessions.lock()
    {
        sessions.remove(&terminal_id);
    }
    callback(TerminalNotification {
        terminal_id,
        sequence: sequence.fetch_add(1, Ordering::Relaxed) + 1,
        event: TerminalEvent::Exit(Some(code)),
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
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::OsStringExt;
    use std::process::Command;
    use std::sync::mpsc;
    use std::time::Duration;

    use base64::Engine as _;
    use serde_json::json;
    use tempfile::TempDir;

    use super::PtyTerminalSession;
    use crate::agents_create;
    use crate::terminal_driver::{TerminalEvent, TerminalNotification, TerminalSession};
    use studio_runtime_process::{LocalPtyHost, PtyHost};

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
    fn failed_teardown_keeps_the_live_session_for_recovery() {
        let cwd = terminal_cwd();
        let adapter = PtyTerminalSession::new();
        let (sender, receiver) = mpsc::channel::<TerminalNotification>();
        adapter.set_notification_callback(std::sync::Arc::new(move |event| {
            let _send_result = sender.send(event);
        }));
        adapter
            .send_request(
                "spawn-recoverable-cat",
                "process/spawn",
                json!({
                    "processHandle":"recoverable-cat",
                    "cwd":cwd.path(),
                    "command":["/bin/cat"],
                    "env":{"TERM":"xterm-256color","COLORTERM":"truecolor"},
                    "size":{"cols":100,"rows":28}
                }),
            )
            .expect("spawn recoverable cat");
        adapter.fail_next_close_for_test();
        assert_eq!(
            adapter.send_request(
                "injected-close-failure",
                "process/kill",
                json!({"processHandle":"recoverable-cat"}),
            ),
            Err(crate::terminal_driver::SessionError::Disconnected)
        );
        assert!(
            adapter
                .sessions
                .lock()
                .expect("sessions")
                .contains_key("recoverable-cat")
        );

        adapter.close().expect("retry teardown");
        let mut saw_exit = false;
        while let Ok(event) = receiver.recv_timeout(Duration::from_secs(2)) {
            match event.event {
                TerminalEvent::Exit(Some(_)) => {
                    saw_exit = true;
                    break;
                }
                TerminalEvent::CleanupFailed(message) => panic!("teardown failed: {message}"),
                TerminalEvent::Output(_) | TerminalEvent::Exit(None) => {}
            }
        }
        assert!(saw_exit, "successful recovery reports a reaped child");
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
                TerminalEvent::CleanupFailed(message) => {
                    panic!("size probe cleanup failed: {message}")
                }
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
    fn terminal_child_environment_key_set_matches_python_launch_path() {
        let cwd = terminal_cwd();
        let source =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let python = agents_create::prepared_python(&source);
        let oracle = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/terminal_environment_oracle.py");
        let raw_key = OsString::from_vec(b"STUDIO_RAW_\xff".to_vec());
        for supervisor in [false, true] {
            for backend_overrides in [false, true] {
                for locale_case in [
                    "utf8",
                    "c",
                    "all-c",
                    "ctype-c",
                    "disabled",
                    "none",
                    "all-empty",
                    "ctype-empty",
                ] {
                    let mut base = BTreeMap::from([
                        (OsString::from("HOME"), OsString::from("/tmp/terminal-home")),
                        (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
                        (OsString::from("LANG"), OsString::from("C.UTF-8")),
                        (
                            OsString::from("SSH_AUTH_SOCK"),
                            OsString::from("/tmp/ssh-agent.sock"),
                        ),
                        (OsString::from("DISPLAY"), OsString::from(":42")),
                        (
                            OsString::from("XDG_CONFIG_HOME"),
                            OsString::from("/tmp/terminal-config"),
                        ),
                        (
                            OsString::from("CODEX_AGENTS_BACKEND_ID"),
                            OsString::from("sentinel"),
                        ),
                        (
                            OsString::from("CODEX_RUNTIME_LOCK_METRICS"),
                            OsString::from("sentinel"),
                        ),
                        (
                            OsString::from("CODEX_AGENTS_PROVIDER_CAPTURE"),
                            OsString::from("sentinel"),
                        ),
                        (
                            OsString::from("CODEX_AGENTS_PROVIDER_CAPTURE_FILE"),
                            OsString::from("sentinel"),
                        ),
                        (OsString::from("CODEX_NODE"), OsString::from("/server/node")),
                        (
                            OsString::from("STUDIO_TERMINAL_SERVER_ONLY"),
                            OsString::from("inherited"),
                        ),
                        (
                            OsString::from("CODEX_AGENTS_SUPERVISOR_MODE"),
                            OsString::from(if supervisor { "1" } else { "0" }),
                        ),
                    ]);
                    if locale_case == "c" {
                        base.insert("LANG".into(), "C".into());
                    } else if locale_case == "all-c" {
                        base.remove(OsStr::new("LANG"));
                        base.insert("LC_ALL".into(), "C".into());
                    } else if locale_case == "ctype-c" {
                        base.insert("LC_CTYPE".into(), "C".into());
                    } else if locale_case == "disabled" {
                        base.insert("LANG".into(), "C".into());
                        base.insert("PYTHONCOERCECLOCALE".into(), "0".into());
                    } else if locale_case == "none" {
                        base.remove(OsStr::new("LANG"));
                    } else if locale_case == "all-empty" {
                        base.insert("LANG".into(), "C".into());
                        base.insert("LC_ALL".into(), "".into());
                    } else if locale_case == "ctype-empty" {
                        base.insert("LANG".into(), "C".into());
                        base.insert("LC_CTYPE".into(), "".into());
                    }
                    if backend_overrides {
                        for (key, value) in [
                            ("CODEX_HOME", "/tmp/backend-codex-home"),
                            ("OPENAI_API_KEY", "synthetic-openai-key"),
                            ("CODEX_API_KEY", "synthetic-codex-key"),
                        ] {
                            base.insert(key.into(), value.into());
                        }
                    }
                    base.insert(
                        raw_key.clone(),
                        OsString::from_vec(b"raw-value-\xfe".to_vec()),
                    );
                    let rust_env =
                        studio_runtime_process::terminal_environment_from(&base, supervisor);
                    let mut python_command = Command::new(python.as_os_str());
                    python_command
                        .env_clear()
                        .envs(&base)
                        .arg(&oracle)
                        .arg(source.as_os_str())
                        .arg(cwd.path())
                        .arg(cwd.path().join("private-codex-home"));
                    let python_output = python_command
                        .output()
                        .expect("run Python environment oracle");
                    assert!(
                        python_output.status.success(),
                        "Python launch oracle failed: {}",
                        String::from_utf8_lossy(&python_output.stderr)
                    );
                    let python_keys = python_output
                        .stdout
                        .split(|byte| *byte == b'\n')
                        .filter(|key| !key.is_empty())
                        .map(Vec::from)
                        .collect::<BTreeSet<_>>();

                    let host = LocalPtyHost;
                    let rust_environment = rust_env.into_iter().collect::<Vec<_>>();
                    let mut session = host
                        .open(
                            &["/usr/bin/env".into(), "-0".into()],
                            cwd.path(),
                            &rust_environment,
                        )
                        .expect("spawn Rust environment probe");
                    let mut bytes = Vec::new();
                    let mut chunk = [0; 4096];
                    while let Ok(count) = host.read(&mut session, &mut chunk) {
                        if count == 0 {
                            break;
                        }
                        bytes.extend_from_slice(&chunk[..count]);
                    }
                    let _exit_code = host.wait(&mut session).expect("wait for env probe");
                    let rust_keys: BTreeSet<Vec<u8>> = bytes
                        .split(|byte| *byte == 0)
                        .filter(|pair| !pair.is_empty())
                        .map(|pair| {
                            pair.split(|byte| *byte == b'=')
                                .next()
                                .unwrap_or_default()
                                .to_vec()
                        })
                        .collect();
                    let encode = |key: &[u8]| base64::engine::general_purpose::STANDARD.encode(key);
                    let rust_keys = rust_keys
                        .iter()
                        .map(|key| encode(key))
                        .collect::<BTreeSet<_>>();
                    let python_keys = python_keys
                        .iter()
                        .map(|key| {
                            String::from_utf8(key.clone()).expect("base64 key output is ASCII")
                        })
                        .collect::<BTreeSet<_>>();
                    assert_eq!(
                        rust_keys, python_keys,
                        "environment keys differ (supervisor={supervisor}, backend-overrides={backend_overrides}, locale={locale_case})"
                    );
                    assert!(
                        bytes
                            .windows(b"STUDIO_RAW_\xff=raw-value-\xfe".len())
                            .any(|window| window == b"STUDIO_RAW_\xff=raw-value-\xfe")
                    );
                    let encode = |key: &str| encode(key.as_bytes());
                    for key in [
                        "HOME",
                        "PATH",
                        "SSH_AUTH_SOCK",
                        "STUDIO_TERMINAL_SERVER_ONLY",
                    ] {
                        assert!(rust_keys.contains(&encode(key)));
                    }
                    for key in ["CODEX_HOME", "OPENAI_API_KEY", "CODEX_API_KEY"] {
                        assert_eq!(
                            rust_keys.contains(&encode(key)),
                            backend_overrides,
                            "overlay presence for {key}"
                        );
                    }
                    if supervisor {
                        for key in [
                            "CODEX_AGENTS_BACKEND_ID",
                            "CODEX_RUNTIME_LOCK_METRICS",
                            "CODEX_AGENTS_PROVIDER_CAPTURE",
                            "CODEX_AGENTS_PROVIDER_CAPTURE_FILE",
                            "CODEX_NODE",
                        ] {
                            assert!(
                                !rust_keys.contains(&encode(key)),
                                "supervisor-only key leaked: {key}"
                            );
                        }
                    } else {
                        assert!(rust_keys.contains(&encode("CODEX_AGENTS_BACKEND_ID")));
                    }
                    let python_coerces = matches!(
                        locale_case,
                        "c" | "ctype-c" | "none" | "all-empty" | "ctype-empty"
                    );
                    assert_eq!(
                        rust_keys.contains(&encode("LC_CTYPE")),
                        python_coerces,
                        "Python locale coercion parity for {locale_case}"
                    );
                }
            }
        }
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
            let _send_result = identity_sender.send(identity.expect("Linux identity").clone());
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
        // A separate session is the unrelated live process the stale cleanup
        // must never signal.
        let unrelated_adapter = PtyTerminalSession::new();
        unrelated_adapter.set_notification_callback(std::sync::Arc::new(|_| {}));
        let (unrelated_sender, unrelated_receiver) = mpsc::channel();
        unrelated_adapter.set_identity_callback(std::sync::Arc::new(move |_, identity| {
            let _send_result = unrelated_sender.send(identity.expect("Linux identity").pid);
            Ok(())
        }));
        unrelated_adapter
            .send_request(
                "spawn-unrelated-identity-check",
                "process/spawn",
                json!({
                    "processHandle":"unrelated-identity-check",
                    "cwd":cwd.path(),
                    "command":["/bin/cat"],
                    "env":{"TERM":"xterm-256color","COLORTERM":"truecolor"},
                    "size":{"cols":100,"rows":28}
                }),
            )
            .expect("spawn unrelated live process");
        let unrelated_pid = unrelated_receiver.recv().expect("unrelated PID");
        stale.start_time_ticks = stale.start_time_ticks.saturating_add(1);
        assert!(cleanup_persisted_identity(&stale).is_err());
        assert!(std::fs::metadata(format!("/proc/{}/stat", stale.pid)).is_ok());
        let unrelated_stat = std::fs::read_to_string(format!("/proc/{unrelated_pid}/stat"))
            .expect("unrelated process remains present");
        let (_, unrelated_fields) = unrelated_stat
            .rsplit_once(") ")
            .expect("unrelated process stat fields");
        assert_ne!(
            unrelated_fields.split_whitespace().next(),
            Some("Z"),
            "stale identity refusal must not signal the unrelated live process"
        );
        adapter
            .send_request(
                "close-identity-check",
                "process/kill",
                json!({"processHandle":"identity-check"}),
            )
            .expect("close verified test process");
        unrelated_adapter
            .send_request(
                "close-unrelated-identity-check",
                "process/kill",
                json!({"processHandle":"unrelated-identity-check"}),
            )
            .expect("close unrelated process");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn identity_persistence_failure_reaps_the_new_child_before_refusing_spawn() {
        let cwd = terminal_cwd();
        let adapter = PtyTerminalSession::new();
        let (pid_sender, pid_receiver) = mpsc::channel();
        adapter.set_identity_callback(std::sync::Arc::new(move |_, identity| {
            let _send_result = pid_sender.send(identity.expect("Linux identity").pid);
            Err(crate::terminal_driver::SessionError::Disconnected)
        }));
        let result = adapter.send_request(
            "spawn-identity-failure",
            "process/spawn",
            json!({
                "processHandle":"identity-failure",
                "cwd":cwd.path(),
                "command":["/bin/cat"],
                "env":{"TERM":"xterm-256color","COLORTERM":"truecolor"},
                "size":{"cols":100,"rows":28}
            }),
        );
        assert_eq!(
            result,
            Err(crate::terminal_driver::SessionError::Disconnected)
        );
        let pid = pid_receiver.recv().expect("spawned child identity");
        let remains = std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(") ")
                .and_then(|(_, fields)| fields.split_whitespace().next())
                .is_some_and(|state| state != "Z")
        });
        assert!(!remains, "child survived refusal to persist its identity");
    }

    #[test]
    fn missing_restart_identity_does_not_disable_live_creation() {
        let cwd = terminal_cwd();
        let adapter = PtyTerminalSession::new();
        adapter.force_identity_unavailable_for_test();
        let (identity_sender, identity_receiver) = mpsc::channel();
        adapter.set_identity_callback(std::sync::Arc::new(move |_, identity| {
            let _send_result = identity_sender.send(identity.is_none());
            Ok(())
        }));
        adapter.set_notification_callback(std::sync::Arc::new(|_| {}));
        adapter
            .send_request(
                "spawn-without-restart-identity",
                "process/spawn",
                json!({
                    "processHandle":"without-restart-identity",
                    "cwd":cwd.path(),
                    "command":["/bin/cat"],
                    "env":{"TERM":"xterm-256color","COLORTERM":"truecolor"},
                    "size":{"cols":100,"rows":28}
                }),
            )
            .expect("live terminal creation is still available");
        assert!(identity_receiver.recv().expect("identity callback"));
        adapter
            .send_request(
                "close-without-restart-identity",
                "process/kill",
                json!({"processHandle":"without-restart-identity"}),
            )
            .expect("known child process group closes without persisted identity");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn restart_cleanup_does_not_signal_session_jobs_when_leader_identity_is_gone() {
        use std::io::{BufRead, BufReader, Write};

        let cwd = terminal_cwd();
        let fifo = cwd.path().join("terminal-fifo");
        let status = Command::new("/usr/bin/mkfifo")
            .arg(&fifo)
            .status()
            .expect("create harmless FIFO for blocked cat");
        assert!(status.success());
        let mut leader = Command::new("/usr/bin/setsid")
            .arg("/bin/sh")
            .arg("-c")
            .arg("set -m; /bin/sh -c 'trap \\\"\\\" HUP TERM; echo JOB_READY; exec /bin/cat terminal-fifo >/dev/null' & job=$!; echo CHILD_PID:$job; IFS= read -r release; exit 0")
            .current_dir(cwd.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start disposable session leader");
        let leader_pid = leader.id();
        let mut stdout = BufReader::new(leader.stdout.take().expect("leader output"));
        let mut child_pid = None;
        let mut saw_ready = false;
        while child_pid.is_none() || !saw_ready {
            let mut line = String::new();
            stdout.read_line(&mut line).expect("read child readiness");
            if let Some(pid) = line.trim().strip_prefix("CHILD_PID:") {
                child_pid = pid.parse::<u32>().ok();
            }
            saw_ready |= line.contains("JOB_READY");
        }
        let child_pid = child_pid.expect("background child PID");
        let stat = std::fs::read_to_string(format!("/proc/{leader_pid}/stat"))
            .expect("leader identity before controlled exit");
        let (_, fields) = stat.rsplit_once(')').expect("leader stat fields");
        let fields = fields.split_whitespace().collect::<Vec<_>>();
        let start_time_ticks = fields
            .get(19)
            .and_then(|value| value.parse::<u64>().ok())
            .expect("leader kernel start ticks");
        let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .expect("Linux boot ID")
            .trim()
            .to_owned();
        let identity = studio_runtime_process::PtyIdentity {
            pid: leader_pid,
            process_group: leader_pid,
            session: leader_pid,
            start_time_ticks,
            boot_id,
        };
        let stat = std::fs::read_to_string(format!("/proc/{child_pid}/stat"))
            .expect("session job alive before leader exit");
        let (_, fields) = stat.rsplit_once(')').expect("child stat fields");
        let fields = fields.split_whitespace().collect::<Vec<_>>();
        let child_group = fields
            .get(2)
            .and_then(|value| value.parse::<i32>().ok())
            .expect("child process group");
        assert_eq!(
            fields.get(3).and_then(|value| value.parse::<u32>().ok()),
            Some(leader_pid)
        );
        struct KillGroup(i32);
        impl Drop for KillGroup {
            fn drop(&mut self) {
                let _kill_result = Command::new("/bin/kill")
                    .args(["-KILL", "--", &format!("-{}", self.0)])
                    .status();
            }
        }
        let _cleanup_group = KillGroup(child_group);
        leader
            .stdin
            .take()
            .expect("leader input")
            .write_all(b"exit\n")
            .expect("release leader");
        leader.wait().expect("reap session leader");

        assert!(super::cleanup_persisted_identity(&identity).is_err());
        let stat = std::fs::read_to_string(format!("/proc/{child_pid}/stat"))
            .expect("surviving session job");
        assert_ne!(
            stat.rsplit_once(')')
                .and_then(|(_, fields)| fields.split_whitespace().next()),
            Some("Z"),
            "unverified stale session identity must not signal a surviving job"
        );
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
            let _send_result = identity_sender.send(identity.expect("Linux identity").clone());
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

    #[cfg(target_os = "linux")]
    #[test]
    fn close_kills_term_ignoring_job_after_reaping_term_responsive_leader() {
        let cwd = terminal_cwd();
        let adapter = PtyTerminalSession::new();
        let (sender, receiver) = mpsc::channel::<TerminalNotification>();
        adapter.set_notification_callback(std::sync::Arc::new(move |event| {
            let _send_result = sender.send(event);
        }));
        adapter
            .send_request(
                "spawn-surviving-job",
                "process/spawn",
                json!({
                    "processHandle":"surviving-job-session",
                    "cwd":cwd.path(),
                    "command":["/bin/bash","-c",r#"set -m; /usr/bin/python3 -c 'import signal; signal.signal(signal.SIGTERM, signal.SIG_IGN); print("JOB_READY", flush=True); signal.pause()' & job=$!; echo CHILD_PID:$job; wait"#],
                    "env":{"TERM":"xterm-256color","COLORTERM":"truecolor"},
                    "size":{"cols":100,"rows":28}
                }),
            )
            .expect("spawn leader and TERM-ignoring job");

        let mut output = String::new();
        let mut child_pid = None;
        while child_pid.is_none() || !output.contains("JOB_READY") {
            let event = receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("readiness handshake from TERM-ignoring job");
            if let TerminalEvent::Output(bytes) = event.event {
                output.push_str(&String::from_utf8_lossy(&bytes));
                child_pid = output
                    .lines()
                    .find_map(|line| line.strip_prefix("CHILD_PID:")?.trim().parse::<u32>().ok());
            }
        }
        let child_pid = child_pid.expect("reported child PID");
        let stat = std::fs::read_to_string(format!("/proc/{child_pid}/stat"))
            .expect("TERM-ignoring child remains before close");
        let (_, fields) = stat.rsplit_once(") ").expect("proc stat fields");
        let process_group = fields
            .split_whitespace()
            .nth(2)
            .and_then(|value| value.parse::<i32>().ok())
            .expect("child process group");
        assert_ne!(process_group, std::process::id() as i32);

        std::process::Command::new("/bin/kill")
            .args(["-TERM", "--", &format!("-{process_group}")])
            .status()
            .expect("send TERM to the job before terminal close");
        let survived_term =
            std::fs::read_to_string(format!("/proc/{child_pid}/stat")).is_ok_and(|stat| {
                stat.rsplit_once(") ")
                    .and_then(|(_, fields)| fields.split_whitespace().next())
                    .is_some_and(|state| state != "Z")
            });
        assert!(survived_term, "job must ignore TERM before the KILL pass");

        adapter
            .send_request(
                "close-surviving-job",
                "process/kill",
                json!({"processHandle":"surviving-job-session"}),
            )
            .expect("KILL surviving job after leader reap");
        loop {
            if let TerminalEvent::Exit(Some(_)) =
                receiver.recv().expect("leader exit notification").event
            {
                break;
            }
        }
        let remains =
            std::fs::read_to_string(format!("/proc/{child_pid}/stat")).is_ok_and(|stat| {
                stat.rsplit_once(") ")
                    .and_then(|(_, fields)| fields.split_whitespace().next())
                    .is_some_and(|state| state != "Z")
            });
        if remains {
            let _kill_result = std::process::Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{process_group}")])
                .status();
        }
        assert!(
            !remains,
            "TERM-ignoring session job survived leader reap and close"
        );
        adapter
            .send_request(
                "repeat-close-surviving-job",
                "process/kill",
                json!({"processHandle":"surviving-job-session"}),
            )
            .expect("second completed close is a no-op");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn close_has_a_bounded_deadline_when_the_terminal_ignores_term() {
        let cwd = terminal_cwd();
        let adapter = PtyTerminalSession::new();
        adapter.set_notification_callback(std::sync::Arc::new(|_| {}));
        adapter
            .send_request(
                "spawn-term-resistant",
                "process/spawn",
                json!({
                    "processHandle":"term-resistant",
                    "cwd":cwd.path(),
                    "command":["/bin/sh","-c","trap '' TERM; exec /bin/cat"],
                    "env":{"TERM":"xterm-256color","COLORTERM":"truecolor"},
                    "size":{"cols":100,"rows":28}
                }),
            )
            .expect("spawn TERM-resistant child");
        let began = std::time::Instant::now();
        adapter
            .send_request(
                "close-term-resistant",
                "process/kill",
                json!({"processHandle":"term-resistant"}),
            )
            .expect("KILL and reap TERM-resistant child");
        assert!(began.elapsed() < Duration::from_secs(3));
    }
}
