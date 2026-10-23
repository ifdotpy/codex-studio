//! Start a created Linux VM from the Rust backend (docs/linux-vm-workspaces.md,
//! "VM start").
//!
//! The native helper `studio-linux-vm serve <vm-state>` owns the VM. It runs in
//! its own session, so a backend restart keeps the VM. This module starts the
//! helper of a created VM, waits for a guest that answers with a synchronized
//! clock, and gives the guest the `host_exec` channel token. It never creates or
//! provisions a VM. An automatic start (backend startup, a VM chat) happens only
//! when the Mac restarted under a running VM, or to finish an automatic restart
//! that a backend stop interrupted; a VM stopped on purpose or in this Mac boot
//! stays stopped until a person starts it (`studio-runtime-server linux-vm start`).
//!
//! The watchdog checks the VM every [`WATCHDOG_INTERVAL_SECONDS`]. It restarts a
//! failed guest (a kernel panic behind a live helper) under the same restart
//! limits and `maintenance.lock`. After each start, the registered Finder views
//! of the layr projects mount again (`linux_vm_share`).

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::host_exec::HostExecRuntime;
use crate::linux_vm_guest::{GuestError, GuestRpc, GuestSpaceProbe};
use crate::linux_vm_share::ShareMountTool;

/// The subcommand of `studio-runtime-server` that starts the helper in a new session.
pub const HELPER_SUBCOMMAND: &str = "linux-vm-helper";
/// The VM files of the state folder. The native helper writes `console.log`,
/// `host.lock`, `control.sock` and `guest.sock`, and reads `share-bridge.json`.
const CONFIG_FILE: &str = "config.json";
const INTENT_FILE: &str = "intent.json";
const HEALTH_FILE: &str = "guest-health.json";
const RESTARTS_FILE: &str = "watchdog.json";
const SHARE_BRIDGE_FILE: &str = "share-bridge.json";
const CONSOLE_FILE: &str = "console.log";
const HELPER_LOG: &str = "helper.log";
const CLIENT_LOCK: &str = "client.lock";
const HOST_LOCK: &str = "host.lock";
const CONTROL_SOCKET: &str = "control.sock";
/// Held for a whole VM upgrade or automatic restart; neither starts while the
/// other runs. `workspaces/client/apps/desktop/desktop_vm.py` owns the name.
pub(crate) const MAINTENANCE_LOCK: &str = "maintenance.lock";

/// Wait for the client lock of create, start and stop, in seconds.
const CLIENT_LOCK_SECONDS: f64 = 15.0;
/// Wait for the control socket of a new helper, in seconds.
const HELPER_START_SECONDS: f64 = 15.0;
/// Wait of one control request and of one guest `health`, in seconds.
const REQUEST_SECONDS: f64 = 5.0;
/// Poll interval of the start, in milliseconds.
const POLL_MS: u64 = 250;
/// Longest wait for the guest clock, in seconds: direct Linux boot has no RTC, and TLS needs the time.
const CLOCK_SECONDS: f64 = 120.0;
/// A helper log above this size keeps only its last [`HELPER_LOG_KEEP`] bytes at a start.
const HELPER_LOG_LIMIT: u64 = 1024 * 1024;
const HELPER_LOG_KEEP: u64 = 512 * 1024;
/// Longest start of a person's command, in seconds.
pub const START_SECONDS: f64 = 3600.0;
/// Longest automatic start, in seconds.
const RESTART_SECONDS: f64 = 1800.0;
/// At most one automatic restart in this many seconds.
const MIN_RESTART_INTERVAL_SECONDS: f64 = 600.0;
/// At most this many automatic restarts in 24 hours.
const DAILY_RESTART_LIMIT: usize = 4;
/// An automatic restart that a backend stop interrupted ends this late, in seconds.
const RESTART_RESUME_SECONDS: f64 = 1800.0;
/// Restart records that `watchdog.json` keeps.
const RECORDS_KEPT: usize = 50;
const DAY_SECONDS: f64 = 86_400.0;
/// The reason of a start after a Mac restart.
const HOST_RESTART_REASON: &str = "The Mac restarted while the VM ran";
/// Interval of the watchdog checks, in seconds.
pub const WATCHDOG_INTERVAL_SECONDS: u64 = 30;
/// Wait for a failed VM to stop, in seconds: a dead kernel ignores the ACPI
/// stop, and the helper forces it after 20 seconds.
const STOP_SECONDS: f64 = 40.0;
/// Console copies of failed boots that the state folder keeps.
const CRASH_LOGS_KEPT: usize = 5;
/// Console bytes that a restart record keeps.
const CONSOLE_TAIL_BYTES: usize = 4096;
/// The name prefix of a console copy of a failed boot.
const CRASH_LOG_PREFIX: &str = "console-crash-";

/// Why a VM did not start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VmStartError {
    /// No start was attempted: the VM stays as it is.
    #[error("{0}")]
    Stays(String),
    /// A start was attempted and did not prove a running guest.
    #[error("{0}")]
    Failed(String),
    /// A stop on purpose came during an automatic restart.
    #[error("A person or a command stopped the VM during its automatic restart. It stays stopped.")]
    StoppedOnPurpose,
}

fn failed(message: impl Into<String>) -> VmStartError {
    VmStartError::Failed(message.into())
}

/// Starts `studio-linux-vm serve <state>` with its output in `helper.log`.
pub trait HelperLauncher: Send + Sync {
    fn launch(
        &self,
        helper: &Path,
        state: &Path,
        log: fs::File,
    ) -> io::Result<Box<dyn RunningHelper>>;
}

/// A launched helper process.
pub trait RunningHelper: Send {
    /// The exit of the helper process, when it ended.
    fn exit(&mut self) -> Option<String>;
    /// The helper keeps running after the start; collect its exit later.
    fn detach(self: Box<Self>);
}

/// Starts the helper through `studio-runtime-server linux-vm-helper`, which
/// leaves the backend's session and then runs the helper.
pub struct TrampolineLauncher {
    program: PathBuf,
}

impl TrampolineLauncher {
    pub fn new(program: PathBuf) -> Self {
        Self { program }
    }
}

struct ChildHelper(std::process::Child);

impl RunningHelper for ChildHelper {
    fn exit(&mut self) -> Option<String> {
        match self.0.try_wait() {
            Ok(Some(status)) => Some(status.to_string()),
            Ok(None) => None,
            Err(error) => Some(error.to_string()),
        }
    }

    fn detach(self: Box<Self>) {
        let mut child = self.0;
        let reaped = std::thread::Builder::new()
            .name("studio-linux-vm-reaper".into())
            .spawn(move || drop(child.wait()));
        if let Err(error) = reaped {
            eprintln!("linux-vm: the helper exit is not collected: {error}");
        }
    }
}

impl HelperLauncher for TrampolineLauncher {
    fn launch(
        &self,
        helper: &Path,
        state: &Path,
        log: fs::File,
    ) -> io::Result<Box<dyn RunningHelper>> {
        let child = std::process::Command::new(&self.program)
            .arg(HELPER_SUBCOMMAND)
            .arg(helper)
            .arg("serve")
            .arg(state)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?;
        Ok(Box::new(ChildHelper(child)))
    }
}

/// `studio-runtime-server linux-vm-helper <helper> serve <vm-state>`: leave the
/// backend's session, then run the helper in this process. `None` for other
/// command lines.
pub fn helper_main(args: impl IntoIterator<Item = std::ffi::OsString>) -> Option<i32> {
    let args: Vec<std::ffi::OsString> = args.into_iter().skip(1).collect();
    if args.first().and_then(|arg| arg.to_str()) != Some(HELPER_SUBCOMMAND) {
        return None;
    }
    let [_, helper, serve, state] = args.as_slice() else {
        eprintln!("usage: studio-runtime-server {HELPER_SUBCOMMAND} <helper> serve <vm-state-dir>");
        return Some(2);
    };
    if serve != "serve" {
        eprintln!("usage: studio-runtime-server {HELPER_SUBCOMMAND} <helper> serve <vm-state-dir>");
        return Some(2);
    }
    if let Err(error) = rustix::process::setsid() {
        eprintln!("linux-vm: setsid failed: {error}");
        return Some(1);
    }
    use std::os::unix::process::CommandExt;
    let error = std::process::Command::new(helper)
        .arg("serve")
        .arg(state)
        .exec();
    eprintln!("linux-vm: the helper did not start: {error}");
    Some(1)
}

/// The kind and line of the first kernel panic, oops or kernel BUG in the console
/// of the current boot (the helper clears `console.log` at each boot).
fn kernel_death(log: &str) -> Option<(&'static str, String)> {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(
            r"(?m)(?:^|\[\s*\d+\.\d+\]\s*)(Kernel panic - not syncing:|Internal error: Oops|Unable to handle kernel|BUG: |general protection fault)([^\r\n]*)",
        )
        .unwrap_or_else(|error| unreachable!("the kernel death pattern is valid: {error}"))
    });
    let found = pattern.captures(log)?;
    let printable = |text: &str| {
        text.chars()
            .filter(|c| (' '..='~').contains(c))
            .collect::<String>()
    };
    let kind = found.get(1).map_or("", |kind| kind.as_str());
    let detail = printable(found.get(2).map_or("", |detail| detail.as_str()));
    let detail = detail.trim();
    if kind.starts_with("Kernel panic") {
        let mut reason = "Kernel panic - not syncing".to_owned();
        if !detail.is_empty() {
            reason.push_str(": ");
            reason.extend(detail.chars().take(200));
        }
        return Some(("kernel_panic", reason));
    }
    Some((
        "kernel_error",
        printable(&format!("{kind}{detail}"))
            .trim()
            .chars()
            .take(220)
            .collect(),
    ))
}

/// The provisioning state of the current boot in the console: `ready`,
/// `running` or `failed` with its cause.
fn provision_from_log(log: &str) -> Map<String, Value> {
    let failed = log.rfind("STUDIO_PROVISION_ERROR:");
    let ready = log.rfind("STUDIO_PROVISION_READY");
    let last_stage = log.rfind("STUDIO_PROVISION_STAGE:");
    let mut result = Map::new();
    let after = |found: Option<usize>, other: Option<usize>| match (found, other) {
        (Some(found), Some(other)) => found > other,
        (Some(_), None) => true,
        (None, _) => false,
    };
    let ready_now = after(ready, failed.max(last_stage));
    result.insert(
        "state".into(),
        json!(if ready_now { "ready" } else { "running" }),
    );
    let timed_out = log.rfind("codex-studio-provision.service: start operation timed out");
    if after(timed_out, failed.max(ready)) {
        result.insert("state".into(), json!("failed"));
        result.insert("cause".into(), json!("deadline exceeded"));
    }
    if let Some(at) = failed.filter(|at| ready.is_none_or(|ready| *at > ready)) {
        let line = log[at..].lines().next().unwrap_or_default();
        let detail = line.split_once(':').map_or("", |(_, detail)| detail.trim());
        let cause: String = detail
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || " .,:_-".contains(*c))
            .take(120)
            .collect();
        result.insert("state".into(), json!("failed"));
        result.insert("cause".into(), json!(cause));
    }
    result
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

fn utc(at: f64) -> String {
    chrono::DateTime::from_timestamp(at as i64, 0)
        .map(|time| time.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

/// When this Mac booted (`kern.boottime`), or `None` when macOS does not say.
pub fn host_boot_time() -> Option<f64> {
    let outcome = crate::bounded_child::run(crate::bounded_child::Request {
        program: std::ffi::OsStr::new("/usr/sbin/sysctl"),
        git_directory: None,
        args: &["-n", "kern.boottime"],
        environment: &[],
        input: None,
        stdout_limit: 4096,
        stderr_limit: 4096,
        timeout: Duration::from_secs(5),
    })
    .ok()?;
    let text = String::from_utf8_lossy(&outcome.stdout.bytes);
    let seconds = text.split("sec = ").nth(1)?;
    let digits: String = seconds.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn write_json(path: &Path, value: &Value) -> io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(
            "{}.",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("vm")
        ))
        .tempfile_in(parent)?;
    temporary.write_all(value.to_string().as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn lock_file(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path)
}

#[allow(
    clippy::disallowed_methods,
    reason = "VM starts run on their own thread"
)]
fn pause(duration: Duration) {
    std::thread::sleep(duration);
}

fn remaining(deadline: Instant, maximum: f64) -> Result<Duration, VmStartError> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(failed("The Linux VM operation exceeded its timeout."));
    }
    Ok(left.min(Duration::from_secs_f64(maximum)))
}

/// Run blocking VM work on its own thread: a start can take half an hour, and it
/// must not hold a thread of the runtime's blocking pool.
async fn on_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, VmStartError> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("studio-linux-vm-start".into())
        .spawn(move || drop(sender.send(work())))
        .map_err(|error| VmStartError::Stays(format!("The VM start did not begin: {error}")))?;
    receiver
        .await
        .map_err(|_| failed("The VM start ended without a result."))
}

/// How the stop of a failed VM ended.
enum FailedStop {
    /// Another VM operation holds the client lock.
    Busy,
    /// The VM is no longer failed with the same helper.
    Recovered,
    Stopped,
    /// The stop has no proven result.
    Unproven(String),
}

/// One VM state folder: its helper, its control and guest sockets, and the
/// rules of an automatic start.
pub struct VmRuntime {
    state_dir: PathBuf,
    helper: PathBuf,
    launcher: Arc<dyn HelperLauncher>,
    control: Arc<dyn GuestRpc>,
    guest: Arc<dyn GuestRpc>,
    space: Arc<dyn GuestSpaceProbe>,
    boot_time: fn() -> Option<f64>,
    /// Virtualization runs Linux VMs on macOS with Apple silicon only.
    supported: bool,
    host_exec: OnceLock<Arc<HostExecRuntime>>,
    /// `~/Studio`, the parent of the Finder views; `None` without a home folder.
    studio_root: Option<PathBuf>,
    shares: Arc<dyn ShareMountTool>,
    /// One start at a time in this process; the client lock covers other processes.
    gate: tokio::sync::Mutex<()>,
}

/// The parts of a [`VmRuntime`].
pub struct VmRuntimeParts {
    pub state_dir: PathBuf,
    pub helper: PathBuf,
    pub launcher: Arc<dyn HelperLauncher>,
    pub control: Arc<dyn GuestRpc>,
    pub guest: Arc<dyn GuestRpc>,
    pub boot_time: fn() -> Option<f64>,
    pub supported: bool,
    /// `~/Studio`, the parent of the Finder views; `None` mounts no view.
    pub studio_root: Option<PathBuf>,
    pub shares: Arc<dyn ShareMountTool>,
}

impl VmRuntime {
    pub fn new(parts: VmRuntimeParts) -> Arc<Self> {
        Self::with_space(parts, Arc::new(crate::linux_vm_guest::HostGuestSpaceProbe))
    }

    pub(crate) fn with_space(parts: VmRuntimeParts, space: Arc<dyn GuestSpaceProbe>) -> Arc<Self> {
        Arc::new(Self {
            state_dir: parts.state_dir,
            helper: parts.helper,
            launcher: parts.launcher,
            control: parts.control,
            guest: parts.guest,
            space,
            boot_time: parts.boot_time,
            supported: parts.supported,
            host_exec: OnceLock::new(),
            studio_root: parts.studio_root,
            shares: parts.shares,
            gate: tokio::sync::Mutex::new(()),
        })
    }

    /// The production parts of the VM state folder `state_dir`.
    pub fn production(state_dir: PathBuf, helper: PathBuf, program: PathBuf) -> Arc<Self> {
        Self::new(VmRuntimeParts {
            control: Arc::new(crate::linux_vm_guest::UnixVmControlRpc::new(&state_dir)),
            guest: Arc::new(crate::linux_vm_guest::UnixGuestRpc::new(&state_dir)),
            launcher: Arc::new(TrampolineLauncher::new(program)),
            boot_time: host_boot_time,
            supported: cfg!(all(target_os = "macos", target_arch = "aarch64")),
            studio_root: std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join("Studio")),
            shares: Arc::new(crate::linux_vm_share::MacShareMountTool::default()),
            helper,
            state_dir,
        })
    }

    /// Give the guest the `host_exec` channel token after each start.
    pub fn attach_host_exec(&self, runtime: Arc<HostExecRuntime>) {
        if self.host_exec.set(runtime).is_err() {
            // The first attached runtime stays the owner of host commands.
        }
    }

    fn read_json(&self, name: &str) -> Map<String, Value> {
        fs::read_to_string(self.state_dir.join(name))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| match value {
                Value::Object(map) => Some(map),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// The last start or stop: `state`, `by`, `at`, and `restart` while an
    /// automatic restart is between its stop and its start.
    fn intent(&self) -> Map<String, Value> {
        self.read_json(INTENT_FILE)
    }

    fn write_intent(&self, state: &str, by: &str, restart: Option<&str>) -> io::Result<()> {
        let mut value = json!({"state": state, "by": by, "at": now()});
        if let Some(restart) = restart {
            value["restart"] = json!(restart);
        }
        write_json(&self.state_dir.join(INTENT_FILE), &value)
    }

    /// Hold `client.lock` like create, start and stop do; a busy lock waits up to `seconds`.
    fn client_lock(&self, seconds: f64) -> Result<fs::File, VmStartError> {
        let lease = lock_file(&self.state_dir.join(CLIENT_LOCK))
            .map_err(|error| failed(error.to_string()))?;
        let deadline = Instant::now() + Duration::from_secs_f64(seconds);
        loop {
            if rustix::fs::flock(&lease, rustix::fs::FlockOperation::NonBlockingLockExclusive)
                .is_ok()
            {
                return Ok(lease);
            }
            if Instant::now() >= deadline {
                return Err(VmStartError::Stays(
                    "Another VM operation exceeds the client lock timeout.".into(),
                ));
            }
            pause(Duration::from_millis(50));
        }
    }

    fn maintenance_lock(&self) -> Result<Option<fs::File>, VmStartError> {
        let lease = lock_file(&self.state_dir.join(MAINTENANCE_LOCK))
            .map_err(|error| failed(error.to_string()))?;
        Ok(
            rustix::fs::flock(&lease, rustix::fs::FlockOperation::NonBlockingLockExclusive)
                .is_ok()
                .then_some(lease),
        )
    }

    fn console(&self) -> Option<String> {
        fs::read(self.state_dir.join(CONSOLE_FILE))
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }

    fn provision(&self) -> Map<String, Value> {
        self.console()
            .map(|log| provision_from_log(&log))
            .unwrap_or_default()
    }

    /// The helper state: `stopped`, `running`, `failed` (a kernel death in the
    /// console of the current boot, or the helper's own failure), or another
    /// helper state. A stale socket is not proof that the helper can be replaced.
    pub fn status(&self) -> Result<Map<String, Value>, VmStartError> {
        let answer = self.control.call(
            "host.status",
            &json!({}),
            None,
            Duration::from_secs_f64(REQUEST_SECONDS),
        );
        let mut result = match answer {
            Ok(Value::Object(result)) => result,
            Ok(_) => return Err(failed("The VM helper returned an invalid status.")),
            Err(_) => {
                let host_lock = self.state_dir.join(HOST_LOCK);
                if self.state_dir.join(CONTROL_SOCKET).exists() || host_lock.exists() {
                    let lease = lock_file(&host_lock).map_err(|error| failed(error.to_string()))?;
                    if rustix::fs::flock(
                        &lease,
                        rustix::fs::FlockOperation::NonBlockingLockExclusive,
                    )
                    .is_err()
                    {
                        return Err(VmStartError::Stays(
                            "The VM helper owns its lease but does not respond.".into(),
                        ));
                    }
                }
                let mut stopped = Map::new();
                stopped.insert("state".into(), json!("stopped"));
                stopped
            }
        };
        if result.get("state").and_then(Value::as_str) == Some("running")
            && let Some((kind, reason)) = self.console().as_deref().and_then(kernel_death)
        {
            // The helper and its Virtualization process stay alive after a guest panic.
            result.insert("state".into(), json!("failed"));
            result.insert("helperState".into(), json!("running"));
            result.insert("error".into(), json!(reason));
            result.insert(
                "guestFailure".into(),
                json!({"kind": kind, "reason": reason}),
            );
        }
        Ok(result)
    }

    fn state_of(status: &Map<String, Value>) -> &str {
        status
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("stopped")
    }

    fn health(&self, wait: Duration) -> Result<Value, GuestError> {
        self.guest.call("health", &json!({}), None, wait)
    }

    /// Whether the guest service answers at all: an unknown method gets an error
    /// answer from its event loop.
    fn answers(&self) -> bool {
        matches!(
            self.guest
                .call("ping", &json!({}), None, Duration::from_secs(15)),
            Ok(_) | Err(GuestError::Rejected { .. })
        )
    }

    /// The Mac loopback port of the helper's share bridge, chosen once and kept,
    /// so a Finder mount survives VM restarts.
    fn share_bridge_port(&self) -> io::Result<u16> {
        let path = self.state_dir.join(SHARE_BRIDGE_FILE);
        if let Some(port) = self
            .read_json(SHARE_BRIDGE_FILE)
            .get("port")
            .and_then(Value::as_u64)
            .filter(|port| (1024..=65535).contains(port))
        {
            return Ok(port as u16);
        }
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))?
            .local_addr()?
            .port();
        write_json(&path, &json!({"port": port}))?;
        Ok(port)
    }

    fn trim_helper_log(&self) -> io::Result<()> {
        let path = self.state_dir.join(HELPER_LOG);
        let Ok(meta) = fs::metadata(&path) else {
            return Ok(());
        };
        if meta.len() <= HELPER_LOG_LIMIT {
            return Ok(());
        }
        let mut file = fs::File::open(&path)?;
        file.seek(SeekFrom::End(-(HELPER_LOG_KEEP as i64)))?;
        let mut tail = Vec::new();
        file.read_to_end(&mut tail)?;
        fs::write(&path, tail)
    }

    /// Save the first guest answer of this boot (one helper PID) in `guest-health.json`.
    fn record_health(&self, pid: Option<&Value>) {
        let Some(pid) = pid.filter(|pid| pid.is_i64() || pid.is_u64()) else {
            return;
        };
        let mut saved = self.read_json(HEALTH_FILE);
        if saved.get("helperPid") != Some(pid) {
            saved = Map::new();
            saved.insert("helperPid".into(), pid.clone());
        }
        let at = now();
        for (field, value) in [
            ("healthyAt", json!(at)),
            ("failingSince", Value::Null),
            ("lastFailureAt", Value::Null),
            ("failures", json!(0)),
            ("error", Value::Null),
            ("lastProbeAt", json!(at)),
        ] {
            saved.insert(field.into(), value);
        }
        if let Err(error) = write_json(&self.state_dir.join(HEALTH_FILE), &Value::Object(saved)) {
            eprintln!("linux-vm: the guest health record is not saved: {error}");
        }
    }

    /// Wait until the guest clock is synchronized (`timedatectl`).
    fn wait_clock(&self, deadline: Instant) -> Result<(), VmStartError> {
        let seconds = remaining(deadline, CLOCK_SECONDS)?.as_secs().max(1);
        let script = "until [ \"$(timedatectl show --property=NTPSynchronized --value)\" = yes ]; do sleep 1; done";
        let params = json!({
            "argv": ["timeout", "--kill-after=5", seconds.to_string(), "bash", "-c", script],
            "cwd": "/home/studio", "timeoutSeconds": seconds + 5,
        });
        let wait = remaining(deadline, (seconds + 10) as f64)?;
        let result = self
            .guest
            .call("exec", &params, None, wait)
            .map_err(|error| failed(format!("The Linux VM clock check failed: {error}")))?;
        if result.get("exitCode").and_then(Value::as_i64) != Some(0) {
            return Err(failed(
                "The Linux VM clock did not synchronize within its timeout.",
            ));
        }
        Ok(())
    }

    fn provision_error(info: &Map<String, Value>, timed_out: bool) -> VmStartError {
        let cause = info
            .get("cause")
            .and_then(Value::as_str)
            .unwrap_or("no reason");
        if timed_out {
            return failed(format!(
                "The Linux VM did not answer before its start timeout (provisioning {}).",
                info.get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            ));
        }
        failed(format!(
            "The Linux VM provisioning failed: {cause}. Create the VM again."
        ))
    }

    /// Start a created VM and wait for a guest that answers (Python
    /// `ensure_running` without create). `restart` names the automatic restart
    /// that this start belongs to: a stop or a start since then cancels it.
    fn start_blocking(
        &self,
        timeout: f64,
        by: &str,
        restart: Option<&str>,
    ) -> Result<Map<String, Value>, VmStartError> {
        if !self.supported {
            return Err(VmStartError::Stays(
                "Linux VM workspaces require macOS on Apple silicon.".into(),
            ));
        }
        let deadline = Instant::now() + Duration::from_secs_f64(timeout);
        if !self.state_dir.join(CONFIG_FILE).exists() {
            return Err(VmStartError::Stays(
                "The Linux VM is not created. Create it with a Studio build that can provision a VM.".into(),
            ));
        }
        let mut launched: Option<Box<dyn RunningHelper>> = None;
        let mut healthy_previous = false;
        {
            let _lease = self.client_lock(CLIENT_LOCK_SECONDS)?;
            let state = self.status()?;
            if self.provision().get("state").and_then(Value::as_str) == Some("failed") {
                healthy_previous = Self::state_of(&state) == "running"
                    && self.health(remaining(deadline, REQUEST_SECONDS)?).is_ok();
                if !healthy_previous {
                    // A new provisioning belongs to VM creation, which this backend does not own.
                    return Err(Self::provision_error(&self.provision(), false));
                }
            }
            if Self::state_of(&state) == "stopped" {
                if let Some(restart) = restart
                    && self.intent().get("restart").and_then(Value::as_str) != Some(restart)
                {
                    return Err(VmStartError::StoppedOnPurpose);
                }
                crate::linux_vm_guest::require_agent_free_space(
                    &self.state_dir,
                    self.space.as_ref(),
                )
                .map_err(|error| VmStartError::Stays(error.to_string()))?;
                if !is_executable(&self.helper) {
                    return Err(VmStartError::Stays(format!(
                        "The signed Linux VM helper is unavailable: {}",
                        self.helper.display()
                    )));
                }
                self.write_intent("running", by, restart)
                    .map_err(|error| failed(error.to_string()))?;
                // A new helper can get the PID of an old one: its probe record starts empty.
                match fs::remove_file(self.state_dir.join(HEALTH_FILE)) {
                    Err(error) if error.kind() != io::ErrorKind::NotFound => {
                        return Err(failed(error.to_string()));
                    }
                    _ => {}
                }
                self.trim_helper_log()
                    .map_err(|error| failed(error.to_string()))?;
                self.share_bridge_port()
                    .map_err(|error| failed(error.to_string()))?;
                let log = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.state_dir.join(HELPER_LOG))
                    .map_err(|error| failed(error.to_string()))?;
                let mut helper = self
                    .launcher
                    .launch(&self.helper, &self.state_dir, log)
                    .map_err(|error| {
                        failed(format!("The Linux VM helper did not start: {error}"))
                    })?;
                let start_deadline = Instant::now() + Duration::from_secs_f64(HELPER_START_SECONDS);
                loop {
                    if let Some(exit) = helper.exit() {
                        return Err(failed(format!(
                            "The Linux VM helper exited ({exit}). Log: {}",
                            self.state_dir.join(HELPER_LOG).display()
                        )));
                    }
                    if self
                        .control
                        .call("host.status", &json!({}), None, Duration::from_secs(1))
                        .is_ok()
                    {
                        break;
                    }
                    if Instant::now() >= start_deadline.min(deadline) {
                        helper.detach();
                        return Err(failed(
                            "The Linux VM helper did not publish its control socket within 15 seconds.",
                        ));
                    }
                    pause(Duration::from_millis(50));
                }
                launched = Some(helper);
            }
        }
        let outcome = self.wait_healthy(deadline, launched.as_mut(), healthy_previous);
        if let Some(helper) = launched {
            helper.detach();
        }
        outcome
    }

    fn wait_healthy(
        &self,
        deadline: Instant,
        mut launched: Option<&mut Box<dyn RunningHelper>>,
        healthy_previous: bool,
    ) -> Result<Map<String, Value>, VmStartError> {
        while Instant::now() < deadline {
            if let Some(exit) = launched.as_mut().and_then(|helper| helper.exit()) {
                return Err(failed(format!(
                    "The Linux VM helper exited ({exit}). Log: {}",
                    self.state_dir.join(HELPER_LOG).display()
                )));
            }
            let mut state = self.status()?;
            match Self::state_of(&state) {
                "failed" => {
                    let reason = state
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("no reason");
                    return Err(failed(format!("The Linux VM failed: {reason}")));
                }
                "running" | "unresponsive" => {
                    let wait = remaining(deadline, REQUEST_SECONDS)?;
                    if let Ok(health) = self.health(wait) {
                        if !healthy_previous
                            && self.provision().get("state").and_then(Value::as_str)
                                != Some("ready")
                        {
                            pause(Duration::from_millis(POLL_MS));
                            continue;
                        }
                        self.wait_clock(deadline)?;
                        // The guest answered in this boot: from now on, no answer is a failure.
                        self.record_health(state.get("pid"));
                        state.insert("health".into(), health);
                        return Ok(state);
                    }
                }
                _ => {}
            }
            let info = self.provision();
            if info.get("state").and_then(Value::as_str) == Some("failed") {
                return Err(Self::provision_error(&info, false));
            }
            pause(Duration::from_millis(POLL_MS));
        }
        Err(Self::provision_error(&self.provision(), true))
    }

    /// After a start: the Mac host_exec service and the guest token, then the
    /// Finder views. A failure leaves the VM running; each host_exec call
    /// configures the guest again, and a failed view keeps its reason.
    async fn configure_after_start(self: &Arc<Self>) {
        if let Some(host_exec) = self.host_exec.get()
            && let Err(error) = host_exec.configure_guest().await
        {
            eprintln!("linux-vm: the host_exec channel is not configured yet: {error}");
        }
        let runtime = Arc::clone(self);
        if let Err(error) = on_thread(move || runtime.remount_shares()).await {
            eprintln!("linux-vm: the project shares did not mount again: {error}");
        }
    }

    /// Mount the registered Finder views of the layr projects again
    /// (Python `remount_registered` at the end of `ensure_running`).
    fn remount_shares(&self) {
        let Some(studio_root) = &self.studio_root else {
            return;
        };
        let status = self.status().ok();
        let parts = crate::linux_vm_share::Remount {
            state_dir: &self.state_dir,
            studio_root,
            guest: self.guest.as_ref(),
            bridge: status.as_ref().and_then(|status| status.get("share")),
            tool: self.shares.as_ref(),
        };
        if let Err(error) = crate::linux_vm_share::remount_registered(&parts) {
            eprintln!("linux-vm: the project shares did not mount again: {error}");
        }
    }

    /// A person's start of a created VM (`studio-runtime-server linux-vm start`).
    pub async fn start(self: &Arc<Self>) -> Result<Map<String, Value>, VmStartError> {
        let _gate = self.gate.lock().await;
        let runtime = Arc::clone(self);
        let status =
            on_thread(move || runtime.start_blocking(START_SECONDS, "start", None)).await??;
        self.configure_after_start().await;
        Ok(status)
    }

    fn restarts(&self) -> Map<String, Value> {
        let mut saved = self.read_json(RESTARTS_FILE);
        let rows: Vec<Value> = saved
            .get("restarts")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter(|row| row.get("at").and_then(Value::as_f64).is_some())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        saved.insert("restarts".into(), Value::Array(rows));
        saved
    }

    fn save_restarts(&self, mut saved: Map<String, Value>) {
        if let Some(Value::Array(rows)) = saved.get_mut("restarts") {
            let extra = rows.len().saturating_sub(RECORDS_KEPT);
            rows.drain(..extra);
        }
        if let Err(error) = write_json(&self.state_dir.join(RESTARTS_FILE), &Value::Object(saved)) {
            eprintln!("linux-vm: the restart record is not saved: {error}");
        }
    }

    fn update_record(&self, record: &Map<String, Value>) {
        let mut saved = self.restarts();
        if let Some(Value::Array(rows)) = saved.get_mut("restarts") {
            match rows
                .iter_mut()
                .find(|row| row.get("id") == record.get("id"))
            {
                Some(row) => *row = Value::Object(record.clone()),
                None => rows.push(Value::Object(record.clone())),
            }
        }
        self.save_restarts(saved);
    }

    fn hold(&self, held: Option<Value>) {
        let mut saved = self.restarts();
        match held {
            Some(held) => {
                saved.insert("held".into(), held);
            }
            None if saved.contains_key("held") => {
                saved.remove("held");
            }
            None => return,
        }
        self.save_restarts(saved);
    }

    /// When the limits permit the next automatic restart, and the limit that holds it.
    fn next_allowed(restarts: &[Value], at: f64) -> Option<(f64, String)> {
        let mut times: Vec<f64> = restarts
            .iter()
            .filter_map(|row| row.get("at").and_then(Value::as_f64))
            .collect();
        times.sort_by(f64::total_cmp);
        if let Some(last) = times.last()
            && at - last < MIN_RESTART_INTERVAL_SECONDS
        {
            return Some((
                last + MIN_RESTART_INTERVAL_SECONDS,
                format!(
                    "one automatic restart in {} minutes",
                    MIN_RESTART_INTERVAL_SECONDS as u64 / 60
                ),
            ));
        }
        let day: Vec<f64> = times
            .into_iter()
            .filter(|time| at - time < DAY_SECONDS)
            .collect();
        if day.len() >= DAILY_RESTART_LIMIT {
            return Some((
                day[day.len() - DAILY_RESTART_LIMIT] + DAY_SECONDS,
                format!("{DAILY_RESTART_LIMIT} automatic restarts in 24 hours"),
            ));
        }
        None
    }

    fn finish_intent(&self, restart: &str) {
        let Ok(_lease) = self.client_lock(CLIENT_LOCK_SECONDS) else {
            // A held client lock: the next check clears it, as the record is final.
            return;
        };
        if self.intent().get("restart").and_then(Value::as_str) == Some(restart)
            && let Err(error) = self.write_intent("running", "watchdog", None)
        {
            eprintln!("linux-vm: the start intent is not saved: {error}");
        }
    }

    fn automatic_start(&self, mut record: Map<String, Value>) -> String {
        let id = record
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let outcome = match self.start_blocking(RESTART_SECONDS, "watchdog", Some(&id)) {
            Ok(_) => {
                record.insert("stage".into(), json!("done"));
                "restarted"
            }
            Err(error) => {
                // A stop on purpose before the launch, or one that ended the new helper.
                let cancelled = error == VmStartError::StoppedOnPurpose
                    || self.intent().get("state").and_then(Value::as_str) == Some("stopped");
                let running = !cancelled
                    && self
                        .status()
                        .is_ok_and(|status| Self::state_of(&status) == "running")
                    && self.answers();
                let outcome = if cancelled {
                    "cancelled"
                } else if running {
                    "restarted"
                } else {
                    "failed"
                };
                record.insert(
                    "stage".into(),
                    json!(if running { "done" } else { outcome }),
                );
                record.insert(
                    "error".into(),
                    json!(error.to_string().chars().take(500).collect::<String>()),
                );
                outcome
            }
        };
        record.insert("outcome".into(), json!(outcome));
        record.insert("finishedAt".into(), json!(now()));
        if outcome != "cancelled" {
            self.finish_intent(&id);
        }
        if outcome == "restarted" {
            self.hold(None);
        }
        self.update_record(&record);
        outcome.to_owned()
    }

    /// The automatic start rule: finish an automatic restart that a backend
    /// stop interrupted, or start a VM whose last intent is a start from before
    /// the current Mac boot. Any other stopped VM stays stopped.
    fn resume_blocking(&self) -> Result<String, VmStartError> {
        if !self.state_dir.join(CONFIG_FILE).exists() {
            return Ok("absent".into());
        }
        let status = self.status()?;
        if Self::state_of(&status) != "stopped" {
            return Ok(Self::state_of(&status).to_owned());
        }
        let intent = self.intent();
        if let Some(restart) = intent.get("restart").and_then(Value::as_str) {
            let Some(_maintenance) = self.maintenance_lock()? else {
                return Ok("busy".into());
            };
            let saved = self.restarts();
            let record = saved
                .get("restarts")
                .and_then(Value::as_array)
                .and_then(|rows| {
                    rows.iter()
                        .find(|row| row.get("id").and_then(Value::as_str) == Some(restart))
                })
                .and_then(Value::as_object)
                .cloned();
            let Some(mut record) = record.filter(|record| {
                matches!(
                    record.get("stage").and_then(Value::as_str),
                    Some("stopping" | "starting")
                )
            }) else {
                self.finish_intent(restart);
                return Ok("stopped".into());
            };
            if now() - record.get("at").and_then(Value::as_f64).unwrap_or(0.0)
                > RESTART_RESUME_SECONDS
            {
                record.insert("stage".into(), json!("failed"));
                record.insert("outcome".into(), json!("abandoned"));
                record.insert("finishedAt".into(), json!(now()));
                record.insert(
                    "error".into(),
                    json!("The restart did not finish. The VM stays stopped."),
                );
                self.finish_intent(restart);
                self.update_record(&record);
                return Ok("abandoned".into());
            }
            record.insert("stage".into(), json!("starting"));
            self.update_record(&record);
            return Ok(self.automatic_start(record));
        }
        let boot = (self.boot_time)();
        let before_boot = match (boot, intent.get("at").and_then(Value::as_f64)) {
            (Some(boot), Some(at)) => at < boot,
            _ => false,
        };
        if intent.get("state").and_then(Value::as_str) != Some("running") || !before_boot {
            return Ok("stopped".into());
        }
        let Some(_maintenance) = self.maintenance_lock()? else {
            return Ok("busy".into());
        };
        let at = now();
        let restarts = self.restarts();
        let rows = restarts
            .get("restarts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if let Some((until, rule)) = Self::next_allowed(&rows, at) {
            self.hold(Some(json!({"reason": HOST_RESTART_REASON, "rule": rule, "until": until, "untilTime": utc(until)})));
            return Ok("limited".into());
        }
        if self.intent() != intent || Self::state_of(&self.status()?) != "stopped" {
            return Ok("busy".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let mut record = Map::new();
        record.insert("id".into(), json!(id));
        record.insert("at".into(), json!(at));
        record.insert("time".into(), json!(utc(at)));
        record.insert("kind".into(), json!("host_restart"));
        record.insert("reason".into(), json!(HOST_RESTART_REASON));
        record.insert("stage".into(), json!("starting"));
        self.update_record(&record);
        self.write_intent("running", "watchdog", Some(&id))
            .map_err(|error| failed(error.to_string()))?;
        Ok(self.automatic_start(record))
    }

    /// The automatic start rule, at backend startup and for a VM chat. Returns
    /// what it did: absent, a VM state, stopped, busy, limited, restarted,
    /// failed, cancelled or abandoned.
    pub async fn resume(self: &Arc<Self>) -> Result<String, VmStartError> {
        let _gate = self.gate.lock().await;
        let runtime = Arc::clone(self);
        let outcome = on_thread(move || runtime.resume_blocking()).await??;
        if outcome == "restarted" {
            self.configure_after_start().await;
        }
        Ok(outcome)
    }

    /// Before a VM chat uses the guest: a running VM is ready; a stopped VM
    /// starts only by the automatic rule.
    pub async fn ensure_for_chat(self: &Arc<Self>) -> Result<(), VmStartError> {
        let runtime = Arc::clone(self);
        let status = on_thread(move || runtime.status()).await??;
        match Self::state_of(&status) {
            "running" | "unresponsive" | "starting" => return Ok(()),
            "stopped" => {}
            state => {
                let reason = status.get("error").and_then(Value::as_str).unwrap_or(state);
                return Err(VmStartError::Stays(format!(
                    "The Linux VM is {state}: {reason}"
                )));
            }
        }
        match self.resume().await?.as_str() {
            "restarted" | "running" => Ok(()),
            "absent" => Err(VmStartError::Stays("The Linux VM is not created.".into())),
            "failed" => Err(failed("The Linux VM did not start. See its helper log and the Linux VM settings.")),
            "busy" => Err(VmStartError::Stays(
                "A VM upgrade or start holds the Linux VM. Send the message again in a moment.".into(),
            )),
            "limited" => Err(VmStartError::Stays(
                "The Linux VM waits for its automatic restart limit. Start it with `studio-runtime-server linux-vm start`, then send the message again.".into(),
            )),
            _ => Err(VmStartError::Stays(
                "The Linux VM is stopped. Start it with `studio-runtime-server linux-vm start`, then send the message again."
                    .into(),
            )),
        }
    }

    /// Copy the console of the failed boot before the next boot clears it.
    /// Returns the name of the copy and the tail of the console.
    fn keep_console(&self, at: f64) -> (Option<String>, String) {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let Ok(data) = fs::read(self.state_dir.join(CONSOLE_FILE)) else {
            return (None, String::new());
        };
        let tail = String::from_utf8_lossy(&data[data.len().saturating_sub(CONSOLE_TAIL_BYTES)..])
            .into_owned();
        let stamp = chrono::DateTime::from_timestamp(at as i64, 0)
            .map(|time| time.format("%Y%m%dT%H%M%SZ").to_string())
            .unwrap_or_default();
        let name = format!("{CRASH_LOG_PREFIX}{stamp}.log");
        let saved = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(self.state_dir.join(&name))
            .and_then(|mut file| {
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
                file.write_all(&data)
            });
        if let Err(error) = saved {
            eprintln!("linux-vm: the console of the failed boot is not kept: {error}");
            return (None, tail);
        }
        let mut copies: Vec<PathBuf> = fs::read_dir(&self.state_dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| {
                                name.starts_with(CRASH_LOG_PREFIX) && name.ends_with(".log")
                            })
                    })
                    .collect()
            })
            .unwrap_or_default();
        copies.sort();
        for old in &copies[..copies.len().saturating_sub(CRASH_LOGS_KEPT)] {
            drop(fs::remove_file(old));
        }
        (Some(name), tail)
    }

    /// The restart record `id` of `watchdog.json`.
    fn record(&self, id: &str) -> Option<Map<String, Value>> {
        self.restarts()
            .get("restarts")
            .and_then(Value::as_array)
            .and_then(|rows| {
                rows.iter()
                    .find(|row| row.get("id").and_then(Value::as_str) == Some(id))
            })
            .and_then(Value::as_object)
            .cloned()
    }

    /// Close an automatic restart whose start a backend stop interrupted
    /// after the launch: the guest answers, so the VM must not start again
    /// after a later stop.
    fn settle(&self) {
        let Some(restart) = self
            .intent()
            .get("restart")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return;
        };
        if let Some(mut record) = self.record(&restart).filter(|record| {
            matches!(
                record.get("stage").and_then(Value::as_str),
                Some("stopping" | "starting")
            )
        }) {
            record.insert("stage".into(), json!("done"));
            record.insert("outcome".into(), json!("restarted"));
            record.insert("finishedAt".into(), json!(now()));
            self.update_record(&record);
        }
        self.finish_intent(&restart);
    }

    /// Stop a failed VM for an automatic restart. Before the stop, the console
    /// copy, the record and the restart intent are saved: a backend that stops
    /// during the restart leaves a restart that the next backend can finish.
    fn stop_failed(
        &self,
        pid: Option<&Value>,
        record: &mut Map<String, Value>,
    ) -> Result<FailedStop, VmStartError> {
        {
            // Create, start and stop hold this lock; the next check tries again.
            let Ok(_lease) = self.client_lock(0.0) else {
                return Ok(FailedStop::Busy);
            };
            let state = self.status()?;
            if Self::state_of(&state) != "failed" || state.get("pid") != pid {
                return Ok(FailedStop::Recovered);
            }
            let at = record.get("at").and_then(Value::as_f64).unwrap_or_else(now);
            let (log, tail) = self.keep_console(at);
            record.insert("consoleLog".into(), json!(log));
            record.insert("consoleTail".into(), json!(tail));
            self.update_record(record);
            let id = record
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            self.write_intent("running", "watchdog", Some(&id))
                .map_err(|error| failed(error.to_string()))?;
            if let Err(error) = self.control.call(
                "host.stop",
                &json!({}),
                None,
                Duration::from_secs_f64(REQUEST_SECONDS),
            ) {
                // The intent stays: if the helper exits later, a check starts the VM.
                return Ok(FailedStop::Unproven(format!(
                    "The VM helper did not take the stop: {error}"
                )));
            }
        }
        // Wait without the lock: a stop on purpose in this time still runs.
        let deadline = Instant::now() + Duration::from_secs_f64(STOP_SECONDS);
        loop {
            if self
                .status()
                .is_ok_and(|status| Self::state_of(&status) == "stopped")
            {
                return Ok(FailedStop::Stopped);
            }
            if Instant::now() >= deadline {
                return Ok(FailedStop::Unproven(
                    "The Linux VM did not stop within its timeout.".into(),
                ));
            }
            pause(Duration::from_millis(100));
        }
    }

    /// Restart a failed VM through the normal start, within the restart limits.
    fn restart_failed(&self, status: &Map<String, Value>) -> Result<String, VmStartError> {
        let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_owned);
        let (kind, reason) = match status.get("guestFailure") {
            Some(failure) => (
                text(failure.get("kind")).unwrap_or_else(|| "kernel_error".into()),
                text(failure.get("reason")).unwrap_or_else(|| "no reason".into()),
            ),
            None => (
                "helper_failed".into(),
                format!(
                    "The VM helper failed: {}",
                    text(status.get("error"))
                        .filter(|error| !error.is_empty())
                        .unwrap_or_else(|| "no reason".into())
                ),
            ),
        };
        let Some(_maintenance) = self.maintenance_lock()? else {
            return Ok("busy".into());
        };
        let at = now();
        let rows = self
            .restarts()
            .get("restarts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if let Some((until, rule)) = Self::next_allowed(&rows, at) {
            self.hold(Some(
                json!({"reason": reason, "rule": rule, "until": until, "untilTime": utc(until)}),
            ));
            return Ok("limited".into());
        }
        let pid = status.get("pid").cloned();
        let mut record = Map::new();
        record.insert("id".into(), json!(uuid::Uuid::new_v4().to_string()));
        record.insert("at".into(), json!(at));
        record.insert("time".into(), json!(utc(at)));
        record.insert("kind".into(), json!(kind));
        record.insert("reason".into(), json!(reason));
        record.insert("helperPid".into(), pid.clone().unwrap_or(Value::Null));
        record.insert("stage".into(), json!("stopping"));
        match self.stop_failed(pid.as_ref(), &mut record)? {
            FailedStop::Busy => return Ok("busy".into()),
            FailedStop::Recovered => return Ok("recovered".into()),
            FailedStop::Unproven(error) => {
                record.insert(
                    "error".into(),
                    json!(error.chars().take(500).collect::<String>()),
                );
                self.update_record(&record);
                return Ok("stop_failed".into());
            }
            FailedStop::Stopped => {}
        }
        record.insert("stage".into(), json!("starting"));
        record.insert("stoppedAt".into(), json!(now()));
        self.update_record(&record);
        Ok(self.automatic_start(record))
    }

    fn check_blocking(&self) -> Result<String, VmStartError> {
        if !self.state_dir.join(CONFIG_FILE).exists() {
            return Ok("absent".into());
        }
        let Ok(status) = self.status() else {
            return Ok("unknown".into());
        };
        match Self::state_of(&status) {
            "stopped" => self.resume_blocking(),
            "failed" => self.restart_failed(&status),
            "running" => {
                self.hold(None);
                if self.intent().contains_key("restart") && self.answers() {
                    self.settle();
                }
                Ok("running".into())
            }
            state => Ok(state.to_owned()),
        }
    }

    /// One watchdog check (Python `Watchdog.check`): restart a failed guest
    /// within the limits, finish an interrupted restart, or start a VM that the
    /// Mac restart stopped. No answer alone never restarts the VM: a busy
    /// guest can refuse connections. Returns what it did: absent, unknown, a
    /// VM state, busy, limited, recovered, restarted, failed, cancelled,
    /// stop_failed or abandoned.
    pub async fn check(self: &Arc<Self>) -> Result<String, VmStartError> {
        let _gate = self.gate.lock().await;
        let runtime = Arc::clone(self);
        let outcome = on_thread(move || runtime.check_blocking()).await??;
        if outcome == "restarted" {
            self.configure_after_start().await;
        }
        Ok(outcome)
    }
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests;
