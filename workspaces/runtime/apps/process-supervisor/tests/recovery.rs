#![cfg(any(target_os = "linux", target_os = "macos"))]
#![allow(clippy::unwrap_used)]
#[path = "../../../packages/runtime-process/tests/support/mod.rs"]
mod support;
use std::{collections::BTreeMap, process::Command};
use studio_runtime_process::{
    AdminCloseHandleRequest, HealthRequest, SupervisorClient, SupervisorHandle,
};
use studio_runtime_store::supervisor_journal::SupervisorJournal;
use support::*;

#[test]
#[allow(clippy::disallowed_methods)]
fn unknown_ownership_survives_restart_until_exact_operator_close() {
    let state = DisposableState::new();
    // The reservation is the crash boundary before a process identity exists.
    SupervisorJournal::open(&state.0)
        .unwrap()
        .reserve("unknown", "saved", 1.0)
        .unwrap();
    for restart in 0..2 {
        let supervisor = SupervisorProcess::start(&state);
        let mut client = connect_client(&state);
        let health = client.health(HealthRequest::default()).unwrap();
        assert!(health.recovery.blocked.is_some());
        assert_eq!(health.handles[0].id, "unknown");
        assert!(
            client
                .open(open_request("unknown", shell_command("exit 1"), &state.0))
                .is_err()
        );
        if restart == 1 {
            let request = |signature: &str| AdminCloseHandleRequest {
                handle: SupervisorHandle("unknown".into()),
                expected_pid: 0,
                expected_start_time: String::new(),
                expected_signature: signature.into(),
                extra: BTreeMap::new(),
            };
            assert!(client.admin_close_handle(request("saved")).is_err());
            let mut operator =
                SupervisorClient::connect_operator(safe_socket_path(&state), state.0.clone())
                    .unwrap();
            assert!(operator.admin_close_handle(request("wrong")).is_err());
            assert!(
                operator
                    .admin_close_handle(request("saved"))
                    .unwrap()
                    .closed
            );
            let cleared = client.health(HealthRequest::default()).unwrap();
            assert!(cleared.recovery.blocked.is_none());
            assert!(cleared.handles.is_empty());
        }
        drop(client);
        drop(supervisor);
        std::fs::remove_file(state.socket()).unwrap();
    }
    let _supervisor = SupervisorProcess::start(&state);
    let mut client = connect_client(&state);
    let health = client.health(HealthRequest::default()).unwrap();
    assert!(health.recovery.blocked.is_none());
    assert!(health.handles.is_empty());
}

struct Probe(std::process::Child);
impl Drop for Probe {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
        }
        self.0.wait().unwrap();
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum RecoveryCase {
    Collision,
    Legacy,
    Native,
}

struct StartIdentity {
    native: String,
    legacy: String,
    collision: String,
}

#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods)]
fn start_identity(pid: u32) -> StartIdentity {
    // Linux proc(5): starttime is field 22, or field 19 after the comm tail.
    const START_TICKS_AFTER_COMM: usize = 19;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let ticks: u64 = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(START_TICKS_AFTER_COMM)
        .unwrap()
        .parse()
        .unwrap();
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    let hz = rustix::param::clock_ticks_per_second();
    let stale = if ticks % hz < hz - 1 {
        ticks + 1
    } else {
        ticks - 1
    };
    assert_ne!(stale, ticks);
    assert_eq!(stale / hz, ticks / hz, "same-second PID collision");
    let output = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(output.status.success());
    StartIdentity {
        native: format!("linux:{}:{ticks}", boot.trim()),
        collision: format!("linux:{}:{stale}", boot.trim()),
        legacy: String::from_utf8(output.stdout).unwrap().trim().into(),
    }
}

#[cfg(target_os = "macos")]
fn start_identity(pid: u32) -> StartIdentity {
    let info = studio_runtime_proc_sys::bsd_info(pid, false)
        .unwrap()
        .unwrap();
    let boot = studio_runtime_proc_sys::boot_session_uuid().unwrap();
    let seconds = info.start_seconds;
    let micros = info.start_microseconds;
    let stale = micros ^ 1;
    StartIdentity {
        native: format!("macos:{boot}:{seconds}.{micros:06}"),
        collision: format!("macos:{boot}:{seconds}.{stale:06}"),
        legacy: format!("{seconds}.{micros:06}"),
    }
}

#[test]
#[allow(clippy::disallowed_methods)]
fn recovery_signals_only_exact_native_identity() {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    for case in [
        RecoveryCase::Collision,
        RecoveryCase::Legacy,
        RecoveryCase::Native,
    ] {
        let state = DisposableState::new();
        let mut probe = Probe(
            Command::new("/bin/sh")
                .args(["-c", "while IFS= read -r line; do :; done"])
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .env_clear()
                .envs(child_environment(&state.0))
                .spawn()
                .unwrap(),
        );
        let identity = start_identity(probe.0.id());
        let saved = if case == RecoveryCase::Collision {
            &identity.collision
        } else if case == RecoveryCase::Native {
            &identity.native
        } else {
            &identity.legacy
        };
        let journal = SupervisorJournal::open(&state.0).unwrap();
        journal.reserve("fixture", "saved", 1.0).unwrap();
        journal
            .opened("fixture", "saved", probe.0.id(), saved)
            .unwrap();
        drop(journal);
        let _supervisor = SupervisorProcess::start(&state);
        let mut client = connect_client(&state);
        let health = client.health(HealthRequest::default()).unwrap();
        if case == RecoveryCase::Native {
            assert!(health.recovery.blocked.is_none());
            assert!(health.handles.is_empty());
            probe.0.wait().expect("verified native child termination");
        } else {
            assert!(
                probe.0.try_wait().unwrap().is_none(),
                "{case:?}: must not signal probe"
            );
            assert!(health.recovery.blocked.is_none());
        }
        let db = rusqlite::Connection::open_with_flags(
            state.0.join("supervisor.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let start: String = db
            .query_row(
                "SELECT start_time FROM child_identities WHERE handle='fixture'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let (outcome, outcome_start): (String, String) = db.query_row("SELECT outcome,start_time FROM recovery_events WHERE handle='fixture' ORDER BY id DESC LIMIT 1", [], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        if case == RecoveryCase::Native {
            assert_eq!(start, identity.native);
            assert_eq!(outcome_start, identity.native);
            assert!(matches!(outcome.as_str(), "terminated" | "killed"));
        } else {
            assert_eq!(&start, saved);
            assert_eq!(outcome, "identity-mismatch");
        }
    }
}
