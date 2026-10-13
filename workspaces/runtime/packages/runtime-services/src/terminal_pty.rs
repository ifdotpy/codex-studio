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
    Arc<dyn Fn(&str, Option<&PtyIdentity>) -> Result<(), SessionError> + Send + Sync>;

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
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let live = self
            .sessions
            .lock()
            .map_err(|_| SessionError::Disconnected)?
            .remove(id);
        if let Some(mut live) = live {
            let close_failed = self.host.close(live.session).is_err();
            let reader_failed = live.reader.take().is_some_and(|reader| {
                match live
                    .reader_done
                    .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                {
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
    use std::collections::{BTreeMap, BTreeSet};
    use std::process::Command;
    use std::sync::mpsc;
    use std::time::Duration;

    use serde_json::json;
    use tempfile::TempDir;

    use super::PtyTerminalSession;
    use crate::terminal_driver::prepared_python;
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
    fn terminal_child_environment_key_set_matches_python_launch_path() {
        let cwd = terminal_cwd();
        let source =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let python = prepared_python::prepared_python(&source);
        for supervisor in [false, true] {
            let base = BTreeMap::from([
                ("HOME".into(), "/home/test-user".into()),
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("SSH_AUTH_SOCK".into(), "/tmp/ssh-agent.sock".into()),
                ("DISPLAY".into(), ":42".into()),
                ("XDG_CONFIG_HOME".into(), "/home/test-user/.config".into()),
                ("CODEX_HOME".into(), "/home/test-user/.codex".into()),
                ("OPENAI_API_KEY".into(), "redacted-test-value".into()),
                ("CODEX_API_KEY".into(), "redacted-test-value".into()),
                ("CODEX_AGENTS_BACKEND_ID".into(), "server-only".into()),
                ("CODEX_NODE".into(), "/server/node".into()),
                ("STUDIO_TERMINAL_SERVER_ONLY".into(), "inherited".into()),
                (
                    "CODEX_AGENTS_SUPERVISOR_MODE".into(),
                    if supervisor { "1" } else { "0" }.into(),
                ),
            ]);
            let terminal_home = cwd.path().join("terminal-home");
            let rust_env = studio_runtime_process::terminal_environment_from(
                &base,
                &terminal_home,
                supervisor,
            );
            let mut python_command = Command::new(python.as_os_str());
            python_command.env_clear().envs(&base)
                .arg("-c")
                .arg("import json, os, subprocess; home=os.environ.pop('TERMINAL_PRIVATE_HOME'); env=os.environ.copy(); env['CODEX_HOME']=home; env.pop('OPENAI_API_KEY',None); env.pop('CODEX_API_KEY',None); names=('CODEX_HOME','OPENAI_API_KEY','CODEX_API_KEY'); overlay={k:os.environ[k] for k in names if k in os.environ}; overlay.update(TERM='xterm-256color',COLORTERM='truecolor'); env.update(overlay);\nif os.environ.get('CODEX_AGENTS_SUPERVISOR_MODE')=='1':\n  [env.pop(k,None) for k in ('CODEX_AGENTS_BACKEND_ID','CODEX_RUNTIME_LOCK_METRICS','CODEX_AGENTS_PROVIDER_CAPTURE','CODEX_AGENTS_PROVIDER_CAPTURE_FILE','CODEX_NODE')]\nr=subprocess.run(['/usr/bin/env','-0'],env=env,check=True,capture_output=True); print(json.dumps(sorted(x.split(b'=',1)[0].decode() for x in r.stdout.split(b'\\0') if x)))")
                .env("TERMINAL_PRIVATE_HOME", &terminal_home);
            let python_output = python_command
                .output()
                .expect("run Python environment oracle");
            assert!(python_output.status.success(), "Python oracle starts");
            let python_keys: BTreeSet<String> =
                serde_json::from_slice(&python_output.stdout).expect("Python key set only");

            let adapter = PtyTerminalSession::new();
            let (sender, receiver) = mpsc::channel::<TerminalNotification>();
            adapter.set_notification_callback(std::sync::Arc::new(move |event| {
                let _sent = sender.send(event);
            }));
            adapter
                .send_request(
                    "spawn-env-parity",
                    "process/spawn",
                    json!({
                        "processHandle": if supervisor {"env-supervised"} else {"env-normal"},
                        "cwd":cwd.path(), "command":["/usr/bin/env","-0"],
                        "env":rust_env, "size":{"cols":100,"rows":28}
                    }),
                )
                .expect("spawn env probe");
            let mut bytes = Vec::new();
            while let TerminalEvent::Output(chunk) =
                receiver.recv().expect("environment child event").event
            {
                bytes.extend(chunk);
            }
            let rust_keys: BTreeSet<String> = bytes
                .split(|byte| *byte == 0)
                .filter(|pair| !pair.is_empty())
                .filter_map(|pair| {
                    let mut fields = pair.splitn(2, |byte| *byte == b'=');
                    fields
                        .next()
                        .and_then(|key| std::str::from_utf8(key).ok())
                        .map(str::to_owned)
                })
                .collect();
            assert_eq!(
                rust_keys, python_keys,
                "shell key set differs (supervisor={supervisor})"
            );
            assert!(
                rust_keys.contains("HOME")
                    && rust_keys.contains("PATH")
                    && rust_keys.contains("SSH_AUTH_SOCK")
            );
            assert!(rust_keys.contains("STUDIO_TERMINAL_SERVER_ONLY"));
            if supervisor {
                assert!(
                    !rust_keys.contains("CODEX_AGENTS_BACKEND_ID")
                        && !rust_keys.contains("CODEX_NODE")
                );
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
