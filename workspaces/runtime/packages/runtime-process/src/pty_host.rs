//! Unix PTY host backed by the blocking `pty-process` API.
//!
//! Terminals can be created without a restart-verifiable process identity,
//! matching Python. In that case restart cleanup leaves the row marked as an
//! unresolved cleanup and never signals the saved PID. Linux and macOS verify
//! the boot ID and process start instant before signalling. The member
//! verify-to-signal gap remains narrower than Python's unverified `killpg`, but
//! cannot be made atomic without pidfd support.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::fd::AsFd;
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
use crate::child_priority::ChildPriority;
#[cfg(target_os = "linux")]
use crate::supervisor_identity::ProcStat;
#[cfg(target_os = "macos")]
use crate::supervisor_identity::{MICROS_PER_SECOND, macos_start_identity};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::supervisor_identity::{boot_id, signal_verified_group_member, start_time};

#[cfg(test)]
type BeforeKillHook = Box<dyn Fn(bool) + Send>;

/// How long close waits for the terminal session to exit after SIGTERM before
/// it sends SIGKILL. The wait ends early when the leader is reaped.
const TERM_GRACE: Duration = Duration::from_millis(500);

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
    #[error("terminal input write timed out; delivery is uncertain")]
    InputTimeout,
}

/// A restart-verifiable identity for a PTY session leader.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PtyIdentity {
    pub pid: u32,
    pub process_group: u32,
    pub session: u32,
    /// The kernel start instant of the leader. Linux: clock ticks since boot
    /// (`/proc/<pid>/stat` field 22). macOS: microseconds since the Unix epoch
    /// (libproc `pbi_start_tvsec` and `pbi_start_tvusec`). The boot ID makes an
    /// identity from another boot or platform never match.
    pub start_time_ticks: u64,
    /// Linux `/proc/sys/kernel/random/boot_id`; macOS `kern.bootsessionuuid`.
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
    termination_complete: AtomicBool,
    captured_groups: Mutex<Vec<SessionGroup>>,
    #[cfg(test)]
    before_kill: Mutex<Option<BeforeKillHook>>,
    #[cfg(test)]
    fail_next_kill: AtomicBool,
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
        self.terminate_with_term_grace(TERM_GRACE, deadline)
    }

    fn terminate_with_term_grace(
        &self,
        term_grace: Duration,
        deadline: Instant,
    ) -> Result<ExitStatus, PtyHostError> {
        let mut child = self.child()?;
        self.termination_attempted.store(true, Ordering::Release);
        if self.termination_complete.load(Ordering::Acquire) {
            return child.status.ok_or_else(|| {
                PtyHostError::Io(std::io::Error::other(
                    "completed terminal has no exit status",
                ))
            });
        }
        let already_reaped = child.status.is_some();
        // Keep the first ownership snapshot for retry and repeated-close safety.
        let groups = {
            let mut captured = self
                .captured_groups
                .lock()
                .map_err(|_| PtyHostError::Lock)?;
            if captured.is_empty() && !already_reaped {
                *captured = session_groups(self.session);
            }
            captured.clone()
        };
        let term_deadline = (Instant::now() + term_grace).min(deadline);
        let _term_result = signal_groups(
            &groups,
            self.session,
            self.process_group,
            child.process.id(),
            Signal::TERM,
            !already_reaped,
        );
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        while !already_reaped && Instant::now() < term_deadline {
            if let Some(status) = child.process.try_wait()? {
                child.status = Some(status);
                break;
            }
            std::thread::park_timeout(Duration::from_millis(10));
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        while !already_reaped && Instant::now() < term_deadline {
            std::thread::park_timeout(Duration::from_millis(10));
        }
        #[cfg(test)]
        if let Ok(hook) = self.before_kill.lock()
            && let Some(hook) = hook.as_ref()
        {
            hook(child.status.is_some());
        }
        // The live PTY retains ownership of this session while close runs, so
        // match Python and kill every group in the fresh session enumeration.
        let current_groups = fresh_groups_for_kill(self.session, &groups, session_groups, false)?;
        #[cfg(test)]
        let signal_error = if self.fail_next_kill.swap(false, Ordering::AcqRel) {
            Some(rustix::io::Errno::PERM)
        } else {
            signal_groups(
                &current_groups,
                self.session,
                self.process_group,
                child.process.id(),
                Signal::KILL,
                false,
            )
            .err()
        };
        #[cfg(not(test))]
        let signal_error = signal_groups(
            &current_groups,
            self.session,
            self.process_group,
            child.process.id(),
            Signal::KILL,
            false,
        )
        .err();
        if already_reaped {
            if let Some(error) = signal_error
                && !crate::owned_process_group::vanished_after_reap(
                    Some(self.process_group),
                    &std::io::Error::from_raw_os_error(error.raw_os_error()),
                )
            {
                return Err(PtyHostError::Signal(error));
            }
            wait_for_session_processes(self.session, deadline)?;
            self.termination_complete.store(true, Ordering::Release);
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
        if let Some(error) = signal_error
            && !crate::owned_process_group::vanished_after_reap(
                Some(self.process_group),
                &std::io::Error::from_raw_os_error(error.raw_os_error()),
            )
        {
            return Err(PtyHostError::Signal(error));
        }
        wait_for_session_processes(self.session, deadline)?;
        self.termination_complete.store(true, Ordering::Release);
        Ok(status)
    }
}

fn wait_for_session_processes(session: u32, deadline: Instant) -> Result<(), PtyHostError> {
    while !session_groups(session).is_empty() {
        if Instant::now() >= deadline {
            return Err(PtyHostError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "terminal session jobs did not exit before shutdown deadline",
            )));
        }
        std::thread::park_timeout(Duration::from_millis(5));
    }
    Ok(())
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
        ChildPriority::from_environment().apply_after_spawn(&child);
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
            termination_complete: AtomicBool::new(false),
            captured_groups: Mutex::new(Vec::new()),
            #[cfg(test)]
            before_kill: Mutex::new(None),
            #[cfg(test)]
            fail_next_kill: AtomicBool::new(false),
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
        let master = session.0.master.as_ref();
        write_bounded(master, bytes, Instant::now() + Duration::from_secs(2))?;
        let mut master = master;
        master.flush()?;
        Ok(())
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "PTY reads run on the dedicated terminal-pty reader thread"
    )]
    fn read(&self, session: &mut Self::Session, buffer: &mut [u8]) -> Result<usize, Self::Error> {
        let mut master = session.0.master.as_ref();
        loop {
            match master.read(buffer) {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                result => return result.map_err(PtyHostError::Io),
            }
        }
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

#[allow(
    clippy::disallowed_methods,
    reason = "PTY writes run on the labeled terminal I/O blocking-pool worker"
)]
fn write_bounded(master: &Pty, bytes: &[u8], deadline: Instant) -> Result<(), PtyHostError> {
    let flags = rustix::fs::fcntl_getfl(master.as_fd()).map_err(std::io::Error::from)?;
    rustix::fs::fcntl_setfl(master.as_fd(), flags | rustix::fs::OFlags::NONBLOCK)
        .map_err(std::io::Error::from)?;
    let mut master = master;
    write_all_until(
        bytes,
        deadline,
        Instant::now,
        |remaining| master.write(remaining),
        std::thread::sleep,
    )
}

fn write_all_until(
    bytes: &[u8],
    deadline: Instant,
    mut now: impl FnMut() -> Instant,
    mut write: impl FnMut(&[u8]) -> std::io::Result<usize>,
    mut sleep: impl FnMut(Duration),
) -> Result<(), PtyHostError> {
    const RETRY_DELAY: Duration = Duration::from_millis(10);
    let mut written = 0;
    while written < bytes.len() {
        let current = now();
        if current >= deadline {
            return Err(PtyHostError::InputTimeout);
        }
        match write(&bytes[written..]) {
            Ok(0) => return Err(PtyHostError::InputTimeout),
            Ok(count) => written += count,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                sleep(RETRY_DELAY.min(deadline.saturating_duration_since(now())));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                sleep(Duration::from_millis(1).min(deadline.saturating_duration_since(now())));
            }
            Err(error) => return Err(PtyHostError::Io(error)),
        }
    }
    Ok(())
}

impl PtySession {
    pub fn identity(&self) -> Option<&PtyIdentity> {
        self.0.identity.as_ref()
    }
}

/// Verify a persisted Linux or macOS leader identity, then clean every captured
/// group. Platforms without a native identity/session source never signal a
/// persisted identity; live close still owns and terminates its child group.
pub fn cleanup_orphan(identity: &PtyIdentity) -> Result<bool, PtyHostError> {
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = identity;
        Ok(false)
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
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
        // Restart cleanup has no live PTY ownership, so a captured live member
        // must still anchor this session before newly discovered groups are used.
        let current_groups =
            fresh_groups_for_kill(identity.session, &groups, session_groups, true)?;
        signal_groups(
            &current_groups,
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
    #[cfg(target_os = "linux")]
    fn start_token(&self) -> String {
        format!("linux:{}:{}", self.boot_id, self.start_time_ticks)
    }

    #[cfg(target_os = "macos")]
    fn start_token(&self) -> String {
        macos_start_identity(
            &self.boot_id,
            self.start_time_ticks / MICROS_PER_SECOND,
            self.start_time_ticks % MICROS_PER_SECOND,
        )
    }
}

#[cfg(target_os = "linux")]
fn process_identity(pid: i32) -> Option<PtyIdentity> {
    let pid = u32::try_from(pid).ok()?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let stat = ProcStat::parse(&stat).ok()?;
    if stat.state == 'Z' {
        return None;
    }
    let boot_id = boot_id().ok()?;
    Some(PtyIdentity {
        pid,
        process_group: stat.process_group,
        session: stat.session,
        start_time_ticks: stat.start_ticks,
        boot_id,
    })
}

#[cfg(target_os = "macos")]
fn process_identity(pid: i32) -> Option<PtyIdentity> {
    let raw = Pid::from_raw(pid)?;
    let pid = u32::try_from(pid).ok()?;
    let info = studio_runtime_proc_sys::bsd_info(pid, false).ok()??;
    let session = rustix::process::getsid(Some(raw)).ok()?;
    let boot_id = boot_id().ok()?;
    Some(PtyIdentity {
        pid,
        process_group: info.process_group,
        session: u32::try_from(session.as_raw_nonzero().get()).ok()?,
        start_time_ticks: info
            .start_seconds
            .checked_mul(MICROS_PER_SECOND)?
            .checked_add(info.start_microseconds)?,
        boot_id,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_identity(_pid: i32) -> Option<PtyIdentity> {
    None
}

#[cfg(target_os = "linux")]
fn session_groups(session: u32) -> Vec<SessionGroup> {
    let mut groups = BTreeMap::<u32, Vec<GroupMember>>::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let Ok(boot_id) = boot_id() else {
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
        let Ok(stat) = ProcStat::parse(&raw) else {
            continue;
        };
        if stat.state == 'Z' || stat.session != session {
            continue;
        }
        groups
            .entry(stat.process_group)
            .or_default()
            .push(GroupMember {
                pid,
                start_time: stat.start_identity(&boot_id),
            });
    }
    groups
        .into_iter()
        .map(|(id, mut members)| {
            members.sort_by_key(|member| member.pid);
            SessionGroup { id, members }
        })
        .collect()
}

/// macOS has no `/proc`: list every PID with libproc, keep the PIDs whose
/// `getsid` is this session, then read each member's libproc record. Zombies
/// are skipped as on Linux. A member whose record is unreadable (`EPERM` for
/// another user's process, such as a `sudo` job) stays listed with no
/// verifiable identity: its group is never signalled through it, the signal
/// pass reports the refusal, and close reports that session jobs remain.
#[cfg(target_os = "macos")]
fn session_groups(session: u32) -> Vec<SessionGroup> {
    let Ok(pids) = studio_runtime_proc_sys::all_pids() else {
        return Vec::new();
    };
    let Ok(boot_id) = boot_id() else {
        return Vec::new();
    };
    let mut groups = BTreeMap::<u32, Vec<GroupMember>>::new();
    for pid in pids {
        let Some(raw) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
            continue;
        };
        if !rustix::process::getsid(Some(raw)).is_ok_and(|sid| raw_id(sid) == Some(session)) {
            continue;
        }
        let (process_group, start_time) = match studio_runtime_proc_sys::bsd_info(pid, false) {
            Ok(Some(info)) if !info.is_zombie() => (
                info.process_group,
                macos_start_identity(&boot_id, info.start_seconds, info.start_microseconds),
            ),
            Ok(_) => continue,
            Err(_) => match rustix::process::getpgid(Some(raw)).ok().and_then(raw_id) {
                Some(group) => (group, String::new()),
                None => continue,
            },
        };
        groups
            .entry(process_group)
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

#[cfg(target_os = "macos")]
fn raw_id(pid: Pid) -> Option<u32> {
    u32::try_from(pid.as_raw_nonzero().get()).ok()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn session_groups(_session: u32) -> Vec<SessionGroup> {
    Vec::new()
}

#[cfg(target_os = "linux")]
fn has_live_anchor(groups: &[SessionGroup], session: u32) -> bool {
    let Ok(boot_id) = boot_id() else {
        return false;
    };
    groups.iter().any(|group| {
        group.members.iter().any(|member| {
            let current = std::fs::read_to_string(format!("/proc/{}/stat", member.pid))
                .ok()
                .and_then(|raw| ProcStat::parse(&raw).ok());
            current.is_some_and(|stat| {
                stat.state != 'Z'
                    && stat.process_group == group.id
                    && stat.session == session
                    && stat.start_identity(&boot_id) == member.start_time
            })
        })
    })
}

#[cfg(target_os = "macos")]
fn has_live_anchor(groups: &[SessionGroup], session: u32) -> bool {
    groups.iter().any(|group| {
        group.members.iter().any(|member| {
            let Some(raw) = i32::try_from(member.pid).ok().and_then(Pid::from_raw) else {
                return false;
            };
            let same_start = || {
                start_time(member.pid, false)
                    .ok()
                    .flatten()
                    .is_some_and(|current| current == member.start_time)
            };
            // Read the start again after the group reads, so a PID replaced
            // between them cannot lend its group or session to the member.
            same_start()
                && rustix::process::getpgid(Some(raw))
                    .ok()
                    .and_then(raw_id)
                    .is_some_and(|current| current == group.id)
                && rustix::process::getsid(Some(raw))
                    .ok()
                    .and_then(raw_id)
                    .is_some_and(|current| current == session)
                && same_start()
        })
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn has_live_anchor(_groups: &[SessionGroup], _session: u32) -> bool {
    true
}

fn fresh_groups_for_kill(
    session: u32,
    captured: &[SessionGroup],
    enumerate: impl FnOnce(u32) -> Vec<SessionGroup>,
    require_anchor: bool,
) -> Result<Vec<SessionGroup>, PtyHostError> {
    let current = enumerate(session);
    if require_anchor && !current.is_empty() && !has_live_anchor(captured, session) {
        return Err(PtyHostError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "terminal session jobs remain without a verified original member",
        )));
    }
    Ok(current)
}

fn signal_groups(
    groups: &[SessionGroup],
    session: u32,
    owned_group: u32,
    owned_pid: u32,
    signal: Signal,
    allow_owned_fallback: bool,
) -> Result<(), rustix::io::Errno> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
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
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    #[cfg(target_os = "linux")]
    use std::sync::mpsc;
    use std::thread;

    use super::{LocalPtyHost, PtyHost, PtyHostError, write_all_until, write_bounded};

    /// Upper bound for a test process to react to SIGTERM. A close under test
    /// with this grace ends its TERM wait as soon as the leader is reaped, so
    /// the bound only limits how long a broken leader can hang the test.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    const TEST_TERM_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

    /// Upper bound for a whole close under test: the TERM grace, the time the
    /// test holds the pre-KILL hook, and the reap after KILL.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    const TEST_CLOSE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

    /// Close a session as `LocalPtyHost::close` does, with the test bounds in
    /// place of the product TERM grace and close deadline.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn close_with_test_bounds(session: super::PtySession) -> Result<(), PtyHostError> {
        let outcome = session
            .0
            .terminate_with_term_grace(
                TEST_TERM_GRACE,
                std::time::Instant::now() + TEST_CLOSE_DEADLINE,
            )
            .map(|_| ());
        drop(session);
        outcome
    }

    #[test]
    fn bounded_write_checks_deadline_after_interruption_and_partial_progress() {
        let start = std::time::Instant::now();
        let deadline = start + std::time::Duration::from_millis(5);
        let elapsed = std::cell::Cell::new(std::time::Duration::ZERO);
        let attempts = std::cell::Cell::new(0);
        let result = write_all_until(
            b"payload",
            deadline,
            || start + elapsed.get(),
            |_| {
                attempts.set(attempts.get() + 1);
                Err(std::io::Error::from(std::io::ErrorKind::Interrupted))
            },
            |_| elapsed.set(std::time::Duration::from_millis(5)),
        );
        assert!(matches!(result, Err(PtyHostError::InputTimeout)));
        assert_eq!(
            attempts.get(),
            1,
            "interrupted writes must not bypass deadline"
        );

        let elapsed = std::cell::Cell::new(std::time::Duration::ZERO);
        let attempts = std::cell::Cell::new(0);
        let result = write_all_until(
            b"payload",
            deadline,
            || start + elapsed.get(),
            |_| {
                attempts.set(attempts.get() + 1);
                elapsed.set(std::time::Duration::from_millis(5));
                Ok(1)
            },
            |_| panic!("partial writes should not sleep"),
        );
        assert!(matches!(result, Err(PtyHostError::InputTimeout)));
        assert_eq!(
            attempts.get(),
            1,
            "partial progress must not bypass deadline"
        );
    }

    #[test]
    fn backpressured_terminal_input_returns_uncertain_instead_of_blocking() {
        let root = std::env::temp_dir().join(format!("pty-backpressure-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create temporary working directory");
        let host = LocalPtyHost;
        let command = [
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "stty raw -echo; printf READY; sleep 10".to_owned(),
        ];
        let mut session = host.open(&command, &root, &[]).expect("open test PTY");
        let mut output = Vec::new();
        while !output.windows(5).any(|window| window == b"READY") {
            let mut buffer = [0; 128];
            let count = host.read(&mut session, &mut buffer).expect("read READY");
            assert_ne!(count, 0, "test shell exited before READY");
            output.extend_from_slice(&buffer[..count]);
        }
        let master = session.0.master.as_ref();
        let error = write_bounded(
            master,
            &[b'x'; 64 * 1024],
            std::time::Instant::now() + std::time::Duration::from_millis(50),
        )
        .expect_err("non-reading shell must time out with uncertain delivery");
        assert!(matches!(error, PtyHostError::InputTimeout));
        host.close(session).expect("close test PTY");
        std::fs::remove_dir_all(root).expect("remove temporary working directory");
    }

    #[cfg(target_os = "linux")]
    struct ReleaseGate(Option<mpsc::Sender<()>>);

    #[cfg(target_os = "linux")]
    impl ReleaseGate {
        fn release(&mut self) {
            if let Some(sender) = self.0.take() {
                sender.send(()).expect("release KILL gate");
            }
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for ReleaseGate {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _release_result = sender.send(());
            }
        }
    }

    // Linux only: a macOS session leader that exits waits in the kernel until
    // its PTY output drains (state `E`), and this test holds the only reader
    // behind the KILL gate, so the leader cannot be reaped before KILL there.
    #[cfg(target_os = "linux")]
    #[test]
    fn live_close_kills_handoff_group_after_try_wait_reaps_leader() {
        let root = std::env::var_os("CS_TERMINAL_TEST_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("cs-terminals-test-state"))
            .join("process-host");
        std::fs::create_dir_all(&root).expect("tmpfs cwd");
        let cwd = root.join(format!("leader-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).expect("isolated cwd");
        let host = LocalPtyHost;
        // The leader blocks TERM before it writes READY and then takes TERM
        // with sigwait, so a TERM sent at any time after READY is consumed.
        // Do not use a Python handler with signal.pause(): CPython runs the
        // handler only at an eval-loop check, and a TERM that arrives after
        // the last check and before pause(2) starts is lost. The leader then
        // never exits and the TERM wait below reaches its bound.
        // The replacement waits for SIGKILL, which no process can lose.
        let command = [
            "/usr/bin/python3".to_owned(),
            "-c".to_owned(),
            "import os,signal\nsignal.pthread_sigmask(signal.SIG_BLOCK,{signal.SIGTERM})\nr,w=os.pipe()\nos.write(1,b'READY\\n')\nsignal.sigwait({signal.SIGTERM})\nif os.fork()==0:\n os.close(r); os.setpgid(0,0); signal.signal(signal.SIGTERM,signal.SIG_IGN)\n os.write(1,f'REPLACEMENT:{os.getpid()}:{os.getpgrp()}\\n'.encode()); os.write(w,b'x')\n while True: signal.pause()\nos.close(w); os.read(r,1); raise SystemExit(0)".to_owned(),
        ];
        let env = [
            ("TERM".into(), "xterm-256color".into()),
            ("COLORTERM".into(), "truecolor".into()),
        ];
        let mut session = host
            .open(&command, &cwd, &env)
            .expect("open terminal shell");
        let leader = session.identity().expect("Linux identity").pid;
        let (observed_sender, observed_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let mut release_gate = ReleaseGate(Some(release_sender));
        let mut output = String::new();
        while !output.contains("READY") {
            let mut bytes = [0_u8; 512];
            let count = host.read(&mut session, &mut bytes).expect("read handshake");
            assert_ne!(count, 0, "leader exited before announcing its child");
            output.push_str(&String::from_utf8_lossy(&bytes[..count]));
        }
        let hook = Box::new(move |reaped| {
            observed_sender
                .send(reaped)
                .expect("report try_wait status");
            release_receiver.recv().expect("test releases KILL");
        });
        *session.0.before_kill.lock().expect("termination hook") = Some(hook);
        let repeated_close = session.clone();
        // The product TERM grace is a fixed 500 ms. A loaded runner can need
        // longer for the leader to fork, hand off and exit, and the KILL pass
        // then runs before try_wait reaps it. The test bound keeps the TERM
        // wait open until the leader is reaped, so the witness is exact.
        let close_thread = thread::spawn(move || close_with_test_bounds(session));
        assert!(
            observed_receiver.recv().expect("pre-KILL reaping witness"),
            "Child::try_wait must reap the TERM-responsive leader before KILL"
        );
        let mut replacement = None;
        let mut read_session = repeated_close.clone();
        while replacement.is_none() {
            let mut bytes = [0_u8; 512];
            let count = host
                .read(&mut read_session, &mut bytes)
                .expect("read replacement handshake");
            assert_ne!(count, 0, "replacement exited before the KILL gate");
            output.push_str(&String::from_utf8_lossy(&bytes[..count]));
            replacement = output.lines().find_map(|line| {
                let fields = line
                    .strip_prefix("REPLACEMENT:")?
                    .split(':')
                    .collect::<Vec<_>>();
                Some((
                    fields.first()?.parse::<u32>().ok()?,
                    fields.get(1)?.parse::<u32>().ok()?,
                ))
            });
        }
        let (replacement_pid, replacement_group) = replacement.expect("replacement pid and group");
        assert_ne!(
            replacement_group, leader,
            "replacement owns a new process group"
        );
        let replacement_stat = std::fs::read_to_string(format!("/proc/{replacement_pid}/stat"))
            .expect("replacement process stat before KILL");
        assert!(
            replacement_stat
                .rsplit_once(')')
                .and_then(|(_, fields)| fields.split_whitespace().next())
                .is_some_and(|state| state != "Z"),
            "replacement must still be alive at the fresh KILL enumeration"
        );
        let replacement_start = super::start_time(replacement_pid, false)
            .expect("read replacement identity")
            .expect("replacement is live before KILL");
        release_gate.release();
        close_thread
            .join()
            .expect("close thread")
            .expect("TERM/KILL and reap session");
        host.close(repeated_close)
            .expect("repeated close uses completed state without a fresh snapshot");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let replacement_killed = loop {
            let current = super::start_time(replacement_pid, false)
                .expect("read replacement identity after KILL");
            if current.as_deref() != Some(&replacement_start) {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            thread::yield_now();
        };
        assert!(
            replacement_killed,
            "freshly enumerated replacement group must be killed"
        );
        let _remove = std::fs::remove_dir_all(cwd);
    }

    /// Start an interactive-style zsh in a PTY with job control, so its
    /// background job gets a separate process group in the terminal session,
    /// as `npm run dev &` does. Returns the session and the job PID.
    #[cfg(target_os = "macos")]
    fn zsh_with_background_job(
        host: &LocalPtyHost,
        cwd: &std::path::Path,
    ) -> (super::PtySession, u32) {
        let command = [
            "/bin/zsh".to_owned(),
            "-f".to_owned(),
            "-c".to_owned(),
            "setopt monitor; /bin/sleep 300 & print JOB:$!; wait".to_owned(),
        ];
        let env = [("TERM".into(), "xterm-256color".into())];
        let mut session = host.open(&command, cwd, &env).expect("open zsh PTY");
        let mut output = String::new();
        let job = loop {
            if let Some(pid) = output
                .lines()
                .find_map(|line| line.trim().strip_prefix("JOB:")?.parse::<u32>().ok())
            {
                break pid;
            }
            let mut bytes = [0_u8; 512];
            let count = host.read(&mut session, &mut bytes).expect("read job PID");
            assert_ne!(count, 0, "zsh exited before it started the job");
            output.push_str(&String::from_utf8_lossy(&bytes[..count]));
        };
        (session, job)
    }

    #[cfg(target_os = "macos")]
    fn group_and_session(pid: u32) -> (u32, u32) {
        let raw = rustix::process::Pid::from_raw(i32::try_from(pid).expect("pid")).expect("pid");
        let group = rustix::process::getpgid(Some(raw)).expect("job group");
        let session = rustix::process::getsid(Some(raw)).expect("job session");
        (
            u32::try_from(group.as_raw_nonzero().get()).expect("group"),
            u32::try_from(session.as_raw_nonzero().get()).expect("session"),
        )
    }

    #[cfg(target_os = "macos")]
    fn wait_until_gone(pid: u32, identity: &str) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if super::start_time(pid, false)
                .expect("read identity")
                .as_deref()
                != Some(identity)
            {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_close_stops_a_background_job_in_another_group_of_the_session() {
        let cwd = tempfile::tempdir().expect("temporary working directory");
        let host = LocalPtyHost;
        let (session, job) = zsh_with_background_job(&host, cwd.path());
        let leader = session.identity().expect("macOS identity").clone();
        assert_eq!(leader.pid, leader.session);
        assert_eq!(leader.pid, leader.process_group);
        let (job_group, job_session) = group_and_session(job);
        assert_ne!(
            job_group, leader.pid,
            "job control gives the job its own group"
        );
        assert_eq!(
            job_session, leader.session,
            "the job stays in the terminal session"
        );
        let groups = super::session_groups(leader.session);
        assert!(groups.iter().any(|group| group.id == leader.pid));
        assert!(
            groups
                .iter()
                .any(|group| group.id == job_group && group.members.iter().any(|m| m.pid == job)),
            "session enumeration finds the background job group"
        );
        let job_identity = super::start_time(job, false)
            .expect("job identity")
            .expect("job is live");
        host.close(session)
            .expect("close terminates every session group");
        assert!(
            wait_until_gone(job, &job_identity),
            "background job survived close"
        );
        assert!(super::session_groups(leader.session).is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_orphan_cleanup_verifies_the_leader_and_stops_every_group() {
        let cwd = tempfile::tempdir().expect("temporary working directory");
        let host = LocalPtyHost;
        let (session, job) = zsh_with_background_job(&host, cwd.path());
        let identity = session.identity().expect("macOS identity").clone();
        let job_identity = super::start_time(job, false)
            .expect("job identity")
            .expect("job is live");
        let leader_identity = super::start_time(identity.pid, false)
            .expect("leader identity")
            .expect("leader is live");
        assert_eq!(identity.start_token(), leader_identity);

        // A stale saved identity (same PID, another start) never signals.
        let mut stale = identity.clone();
        stale.start_time_ticks += 1;
        assert!(super::cleanup_orphan(&stale).is_err());
        let mut other_boot = identity.clone();
        other_boot.boot_id = "00000000-0000-0000-0000-000000000000".into();
        assert!(super::cleanup_orphan(&other_boot).is_err());
        assert_eq!(
            super::start_time(job, false).expect("job read").as_deref(),
            Some(job_identity.as_str()),
            "a refused cleanup must leave the job alive"
        );

        assert!(super::cleanup_orphan(&identity).expect("verified orphan cleanup"));
        assert!(
            wait_until_gone(job, &job_identity),
            "background job survived cleanup"
        );
        host.close(session).expect("close after cleanup");
        assert!(super::session_groups(identity.session).is_empty());
    }

    /// SIGKILL a test process on drop if it still has its recorded identity,
    /// so a failed assertion cannot leave it running.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    struct KillOnDrop {
        pid: u32,
        identity: String,
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let alive = super::start_time(self.pid, false)
                .ok()
                .flatten()
                .is_some_and(|current| current == self.identity);
            if alive
                && let Some(pid) = i32::try_from(self.pid)
                    .ok()
                    .and_then(rustix::process::Pid::from_raw)
            {
                let _kill = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
    }

    // The session keeps a TERM-immune job in the leader's group after the
    // leader exits. A KILL failure then cannot be explained by a vanished
    // group, so close must report it on every platform. A session whose
    // processes are all gone does not prove this: macOS forgives EPERM after
    // the reaped group vanishes, because Darwin returns EPERM for such groups.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn failed_kill_signal_returns_error_and_retains_the_recovery_identity() {
        let root = std::env::var_os("CS_TERMINAL_TEST_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("cs-terminals-test-state"))
            .join("process-host");
        std::fs::create_dir_all(&root).expect("tmpfs cwd");
        let cwd = root.join(format!("signal-failure-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).expect("isolated cwd");
        let host = LocalPtyHost;
        // The job announces itself only after it ignores TERM and HUP, so the
        // close below cannot signal it before the dispositions are in place.
        let command = [
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "/bin/sh -c 'trap \"\" TERM HUP; echo SURVIVOR:$$; exec /bin/sleep 60' & wait"
                .to_owned(),
        ];
        let mut session = host
            .open(&command, &cwd, &[("TERM".into(), "xterm".into())])
            .expect("open shell with a TERM-immune job");
        let leader = session.identity().expect("leader identity").clone();
        let mut output = String::new();
        let survivor = loop {
            if let Some(pid) = output
                .lines()
                .find_map(|line| line.trim().strip_prefix("SURVIVOR:")?.parse::<u32>().ok())
            {
                break pid;
            }
            let mut bytes = [0_u8; 512];
            let count = host.read(&mut session, &mut bytes).expect("read job PID");
            assert_ne!(count, 0, "shell exited before it started the job");
            output.push_str(&String::from_utf8_lossy(&bytes[..count]));
        };
        let survivor_identity = super::start_time(survivor, false)
            .expect("job identity")
            .expect("job is live");
        let _cleanup = KillOnDrop {
            pid: survivor,
            identity: survivor_identity.clone(),
        };
        let recovery = session.clone();
        session
            .0
            .fail_next_kill
            .store(true, std::sync::atomic::Ordering::Release);
        let error =
            close_with_test_bounds(session).expect_err("injected signal failure is reported");
        assert!(
            matches!(error, PtyHostError::Signal(rustix::io::Errno::PERM)),
            "close reports the injected KILL failure, not another error: {error}"
        );
        assert!(
            recovery.0.child().expect("child lock").status.is_some(),
            "the TERM-responsive leader is reaped before the error"
        );
        assert_eq!(
            super::start_time(survivor, false)
                .expect("job identity after failed close")
                .as_deref(),
            Some(survivor_identity.as_str()),
            "the job survives the failed KILL"
        );
        assert_eq!(
            recovery.identity(),
            Some(&leader),
            "recovery identity remains available"
        );
        host.close(recovery)
            .expect("a later close completes after the injected failure");
        assert_ne!(
            super::start_time(survivor, false)
                .expect("job identity after recovery close")
                .as_deref(),
            Some(survivor_identity.as_str()),
            "the later close kills the remaining job"
        );
        let _remove = std::fs::remove_dir_all(cwd);
    }
}
