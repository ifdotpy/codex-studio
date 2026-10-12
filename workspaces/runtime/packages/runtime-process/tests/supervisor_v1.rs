#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::BufRead as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use studio_runtime_process::{
    AckRequest, AdminCloseHandleRequest, BackendId, BackendIdentity, CursorRequest, EventCursor,
    EventKind, FinishFallbackRequest, HandleRequest, HealthRequest, OpenRequest, OperationId,
    OperationStatusRequest, RetireCommandRequest, SupervisorClient, SupervisorError,
    SupervisorHandle, WriteRequest,
};

const SOCKET: &str = "supervisor.sock";
const SOURCE: &str = env!("CARGO_MANIFEST_DIR");
const SUPERVISOR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../apps/server/src/codex_process_supervisor.py"
);

struct DisposableState(PathBuf);

impl DisposableState {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "studio-runtime-process-{label}-{}-{unique}",
            std::process::id()
        ));
        assert_outside_runtime_state(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn socket(&self) -> PathBuf {
        self.0.join(SOCKET)
    }

    fn identity(&self) -> BackendIdentity {
        BackendIdentity {
            backend_id: BackendId(format!("rust-runtime-process-{}", std::process::id())),
            state_dir: self.0.clone(),
        }
    }
}

impl Drop for DisposableState {
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.0));
    }
}

fn runtime_default_state_dir() -> PathBuf {
    if let Some(configured) = std::env::var_os("CODEX_AGENTS_STATE_DIR") {
        let configured = PathBuf::from(configured);
        if let Ok(relative) = configured.strip_prefix("~/") {
            return std::env::var_os("HOME")
                .map(PathBuf::from)
                .expect("HOME is required to resolve the runtime state directory")
                .join(relative);
        }
        return configured;
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .expect("HOME is required to resolve the runtime state directory")
        .join(".local/state/codex-agents")
}

fn assert_outside_runtime_state(path: &Path) {
    let runtime_state = runtime_default_state_dir();
    assert_ne!(path, runtime_state, "test state must not be runtime state");
    assert!(
        !path.starts_with(&runtime_state),
        "test state must be outside the runtime state directory: {path:?} is under {runtime_state:?}"
    );
}

struct SupervisorProcess {
    child: Child,
    owned_children: Vec<u32>,
}

impl SupervisorProcess {
    fn start(state: &DisposableState, python: &str) -> Self {
        assert!(
            Path::new(SUPERVISOR).is_file(),
            "Rust supervisor integration test requires the checked-out supervisor script at {SUPERVISOR}"
        );
        let log = File::create(state.0.join("supervisor-stderr.log")).unwrap();
        let child = Command::new(python)
            .arg(SUPERVISOR)
            .arg("--state")
            .arg(&state.0)
            .env("XDG_CACHE_HOME", state.0.join("cache"))
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .current_dir(SOURCE)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap_or_else(|error| {
                panic!(
                    "Rust supervisor integration test requires Python 3; could not start {python:?}: {error}"
                )
            });
        let process = Self {
            child,
            owned_children: Vec::new(),
        };
        let socket = state.socket();
        assert!(
            socket.starts_with(&state.0),
            "refuse a socket outside this test's disposable directory: {socket:?}"
        );
        wait_until("supervisor socket", || socket.exists());
        process
    }

    fn id(&self) -> u32 {
        self.child.id()
    }

    fn own_child(&mut self, pid: u32) {
        self.owned_children.push(pid);
    }
}

fn python_executable() -> String {
    let output = Command::new("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "Rust supervisor integration test requires `python3` in PATH (CI jobs that run this test must provide Python 3): {error}"
            )
        });
    assert!(
        output.status.success(),
        "Rust supervisor integration test requires a working Python 3 interpreter (`python3 -c ...` failed): {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout)
        .expect("python3 executable path must be UTF-8")
        .trim()
        .to_owned()
}

impl Drop for SupervisorProcess {
    fn drop(&mut self) {
        for pid in self
            .owned_children
            .iter()
            .copied()
            .filter(|pid| pid_exists(*pid))
        {
            drop(
                Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .status(),
            );
        }
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        self.child.wait().unwrap();
    }
}

fn wait_until(label: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        thread::yield_now();
    }
    panic!("timed out waiting for {label}");
}

fn connect_client(state: &DisposableState) -> SupervisorClient<std::os::unix::net::UnixStream> {
    let socket = safe_socket_path(state);
    SupervisorClient::connect(&socket, state.identity()).unwrap()
}

fn safe_socket_path(state: &DisposableState) -> PathBuf {
    let socket = state.socket();
    assert!(
        socket.starts_with(&state.0),
        "refuse to connect outside this test's disposable directory: {socket:?}"
    );
    assert_outside_runtime_state(&socket);
    socket
}

fn open_request(handle: &str, command: Vec<String>, cwd: &Path) -> OpenRequest {
    OpenRequest {
        handle: SupervisorHandle(handle.to_owned()),
        command,
        env: BTreeMap::from([("PYTHONUNBUFFERED".into(), "1".into())]),
        cwd: cwd.to_string_lossy().into_owned(),
        extra: BTreeMap::new(),
    }
}

fn poll_event(
    client: &mut SupervisorClient<std::os::unix::net::UnixStream>,
    handle: &SupervisorHandle,
    cursor: EventCursor,
    predicate: impl Fn(&studio_runtime_process::SupervisorEvent) -> bool,
) -> studio_runtime_process::SupervisorEvent {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut current = cursor;
    while Instant::now() < deadline {
        let result = client
            .next(CursorRequest {
                handle: handle.clone(),
                cursor: current,
                extra: BTreeMap::new(),
            })
            .unwrap();
        if let Some(event) = result.event {
            current = event.sequence;
            if predicate(&event) {
                return event;
            }
        }
    }
    panic!(
        "timed out waiting for supervisor event after cursor {}",
        cursor.0
    );
}

fn pid_exists(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "pid="])
            .output()
            .map(|output| !String::from_utf8_lossy(&output.stdout).trim().is_empty())
            .unwrap_or(false)
    }
}

#[test]
fn disposable_python_supervisor_v1_actions_events_replay_and_reattach() {
    let state = DisposableState::new("integration");
    let workspace = state.0.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let python = python_executable();
    let mut supervisor = SupervisorProcess::start(&state, &python);
    let supervisor_pid = supervisor.id();

    let wait_script = "import json,sys; print(json.dumps({'ready':True}),flush=True); m=json.loads(sys.stdin.readline()); print(json.dumps({'id':m['id'],'result':{'echo':m['params']['text']}}),flush=True); print('stderr:'+m['params']['text'],file=sys.stderr,flush=True)";
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
    let mut kinds = Vec::new();
    let mut final_sequence = cursor;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline
        && !(kinds.contains(&EventKind::Stdout)
            && kinds.contains(&EventKind::Stderr)
            && kinds.contains(&EventKind::Exit))
    {
        let result = client
            .next(CursorRequest {
                handle: handle.clone(),
                cursor,
                extra: BTreeMap::new(),
            })
            .unwrap();
        if let Some(event) = result.event {
            cursor = event.sequence;
            final_sequence = cursor;
            if event.kind == EventKind::Stdout {
                assert_eq!(event.payload["result"]["echo"], "hello-from-rust");
            }
            if event.kind == EventKind::Stderr {
                assert!(
                    event.payload["data"]
                        .as_str()
                        .unwrap()
                        .contains("stderr:hello-from-rust")
                );
            }
            if event.kind == EventKind::Exit {
                assert_eq!(event.payload["returnCode"], 0);
            }
            if !kinds.contains(&event.kind) {
                kinds.push(event.kind.clone());
            }
        }
    }
    assert!(kinds.contains(&EventKind::Stdout));
    assert!(kinds.contains(&EventKind::Stderr));
    assert!(kinds.contains(&EventKind::Exit));
    client
        .ack(AckRequest {
            handle: handle.clone(),
            sequence: final_sequence,
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
    let reader_deadline = Instant::now() + Duration::from_secs(15);
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
            thread::yield_now();
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
    let test_state = DisposableState::new("unknown-outcome");
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
    let test_state = DisposableState::new("protocol-mismatch");
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
