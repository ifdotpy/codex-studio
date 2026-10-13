#![allow(dead_code)]
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use studio_runtime_process::{
    BackendId, BackendIdentity, CursorRequest, EventCursor, OpenRequest, SupervisorClient,
    SupervisorHandle,
};
const SOCKET: &str = "supervisor.sock";
#[cfg(target_os = "linux")]
const UNIX_SOCKET_PATH_LIMIT: usize = 108;
#[cfg(target_os = "macos")]
const UNIX_SOCKET_PATH_LIMIT: usize = 104;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const UNIX_SOCKET_PATH_LIMIT: usize = 104;
const SOURCE: &str = env!("CARGO_MANIFEST_DIR");
const SUPERVISOR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../apps/server/src/codex_process_supervisor.py"
);

pub(crate) struct DisposableState(pub(crate) PathBuf);

impl DisposableState {
    pub(crate) fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let serial = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("rp-{}-{unique}-{serial}", std::process::id()));
            assert_outside_runtime_state(&path);
            assert_socket_path_fits(&path.join(SOCKET));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create disposable directory: {error}"),
            }
        }
    }

    pub(crate) fn socket(&self) -> PathBuf {
        self.0.join(SOCKET)
    }

    pub(crate) fn identity(&self) -> BackendIdentity {
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

pub(crate) fn assert_socket_path_fits(socket: &Path) {
    let path_bytes = socket.as_os_str().as_bytes().len();
    assert!(
        path_bytes < UNIX_SOCKET_PATH_LIMIT,
        "test TMPDIR is too long for an AF_UNIX supervisor socket: {} ({path_bytes} bytes; platform limit {UNIX_SOCKET_PATH_LIMIT}); choose a shorter TMPDIR",
        socket.display()
    );
}

pub(crate) fn runtime_default_state_dir() -> PathBuf {
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

pub(crate) fn assert_outside_runtime_state(path: &Path) {
    let runtime_state = runtime_default_state_dir();
    assert_ne!(path, runtime_state, "test state must not be runtime state");
    assert!(
        !path.starts_with(&runtime_state),
        "test state must be outside the runtime state directory: {path:?} is under {runtime_state:?}"
    );
}

pub(crate) struct SupervisorProcess {
    child: Child,
    owned_children: Vec<u32>,
    stderr_path: PathBuf,
}

impl SupervisorProcess {
    pub(crate) fn start(state: &DisposableState, python: &str) -> Self {
        assert!(
            Path::new(SUPERVISOR).is_file(),
            "Rust supervisor integration test requires the checked-out supervisor script at {SUPERVISOR}"
        );
        let stderr_path = state.0.join("supervisor-stderr.log");
        let log = File::create(&stderr_path).unwrap();
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
        let mut process = Self {
            child,
            owned_children: Vec::new(),
            stderr_path,
        };
        let socket = state.socket();
        assert!(
            socket.starts_with(&state.0),
            "refuse a socket outside this test's disposable directory: {socket:?}"
        );
        process.wait_for_socket(&socket);
        process
    }

    pub(crate) fn wait_for_socket(&mut self, socket: &Path) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if socket.exists() {
                return;
            }
            match self.child.try_wait() {
                Ok(Some(status)) => panic!(
                    "disposable supervisor exited before creating socket {} (status {status}); stderr:\n{}",
                    socket.display(),
                    self.stderr_output()
                ),
                Ok(None) => thread::yield_now(),
                Err(error) => panic!(
                    "failed checking disposable supervisor while waiting for socket {}: {error}; stderr:\n{}",
                    socket.display(),
                    self.stderr_output()
                ),
            }
        }
        panic!(
            "timed out waiting for disposable supervisor socket {} after 15-second hang guard; child status: {:?}; stderr:\n{}",
            socket.display(),
            self.child.try_wait(),
            self.stderr_output()
        );
    }

    pub(crate) fn stderr_output(&self) -> String {
        fs::read_to_string(&self.stderr_path)
            .unwrap_or_else(|error| format!("<could not read stderr log: {error}>"))
    }

    pub(crate) fn id(&self) -> u32 {
        self.child.id()
    }

    pub(crate) fn own_child(&mut self, pid: u32) {
        self.owned_children.push(pid);
    }
}

pub(crate) fn python_executable() -> String {
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

pub(crate) fn wait_until(label: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        thread::yield_now();
    }
    panic!("timed out waiting for {label}");
}

pub(crate) fn connect_client(
    state: &DisposableState,
) -> SupervisorClient<std::os::unix::net::UnixStream> {
    let socket = safe_socket_path(state);
    SupervisorClient::connect(&socket, state.identity()).unwrap()
}

pub(crate) fn safe_socket_path(state: &DisposableState) -> PathBuf {
    let socket = state.socket();
    assert!(
        socket.starts_with(&state.0),
        "refuse to connect outside this test's disposable directory: {socket:?}"
    );
    assert_outside_runtime_state(&socket);
    socket
}

pub(crate) fn open_request(handle: &str, command: Vec<String>, cwd: &Path) -> OpenRequest {
    OpenRequest {
        handle: SupervisorHandle(handle.to_owned()),
        command,
        env: BTreeMap::from([("PYTHONUNBUFFERED".into(), "1".into())]),
        cwd: cwd.to_string_lossy().into_owned(),
        extra: BTreeMap::new(),
    }
}

pub(crate) fn poll_event(
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

pub(crate) fn pid_exists(pid: u32) -> bool {
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
