//! Advisory scheduling for owned child groups. Never changes the backend.

use std::io;
use std::process::{Child, Command};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use process_wrap::std::{CommandWrap, CommandWrapper};
use serde::{Deserialize, Serialize};

pub const CHILD_PRIORITY_ENV: &str = "CODEX_AGENT_CHILD_PRIORITY";
#[cfg(unix)]
const BACKGROUND_NICE: i32 = 19;
static WARNED: AtomicBool = AtomicBool::new(false);
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
                warn_once(&WARNED, "configuration", io::ErrorKind::InvalidInput);
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
    /// Errors are advisory: log once per backend process and keep the child running.
    pub fn apply_after_spawn(self, child: &Child) {
        if self == Self::Normal {
            return;
        }
        #[cfg(unix)]
        {
            use rustix::process::{Pid, getpgid, setpriority_pgrp};
            let Some(pid) = i32::try_from(child.id()).ok().and_then(Pid::from_raw) else {
                warn_once(&WARNED, "child identity", io::ErrorKind::InvalidInput);
                return;
            };
            // Refuse callers that forgot group isolation: never target the backend group.
            if getpgid(Some(pid)) != Ok(pid) {
                warn_once(&WARNED, "child process group", io::ErrorKind::InvalidInput);
                return;
            }
            report(
                "CPU priority",
                setpriority_pgrp(Some(pid), BACKGROUND_NICE).map_err(io::Error::from),
            );
            #[cfg(target_os = "linux")]
            report(
                "I/O priority",
                studio_runtime_priority_sys::set_group_io_idle(pid.as_raw_nonzero().get()),
            );
            #[cfg(target_os = "macos")]
            report(
                "background policy",
                studio_runtime_priority_sys::set_process_background(pid.as_raw_nonzero().get()),
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
        warn_once(&WARNED, mechanism, error.kind());
    }
}

fn warn_once(warned: &AtomicBool, mechanism: &'static str, error: io::ErrorKind) {
    if !warned.swap(true, Ordering::Relaxed) {
        if tracing::dispatcher::has_been_set() {
            tracing::warn!(
                mechanism,
                ?error,
                "child priority not applied; continuing child execution"
            );
        } else {
            // The initial Rust server uses stderr until its tracing subscriber is installed.
            use std::io::Write as _;
            drop(writeln!(
                std::io::stderr().lock(),
                "child priority not applied ({mechanism}: {error:?}); continuing child execution"
            ));
        }
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
    }
}
