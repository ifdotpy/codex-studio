#![cfg(unix)]
#![allow(clippy::disallowed_methods, clippy::unwrap_used, clippy::expect_used)]

//! The Mac `host_exec` service as the backend runs it: the
//! `studio-runtime-server host-exec-service` process, its start and its
//! replacement, and the unchanged guest broker against it.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use studio_runtime_services::host_exec_service::lifecycle::{ensure_service, service_call};

const TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// Only guards against a hang; a passing run never waits this long.
const HANG_GUARD: Duration = Duration::from_secs(60);

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_studio-runtime-server"))
}

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..")
}

struct State {
    _temp: tempfile::TempDir,
    path: PathBuf,
}

impl State {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(temp.path()).unwrap().join("linux-vm");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("host-exec-token"), TOKEN).unwrap();
        Self { _temp: temp, path }
    }

    fn call(&self, method: &str, params: Option<&Value>) -> Value {
        service_call(&self.path, TOKEN, method, params)
            .unwrap_or_else(|error| panic!("{method}: {error:?}"))
    }

    fn health(&self) -> Value {
        self.call("health", None)
    }

    fn start(&self, build: &str) -> Child {
        let child = Command::new(binary())
            .args(["host-exec-service"])
            .arg(&self.path)
            .args(["--build", build])
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + HANG_GUARD;
        while service_call(&self.path, TOKEN, "health", None).is_err() {
            assert!(Instant::now() < deadline, "the service did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        child
    }

    fn wait_idle(&self) {
        let deadline = Instant::now() + HANG_GUARD;
        while self.health()["activeCommands"] != 0 {
            assert!(Instant::now() < deadline, "a command did not end");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Retire the running service; a fixture never leaves a service behind.
    fn retire(&self) {
        let Ok(health) = service_call(&self.path, TOKEN, "health", None) else {
            return;
        };
        let params = json!({"buildId": health["buildId"], "pid": health["pid"]});
        drop(service_call(
            &self.path,
            TOKEN,
            "service.retire",
            Some(&params),
        ));
        let pid =
            nix::unistd::Pid::from_raw(i32::try_from(health["pid"].as_u64().unwrap()).unwrap());
        let deadline = Instant::now() + HANG_GUARD;
        while nix::sys::signal::kill(pid, None).is_ok() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn acquire(&self) -> Value {
        self.call(
            "acquire",
            Some(
                &json!({"operationId": "upgrade-operation", "agentId": "agent",
                         "projectId": "project", "linePath": "/line"}),
            ),
        )
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.retire();
    }
}

#[test]
fn an_idle_service_of_another_build_is_replaced_and_keeps_receipts_and_leases() {
    let state = State::new();
    let mut old = state.start("old-build");
    let before = state.health();
    assert_eq!(before["buildId"], "old-build");
    let op = state.acquire();
    let lease = Path::new(op["slotPath"].as_str().unwrap())
        .parent()
        .unwrap()
        .join("lease.json");
    let lease_bytes = std::fs::read(&lease).unwrap();
    let token = ensure_service(&state.path, &binary(), "new-build", None).unwrap();
    assert_eq!(token, TOKEN);
    let after = state.health();
    assert_eq!(after["buildId"], "new-build");
    assert_ne!(after["pid"], before["pid"]);
    assert!(old.wait().unwrap().success());
    assert_eq!(std::fs::read(&lease).unwrap(), lease_bytes);
    // The saved receipt answers the exact retry; the operation keeps its lease.
    assert_eq!(state.acquire(), op);
    assert_eq!(
        state.call(
            "status",
            Some(&json!({"operationId": "upgrade-operation", "agentId": "agent"}))
        ),
        op
    );
    // The current build stays as it is.
    ensure_service(&state.path, &binary(), "new-build", None).unwrap();
    assert_eq!(state.health()["pid"], after["pid"]);
}

#[test]
fn a_busy_service_of_another_build_stays_with_its_command() {
    let state = State::new();
    let mut old = state.start("old-build");
    let before = state.health();
    let op = state.acquire();
    let empty = "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";
    state.call(
        "run",
        Some(
            &json!({"operationId": op["operationId"], "agentId": "agent",
                     "argv": ["/bin/sleep", "30"], "manifestSha256": empty, "timeoutSeconds": 60}),
        ),
    );
    ensure_service(&state.path, &binary(), "new-build", None).unwrap();
    assert_eq!(state.health()["pid"], before["pid"]);
    assert_eq!(state.health()["buildId"], "old-build");
    assert!(old.try_wait().unwrap().is_none());
    let refused = service_call(
        &state.path,
        TOKEN,
        "service.retire",
        Some(&json!({"buildId": "old-build", "pid": before["pid"]})),
    )
    .unwrap_err();
    assert_eq!(refused.code, "busy");
    state.call(
        "cancel",
        Some(&json!({"operationId": op["operationId"], "agentId": "agent"})),
    );
    state.wait_idle();
    state.retire();
    assert!(old.wait().unwrap().success());
}

#[test]
fn a_new_state_gets_a_token_and_a_service() {
    let temp = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(temp.path()).unwrap().join("linux-vm");
    let token = ensure_service(&path, &binary(), "fresh-build", None).unwrap();
    assert_eq!(token.len(), 64);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(path.join("host-exec-token"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let health = service_call(&path, &token, "health", None).unwrap();
    assert_eq!(health["buildId"], "fresh-build");
    let params = json!({"buildId": "fresh-build", "pid": health["pid"]});
    service_call(&path, &token, "service.retire", Some(&params)).unwrap();
}

/// The guest broker of the VM (`vm-guest/host_exec.py`) runs unchanged against
/// the service process (`vm-guest/host_exec_mac_contract.py`).
#[test]
fn the_guest_broker_runs_against_the_mac_service() {
    let script = repository().join("workspaces/runtime/apps/vm-guest/host_exec_mac_contract.py");
    let output = Command::new("python3")
        .arg("-B")
        .arg(&script)
        .env("HOST_EXEC_SERVICE_BINARY", binary())
        .output()
        .expect("python3 3.11 or newer runs the guest broker");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
