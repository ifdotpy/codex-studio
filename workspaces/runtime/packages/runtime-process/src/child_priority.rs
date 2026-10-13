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
    pub fn apply_before_spawn(self, command: &mut CommandWrap) {
        if self == Self::Normal {
            return;
        }
        command.wrap(self);
    }

    /// Apply only to a freshly spawned, unreaped child in its own process group.
    /// The caller must retain the child until this returns (no concurrent reaping).
    /// Errors are recorded without I/O, allocation, or waiting; keep the child running.
    pub fn apply_after_spawn(self, child: &Child) {
        if self == Self::Normal {
            return;
        }
        #[cfg(unix)]
        {
            use rustix::process::{Pid, getpgid, setpriority_pgrp};
            let Some(pid) = i32::try_from(child.id()).ok().and_then(Pid::from_raw) else {
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
        #[cfg(not(unix))]
        let _child = child;
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
    #[test]
    fn rejected_priority_returns_with_a_full_undrained_stderr_pipe() {
        // Fill one small pipe, not system resources. The read end stays open and is
        // never drained until the isolated test process has completed its spawn.
        // Python only sets up the descriptors; the Rust hook injects OS rejections.
        const PROBE: &str = r#"
import fcntl, os, subprocess, sys
reader, writer = os.pipe()
try:
    flags = fcntl.fcntl(writer, fcntl.F_GETFL)
    fcntl.fcntl(writer, fcntl.F_SETFL, flags | os.O_NONBLOCK)
    while True:
        try:
            os.write(writer, b'x' * 4096)
        except BlockingIOError:
            break
    fcntl.fcntl(writer, fcntl.F_SETFL, flags)
    result = subprocess.run([sys.argv[1], '--exact',
        'child_priority::tests::rejected_priority_is_advisory_and_child_still_completes',
        '--nocapture'], stdout=subprocess.PIPE, stderr=writer, timeout=30)
    assert result.returncode == 0, result.stdout
finally:
    os.close(writer)
    os.close(reader)
"#;
        let output = Command::new("python3")
            .args(["-c", PROBE])
            .arg(std::env::current_exe().expect("test executable"))
            .output()
            .expect("full-pipe regression fixture");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
