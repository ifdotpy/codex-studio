#![cfg(unix)]
#![allow(clippy::disallowed_methods, clippy::unwrap_used, clippy::expect_used)]

//! The Mac `host_exec` service as the backend runs it: the
//! `studio-runtime-server host-exec-service` process, its start and its
//! replacement, and the unchanged guest broker against it.

use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
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
    children: RefCell<Vec<Child>>,
}

impl State {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(temp.path()).unwrap().join("linux-vm");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("host-exec-token"), TOKEN).unwrap();
        Self {
            _temp: temp,
            path,
            children: RefCell::default(),
        }
    }

    fn call(&self, method: &str, params: Option<&Value>) -> Value {
        service_call(&self.path, TOKEN, method, params)
            .unwrap_or_else(|error| panic!("{method}: {error:?}"))
    }

    fn health(&self) -> Value {
        self.call("health", None)
    }

    fn start(&self, build: &str) -> u32 {
        let child = Command::new(binary())
            .args(["host-exec-service"])
            .arg(&self.path)
            .args(["--build", build])
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        self.children.borrow_mut().push(child);
        let deadline = Instant::now() + HANG_GUARD;
        while service_call(&self.path, TOKEN, "health", None).is_err() {
            assert!(Instant::now() < deadline, "the service did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        pid
    }

    fn wait_idle(&self) {
        let deadline = Instant::now() + HANG_GUARD;
        while self.health()["activeCommands"] != 0 {
            assert!(Instant::now() < deadline, "a command did not end");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_child(&self, pid: u32) -> ExitStatus {
        let mut children = self.children.borrow_mut();
        let child = children.iter_mut().find(|child| child.id() == pid).unwrap();
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("owned service {pid} did not exit after retirement");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Reap owned children directly; detached replacements can be zombies until
    /// their OS parent reaps them. A live service at the watchdog is a failure.
    fn retire(&self) {
        if let Ok(health) = service_call(&self.path, TOKEN, "health", None) {
            let params = json!({"buildId": health["buildId"], "pid": health["pid"]});
            service_call(&self.path, TOKEN, "service.retire", Some(&params)).unwrap();
            let pid = u32::try_from(health["pid"].as_u64().unwrap()).unwrap();
            if self.children.borrow().iter().any(|child| child.id() == pid) {
                assert!(self.wait_child(pid).success());
            } else {
                let deadline = Instant::now() + HANG_GUARD;
                while service_is_live(pid) {
                    assert!(
                        Instant::now() < deadline,
                        "detached service {pid} did not retire"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
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
        for child in self.children.get_mut() {
            if child.try_wait().unwrap().is_none() {
                child.kill().unwrap();
                child.wait().unwrap();
            }
        }
    }
}

fn service_is_live(pid: u32) -> bool {
    let status = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    assert!(
        status.status.success() || status.status.code() == Some(1),
        "ps failed"
    );
    let state = String::from_utf8(status.stdout).unwrap();
    let state = state.trim();
    !state.is_empty() && !state.starts_with('Z')
}

#[test]
fn an_idle_service_of_another_build_is_replaced_and_keeps_receipts_and_leases() {
    let state = State::new();
    let old = state.start("old-build");
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
    assert!(state.wait_child(old).success());
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
    let old = state.start("old-build");
    let before = state.health();
    let op = state.acquire();
    let empty = "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";
    let pipe = state.path.join("busy-release");
    assert!(
        Command::new("mkfifo")
            .arg(&pipe)
            .status()
            .unwrap()
            .success()
    );
    let mut release = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&pipe)
        .unwrap();
    state.call(
        "run",
        Some(
            &json!({"operationId": op["operationId"], "agentId": "agent",
                     "argv": ["/bin/sh", "-c", "read release < \"$1\"", "busy-holder", pipe], "manifestSha256": empty, "timeoutSeconds": 60}),
        ),
    );
    ensure_service(&state.path, &binary(), "new-build", None).unwrap();
    assert_eq!(state.health()["pid"], before["pid"]);
    assert_eq!(state.health()["buildId"], "old-build");
    assert!(
        state
            .children
            .borrow_mut()
            .iter_mut()
            .find(|child| child.id() == old)
            .unwrap()
            .try_wait()
            .unwrap()
            .is_none()
    );
    let refused = service_call(
        &state.path,
        TOKEN,
        "service.retire",
        Some(&json!({"buildId": "old-build", "pid": before["pid"]})),
    )
    .unwrap_err();
    assert_eq!(refused.code, "busy");
    release.write_all(b"release\n").unwrap();
    state.wait_idle();
    let retired_at = Instant::now();
    state.retire();
    assert!(
        retired_at.elapsed() < Duration::from_secs(2),
        "retirement must reap the owned service promptly"
    );
    assert!(state.wait_child(old).success());
}

#[test]
fn a_new_state_gets_a_token_and_a_service() {
    let state = State::new();
    std::fs::remove_file(state.path.join("host-exec-token")).unwrap();
    let path = &state.path;
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
    let deadline = Instant::now() + HANG_GUARD;
    while service_is_live(health["pid"].as_u64().unwrap() as u32) {
        assert!(Instant::now() < deadline, "fresh service did not retire");
        std::thread::sleep(Duration::from_millis(5));
    }
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

/// An idle service that an earlier build started without the host bridge folder is
/// replaced by the current build, which starts with `--browser-host`; its commands then get
/// the launcher as `BROWSER`. The service has no launch signature: only the build identity
/// decides, and a busy service of another build stays
/// (`a_busy_service_of_another_build_stays_with_its_command`).
#[test]
fn an_idle_service_without_the_browser_folder_is_replaced_and_its_commands_get_the_launcher() {
    let state = State::new();
    let old = state.start("old-build");
    let before = state.health();
    let browser_host = state.path.parent().unwrap().join("browser-host");
    std::fs::create_dir(&browser_host).unwrap();
    let launcher = browser_host.join("studio-browser");
    std::fs::write(&launcher, "#!/bin/sh\n").unwrap();
    ensure_service(&state.path, &binary(), "new-build", Some(&browser_host)).unwrap();
    let after = state.health();
    assert_eq!(after["buildId"], "new-build");
    assert_ne!(after["pid"], before["pid"]);
    assert!(state.wait_child(old).success());
    let op = state.acquire();
    let empty = "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";
    state.call(
        "run",
        Some(&json!({"operationId": op["operationId"], "agentId": "agent",
                     "argv": ["/bin/sh", "-c", "printf '%s|%s' \"$BROWSER\" \"$STUDIO_BROWSER_SESSION\""],
                     "manifestSha256": empty, "timeoutSeconds": 30})),
    );
    let mut output = String::new();
    let mut after_seq = 0;
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        assert!(Instant::now() < deadline, "the command did not end");
        let attached = state.call(
            "attach",
            Some(
                &json!({"operationId": op["operationId"], "agentId": "agent",
                         "afterSeq": after_seq, "waitSeconds": 1}),
            ),
        );
        let events = attached["events"].as_array().unwrap();
        for event in events {
            after_seq = event["seq"].as_i64().unwrap();
            output.push_str(
                event
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
        }
        if attached["status"]["state"] != "running" && events.is_empty() {
            break;
        }
    }
    assert_eq!(output, format!("{}|agent", launcher.display()));
}
