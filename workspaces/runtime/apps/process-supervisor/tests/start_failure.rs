//! A child that fails to start must not leave its handle reserved. The built
//! supervisor closes the reservation with a recorded reason, a restart does not
//! report blocked recovery, and a later open of the same handle proceeds.
#![cfg(any(target_os = "linux", target_os = "macos"))]
#![allow(clippy::unwrap_used)]
#[path = "../../../packages/runtime-process/tests/support/mod.rs"]
mod support;
use std::process::Command;
use studio_runtime_process::HealthRequest;
use studio_runtime_store::supervisor_journal::START_FAILED_REASON;
use support::*;

/// Read `(pid, closed, closed_reason)` of one journal row, read-only.
#[allow(clippy::disallowed_methods)]
fn handle_row(python: &str, state: &DisposableState, handle: &str) -> serde_json::Value {
    let output = Command::new(python)
        .args([
            "-B",
            "-c",
            "import json,sqlite3,sys\n\
             db=sqlite3.connect('file:'+sys.argv[1]+'?mode=ro',uri=True)\n\
             row=db.execute('SELECT pid,closed_at IS NOT NULL,closed_reason FROM handles WHERE id=?',(sys.argv[2],)).fetchone()\n\
             print(json.dumps(None if row is None else {'pid':row[0],'closed':bool(row[1]),'reason':row[2]}))",
        ])
        .arg(state.0.join("supervisor.sqlite3"))
        .arg(handle)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn failed_start_closes_its_reservation_and_a_later_open_proceeds() {
    let state = DisposableState::new();
    let python = python_executable();
    let handle = "start-failure";
    let missing = vec![
        state
            .0
            .join("missing-provider")
            .to_string_lossy()
            .into_owned(),
    ];
    let failed = serde_json::json!({"pid":0,"closed":true,"reason":START_FAILED_REASON});
    {
        let _supervisor = SupervisorProcess::start_v2(&state);
        let mut client = connect_client(&state);
        let error = client
            .open(open_request(handle, missing.clone(), &state.0))
            .expect_err("a missing executable cannot start");
        assert!(
            !error.to_string().contains("orphaned"),
            "the first open reports the start failure: {error}"
        );
        assert_eq!(handle_row(&python, &state, handle), failed);
        // The same launch settings are tried again and fail for the same
        // reason; the closed reservation does not turn them into an orphan.
        let error = client
            .open(open_request(handle, missing.clone(), &state.0))
            .expect_err("a missing executable cannot start");
        assert!(
            !error.to_string().contains("orphaned"),
            "a retry after a failed start is not refused as orphaned: {error}"
        );
        assert_eq!(handle_row(&python, &state, handle), failed);
        let health = client.health(HealthRequest::default()).unwrap();
        assert!(health.recovery.blocked.is_none());
        assert!(health.handles.is_empty(), "no open handle remains");
    }
    std::fs::remove_file(state.socket()).unwrap();

    let mut supervisor = SupervisorProcess::start_v2(&state);
    let mut client = connect_client(&state);
    let health = client.health(HealthRequest::default()).unwrap();
    assert!(
        health.recovery.blocked.is_none(),
        "a failed start is not unknown ownership after a restart: {:?}",
        health.recovery.blocked
    );
    assert_eq!(handle_row(&python, &state, handle), failed);
    // Different launch settings (a corrected command) also proceed: the
    // failed start left no native outcome to prove.
    let command = vec![
        python.clone(),
        "-u".into(),
        "-c".into(),
        "import sys; sys.stdin.read()".into(),
    ];
    let opened = client
        .open(open_request(handle, command, &state.0))
        .expect("a later open of the handle proceeds");
    supervisor.own_child(opened.pid);
    assert!(!opened.resumed);
    let row = handle_row(&python, &state, handle);
    assert_eq!(row["pid"], opened.pid);
    assert_eq!(row["closed"], false);
    assert_eq!(row["reason"], serde_json::Value::Null);
}
