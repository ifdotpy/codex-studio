#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::fs;
use std::io::BufRead as _;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use studio_runtime_process::{
    AckRequest, AdminCloseHandleRequest, BackendId, BackendIdentity, CursorRequest, EventCursor,
    EventKind, FinishFallbackRequest, HandleRequest, HealthRequest, OperationId,
    OperationStatusRequest, RetireCommandRequest, SupervisorClient, SupervisorError,
    SupervisorHandle, WriteRequest,
};
mod support;
use support::*;

#[test]
fn disposable_python_supervisor_v1_actions_events_replay_and_reattach() {
    let state = DisposableState::new();
    let workspace = state.0.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let python = python_executable();
    let mut supervisor = SupervisorProcess::start(&state, &python);
    let supervisor_pid = supervisor.id();

    let wait_script = r#"
import json, sys
print(json.dumps({'ready': True}), flush=True)
message = json.loads(sys.stdin.readline())
text = message['params']['text']
for chunk in ('stderr:', text[:5], text[5:]):
    sys.stderr.write(chunk)
    sys.stderr.flush()
print(json.dumps({'id': message['id'], 'result': {'echo': text}}), flush=True)
"#;
    let command = vec![python.clone(), "-u".into(), "-c".into(), wait_script.into()];
    let handle = SupervisorHandle("rust-v1-main".into());

    let mut client = connect_client(&state);
    let health = client.health(HealthRequest::default()).unwrap();
    assert_eq!(health.protocol, 1);
    assert_eq!(
        health.durability,
        studio_runtime_process::Durability::SqliteFullSyncPerRequest
    );

    let opened = client
        .open(open_request(&handle.0, command.clone(), &workspace))
        .unwrap();
    supervisor.own_child(opened.pid);
    assert!(!opened.resumed);
    let generation = opened.generation;
    let ready = poll_event(
        &mut client,
        &handle,
        EventCursor(opened.acknowledged),
        |event| {
            event.kind == EventKind::Stdout
                && event.payload.get("ready") == Some(&Value::Bool(true))
        },
    );
    assert_eq!(ready.generation, generation);

    assert!(
        client
            .detach(HandleRequest {
                handle: handle.clone(),
                extra: BTreeMap::new()
            })
            .unwrap()
            .detached
    );
    drop(client);
    let mut client = connect_client(&state);
    let resumed = client
        .open(open_request(&handle.0, command, &workspace))
        .unwrap();
    assert!(resumed.resumed);
    assert_eq!(resumed.generation, generation);
    let status = client
        .status(HandleRequest {
            handle: handle.clone(),
            extra: BTreeMap::new(),
        })
        .unwrap();
    assert_eq!(status.pid, resumed.pid);
    let replay = client
        .replay(CursorRequest {
            handle: handle.clone(),
            cursor: EventCursor(0),
            extra: BTreeMap::new(),
        })
        .unwrap();
    assert!(
        replay
            .events
            .iter()
            .any(|event| event.sequence == ready.sequence)
    );

    let operation_id = OperationId("monitor:rust-supervisor-test".into());
    let write = client
        .write(WriteRequest {
            handle: handle.clone(),
            operation_id: operation_id.clone(),
            native_id: Some(42),
            message: json!({"method":"test/echo","params":{"text":"hello-from-rust"}}),
            extra: BTreeMap::new(),
        })
        .unwrap();
    assert!(write.accepted);
    let receipt = client
        .operation_status(OperationStatusRequest {
            handle: handle.clone(),
            operation_id,
            extra: BTreeMap::new(),
        })
        .unwrap();
    assert!(receipt.accepted);
    assert_eq!(receipt.native_id, write.remote_id);

    let mut cursor = ready.sequence;
    let mut stdout_payloads = Vec::new();
    let mut stderr_text = String::new();
    let mut stderr_seen = false;
    let mut exit_code = None;
    let deadline = Instant::now() + HANG_GUARD;
    let mut readers_drained = false;
    while Instant::now() < deadline && !(readers_drained && stderr_seen) {
        let result = client
            .next(CursorRequest {
                handle: handle.clone(),
                cursor,
                extra: BTreeMap::new(),
            })
            .unwrap();
        if let Some(event) = result.event {
            cursor = event.sequence;
            match event.kind {
                EventKind::Stdout => stdout_payloads.push(event.payload),
                EventKind::Stderr => {
                    stderr_text.push_str(
                        event.payload["data"]
                            .as_str()
                            .expect("stderr event payload contains text"),
                    );
                    stderr_seen = stderr_text.contains("stderr:hello-from-rust");
                }
                EventKind::Exit => {
                    exit_code = Some(
                        event.payload["returnCode"]
                            .as_i64()
                            .expect("exit event contains an integer returnCode"),
                    );
                }
                EventKind::Unknown(kind) => panic!("unexpected supervisor event kind: {kind}"),
            }
        }
        readers_drained =
            exit_code.is_some() && !result.stdout_reader_alive && !result.stderr_reader_alive;
    }
    assert!(
        readers_drained,
        "supervisor output readers did not drain before timeout"
    );
    assert!(
        stderr_seen,
        "timed out waiting for expected stderr stream event"
    );
    assert_eq!(exit_code, Some(0));
    assert!(
        stdout_payloads
            .iter()
            .any(|payload| payload["result"]["echo"] == "hello-from-rust")
    );
    client
        .ack(AckRequest {
            handle: handle.clone(),
            sequence: cursor,
            extra: BTreeMap::new(),
        })
        .unwrap();
    let receipt = client
        .operation_status(OperationStatusRequest {
            handle: handle.clone(),
            operation_id: OperationId("monitor:rust-supervisor-test".into()),
            extra: BTreeMap::new(),
        })
        .unwrap();
    assert_eq!(
        receipt.response.unwrap()["result"]["echo"],
        "hello-from-rust"
    );

    let command_handle = SupervisorHandle(format!("server-command:retire-{}", std::process::id()));
    let quick_script = "import json; print(json.dumps({'done':True}),flush=True)";
    let mut quick = open_request(
        &command_handle.0,
        vec![
            python.clone(),
            "-u".into(),
            "-c".into(),
            quick_script.into(),
        ],
        &workspace,
    );
    quick.env.clear();
    let quick_open = client.open(quick).unwrap();
    supervisor.own_child(quick_open.pid);
    let exit = poll_event(
        &mut client,
        &command_handle,
        EventCursor(quick_open.acknowledged),
        |event| event.kind == EventKind::Exit,
    );
    let mut ended = false;
    let reader_deadline = Instant::now() + HANG_GUARD;
    while !ended && Instant::now() < reader_deadline {
        let next = client
            .next(CursorRequest {
                handle: command_handle.clone(),
                cursor: exit.sequence,
                extra: BTreeMap::new(),
            })
            .unwrap();
        ended =
            next.return_code.is_some() && !next.stdout_reader_alive && !next.stderr_reader_alive;
        if !ended {
            thread::park_timeout(Duration::from_millis(1));
        }
    }
    assert!(ended, "command readers did not stop before retirement");
    client
        .ack(AckRequest {
            handle: command_handle.clone(),
            sequence: exit.sequence,
            extra: BTreeMap::new(),
        })
        .unwrap();
    assert!(
        client
            .retire_command(RetireCommandRequest {
                handle: command_handle,
                generation: quick_open.generation,
                extra: BTreeMap::new(),
            })
            .unwrap()
            .retired
    );

    let admin_handle = SupervisorHandle(format!("rust-admin-{}", std::process::id()));
    let blocking_script = "import sys; print('waiting',flush=True); sys.stdin.readline()";
    let admin_open = client
        .open(open_request(
            &admin_handle.0,
            vec![python, "-u".into(), "-c".into(), blocking_script.into()],
            &workspace,
        ))
        .unwrap();
    supervisor.own_child(admin_open.pid);
    let health = client.health(HealthRequest::default()).unwrap();
    let identity = health
        .handles
        .iter()
        .find(|item| item.id == admin_handle.0)
        .unwrap();
    let mut operator =
        SupervisorClient::connect_operator(safe_socket_path(&state), state.0.clone()).unwrap();
    let closed = operator
        .admin_close_handle(AdminCloseHandleRequest {
            handle: admin_handle,
            expected_pid: admin_open.pid,
            expected_start_time: identity.start_time.clone().unwrap(),
            expected_signature: identity.signature.clone(),
            extra: BTreeMap::new(),
        })
        .unwrap();
    assert!(closed.closed);
    assert_eq!(closed.pid, admin_open.pid);

    assert!(
        client
            .finish_fallback(FinishFallbackRequest::default())
            .unwrap()
            .finished
    );

    let mut bad = std::os::unix::net::UnixStream::connect(safe_socket_path(&state)).unwrap();
    let bad_handshake =
        json!({"protocol":99,"stateDir":state.0.to_string_lossy(),"backendId":"bad-version"});
    serde_json::to_writer(&mut bad, &bad_handshake).unwrap();
    use std::io::Write as _;
    bad.write_all(b"\n").unwrap();
    let mut response = String::new();
    std::io::BufReader::new(&bad)
        .read_line(&mut response)
        .unwrap();
    let response: Value = serde_json::from_str(&response).unwrap();
    assert!(response["error"].as_str().unwrap().contains("protocol"));

    drop(operator);
    drop(client);
    assert!(
        !pid_exists(quick_open.pid),
        "retired test child remains alive"
    );
    drop(supervisor);
    wait_until("supervisor process exit", || !pid_exists(supervisor_pid));
    wait_until("operator-closed child exit", || !pid_exists(admin_open.pid));
    assert!(
        !pid_exists(resumed.pid),
        "provider test child remains alive"
    );
    if state.socket().exists() {
        fs::remove_file(state.socket()).unwrap();
    }
    assert!(
        !state.socket().exists(),
        "disposable supervisor socket cleanup failed"
    );
}

#[test]
fn lost_action_response_is_unknown_outcome() {
    use std::io::{BufRead as _, Write};
    use std::os::unix::net::UnixStream;

    let (client_side, mut server_side) = UnixStream::pair().unwrap();
    thread::spawn(move || {
        let mut line = String::new();
        std::io::BufReader::new(&server_side)
            .read_line(&mut line)
            .unwrap();
        server_side.write_all(b"{\"ok\":true}\n").unwrap();
        let mut line = String::new();
        std::io::BufReader::new(&server_side)
            .read_line(&mut line)
            .unwrap();
        drop(server_side);
    });
    let test_state = DisposableState::new();
    let identity = BackendIdentity {
        backend_id: BackendId("unit-test".into()),
        state_dir: test_state.0.clone(),
    };
    let mut client = SupervisorClient::from_transport(client_side, identity).unwrap();
    let result = client.health(HealthRequest::default());
    assert!(matches!(
        result,
        Err(SupervisorError::UnknownOutcome {
            action: studio_runtime_process::SupervisorAction::Health,
            ..
        })
    ));
}

#[test]
fn handshake_rejection_is_a_typed_protocol_mismatch() {
    use std::io::{BufRead as _, Write};
    use std::os::unix::net::UnixStream;

    let (client_side, mut server_side) = UnixStream::pair().unwrap();
    let server = thread::spawn(move || {
        let mut request = String::new();
        std::io::BufReader::new(&server_side)
            .read_line(&mut request)
            .unwrap();
        server_side
            .write_all(b"{\"error\":\"Supervisor protocol or state directory mismatch\"}\n")
            .unwrap();
    });
    let test_state = DisposableState::new();
    let identity = BackendIdentity {
        backend_id: BackendId("wrong-version-test".into()),
        state_dir: test_state.0.clone(),
    };
    let result = SupervisorClient::from_transport(client_side, identity);
    assert!(matches!(
        result,
        Err(SupervisorError::ProtocolMismatch { expected: 1, .. })
    ));
    server.join().unwrap();
}
