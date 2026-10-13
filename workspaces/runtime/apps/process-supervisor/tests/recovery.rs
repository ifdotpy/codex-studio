#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]
#[path = "../../../packages/runtime-process/tests/support/mod.rs"]
mod support;
use std::{collections::BTreeMap, path::Path, process::Command};
use studio_runtime_process::{
    AdminCloseHandleRequest, HealthRequest, SupervisorClient, SupervisorHandle,
};
use support::*;

#[test]
fn unknown_ownership_survives_restart_until_exact_operator_close() {
    let state = DisposableState::new();
    let python = python_executable();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../server/src");
    let created = Command::new(&python).args(["-B", "-c", "import sys; sys.path.insert(0,sys.argv[1]); from codex_process_supervisor import Journal; j=Journal(sys.argv[2]);\nwith j.db() as db: db.execute(\"INSERT INTO handles(id,signature,pid,created) VALUES ('unknown','saved',0,1)\")\nj.close()"])
        .arg(source).arg(&state.0).envs(child_environment(&state.0)).status().unwrap();
    assert!(created.success());
    for restart in 0..2 {
        let supervisor = SupervisorProcess::start(&state, &python);
        let mut client = connect_client(&state);
        let health = client.health(HealthRequest::default()).unwrap();
        assert!(health.recovery.blocked.is_some());
        assert_eq!(health.handles[0].id, "unknown");
        assert!(
            client
                .open(open_request(
                    "unknown",
                    vec![
                        python.clone(),
                        "-c".into(),
                        "raise AssertionError('must not spawn')".into()
                    ],
                    &state.0
                ))
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
    let _supervisor = SupervisorProcess::start(&state, &python);
    let mut client = connect_client(&state);
    let health = client.health(HealthRequest::default()).unwrap();
    assert!(health.recovery.blocked.is_none());
    assert!(health.handles.is_empty());
}
