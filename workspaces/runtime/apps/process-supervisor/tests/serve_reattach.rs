//! The built supervisor keeps a child across an abrupt client disconnect (a
//! backend restart) and records the platform's native start identity.
#![cfg(any(target_os = "linux", target_os = "macos"))]
#![allow(clippy::unwrap_used)]
#[path = "../../../packages/runtime-process/tests/support/mod.rs"]
mod support;
use studio_runtime_process::HealthRequest;
use support::*;

#[cfg(target_os = "linux")]
const NATIVE_PREFIX: &str = "linux:";
#[cfg(target_os = "macos")]
const NATIVE_PREFIX: &str = "macos:";

#[test]
fn child_survives_abrupt_disconnect_and_a_new_connection_sees_it() {
    let state = DisposableState::new();
    let mut supervisor = SupervisorProcess::start(&state);
    let handle = "serve-reattach";
    let command = shell_command("printf 'ready\\n'; while IFS= read -r line; do :; done");
    let mut client = connect_client(&state);
    let opened = client
        .open(open_request(handle, command.clone(), &state.0))
        .unwrap();
    supervisor.own_child(opened.pid);
    assert!(!opened.resumed);
    // No detach: the connection ends as a crashed or restarted backend's does.
    drop(client);

    let mut client = connect_client(&state);
    let health = client.health(HealthRequest::default()).unwrap();
    let saved = health
        .handles
        .iter()
        .find(|saved| saved.id == handle)
        .expect("the journal keeps the handle after disconnect");
    assert_eq!(saved.pid, Some(opened.pid));
    let start = saved.start_time.as_deref().expect("child start identity");
    assert!(
        start.starts_with(NATIVE_PREFIX),
        "native start identity expected, got {start}"
    );
    assert!(pid_exists(opened.pid), "child must survive the disconnect");

    let resumed = client
        .open(open_request(handle, command, &state.0))
        .unwrap();
    assert!(
        resumed.resumed,
        "a new connection reattaches the same child"
    );
    assert_eq!(resumed.pid, opened.pid);
    assert_eq!(resumed.generation, opened.generation);
}
