//! The backend side of the Mac service: keep one service of the current build
//! running for a VM state folder.
//!
//! A running service keeps its code after an app update. An idle service of
//! another build is replaced; a busy one stays until a later start, and its
//! running commands keep their slots. An older service is never killed: it is
//! asked to retire, and only an idle service accepts.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::protocol;
use super::{
    LEASE_FILE, LOG_FILE, PROTOCOL, SOCKET_FILE, START_LEASE_FILE, SUBCOMMAND, TOKEN_FILE,
};

/// Socket wait of one service request, in seconds.
const CALL_SECONDS: u64 = 2;
/// Largest answer of a lifecycle request, in bytes.
const CALL_RESPONSE_BYTES: usize = 8192;
/// Wait for a new service to answer `health` with the current build, in seconds.
const START_SECONDS: u64 = 10;
/// Wait for a retired service to release its lease, in seconds.
const STOP_SECONDS: u64 = 10;
/// Poll interval of the start and stop waits, in milliseconds.
const POLL_MS: u64 = 50;
/// Request ID prefix of a lifecycle request.
const CALL_ID_PREFIX: &str = "host-service:";
/// Random bytes of a new channel token (64 hexadecimal digits).
const TOKEN_BYTES: usize = 32;

/// A failed lifecycle request: the service error code and its message.
#[derive(Debug)]
pub struct ServiceError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "host_exec service: {}", self.message)
    }
}

fn unknown(message: impl Into<String>) -> ServiceError {
    ServiceError {
        code: "outcome_unknown".into(),
        message: message.into(),
    }
}

/// One request to the service of `state`, with its exact request identity.
pub fn service_call(
    state: &Path,
    token: &str,
    method: &str,
    params: Option<&Value>,
) -> Result<Value, ServiceError> {
    let identity = format!(
        "{CALL_ID_PREFIX}{}",
        protocol::key(&protocol::sorted_json(&json!([
            method,
            params.cloned().unwrap_or(Value::Null)
        ])))
    );
    let request = json!({"id": identity, "method": method, "params": params.cloned().unwrap_or_else(|| json!({})), "token": token});
    let io = |error: std::io::Error| unknown(error.to_string());
    let mut channel = UnixStream::connect(state.join(SOCKET_FILE)).map_err(io)?;
    channel
        .set_read_timeout(Some(Duration::from_secs(CALL_SECONDS)))
        .map_err(io)?;
    channel
        .set_write_timeout(Some(Duration::from_secs(CALL_SECONDS)))
        .map_err(io)?;
    let mut frame = request.to_string().into_bytes();
    frame.push(b'\n');
    channel.write_all(&frame).map_err(io)?;
    let mut line = Vec::new();
    BufReader::new(channel.take_read_limit())
        .read_until(b'\n', &mut line)
        .map_err(io)?;
    if line.is_empty() || line.len() > CALL_RESPONSE_BYTES {
        return Err(unknown("the response is missing or too large"));
    }
    let response: Value =
        serde_json::from_slice(&line).map_err(|error| unknown(error.to_string()))?;
    if response.get("id").and_then(Value::as_str) != Some(identity.as_str()) {
        return Err(unknown("the response identity differs"));
    }
    if let Some(error) = response.get("error") {
        return Err(ServiceError {
            code: error.get("code").map(value_text).unwrap_or_default(),
            message: error.get("message").map(value_text).unwrap_or_default(),
        });
    }
    response
        .get("result")
        .filter(|result| result.is_object())
        .cloned()
        .ok_or_else(|| unknown("the response has no result"))
}

fn value_text(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

trait ReadLimit {
    fn take_read_limit(self) -> std::io::Take<UnixStream>;
}

impl ReadLimit for UnixStream {
    fn take_read_limit(self) -> std::io::Take<UnixStream> {
        std::io::Read::take(self, CALL_RESPONSE_BYTES as u64 + 1)
    }
}

/// Whether no process holds the service lease of `state`.
fn owner_exited(state: &Path) -> std::io::Result<bool> {
    let lease = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(state.join(LEASE_FILE))?;
    Ok(rustix::fs::flock(&lease, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok())
}

#[allow(
    clippy::disallowed_methods,
    reason = "the service start runs on its own blocking thread"
)]
fn pause() {
    std::thread::sleep(Duration::from_millis(POLL_MS));
}

/// Ask an idle service of another build to retire and wait until its lease is
/// free. Returns why a service stays: it is busy, or it has no build identity.
pub(crate) fn stop_old_service(
    state: &Path,
    token: &str,
    health: &Value,
) -> Result<Option<String>, String> {
    let Some(build) = health
        .get("buildId")
        .and_then(Value::as_str)
        .filter(|build| !build.is_empty())
    else {
        // A service from before build identities has no retire request. It is never
        // stopped here: its process could hold a command that nobody can see.
        return Ok(Some("it has no build identity".into()));
    };
    let params =
        json!({"buildId": build, "pid": health.get("pid").cloned().unwrap_or(Value::Null)});
    match service_call(state, token, "service.retire", Some(&params)) {
        Ok(_) => {}
        Err(error) if error.code == "busy" => {
            return Ok(Some("it has a running command or request".into()));
        }
        Err(error) => return Err(error.to_string()),
    }
    let deadline = Instant::now() + Duration::from_secs(STOP_SECONDS);
    loop {
        if owner_exited(state).map_err(|error| error.to_string())? {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Err("The old host_exec service has no proven stop result".into());
        }
        pause();
    }
}

fn read_token(path: &Path) -> Result<String, String> {
    fs::read_to_string(path)
        .map_err(|error| format!("The host_exec channel token is unreadable: {error}"))
}

/// Make sure a healthy service of `build` serves the VM state folder `state`, and
/// return the channel token. `program` is the `studio-runtime-server` executable.
/// An error proves that no guest request was sent.
/// `browser_host` is the Studio server's host bridge folder for the commands' sign-in links.
pub fn ensure_service(
    state: &Path,
    program: &Path,
    build: &str,
    browser_host: Option<&Path>,
) -> Result<String, String> {
    let failed = |error: std::io::Error| format!("The host_exec service did not start: {error}");
    super::private_dirs(state).map_err(failed)?;
    let state = studio_runtime_store::path_identity::CanonicalPath::existing_directory(state)
        .map_err(failed)?
        .into_path_buf();
    let start_lease = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(state.join(START_LEASE_FILE))
        .map_err(failed)?;
    rustix::fs::flock(&start_lease, rustix::fs::FlockOperation::LockExclusive)
        .map_err(|error| failed(error.into()))?;
    let token_path = state.join(TOKEN_FILE);
    if !token_path.exists() {
        write_token(&token_path).map_err(failed)?;
    }
    let probe = |token: &str| {
        service_call(&state, token, "health", None)
            .ok()
            .filter(|health| health.get("protocol").and_then(Value::as_u64) == Some(PROTOCOL))
    };
    let token = read_token(&token_path)?;
    if let Some(current) = probe(&token) {
        if current.get("buildId").and_then(Value::as_str) == Some(build) {
            return Ok(token);
        }
        if let Some(kept) = stop_old_service(&state, &token, &current)? {
            eprintln!("host_exec: the service of an older build stays because {kept}");
            return Ok(token);
        }
    }
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state.join(LOG_FILE))
        .map_err(failed)?;
    let mut command = std::process::Command::new(program);
    command
        .arg(SUBCOMMAND)
        .arg(&state)
        .arg("--build")
        .arg(build);
    if let Some(folder) = browser_host {
        command.arg(super::BROWSER_HOST_ARGUMENT).arg(folder);
    }
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map_err(failed)?)
        .stderr(log)
        .spawn()
        .map_err(failed)?;
    let deadline = Instant::now() + Duration::from_secs(START_SECONDS);
    let result = loop {
        if child.try_wait().map_err(failed)?.is_some() {
            break Err(format!(
                "The host_exec service exited. Log: {}",
                state.join(LOG_FILE).display()
            ));
        }
        if probe(&token)
            .is_some_and(|health| health.get("buildId").and_then(Value::as_str) == Some(build))
        {
            break Ok(token.clone());
        }
        if Instant::now() >= deadline {
            break Err(format!(
                "The host_exec service has no proven start result. Inspect {}",
                state.join(LOG_FILE).display()
            ));
        }
        pause();
    };
    // The service leaves this process's session; a thread collects its exit.
    let reaped = std::thread::Builder::new()
        .name("studio-host-exec-reaper".into())
        .spawn(move || drop(child.wait()));
    if let Err(error) = reaped {
        eprintln!("host_exec: the service exit is not collected: {error}");
    }
    result
}

fn write_token(path: &PathBuf) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut random = [0_u8; TOKEN_BYTES];
    getrandom::fill(&mut random).map_err(|error| std::io::Error::other(error.to_string()))?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(protocol::hex(&random).as_bytes())?;
    file.sync_all()
}
