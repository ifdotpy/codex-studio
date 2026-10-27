#![cfg(unix)]
#![allow(clippy::disallowed_methods, clippy::unwrap_used, clippy::expect_used)]

//! The Mac `host_exec` service as the backend runs it: the
//! `studio-runtime-server host-exec-service` process, its start and its
//! replacement, and the unchanged guest broker against it.

use std::cell::{Cell, RefCell};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use studio_runtime_services::host_exec_service::{
    LEASE_FILE, SERVICE_READY_LINE,
    lifecycle::{ensure_service, service_call},
};

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
    owner: Option<UnixListener>,
    channels: RefCell<Vec<UnixStream>>,
    launches: RefCell<Vec<PathBuf>>,
    command_possible: Cell<bool>,
}

impl State {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(temp.path()).unwrap().join("linux-vm");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("host-exec-token"), TOKEN).unwrap();
        let owner = UnixListener::bind(temp.path().join("owner.sock")).unwrap();
        std::fs::set_permissions(
            temp.path().join("owner.sock"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        owner.set_nonblocking(true).unwrap();
        Self {
            _temp: temp,
            path,
            children: RefCell::default(),
            owner: Some(owner),
            channels: RefCell::default(),
            launches: RefCell::default(),
            command_possible: Cell::new(false),
        }
    }

    fn token(&self) -> String {
        std::fs::read_to_string(self.path.join("host-exec-token")).unwrap()
    }

    fn collect_channels(&self) {
        if let Some(owner) = &self.owner {
            while let Ok((stream, _)) = owner.accept() {
                self.channels.borrow_mut().push(stream);
            }
        }
    }

    fn ensure(&self, build: &str, browser_host: Option<&Path>) -> Result<String, String> {
        let root = self.path.parent().unwrap();
        let index = self.launches.borrow().len();
        let receipt = root.join(format!("service-child-{index}.json"));
        let launcher = root.join(format!("service-launcher-{index}"));
        let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
        let script = repository()
            .join("workspaces/runtime/apps/runtime-server/tests/fixtures/host_exec_child.py");
        std::fs::write(
            &launcher,
            format!(
                "#!/bin/sh\nexec python3 {} {} {} {} {} \"$@\"\n",
                quote(&script),
                quote(&root.join("owner.sock")),
                quote(&receipt),
                HANG_GUARD.as_secs(),
                quote(&binary()),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
        self.launches.borrow_mut().push(receipt);
        let result = ensure_service(&self.path, &launcher, build, browser_host);
        self.collect_channels();
        result
    }

    fn call(&self, method: &str, params: Option<&Value>) -> Value {
        // A lost run reply does not prove that the command never started.
        if method == "run" {
            self.command_possible.set(true);
        }
        service_call(&self.path, &self.token(), method, params)
            .unwrap_or_else(|error| panic!("{method}: {error:?}"))
    }

    fn health(&self) -> Value {
        self.call("health", None)
    }

    fn start(&self, build: &str) -> u32 {
        let mut child = Command::new(binary())
            .args(["host-exec-service"])
            .arg(&self.path)
            .args(["--build", build])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let stdout = child.stdout.take().unwrap();
        self.children.borrow_mut().push(child);
        let mut ready = String::new();
        BufReader::new(stdout).read_line(&mut ready).unwrap();
        assert_eq!(ready.trim_end(), SERVICE_READY_LINE);
        assert_eq!(self.health()["pid"], pid);
        pid
    }

    fn wait_idle(&self) {
        let mut after = 0;
        loop {
            let attached = self.call(
                "attach",
                Some(&json!({
                    "operationId":"upgrade-operation", "agentId":"agent",
                    "afterSeq":after, "waitSeconds":HANG_GUARD.as_secs()
                })),
            );
            let events = attached["events"].as_array().unwrap();
            if let Some(event) = events.last() {
                after = event["seq"].as_i64().unwrap();
            }
            if attached["status"]["state"] != "running" && events.is_empty() {
                break;
            }
        }
    }

    fn wait_child(&self, pid: u32) -> ExitStatus {
        self.children
            .borrow_mut()
            .iter_mut()
            .find(|child| child.id() == pid)
            .unwrap()
            .wait()
            .unwrap()
    }

    fn retire(&self) {
        assert!(self.cancel_command(), "fixture command cleanup is unproved");
        let token = self.token();
        if let Ok(health) = service_call(&self.path, &token, "health", None) {
            let params = json!({"buildId": health["buildId"], "pid": health["pid"]});
            service_call(&self.path, &token, "service.retire", Some(&params)).unwrap();
            let pid = u32::try_from(health["pid"].as_u64().unwrap()).unwrap();
            if self.children.borrow().iter().any(|child| child.id() == pid) {
                assert!(self.wait_child(pid).success());
            } else {
                self.wait_for_released_state();
            }
        }
    }

    // A detached replacement's reaper owns Child::wait. Its service lease is
    // released only after the service stops using this disposable state.
    fn wait_for_released_state(&self) {
        let lease = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.path.join(LEASE_FILE))
            .unwrap();
        lease.lock().unwrap();
    }

    fn cancel_command(&self) -> bool {
        if !self.command_possible.get() {
            return true;
        }
        let Ok(token) = std::fs::read_to_string(self.path.join("host-exec-token")) else {
            return false;
        };
        let params = json!({"operationId":"upgrade-operation","agentId":"agent"});
        if service_call(&self.path, &token, "cancel", Some(&params)).is_err() {
            return false;
        }
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            let Ok(status) = service_call(&self.path, &token, "status", Some(&params)) else {
                return false;
            };
            let Ok(health) = service_call(&self.path, &token, "health", None) else {
                return false;
            };
            if matches!(
                status["state"].as_str(),
                Some("leased" | "released" | "exited")
            ) && health["activeCommands"] == 0
                && health["activeRequests"] == 0
                && health["busy"] == false
            {
                self.command_possible.set(false);
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
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
        // Cleanup must also run while an earlier assertion unwinds.
        let command_stopped = self.cancel_command();
        if command_stopped
            && let Ok(token) = std::fs::read_to_string(self.path.join("host-exec-token"))
            && let Ok(health) = service_call(&self.path, &token, "health", None)
        {
            let params = json!({"buildId":health["buildId"],"pid":health["pid"]});
            let _result = service_call(&self.path, &token, "service.retire", Some(&params));
        }
        self.collect_channels();
        let owners = self.channels.get_mut().len();
        drop(self.owner.take());
        let mut cleanup_unproved = !command_stopped;
        if command_stopped {
            for channel in self.channels.get_mut().iter_mut() {
                // An already retired child can close first. Its receipt proves exit.
                let _result = channel.write_all(b"C");
            }
        }
        self.channels.get_mut().clear();
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            let records = self
                .launches
                .get_mut()
                .iter()
                .filter_map(|path| {
                    std::fs::read(path)
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                })
                .collect::<Vec<_>>();
            let pending =
                records.len() < owners || records.iter().any(|record| record["phase"] == "running");
            if !pending {
                break;
            }
            if Instant::now() >= deadline {
                cleanup_unproved = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        for path in self.launches.get_mut() {
            if let Ok(bytes) = std::fs::read(path)
                && let Ok(record) = serde_json::from_slice::<Value>(&bytes)
            {
                if record["phase"] != "exited" {
                    cleanup_unproved = true;
                }
                if cleanup_unproved || std::thread::panicking() {
                    eprintln!(
                        "host_exec fixture cleanup identity: {}",
                        String::from_utf8_lossy(&bytes)
                    );
                }
            }
        }
        if (cleanup_unproved || std::thread::panicking())
            && let Ok(log) = std::fs::read(self.path.join("host-exec.log"))
        {
            eprintln!(
                "host_exec fixture failure log: {}",
                String::from_utf8_lossy(&log)
            );
        }
        if !command_stopped {
            eprintln!(
                "host_exec fixture cleanup_unknown: upgrade-operation/agent cancellation or idle proof is missing; the service is not stopped"
            );
        }
        for child in self.children.get_mut() {
            if command_stopped && !matches!(child.try_wait(), Ok(Some(_))) {
                let killed = child.kill();
                let waited = child.wait();
                cleanup_unproved |= killed.is_err() || waited.is_err();
            }
        }
        assert!(
            !cleanup_unproved || std::thread::panicking(),
            "owned host_exec fixture cleanup is unproved"
        );
    }
}

#[test]
#[ignore = "manual process tier: just test-host-exec-processes"]
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
    let token = state.ensure("new-build", None).unwrap();
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
    state.ensure("new-build", None).unwrap();
    assert_eq!(state.health()["pid"], after["pid"]);
}

#[test]
#[ignore = "manual process tier: just test-host-exec-processes"]
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
    state.ensure("new-build", None).unwrap();
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
#[ignore = "manual process tier: just test-host-exec-processes"]
fn a_new_state_gets_a_token_and_a_service() {
    let state = State::new();
    std::fs::remove_file(state.path.join("host-exec-token")).unwrap();
    let path = &state.path;
    let token = state.ensure("fresh-build", None).unwrap();
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
    let health = service_call(path, &token, "health", None).unwrap();
    assert_eq!(health["buildId"], "fresh-build");
    let params = json!({"buildId": "fresh-build", "pid": health["pid"]});
    service_call(path, &token, "service.retire", Some(&params)).unwrap();
    state.wait_for_released_state();
}

/// The guest broker of the VM (`vm-guest/host_exec.py`) runs unchanged against
/// the service process (`vm-guest/host_exec_mac_contract.py`).
#[test]
#[ignore = "manual process tier: just test-host-exec-processes"]
fn the_guest_broker_runs_against_the_mac_service() {
    let script = repository().join("workspaces/runtime/apps/vm-guest/host_exec_mac_contract.py");
    let output = Command::new("python3")
        .arg("-B")
        .arg(&script)
        .arg("Contract.test_source_return_artifact_file_and_warm_path")
        .env("HOST_EXEC_SERVICE_BINARY", binary())
        .env("HOST_EXEC_SERVICE_READY_LINE", SERVICE_READY_LINE)
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
#[ignore = "manual process tier: just test-host-exec-processes"]
fn an_idle_service_without_the_browser_folder_is_replaced_and_its_commands_get_the_launcher() {
    let state = State::new();
    let old = state.start("old-build");
    let before = state.health();
    let browser_host = state.path.parent().unwrap().join("browser-host");
    std::fs::create_dir(&browser_host).unwrap();
    let launcher = browser_host.join("studio-browser");
    std::fs::write(&launcher, "#!/bin/sh\n").unwrap();
    state.ensure("new-build", Some(&browser_host)).unwrap();
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
