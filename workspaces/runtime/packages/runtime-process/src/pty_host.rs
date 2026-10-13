//! Unix PTY host backed by the blocking `pty-process` API.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
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
use crate::supervisor_identity::{signal_verified_group_member, start_time};

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
pub fn terminal_environment() -> BTreeMap<OsString, OsString> {
    let inherited = std::env::vars_os().collect::<BTreeMap<_, _>>();
    let supervisor =
        inherited.get(OsStr::new("CODEX_AGENTS_SUPERVISOR_MODE")) == Some(&OsString::from("1"));
    terminal_environment_from(&inherited, supervisor)
}

pub fn terminal_environment_from(
    inherited: &BTreeMap<OsString, OsString>,
    supervisor: bool,
) -> BTreeMap<OsString, OsString> {
    let mut environment = inherited.clone();
    if python_coerced_locale(inherited) {
        #[cfg(target_os = "macos")]
        let locale = "UTF-8";
        #[cfg(not(target_os = "macos"))]
        let locale = "C.UTF-8";
        environment.insert("LC_CTYPE".into(), locale.into());
    }
    if supervisor {
        for name in [
            "CODEX_AGENTS_BACKEND_ID",
            "CODEX_RUNTIME_LOCK_METRICS",
            "CODEX_AGENTS_PROVIDER_CAPTURE",
            "CODEX_AGENTS_PROVIDER_CAPTURE_FILE",
            "CODEX_NODE",
        ] {
            environment.remove(OsString::from(name).as_os_str());
        }
    }
    // Python's terminal overlay copies these values from its backend process
    // only when present; absent variables stay absent.
    for name in ["CODEX_HOME", "OPENAI_API_KEY", "CODEX_API_KEY"] {
        if let Some(value) = inherited.get(OsString::from(name).as_os_str()) {
            environment.insert(name.into(), value.clone());
        }
    }
    environment.insert("TERM".into(), "xterm-256color".into());
    environment.insert("COLORTERM".into(), "truecolor".into());
    environment
}

fn python_coerced_locale(inherited: &BTreeMap<OsString, OsString>) -> bool {
    if inherited
        .get(OsStr::new("LC_ALL"))
        .is_some_and(|value| !value.is_empty())
        || inherited
            .get(OsStr::new("PYTHONCOERCECLOCALE"))
            .and_then(|value| value.to_str())
            == Some("0")
    {
        return false;
    }
    let ctype = inherited
        .get(OsStr::new("LC_CTYPE"))
        .filter(|value| !value.is_empty());
    let language = inherited
        .get(OsStr::new("LANG"))
        .filter(|value| !value.is_empty());
    let current = ctype
        .or(language)
        .and_then(|value| value.to_str())
        .is_some_and(|value| {
            value.eq_ignore_ascii_case("C") || value.eq_ignore_ascii_case("POSIX")
        });
    let no_locale = ctype.is_none() && language.is_none();
    current || no_locale
}

/// A PTY master and the process-group leader attached to it.
#[derive(Clone)]
pub struct PtySession(Arc<PtyShared>);

struct PtyShared {
    master: Arc<Pty>,
    child: Mutex<PtyChild>,
    identity: Option<PtyIdentity>,
    session: u32,
    process_group: u32,
    termination_attempted: AtomicBool,
}

struct PtyChild {
    process: Child,
    status: Option<ExitStatus>,
}

#[derive(Clone)]
struct GroupMember {
    pid: u32,
    start_time: String,
}

#[derive(Clone)]
struct SessionGroup {
    id: u32,
    members: Vec<GroupMember>,
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
        let already_reaped = child.status.is_some();
        // Freeze all session groups and member identities before TERM. The
        // KILL pass uses this same snapshot even if TERM reaps the leader.
        let groups = session_groups(self.session);
        let term_deadline = (Instant::now() + Duration::from_millis(500)).min(deadline);
        let _term_result = signal_groups(
            &groups,
            self.session,
            self.process_group,
            child.process.id(),
            Signal::TERM,
            !already_reaped,
        );
        #[cfg(target_os = "linux")]
        while !already_reaped && Instant::now() < term_deadline {
            if let Some(status) = child.process.try_wait()? {
                child.status = Some(status);
                break;
            }
            std::thread::park_timeout(Duration::from_millis(10));
        }
        #[cfg(not(target_os = "linux"))]
        while !already_reaped && Instant::now() < term_deadline {
            std::thread::park_timeout(Duration::from_millis(10));
        }
        let signal_error = signal_groups(
            &groups,
            self.session,
            self.process_group,
            child.process.id(),
            Signal::KILL,
            false,
        )
        .err();
        if already_reaped {
            if let Some(error) = signal_error {
                return Err(PtyHostError::Signal(error));
            }
            return child.status.ok_or_else(|| {
                PtyHostError::Io(std::io::Error::other("reaped terminal child has no status"))
            });
        }
        let status = loop {
            if let Some(status) = child.process.try_wait()? {
                child.status = Some(status);
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
        env: &[(OsString, OsString)],
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
        // pty-process calls setsid in the child before exec, so the child PID
        // is the known session and process-group ID on every supported Unix.
        let session = child.id();
        let process_group = child.id();
        Ok(PtySession(Arc::new(PtyShared {
            master: Arc::new(master),
            child: Mutex::new(PtyChild {
                process: child,
                status: None,
            }),
            session,
            process_group,
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
        if child.status.is_none() {
            child.status = child.process.try_wait()?;
        }
        Ok(child.status.map(exit_code))
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

/// Verify a persisted Linux leader identity, then clean every captured group.
/// Platforms without the shared Linux identity/session source never signal a
/// persisted identity; live close still owns and terminates its child group.
pub fn cleanup_orphan(identity: &PtyIdentity) -> Result<bool, PtyHostError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = identity;
        return Ok(false);
    }
    #[cfg(target_os = "linux")]
    {
        let expected = identity.start_token();
        if identity.pid != identity.session || identity.pid != identity.process_group {
            return Ok(false);
        }
        let groups = session_groups(identity.session);
        if start_time(identity.pid, false)
            .map_err(supervisor_error)?
            .as_deref()
            != Some(&expected)
        {
            if groups.is_empty() {
                return Ok(false);
            }
            return Err(PtyHostError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "terminal leader identity is unavailable while session jobs remain; refusing to signal",
            )));
        }
        signal_groups(
            &groups,
            identity.session,
            identity.process_group,
            identity.pid,
            Signal::TERM,
            false,
        )
        .map_err(PtyHostError::Signal)?;
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            std::thread::park_timeout(Duration::from_millis(10));
        }
        signal_groups(
            &groups,
            identity.session,
            identity.process_group,
            identity.pid,
            Signal::KILL,
            false,
        )
        .map_err(PtyHostError::Signal)?;
        Ok(true)
    }
}

impl PtyIdentity {
    fn start_token(&self) -> String {
        format!("linux:{}:{}", self.boot_id, self.start_time_ticks)
    }
}

#[cfg(target_os = "linux")]
fn process_identity(pid: i32) -> Option<PtyIdentity> {
    let pid = u32::try_from(pid).ok()?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(')')?;
    let fields = fields.split_whitespace().collect::<Vec<_>>();
    let process_group = fields.get(2)?.parse().ok()?;
    let session = fields.get(3)?.parse().ok()?;
    let start = start_time(pid, false).ok()??;
    let (boot_id, ticks) = start.strip_prefix("linux:")?.split_once(':')?;
    Some(PtyIdentity {
        pid,
        process_group,
        session,
        start_time_ticks: ticks.parse().ok()?,
        boot_id: boot_id.to_owned(),
    })
}

#[cfg(not(target_os = "linux"))]
fn process_identity(_pid: i32) -> Option<PtyIdentity> {
    None
}

#[cfg(target_os = "linux")]
fn session_groups(session: u32) -> Vec<SessionGroup> {
    let mut groups = BTreeMap::<u32, Vec<GroupMember>>::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let Ok(raw) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((_, tail)) = raw.rsplit_once(')') else {
            continue;
        };
        let fields = tail.split_whitespace().collect::<Vec<_>>();
        if fields.first() == Some(&"Z")
            || fields.get(3).and_then(|v| v.parse::<u32>().ok()) != Some(session)
        {
            continue;
        }
        let Some(group) = fields.get(2).and_then(|v| v.parse::<u32>().ok()) else {
            continue;
        };
        let Some(start_time) = start_time(pid, false).ok().flatten() else {
            continue;
        };
        groups
            .entry(group)
            .or_default()
            .push(GroupMember { pid, start_time });
    }
    groups
        .into_iter()
        .map(|(id, mut members)| {
            members.sort_by_key(|member| member.pid);
            SessionGroup { id, members }
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn session_groups(_session: u32) -> Vec<SessionGroup> {
    Vec::new()
}

fn signal_groups(
    groups: &[SessionGroup],
    session: u32,
    owned_group: u32,
    owned_pid: u32,
    signal: Signal,
    allow_owned_fallback: bool,
) -> Result<(), rustix::io::Errno> {
    #[cfg(target_os = "linux")]
    {
        let mut ordered = groups.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|group| group.id == session);
        let mut first_error = None;
        let mut captured_owned_group = false;
        for group in ordered {
            if group.id <= 1 {
                continue;
            }
            captured_owned_group |= group.id == owned_group;
            let mut signalled = false;
            for member in &group.members {
                match signal_verified_group_member(
                    member.pid,
                    group.id,
                    session,
                    &member.start_time,
                    signal,
                ) {
                    Ok(true) => {
                        signalled = true;
                        break;
                    }
                    Ok(false) => {}
                    Err(_) => {
                        first_error.get_or_insert(rustix::io::Errno::PERM);
                    }
                }
            }
            if allow_owned_fallback && !signalled && group.id == owned_group {
                // The fallback is only used during TERM, before the child is
                // reaped. The later KILL pass relies on captured members.
                if let Some(pid) = i32::try_from(owned_pid).ok().and_then(Pid::from_raw)
                    && let Err(error) = kill_process_group(pid, signal)
                    && error != rustix::io::Errno::SRCH
                {
                    first_error.get_or_insert(error);
                }
            }
        }
        if allow_owned_fallback
            && !captured_owned_group
            && let Some(pid) = i32::try_from(owned_pid).ok().and_then(Pid::from_raw)
            && let Err(error) = kill_process_group(pid, signal)
            && error != rustix::io::Errno::SRCH
        {
            first_error.get_or_insert(error);
        }
        first_error.map_or(Ok(()), Err)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (groups, session, owned_pid, allow_owned_fallback);
        let pid = i32::try_from(owned_group).ok().and_then(Pid::from_raw);
        let Some(pid) = pid else {
            return Err(rustix::io::Errno::INVAL);
        };
        match kill_process_group(pid, signal) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn supervisor_io_error(error: &crate::supervisor_daemon::Error) -> std::io::Error {
    match error {
        crate::supervisor_daemon::Error::Io(error) => {
            std::io::Error::new(error.kind(), "identity check failed")
        }
        _ => std::io::Error::other("identity check failed"),
    }
}

fn supervisor_error(error: crate::supervisor_daemon::Error) -> PtyHostError {
    PtyHostError::Io(supervisor_io_error(&error))
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
