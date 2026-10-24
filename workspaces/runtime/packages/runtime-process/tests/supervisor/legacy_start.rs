//! Open of a handle that the deleted Python supervisor left in the journal. Its
//! start token is the bare macOS start instant `<secs>.<usecs:06>`, without the
//! boot session UUID of a native token.
//! macOS only: the Python supervisor recorded `ps -o lstart` text elsewhere.
#![allow(clippy::unwrap_used, clippy::disallowed_methods)]
use super::deadline_tests::fixture;
use super::*;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command as Process, Stdio};

const ORPHANED: &str = "Supervisor handle is orphaned; native outcome remains unknown";
const PYTHON_SIGNATURE: &str = "python-era-launch-signature";

/// A child in its own process group, as the Python supervisor started them.
struct PythonChild(Child);

impl PythonChild {
    fn start() -> (Self, u32, String) {
        let child = Process::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let native = start_time(pid, false).unwrap().unwrap();
        let legacy = native.rsplit_once(':').unwrap().1.to_owned();
        (Self(child), pid, legacy)
    }

    fn running(&mut self) -> bool {
        self.0.try_wait().unwrap().is_none()
    }
}

impl Drop for PythonChild {
    fn drop(&mut self) {
        let _kill = self.0.kill();
        let _wait = self.0.wait();
    }
}

/// The row shape of the Python era: old launch settings, a legacy start token,
/// and a recovery close. `outcome` "already-exited" leaves the degraded proof.
fn python_era_handle(supervisor: &Supervisor, handle: &str, pid: u32, start: &str, outcome: &str) {
    let journal = &supervisor.journal;
    journal
        .reserve(handle, PYTHON_SIGNATURE, now().unwrap())
        .unwrap();
    journal
        .opened(handle, PYTHON_SIGNATURE, pid, start)
        .unwrap();
    let row = journal.handle(handle).unwrap().unwrap();
    journal
        .recovered(&row, outcome, "recovered in the Python era", now().unwrap())
        .unwrap();
    let saved = journal.handle(handle).unwrap().unwrap();
    assert!(saved.closed_at.is_some());
    assert_eq!(saved.recovery_proof, outcome == "already-exited");
}

fn open(supervisor: &Supervisor, handle: &str) -> Result<Value> {
    supervisor.handle(
        Command::Open {
            handle: handle.to_owned(),
            command: vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
            env: BTreeMap::new(),
            cwd: Some(supervisor.root.to_string_lossy().into_owned()),
        },
        false,
    )
}

fn refused(result: Result<Value>) -> bool {
    matches!(result, Err(Error::Refused(ORPHANED)))
}

fn reopened_with_native_token(supervisor: &Supervisor, handle: &str, result: Result<Value>) {
    assert!(result.is_ok(), "{result:?}");
    let saved = supervisor.journal.handle(handle).unwrap().unwrap();
    assert_ne!(saved.signature, PYTHON_SIGNATURE);
    assert!(
        saved.start_time.as_deref().is_some_and(valid_start),
        "{:?}",
        saved.start_time
    );
}

#[test]
fn a_python_era_handle_with_recovery_proof_reopens_after_its_child_ended() {
    let (_root, supervisor) = fixture();
    let (mut child, pid, legacy) = PythonChild::start();
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    python_era_handle(&supervisor, "python:ended", pid, &legacy, "already-exited");
    let result = open(&supervisor, "python:ended");
    reopened_with_native_token(&supervisor, "python:ended", result);
}

#[test]
fn a_python_era_handle_reopens_when_its_pid_has_another_start() {
    let (_root, supervisor) = fixture();
    let (mut child, pid, legacy) = PythonChild::start();
    let (seconds, micros) = legacy.split_once('.').unwrap();
    let other = format!("{}.{micros}", seconds.parse::<u64>().unwrap() - 1);
    python_era_handle(&supervisor, "python:reused", pid, &other, "already-exited");
    let result = open(&supervisor, "python:reused");
    reopened_with_native_token(&supervisor, "python:reused", result);
    assert!(
        child.running(),
        "the process that now owns the PID was signalled"
    );
}

#[test]
fn a_python_era_handle_stays_refused_while_its_child_runs() {
    let (_root, supervisor) = fixture();
    let (mut child, pid, legacy) = PythonChild::start();
    python_era_handle(&supervisor, "python:live", pid, &legacy, "already-exited");
    assert!(refused(open(&supervisor, "python:live")));
    assert!(child.running(), "a live Python child was signalled");
    let saved = supervisor.journal.handle("python:live").unwrap().unwrap();
    assert_eq!(saved.signature, PYTHON_SIGNATURE);
    assert_eq!(saved.start_time.as_deref(), Some(legacy.as_str()));
}

#[test]
fn a_python_era_handle_needs_the_recovery_proof_and_the_epoch_form() {
    let (_root, supervisor) = fixture();
    let (mut child, pid, legacy) = PythonChild::start();
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    python_era_handle(
        &supervisor,
        "python:no-proof",
        pid,
        &legacy,
        "identity-mismatch",
    );
    assert!(refused(open(&supervisor, "python:no-proof")));
    let lstart = "Fri Oct  2 00:55:21 2026";
    python_era_handle(&supervisor, "python:lstart", pid, lstart, "already-exited");
    assert!(refused(open(&supervisor, "python:lstart")));
}
