//! Advisory scheduling for owned child groups. Never changes the backend.

use std::io;
use std::process::{Child, Command};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use process_wrap::std::{CommandWrap, CommandWrapper};
use serde::{Deserialize, Serialize};

pub const CHILD_PRIORITY_ENV: &str = "CODEX_AGENT_CHILD_PRIORITY";
#[cfg(unix)]
const BACKGROUND_NICE: i32 = 5;
static FAILURE_COUNT: AtomicU64 = AtomicU64::new(0);
static FIRST_FAILURE: OnceLock<PriorityFailure> = OnceLock::new();
static CONFIGURED: OnceLock<ChildPriority> = OnceLock::new();

/// Normal leaves inherited scheduling unchanged; Background lowers owned workloads.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChildPriority {
    Normal,
    #[default]
    Background,
}

impl ChildPriority {
    /// Runtime configuration follows the backend's existing environment settings.
    /// Invalid values are diagnosed once and retain the background default.
    pub fn from_environment() -> Self {
        *CONFIGURED.get_or_init(|| match std::env::var(CHILD_PRIORITY_ENV) {
            Err(std::env::VarError::NotPresent) => Self::default(),
            Ok(value) if value == "normal" => Self::Normal,
            Ok(value) if value == "background" => Self::Background,
            _ => {
                record_failure("configuration", io::ErrorKind::InvalidInput);
                Self::default()
            }
        })
    }

    /// Install before spawn. Unix callers must use a fresh ProcessGroup::leader().
    /// The wrapper applies Unix scheduling immediately in its parent-side post-spawn hook.
    pub fn apply_before_spawn(self, command: &mut impl PriorityCommand) {
        studio_runtime_store::debug_assert_outside_sql();
        if self == Self::Normal {
            return;
        }
        command.install_priority(self);
    }

    /// Apply only to a freshly spawned, unreaped child in its own process group.
    /// The caller must retain the child until this returns (no concurrent reaping).
    /// Errors are recorded without I/O, allocation, or waiting; keep the child running.
    pub fn apply_after_spawn(self, child: &Child) {
        #[cfg(unix)]
        self.apply_to_owned_group(Some(child.id()));
        #[cfg(not(unix))]
        let _child = child;
    }

    #[cfg(unix)]
    fn apply_to_owned_group(self, child_id: Option<u32>) {
        if self == Self::Normal {
            return;
        }
        use rustix::process::{Pid, getpgid, setpriority_pgrp};
        let Some(pid) = child_id
            .and_then(|id| i32::try_from(id).ok())
            .and_then(Pid::from_raw)
        else {
            record_failure("child identity", io::ErrorKind::InvalidInput);
            return;
        };
        // Refuse callers that forgot group isolation: never target the backend group.
        if getpgid(Some(pid)) != Ok(pid) {
            record_failure("child process group", io::ErrorKind::InvalidInput);
            return;
        }
        report(
            "CPU priority",
            setpriority_pgrp(Some(pid), BACKGROUND_NICE).map_err(io::Error::from),
        );
        #[cfg(target_os = "linux")]
        report(
            "I/O priority",
            studio_runtime_priority_sys::set_group_io_best_effort(pid.as_raw_nonzero().get()),
        );
    }
}

/// Command adapters supported by the shared priority policy.
pub trait PriorityCommand {
    fn install_priority(&mut self, priority: ChildPriority);
}

impl PriorityCommand for CommandWrap {
    fn install_priority(&mut self, priority: ChildPriority) {
        self.wrap(priority);
    }
}

#[cfg(unix)]
impl PriorityCommand for process_wrap::tokio::CommandWrap {
    fn install_priority(&mut self, priority: ChildPriority) {
        self.wrap(priority);
    }
}

#[cfg(unix)]
impl process_wrap::tokio::CommandWrapper for ChildPriority {
    fn post_spawn(
        &mut self,
        _command: &mut tokio::process::Command,
        child: &mut tokio::process::Child,
        _core: &process_wrap::tokio::CommandWrap,
    ) -> io::Result<()> {
        // The owner has not started waiting or reaping this child yet.
        self.apply_to_owned_group(child.id());
        Ok(())
    }
}

impl CommandWrapper for ChildPriority {
    #[cfg(windows)]
    fn pre_spawn(&mut self, command: &mut Command, _core: &CommandWrap) -> io::Result<()> {
        use std::os::windows::process::CommandExt;
        // https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        if *self == Self::Normal {
            return Ok(());
        }
        // Current Windows callers set no other creation flags, and the enabled
        // process-wrap features contain no Windows wrapper that replaces them.
        command.creation_flags(BELOW_NORMAL_PRIORITY_CLASS);
        Ok(())
    }

    fn post_spawn(
        &mut self,
        _command: &mut Command,
        child: &mut Child,
        _core: &CommandWrap,
    ) -> io::Result<()> {
        self.apply_after_spawn(child);
        Ok(())
    }
}

#[cfg(unix)]
fn report(mechanism: &'static str, result: io::Result<()>) {
    if let Err(error) = result {
        record_failure(mechanism, error.kind());
    }
}

/// First rejected scheduling operation; no request or environment values are stored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PriorityFailure {
    pub mechanism: &'static str,
    pub error: io::ErrorKind,
}

/// Process-local, bounded snapshot for a health/diagnostics consumer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PriorityDiagnostics {
    pub failure_count: u64,
    pub first_failure: Option<PriorityFailure>,
}

/// Read without waiting or doing I/O. During the first concurrent publication,
/// count may be nonzero before first_failure becomes visible; a later read sees it.
pub fn priority_diagnostics() -> PriorityDiagnostics {
    PriorityDiagnostics {
        failure_count: FAILURE_COUNT.load(Ordering::Relaxed),
        first_failure: FIRST_FAILURE.get().copied(),
    }
}

fn record_failure(mechanism: &'static str, error: io::ErrorKind) {
    // Only the fetch_add winner initializes the OnceLock, so set cannot wait for
    // another writer. Readers use get (also nonblocking), never get_or_init.
    if FAILURE_COUNT.fetch_add(1, Ordering::Relaxed) == 0 {
        let _already_recorded = FIRST_FAILURE.set(PriorityFailure { mechanism, error });
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct RejectedPriority;

    impl CommandWrapper for RejectedPriority {
        fn post_spawn(
            &mut self,
            _command: &mut Command,
            _child: &mut Child,
            _core: &CommandWrap,
        ) -> io::Result<()> {
            report("CPU priority", Err(io::ErrorKind::PermissionDenied.into()));
            report("I/O priority", Err(io::ErrorKind::PermissionDenied.into()));
            Ok(())
        }
    }

    #[test]
    fn rejected_priority_is_advisory_and_child_still_completes() {
        let mut command = CommandWrap::with_new("/bin/sh", |command| {
            command.args(["-c", "exit 0"]);
        });
        command.wrap(RejectedPriority);
        assert!(
            command
                .spawn()
                .expect("spawn is not rejected")
                .wait()
                .expect("child exits")
                .success()
        );
        let diagnostics = priority_diagnostics();
        assert_eq!(diagnostics.failure_count, 2);
        assert_eq!(
            diagnostics.first_failure,
            Some(PriorityFailure {
                mechanism: "CPU priority",
                error: io::ErrorKind::PermissionDenied,
            })
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn rejected_priority_returns_with_a_full_undrained_stderr_pipe() {
        use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
        use std::io::Write;
        use std::process::Stdio;
        use std::time::Duration;
        use tokio::io::AsyncReadExt;

        // Fill one small pipe, not system resources. Keep the read end open
        // and undrained until the isolated priority test has completed.
        const PIPE_FILL_CHUNK_BYTES: usize = 4096;
        const SPAWN_HANG_GUARD: Duration = Duration::from_secs(30);
        let (_reader, mut writer) = std::io::pipe().expect("stderr pipe");
        let flags = fcntl_getfl(&writer).expect("pipe flags");
        fcntl_setfl(&writer, flags | OFlags::NONBLOCK).expect("nonblocking fill");
        loop {
            match writer.write(&[b'x'; PIPE_FILL_CHUNK_BYTES]) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("fill stderr pipe: {error}"),
            }
        }
        fcntl_setfl(&writer, flags).expect("restore blocking stderr");
        let mut child = tokio::process::Command::new(
            std::env::current_exe().expect("test executable"),
        )
        .args([
            "--exact",
            "child_priority::tests::rejected_priority_is_advisory_and_child_still_completes",
            "--nocapture",
        ])
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(writer)
        .spawn()
        .expect("full-pipe regression fixture");
        let mut output = child.stdout.take().expect("probe stdout");
        let mut stdout = String::new();
        let (status, drained) = tokio::join!(
            async {
                match tokio::time::timeout(SPAWN_HANG_GUARD, child.wait()).await {
                    Ok(result) => result.expect("probe exits"),
                    Err(_) => {
                        child.kill().await.expect("kill and reap stalled probe");
                        panic!("priority hook blocked on a full stderr pipe");
                    }
                }
            },
            output.read_to_string(&mut stdout),
        );
        drained.expect("probe output");
        assert!(status.success(), "{stdout}");
    }
}
