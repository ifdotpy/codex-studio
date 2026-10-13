//! Unix PTY host backed by the blocking `pty-process` API.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ExitStatus};
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
    pub session: u32,
    pub start_time_ticks: u64,
}

/// A PTY master and the process-group leader attached to it.
#[derive(Clone)]
pub struct PtySession(Arc<PtyShared>);

struct PtyShared {
    master: Arc<Pty>,
    child: Mutex<PtyChild>,
    identity: PtyIdentity,
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

    fn terminate(&self) -> Result<ExitStatus, PtyHostError> {
        let mut child = self.child()?;
        if child.reaped {
            return child.process.try_wait()?.ok_or_else(|| {
                PtyHostError::Io(std::io::Error::other("reaped terminal child has no status"))
            });
        }
        let _hangup_result = signal_session_groups(self.identity.session, Signal::HUP);
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            if child.process.try_wait()?.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let signal_error = signal_session_groups(self.identity.session, Signal::KILL).err();
        let status = child.process.wait()?;
        child.reaped = true;
        if let Some(error) = signal_error {
            return Err(PtyHostError::Signal(error));
        }
        Ok(status)
    }
}

impl Drop for PtyShared {
    fn drop(&mut self) {
        let _termination = self.terminate();
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
        let mut child = Command::new(program)
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
        let Some(identity) = process_identity(raw_pid) else {
            let _kill_result = child.kill();
            let _wait_result = child.wait();
            return Err(PtyHostError::ProcessId);
        };
        Ok(PtySession(Arc::new(PtyShared {
            master: Arc::new(master),
            child: Mutex::new(PtyChild {
                process: child,
                reaped: false,
            }),
            identity,
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
        let outcome = session.0.terminate().map(|_| ());
        drop(session);
        outcome
    }
}

impl PtySession {
    pub fn identity(&self) -> &PtyIdentity {
        &self.0.identity
    }
}

/// Verify a persisted PID against its start time and session before signaling.
pub fn cleanup_orphan(identity: &PtyIdentity) -> Result<bool, PtyHostError> {
    if identity.pid != identity.session {
        return Ok(false);
    }
    let Some(current) =
        process_identity(i32::try_from(identity.pid).map_err(|_| PtyHostError::ProcessId)?)
    else {
        return Ok(false);
    };
    if current != *identity {
        return Ok(false);
    }
    let _hangup_result = signal_session_groups(identity.session, Signal::HUP);
    signal_session_groups(identity.session, Signal::KILL).map_err(PtyHostError::Signal)?;
    Ok(true)
}

fn process_identity(pid: i32) -> Option<PtyIdentity> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(") ")?;
    let fields = fields.split_whitespace().collect::<Vec<_>>();
    let session = fields.get(3)?.parse().ok()?;
    let start_time_ticks = fields.get(19)?.parse().ok()?;
    Some(PtyIdentity {
        pid: u32::try_from(pid).ok()?,
        session,
        start_time_ticks,
    })
}

fn session_groups(session: u32) -> Vec<i32> {
    let mut groups = std::collections::BTreeSet::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return vec![session as i32];
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
        if let Some(group) = fields.get(2).and_then(|value| value.parse::<i32>().ok()) {
            groups.insert(group);
        }
    }
    if groups.is_empty() {
        groups.insert(session as i32);
    }
    groups.into_iter().collect()
}

fn signal_session_groups(session: u32, signal: Signal) -> Result<(), rustix::io::Errno> {
    let mut first_error = None;
    for group in session_groups(session)
        .into_iter()
        .filter_map(Pid::from_raw)
    {
        if let Err(error) = kill_process_group(group, signal)
            && error != rustix::io::Errno::SRCH
        {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
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
