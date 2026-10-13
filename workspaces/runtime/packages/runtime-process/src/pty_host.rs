//! Unix PTY host backed by the blocking `pty-process` API.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ExitStatus};
use std::sync::{Arc, Mutex, PoisonError};

use pty_process::Size;
use pty_process::blocking::{self, Command, Pty};
use rustix::process::{Pid, Signal, kill_process_group};
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

/// A PTY master and the process-group leader attached to it.
#[derive(Clone)]
pub struct PtySession(Arc<PtyShared>);

struct PtyShared {
    master: Arc<Pty>,
    child: Mutex<PtyChild>,
    group: Pid,
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
        let signal_error = kill_process_group(self.group, Signal::KILL)
            .err()
            .filter(|error| *error != rustix::io::Errno::SRCH);
        let fallback_kill = signal_error.map(|_| child.process.kill());
        let status = child.process.wait()?;
        child.reaped = true;
        if let Some(Err(error)) = fallback_kill {
            return Err(PtyHostError::Io(error));
        }
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
        let child = Command::new(program)
            .args(args)
            .current_dir(cwd)
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
        let Some(group) = Pid::from_raw(raw_pid) else {
            let mut child = child;
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
            group,
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
