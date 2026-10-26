#![allow(dead_code)]
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Instant;
use studio_runtime_store::path_identity::CanonicalPath;

use studio_runtime_process::{
    BackendId, BackendIdentity, CursorRequest, EventCursor, OpenRequest, SupervisorClient,
    SupervisorHandle,
};
mod supervisor_ready;
pub(crate) use supervisor_ready::{HANG_GUARD, watch_changes};

const SOCKET: &str = "supervisor.sock";
#[cfg(target_os = "linux")]
const UNIX_SOCKET_PATH_LIMIT: usize = 108;
#[cfg(target_os = "macos")]
const UNIX_SOCKET_PATH_LIMIT: usize = 104;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const UNIX_SOCKET_PATH_LIMIT: usize = 104;
const SOURCE: &str = env!("CARGO_MANIFEST_DIR");

pub(crate) struct DisposableState(pub(crate) PathBuf, tempfile::TempDir);

impl DisposableState {
    pub(crate) fn new() -> Self {
        let directory = tempfile::Builder::new().prefix("rp-").tempdir().unwrap();
        // Supervisors compare the handshake state directory with their resolved
        // root; macOS temporary directories sit behind the /var symlink.
        let path = CanonicalPath::existing(directory.path()).unwrap();
        assert_outside_runtime_state(&path);
        assert_socket_path_fits(&path.join(SOCKET));
        Self(path.to_path_buf(), directory)
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
}

impl SupervisorProcess {
    pub(crate) fn start(state: &DisposableState) -> Self {
        let binary = std::env::var_os("STUDIO_SUPERVISOR_BINARY")
            .or_else(|| option_env!("CARGO_BIN_EXE_codex-process-supervisor").map(Into::into))
            .unwrap_or_else(|| {
                panic!(
                    "supervisor integration requires the built codex-process-supervisor; run the fast Rust test recipe so STUDIO_SUPERVISOR_BINARY is set"
                )
            });
        Self::start_command(state, Command::new(binary), "Rust supervisor")
    }

    fn start_command(state: &DisposableState, mut command: Command, description: &str) -> Self {
        let stderr_path = state.0.join("supervisor-stderr.log");
        let log = File::create(&stderr_path).unwrap();
        let child = command
            .arg("--state")
            .arg(&state.0)
            .envs(child_environment(&state.0))
            .env("XDG_CACHE_HOME", state.0.join("cache"))
            .current_dir(SOURCE)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap_or_else(|error| panic!("could not start {description}: {error}"));
        let mut process = Self {
            child,
            owned_children: Vec::new(),
        };
        let socket = state.socket();
        assert!(
            socket.starts_with(&state.0),
            "refuse a socket outside this test's disposable directory: {socket:?}"
        );
        supervisor_ready::wait(&mut process.child, state.identity(), &stderr_path);
        process
    }

    pub(crate) fn id(&self) -> u32 {
        self.child.id()
    }

    pub(crate) fn own_child(&mut self, pid: u32) {
        self.owned_children.push(pid);
    }
}

/// Minimal replies for the compact JSON inputs used by these shell fixtures.
/// IDs are the fixtures' plain strings or nonnegative integers, without escapes.
pub(crate) fn shell_command(script: &str) -> Vec<String> {
    const RPC_ID_FUNCTION: &str = r#"
rpc_id() {
 printf '%s\n' "$1" | sed -E 's/.*"id":("[^"]*"|[0-9]+).*/\1/'
}
"#;
    vec![
        "/bin/sh".into(),
        "-c".into(),
        format!("{RPC_ID_FUNCTION}\n{script}"),
    ]
}

/// A POSIX shell for a child that a reattach must verify. macOS does not show
/// the launch environment of an Apple platform binary such as `/bin/sh` to
/// other processes in `KERN_PROCARGS2`, so the reattach would refuse the child
/// as unverifiable. Provider binaries are not platform binaries. On macOS this
/// is an ad-hoc signed copy of `/bin/bash` in `root`, named `sh` so that bash
/// keeps its POSIX mode.
#[cfg(target_os = "macos")]
#[allow(
    clippy::disallowed_methods,
    reason = "test helper; codesign signs one local file and exits at once"
)]
pub(crate) fn retained_shell(root: &Path) -> String {
    let shell = root.join("sh");
    if !shell.exists() {
        std::fs::copy("/bin/bash", &shell).unwrap();
        let signed = Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&shell)
            .output()
            .unwrap();
        assert!(
            signed.status.success(),
            "ad-hoc signature of the test shell failed: {}",
            String::from_utf8_lossy(&signed.stderr)
        );
    }
    shell.to_string_lossy().into_owned()
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn retained_shell(_root: &Path) -> String {
    "/bin/sh".into()
}

/// [`shell_command`] run by [`retained_shell`].
pub(crate) fn retained_shell_command(root: &Path, script: &str) -> Vec<String> {
    let mut command = shell_command(script);
    command[0] = retained_shell(root);
    command
}

impl Drop for SupervisorProcess {
    #[allow(clippy::disallowed_methods)]
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
        env: child_environment(cwd),
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
    let (_watcher, changes) = watch_changes(&client.backend_identity().state_dir);
    let deadline = Instant::now() + HANG_GUARD;
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
        } else {
            changes
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("supervisor journal changes before the hang guard")
                .expect("supervisor journal watcher remains healthy");
        }
    }
    panic!(
        "timed out waiting for supervisor event after cursor {}",
        cursor.0
    );
}

#[cfg_attr(
    not(target_os = "linux"),
    allow(
        clippy::disallowed_methods,
        reason = "test helper; ps prints one line and exits at once"
    )
)]
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

pub(crate) fn child_environment(root: &Path) -> BTreeMap<String, String> {
    let mut env = BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]);
    for (name, dir) in [
        ("HOME", "home"),
        ("CODEX_HOME", "codex"),
        ("CLAUDE_CONFIG_DIR", "claude"),
    ] {
        let path = root.join(dir);
        std::fs::create_dir_all(&path).unwrap();
        env.insert(name.into(), path.to_string_lossy().into_owned());
    }
    // A macOS launcher stub (Homebrew Python) re-executes itself after
    // CoreFoundation adds this ambient value when it is absent. A desktop
    // backend always passes it, so pass it here to keep the launch exact.
    #[cfg(target_os = "macos")]
    if let Ok(value) = std::env::var("__CF_USER_TEXT_ENCODING") {
        env.insert("__CF_USER_TEXT_ENCODING".into(), value);
    }
    env
}
