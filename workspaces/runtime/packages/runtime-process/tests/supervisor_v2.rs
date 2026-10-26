#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::thread;
use std::time::{Duration, Instant};

use crate::process_support as support;
use serde_json::{Value, json};
use studio_runtime_process::{
    AckRequest, AdminCloseHandleRequest, BackendId, BackendIdentity, CursorRequest, EventCursor,
    EventKind, FinishFallbackRequest, HandleRequest, HealthRequest, OperationId,
    OperationStatusRequest, RetireCommandRequest, SupervisorClient, SupervisorError,
    SupervisorHandle, WriteRequest,
};
use support::*;

struct DropOpenResponse {
    stream: std::os::unix::net::UnixStream,
    outgoing: Vec<u8>,
    drop_next_open_response: bool,
}

impl DropOpenResponse {
    fn new(stream: std::os::unix::net::UnixStream) -> Self {
        Self {
            stream,
            outgoing: Vec::new(),
            drop_next_open_response: false,
        }
    }
}

impl Read for DropOpenResponse {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.drop_next_open_response {
            let mut byte = [0];
            loop {
                match self.stream.read(&mut byte)? {
                    0 => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "supervisor closed before the Open response",
                        ));
                    }
                    _ if byte[0] == b'\n' => {
                        self.drop_next_open_response = false;
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionReset,
                            "test transport dropped the committed Open response",
                        ));
                    }
                    _ => {}
                }
            }
        }
        self.stream.read(buffer)
    }
}

impl Write for DropOpenResponse {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.stream.write(buffer)?;
        for byte in &buffer[..written] {
            self.outgoing.push(*byte);
            if *byte == b'\n' {
                let is_open = serde_json::from_slice::<Value>(&self.outgoing)
                    .ok()
                    .and_then(|request| {
                        request
                            .get("action")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .is_some_and(|action| action == "open");
                if is_open {
                    self.drop_next_open_response = true;
                }
                self.outgoing.clear();
            }
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

#[test]
fn disposable_rust_supervisor_v2_actions_events_replay_and_reattach() {
    let state = DisposableState::new();
    let workspace = state.0.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut supervisor = SupervisorProcess::start(&state);
    let supervisor_pid = supervisor.id();

    let wait_script = r#"
printf '{"ready":true}\n'
IFS= read -r line
id=$(rpc_id "$line")
text=$(printf '%s\n' "$line" | sed -E 's/.*"text":"([^"]*)".*/\1/')
printf 'stderr:%s' "$text" >&2
printf '{"id":%s,"result":{"echo":"%s"}}\n' "$id" "$text"
"#;
    let command = shell_command(wait_script);
    let handle = SupervisorHandle("rust-v2-main".into());

    let mut client = connect_client(&state);
    let health = client.health(HealthRequest::default()).unwrap();
    assert_eq!(
        health.protocol,
        studio_runtime_process::SUPERVISOR_PROTOCOL_V2
    );
    assert_eq!(
        health.durability,
        studio_runtime_process::Durability::SqliteFullSyncPerRequest
    );
    drop(client);

    // Force a response loss after the supervisor commits Open, so this test
    // always covers production's stable-handle recovery path.
    let socket = std::os::unix::net::UnixStream::connect(safe_socket_path(&state)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut lost_response =
        SupervisorClient::from_transport(DropOpenResponse::new(socket), state.identity()).unwrap();
    assert!(matches!(
        lost_response.open(open_request(&handle.0, command.clone(), &workspace)),
        Err(SupervisorError::UnknownOutcome {
            action: studio_runtime_process::SupervisorAction::Open,
            ..
        })
    ));
    drop(lost_response);

    // Reattach to the same child instead of using another handle or spawning twice.
    let mut client = connect_client(&state);
    let opened = client
        .open(open_request(&handle.0, command.clone(), &workspace))
        .expect("stable-handle Open recovery");
    assert!(
        opened.resumed,
        "unknown Open must reattach the active child"
    );
    supervisor.own_child(opened.pid);
    assert_eq!(
        opened.generation,
        studio_runtime_process::Generation(1),
        "the child must be spawned exactly once"
    );
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
            native_id: Some(42.into()),
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

    let (_watcher, changes) = watch_changes(&state.0);
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
        let has_event = result.event.is_some();
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
        if !has_event && !(readers_drained && stderr_seen) {
            changes
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("journal change before the hang guard")
                .expect("journal watcher remains healthy");
        }
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
    let mut quick = open_request(
        &command_handle.0,
        shell_command(r#"printf '{"done":true}\n'"#),
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
    let ended = client
        .next(CursorRequest {
            handle: command_handle.clone(),
            cursor: exit.sequence,
            extra: BTreeMap::new(),
        })
        .unwrap();
    assert!(
        ended.return_code.is_some() && !ended.stdout_reader_alive && !ended.stderr_reader_alive,
        "a committed exit event must already prove the readers stopped"
    );
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
    let blocking_script = "printf 'waiting\\n'; IFS= read -r line";
    let admin_open = client
        .open(open_request(
            &admin_handle.0,
            shell_command(blocking_script),
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
    assert!(
        !pid_exists(supervisor_pid),
        "the owned supervisor was reaped"
    );
    assert!(
        !pid_exists(admin_open.pid),
        "operator close reaped its child"
    );
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
        server_side
            .write_all(b"{\"ok\":true,\"protocol\":2}\n")
            .unwrap();
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
        Err(SupervisorError::ProtocolMismatch {
            expected: studio_runtime_process::SUPERVISOR_PROTOCOL_V2,
            ..
        })
    ));
    server.join().unwrap();
}

#[test]
fn supervisor_rejects_retired_protocol_before_admission() {
    let state = DisposableState::new();
    let _supervisor = SupervisorProcess::start(&state);
    let mut socket = std::os::unix::net::UnixStream::connect(state.socket()).unwrap();
    socket.set_read_timeout(Some(HANG_GUARD)).unwrap();
    writeln!(
        socket,
        "{}",
        json!({"protocol":1,"stateDir":state.0,"backendId":"retired-client"})
    )
    .unwrap();
    let mut response = String::new();
    std::io::BufReader::new(&socket)
        .read_line(&mut response)
        .unwrap();
    let response: Value = serde_json::from_str(&response).unwrap();
    assert!(
        response.get("error").is_some(),
        "retired protocol admitted: {response}"
    );
    assert_ne!(response.get("ok"), Some(&Value::Bool(true)));
    drop(socket);
    assert_eq!(
        connect_client(&state)
            .health(HealthRequest::default())
            .unwrap()
            .protocol,
        studio_runtime_process::SUPERVISOR_PROTOCOL_V2
    );
}
