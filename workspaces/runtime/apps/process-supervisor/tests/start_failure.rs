//! A child that fails to start must not leave its handle reserved. The built
//! supervisor closes the reservation with a recorded reason, a restart does not
//! report blocked recovery, and a later open of the same handle proceeds.
#![cfg(any(target_os = "linux", target_os = "macos"))]
#![allow(clippy::unwrap_used)]
#[path = "../../../packages/runtime-process/tests/support/mod.rs"]
mod support;
use studio_runtime_process::HealthRequest;
use studio_runtime_store::supervisor_journal::{START_FAILED_REASON, SupervisorJournal};
use support::*;

#[derive(Debug, PartialEq)]
struct HandleRow {
    pid: u32,
    closed: bool,
    reason: Option<String>,
}

fn handle_row(state: &DisposableState, handle: &str) -> HandleRow {
    let journal = SupervisorJournal::open(&state.0).unwrap();
    let row = journal.handle(handle).unwrap().expect("reserved handle");
    HandleRow {
        pid: row.pid,
        closed: row.closed_at.is_some(),
        reason: row.closed_reason,
    }
}

#[test]
fn failed_start_closes_its_reservation_and_a_later_open_proceeds() {
    let state = DisposableState::new();
    let handle = "start-failure";
    let missing = vec![
        state
            .0
            .join("missing-provider")
            .to_string_lossy()
            .into_owned(),
    ];
    let failed = HandleRow {
        pid: 0,
        closed: true,
        reason: Some(START_FAILED_REASON.into()),
    };
    {
        let _supervisor = SupervisorProcess::start(&state);
        let mut client = connect_client(&state);
        let error = client
            .open(open_request(handle, missing.clone(), &state.0))
            .expect_err("a missing executable cannot start");
        assert!(
            !error.to_string().contains("orphaned"),
            "the first open reports the start failure: {error}"
        );
        assert_eq!(handle_row(&state, handle), failed);
        // The same launch settings are tried again and fail for the same
        // reason; the closed reservation does not turn them into an orphan.
        let error = client
            .open(open_request(handle, missing.clone(), &state.0))
            .expect_err("a missing executable cannot start");
        assert!(
            !error.to_string().contains("orphaned"),
            "a retry after a failed start is not refused as orphaned: {error}"
        );
        assert_eq!(handle_row(&state, handle), failed);
        let health = client.health(HealthRequest::default()).unwrap();
        assert!(health.recovery.blocked.is_none());
        assert!(health.handles.is_empty(), "no open handle remains");
    }
    std::fs::remove_file(state.socket()).unwrap();

    let mut supervisor = SupervisorProcess::start(&state);
    let mut client = connect_client(&state);
    let health = client.health(HealthRequest::default()).unwrap();
    assert!(
        health.recovery.blocked.is_none(),
        "a failed start is not unknown ownership after a restart: {:?}",
        health.recovery.blocked
    );
    assert_eq!(handle_row(&state, handle), failed);
    // Different launch settings (a corrected command) also proceed: the
    // failed start left no native outcome to prove.
    let command = shell_command("while IFS= read -r line; do :; done");
    let opened = client
        .open(open_request(handle, command, &state.0))
        .expect("a later open of the handle proceeds");
    supervisor.own_child(opened.pid);
    assert!(!opened.resumed);
    let row = handle_row(&state, handle);
    assert_eq!(row.pid, opened.pid);
    assert!(!row.closed);
    assert_eq!(row.reason, None);
}
