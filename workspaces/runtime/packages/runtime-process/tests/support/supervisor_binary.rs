//! Where integration tests find `codex-process-supervisor` and how they start it.
//!
//! Owner of the default: the supervisor is the bin that Cargo builds next to the test
//! binary (`<target>/<profile>/deps/<test>` and `<target>/<profile>/codex-process-supervisor`).
//! Under `layr retest` that folder is `target/retest/debug`, so the child has coverage
//! and a change of its source selects the tests that start it. `STUDIO_SUPERVISOR_BINARY`
//! overrides the default (the gate sets it for the plain build).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

// A hang guard for the exit hook, never a delay: a normal exit takes milliseconds.
const TERM_WAIT: Duration = Duration::from_secs(2);

pub(crate) fn path() -> PathBuf {
    resolve(
        std::env::var_os("STUDIO_SUPERVISOR_BINARY"),
        &std::env::current_exe().unwrap(),
    )
}

fn resolve(selected: Option<OsString>, test_binary: &Path) -> PathBuf {
    selected.map(PathBuf::from).unwrap_or_else(|| {
        test_binary
            .parent()
            .and_then(Path::parent)
            .expect("the test binary is in <target>/<profile>/deps")
            .join("codex-process-supervisor")
    })
}

pub(crate) fn command() -> Command {
    Command::new(path())
}

/// Stop the supervisor. A coverage build writes its profile only on a normal exit, so under
/// `layr retest` (`LLVM_PROFILE_FILE` is set) the child gets SIGTERM and a short wait first.
/// SIGKILL stays the fallback and the only signal of a normal run.
#[allow(clippy::disallowed_methods, clippy::unwrap_used)]
pub(crate) fn stop(child: &mut Child) {
    if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
        drop(Command::new("kill").args(["-TERM", &child.id().to_string()]).status());
        let deadline = Instant::now() + TERM_WAIT;
        while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
    }
    child.wait().unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_the_bin_next_to_the_test_binary_and_the_override_wins() {
        let test_binary = Path::new("/t/retest/debug/deps/integration-1a2b");
        assert_eq!(
            resolve(None, test_binary),
            Path::new("/t/retest/debug/codex-process-supervisor")
        );
        assert_eq!(
            resolve(Some("/plain/supervisor".into()), test_binary),
            Path::new("/plain/supervisor")
        );
    }
}
