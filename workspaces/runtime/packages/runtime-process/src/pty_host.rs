//! Unix PTY host backed by the blocking `pty-process` API.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use pty_process::Size;
use pty_process::blocking::{self, Command, Pty};
use rustix::process::{Pid, Signal, kill_process_group};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::PtyHost;

#[derive(Debug, Error)]
pub enum PtyHostError {
    #[error(transparent)]
    Pty(#[from] pty_process::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("terminal process id is out of range")]
    ProcessId,
    #[error("terminal process lock was poisoned")]
    Lock,
    #[error("could not signal terminal process group: {0}")]
    Signal(rustix::io::Errno),
}

/// A restart-verifiable identity for a PTY session leader.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PtyIdentity {
    pub pid: u32,
    pub process_group: u32,
    pub session: u32,
    pub start_time_ticks: u64,
    pub boot_id: String,
}

/// Build the effective local shell environment used by Python terminal sessions.
/// The provider app-server starts from the backend environment, removes its
/// supervisor-only variables when supervised, then applies the five terminal
/// overrides from `codex_terminals.py`.
pub fn terminal_environment(terminal_home: &Path) -> Vec<(String, String)> {
    let inherited = std::env::vars().collect::<std::collections::BTreeMap<_, _>>();
    let supervisor = inherited
        .get("CODEX_AGENTS_SUPERVISOR_MODE")
        .map(String::as_str)
        == Some("1");
    terminal_environment_from(&inherited, terminal_home, supervisor)
        .into_iter()
        .collect()
}

pub fn terminal_environment_from(
    inherited: &std::collections::BTreeMap<String, String>,
    terminal_home: &Path,
    supervisor: bool,
) -> std::collections::BTreeMap<String, String> {
    let mut environment = inherited.clone();
    // CPython's locale coercion exports LC_CTYPE when the process started
    // without one; AppServer then passes that adjusted environment to children.
    environment
        .entry("LC_CTYPE".into())
        .or_insert_with(|| "C.UTF-8".into());
    environment.remove("OPENAI_API_KEY");
    environment.remove("CODEX_API_KEY");
    if supervisor {
        for name in [
            "CODEX_AGENTS_BACKEND_ID",
            "CODEX_RUNTIME_LOCK_METRICS",
            "CODEX_AGENTS_PROVIDER_CAPTURE",
            "CODEX_AGENTS_PROVIDER_CAPTURE_FILE",
            "CODEX_NODE",
        ] {
            environment.remove(name);
        }
    }
    environment
        .entry("CODEX_HOME".into())
        .or_insert_with(|| terminal_home.to_string_lossy().into_owned());
    for name in ["CODEX_HOME", "OPENAI_API_KEY", "CODEX_API_KEY"] {
        if let Some(value) = inherited.get(name) {
            environment.insert(name.into(), value.clone());
        }
    }
    environment.insert("TERM".into(), "xterm-256color".into());
    environment.insert("COLORTERM".into(), "truecolor".into());
    environment
}

/// A PTY master and the process-group leader attached to it.
#[derive(Clone)]
pub struct PtySession(Arc<PtyShared>);

struct PtyShared {
    master: Arc<Pty>,
    child: Mutex<PtyChild>,
    identity: Option<PtyIdentity>,
    session: u32,
    termination_attempted: AtomicBool,
}

struct PtyChild {
    process: Child,
    reaped: bool,
}

impl PtyShared {
    fn child(&self) -> Result<std::sync::MutexGuard<'_, PtyChild>, PtyHostError> {
        self.child
            .lock()
            .map_err(|_: PoisonError<_>| PtyHostError::Lock)
    }

    fn terminate(&self, deadline: Instant) -> Result<ExitStatus, PtyHostError> {
        let mut child = self.child()?;
        self.termination_attempted.store(true, Ordering::Release);
        if child.reaped {
            return child.process.try_wait()?.ok_or_else(|| {
                PtyHostError::Io(std::io::Error::other("reaped terminal child has no status"))
            });
        }
        let term_deadline = (Instant::now() + Duration::from_millis(500)).min(deadline);
        let _hangup_result =
            signal_session_groups(self.session, self.identity.as_ref(), Signal::TERM);
        while Instant::now() < term_deadline {
            if child.process.try_wait()?.is_some() {
                break;
            }
            std::thread::park_timeout(Duration::from_millis(5));
        }
        let signal_error =
            signal_session_groups(self.session, self.identity.as_ref(), Signal::KILL).err();
        let status = loop {
            if let Some(status) = child.process.try_wait()? {
                child.reaped = true;
                break status;
            }
            if Instant::now() >= deadline {
                return Err(PtyHostError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "terminal process did not reap before shutdown deadline",
                )));
            }
            std::thread::park_timeout(Duration::from_millis(5));
        };
        if let Some(error) = signal_error {
            return Err(PtyHostError::Signal(error));
        }
        Ok(status)
    }
}

impl Drop for PtyShared {
    fn drop(&mut self) {
        if !self.termination_attempted.load(Ordering::Acquire) {
            let _termination = self.terminate(Instant::now() + Duration::from_secs(2));
        }
    }
}

/// Process and PTY operations for a local terminal session.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalPtyHost;

impl PtyHost for LocalPtyHost {
    type Session = PtySession;
    type Error = PtyHostError;

    fn open(
        &self,
        command: &[String],
        cwd: &Path,
        env: &[(String, String)],
    ) -> Result<Self::Session, Self::Error> {
        let Some((program, args)) = command.split_first() else {
            return Err(PtyHostError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "terminal command is empty",
            )));
        };
        let (master, slave) = blocking::open()?;
        let child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(env.iter().map(|(key, value)| (key, value)))
            .spawn(slave)?;
        let raw_pid = match i32::try_from(child.id()) {
            Ok(value) => value,
            Err(_) => {
                let mut child = child;
                let _kill_result = child.kill();
                let _wait_result = child.wait();
                return Err(PtyHostError::ProcessId);
            }
        };
        if Pid::from_raw(raw_pid).is_none() {
            let mut child = child;
            let _kill_result = child.kill();
            let _wait_result = child.wait();
            return Err(PtyHostError::ProcessId);
        }
        let identity = process_identity(raw_pid);
        let session = identity
            .as_ref()
            .map_or(child.id(), |identity| identity.session);
        Ok(PtySession(Arc::new(PtyShared {
            master: Arc::new(master),
            child: Mutex::new(PtyChild {
                process: child,
                reaped: false,
            }),
            session,
            identity,
            termination_attempted: AtomicBool::new(false),
        })))
    }

    fn resize(
        &self,
        session: &mut Self::Session,
        columns: u16,
        rows: u16,
    ) -> Result<(), Self::Error> {
        session.0.master.resize(Size::new(rows, columns))?;
        Ok(())
    }

    fn write(&self, session: &mut Self::Session, bytes: &[u8]) -> Result<(), Self::Error> {
        let mut master = session.0.master.as_ref();
        master.write_all(bytes)?;
        master.flush()?;
        Ok(())
    }

    fn read(&self, session: &mut Self::Session, buffer: &mut [u8]) -> Result<usize, Self::Error> {
        let mut master = session.0.master.as_ref();
        master.read(buffer).map_err(PtyHostError::Io)
    }

    fn wait(&self, session: &mut Self::Session) -> Result<Option<i32>, Self::Error> {
        let mut child = session.0.child()?;
        let status = if child.reaped {
            child.process.try_wait()?
        } else {
            let status = child.process.wait()?;
            child.reaped = true;
            Some(status)
        };
        Ok(status.map(exit_code))
    }

    fn close(&self, session: Self::Session) -> Result<(), Self::Error> {
        let outcome = session
            .0
            .terminate(Instant::now() + Duration::from_secs(2))
            .map(|_| ());
        drop(session);
        outcome
    }
}

impl PtySession {
    pub fn identity(&self) -> Option<&PtyIdentity> {
        self.0.identity.as_ref()
    }
}

/// Verify a persisted PID against its start time and session before signaling.
pub fn cleanup_orphan(identity: &PtyIdentity) -> Result<bool, PtyHostError> {
    if identity.pid != identity.session || identity.pid != identity.process_group {
        return Ok(false);
    }
    if process_identity(i32::try_from(identity.pid).map_err(|_| PtyHostError::ProcessId)?).as_ref()
        != Some(identity)
    {
        return Ok(false);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    let _term = signal_session_groups(identity.session, Some(identity), Signal::TERM);
    while Instant::now() < deadline {
        std::thread::park_timeout(Duration::from_millis(5));
    }
    signal_session_groups(identity.session, Some(identity), Signal::KILL)
        .map_err(PtyHostError::Signal)?;
    Ok(true)
}

#[cfg(target_os = "linux")]
fn process_identity(pid: i32) -> Option<PtyIdentity> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(") ")?;
    let fields = fields.split_whitespace().collect::<Vec<_>>();
    let session = fields.get(3)?.parse().ok()?;
    let process_group = fields.get(2)?.parse().ok()?;
    let start_time_ticks = fields.get(19)?.parse().ok()?;
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()?
        .trim()
        .to_owned();
    Some(PtyIdentity {
        pid: u32::try_from(pid).ok()?,
        process_group,
        session,
        start_time_ticks,
        boot_id,
    })
}

#[cfg(not(target_os = "linux"))]
fn process_identity(_pid: i32) -> Option<PtyIdentity> {
    None
}

#[cfg(target_os = "linux")]
fn session_groups(session: u32) -> Vec<(u32, u32)> {
    let mut groups = std::collections::BTreeMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        let Some(stat) = process_identity(pid) else {
            continue;
        };
        if stat.session != session {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((_, fields)) = raw.rsplit_once(") ") else {
            continue;
        };
        let fields = fields.split_whitespace().collect::<Vec<_>>();
        if fields.first() == Some(&"Z") {
            continue;
        }
        if let Some(group) = fields.get(2).and_then(|value| value.parse::<u32>().ok()) {
            groups.entry(group).or_insert(pid as u32);
        }
    }
    groups.into_iter().collect()
}

#[cfg(not(target_os = "linux"))]
fn session_groups(session: u32) -> Vec<(u32, u32)> {
    use std::process::Command;
    let Ok(output) = Command::new("/bin/ps")
        .args(["-axo", "pid=,pgid=,sid=,stat="])
        .env_clear()
        .env("LC_ALL", "C")
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let mut groups = std::collections::BTreeMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() >= 4
            && !fields[3].starts_with('Z')
            && fields[2].parse::<u32>().ok() == Some(session)
            && let (Ok(group), Ok(pid)) = (fields[1].parse::<u32>(), fields[0].parse::<u32>())
        {
            groups.entry(group).or_insert(pid);
        }
    }
    groups.into_iter().collect()
}

fn signal_session_groups(
    session: u32,
    expected: Option<&PtyIdentity>,
    signal: Signal,
) -> Result<(), rustix::io::Errno> {
    let mut first_error = None;
    for (group_id, member_id) in session_groups(session) {
        #[cfg(target_os = "linux")]
        {
            if expected.is_some_and(|identity| {
                process_identity(identity.pid as i32).as_ref() != Some(identity)
            }) {
                continue;
            }
            let Some(member) = process_identity(member_id as i32) else {
                continue;
            };
            if member.session != session || member.process_group != group_id {
                continue;
            }
            // Revalidate both the persisted session leader and a live member
            // of this exact group immediately before the numeric group signal.
            if process_identity(member_id as i32).as_ref() != Some(&member) {
                continue;
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            if expected.is_some() || !non_linux_group_matches(session, group_id, member_id) {
                continue;
            }
        }
        let Some(group) = i32::try_from(group_id).ok().and_then(Pid::from_raw) else {
            continue;
        };
        if let Err(error) = kill_process_group(group, signal)
            && error != rustix::io::Errno::SRCH
        {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

#[cfg(not(target_os = "linux"))]
fn non_linux_group_matches(session: u32, group: u32, member: u32) -> bool {
    use std::process::Command;
    let Ok(output) = Command::new("/bin/ps")
        .args(["-o", "pgid=", "-o", "sid=", "-p"])
        .arg(member.to_string())
        .env_clear()
        .env("LC_ALL", "C")
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let fields = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    fields.first().and_then(|value| value.parse::<u32>().ok()) == Some(group)
        && fields.get(1).and_then(|value| value.parse::<u32>().ok()) == Some(session)
}

fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            128 + status.signal().unwrap_or_default()
        }
        #[cfg(not(unix))]
        {
            -1
        }
    })
}
