//! Bounded JSON-RPC client for the Linux VM guest socket.
//!
//! A request ID identifies one exact guest operation. If a response is lost after the
//! request is written, `GuestError::uncertain` is true and the caller must not resend it
//! under a different ID.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use thiserror::Error;

/// Maximum size of one newline-delimited guest protocol frame, in bytes.
pub const GUEST_FRAME_LIMIT_BYTES: usize = 2 * 1024 * 1024;
/// Maximum duration for an ordinary guest RPC, in seconds.
pub const GUEST_RPC_TIMEOUT_SECONDS: u64 = 3_600;
/// Maximum duration for a guest process execution RPC, in seconds.
pub const GUEST_EXEC_TIMEOUT_SECONDS: u64 = 9_000;
/// Maximum time spent establishing a guest socket connection, in seconds.
pub const GUEST_CONNECT_TIMEOUT_SECONDS: u64 = 15;
const BUFFER_READ_BYTES: usize = 65_536;
/// Maximum guest operation identity length, shared with the Python guest client.
pub const GUEST_REQUEST_ID_MAX_CHARS: usize = 128;
const DEFAULT_AGENT_MIN_FREE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const AGENT_MIN_FREE_BYTES_ENV: &str = "CODEX_WORKSPACE_AGENT_MIN_FREE_BYTES";

/// Host-disk query used before guest writes that can consume substantial VM disk space.
pub trait GuestSpaceProbe: Send + Sync {
    fn free_bytes(&self, path: &Path) -> std::io::Result<u64>;
}

pub(crate) struct HostGuestSpaceProbe;

impl GuestSpaceProbe for HostGuestSpaceProbe {
    fn free_bytes(&self, path: &Path) -> std::io::Result<u64> {
        let stats = nix::sys::statvfs::statvfs(path).map_err(std::io::Error::other)?;
        Ok((stats.blocks_free() as u64).saturating_mul(stats.fragment_size() as u64))
    }
}

/// Error from connecting to the guest or exchanging one RPC.
#[derive(Debug, Error)]
pub enum GuestError {
    #[error("invalid VM guest request")]
    InvalidRequest,
    #[error("could not create VM guest request identity")]
    Identity,
    #[error("{message}")]
    SpacePreflight { message: String },
    #[error("VM guest transport failed: {message}")]
    Transport { message: String, uncertain: bool },
    #[error("VM guest protocol failed: {message}")]
    Protocol {
        message: &'static str,
        uncertain: bool,
    },
    #[error("VM guest rejected the request")]
    Rejected {
        code: Option<String>,
        detail: Option<String>,
        uncertain: bool,
    },
    #[error("VM guest frame callback failed")]
    FrameCallback { uncertain: bool },
}

impl GuestError {
    /// Whether the guest may have completed the request without the caller receiving its result.
    pub fn uncertain(&self) -> bool {
        match self {
            Self::Transport { uncertain, .. }
            | Self::Protocol { uncertain, .. }
            | Self::Rejected { uncertain, .. } => *uncertain,
            Self::InvalidRequest | Self::Identity | Self::SpacePreflight { .. } => false,
            Self::FrameCallback { uncertain } => *uncertain,
        }
    }

    /// The guest RPC error code, if it returned one.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Rejected { code, .. } => code.as_deref(),
            _ => None,
        }
    }

    /// The guest's error message, if it returned one.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Rejected { detail, .. } => detail.as_deref(),
            _ => None,
        }
    }
}

/// One fakeable synchronous boundary for JSON-RPC calls to the VM guest.
pub trait GuestRpc: Send + Sync {
    /// Send one request and return its final `result` frame.
    fn call(
        &self,
        method: &str,
        params: &Value,
        request_id: Option<&str>,
        timeout: Duration,
    ) -> Result<Value, GuestError> {
        let mut ignore_frame = |_| Ok(());
        self.stream(method, params, request_id, timeout, &mut ignore_frame)
    }

    /// Send one request, deliver every response frame to `on_frame`, and return the final result.
    fn stream(
        &self,
        method: &str,
        params: &Value,
        request_id: Option<&str>,
        timeout: Duration,
        on_frame: &mut dyn FnMut(Value) -> Result<(), GuestError>,
    ) -> Result<Value, GuestError>;
}

/// Production client for the guest Unix-domain socket.
#[cfg(unix)]
#[derive(Clone)]
pub struct UnixGuestRpc {
    vm_state_dir: PathBuf,
    socket_path: PathBuf,
    space_probe: Arc<dyn GuestSpaceProbe>,
}

#[cfg(unix)]
impl std::fmt::Debug for UnixGuestRpc {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UnixGuestRpc")
            .field("vm_state_dir", &self.vm_state_dir)
            .field("socket_path", &self.socket_path)
            .finish_non_exhaustive()
    }
}

#[cfg(unix)]
impl UnixGuestRpc {
    /// Connect to `<vm_state_dir>/guest.sock`.
    pub fn new(vm_state_dir: impl Into<PathBuf>) -> Self {
        Self::with_space_probe(vm_state_dir, Arc::new(HostGuestSpaceProbe))
    }

    /// Construct the client with an injectable disk-space boundary, primarily for corpus tests.
    pub fn with_space_probe(
        vm_state_dir: impl Into<PathBuf>,
        space_probe: Arc<dyn GuestSpaceProbe>,
    ) -> Self {
        let vm_state_dir = vm_state_dir.into();
        Self {
            socket_path: vm_state_dir.join("guest.sock"),
            vm_state_dir,
            space_probe,
        }
    }

    /// Use the Python runtime's VM state path: `CODEX_LINUX_VM_STATE_DIR` when set,
    /// otherwise `<runtime_state_dir>/linux-vm`.
    pub fn for_runtime_state(runtime_state_dir: &Path) -> Self {
        let vm_state_dir = std::env::var_os("CODEX_LINUX_VM_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| runtime_state_dir.join("linux-vm"));
        Self::new(vm_state_dir)
    }

    /// The endpoint path, exposed for diagnostics and fake endpoint tests.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

#[cfg(unix)]
impl GuestRpc for UnixGuestRpc {
    fn stream(
        &self,
        method: &str,
        params: &Value,
        request_id: Option<&str>,
        timeout: Duration,
        on_frame: &mut dyn FnMut(Value) -> Result<(), GuestError>,
    ) -> Result<Value, GuestError> {
        guest_write_preflight(method, &self.vm_state_dir, self.space_probe.as_ref())?;
        stream_socket(
            &self.socket_path,
            method,
            params,
            request_id,
            timeout,
            on_frame,
        )
    }
}

#[cfg(unix)]
fn guest_write_preflight(
    method: &str,
    vm_state_dir: &Path,
    space_probe: &dyn GuestSpaceProbe,
) -> Result<(), GuestError> {
    if !matches!(
        method,
        "project.import" | "line.branch" | "upload.begin" | "upload.commit" | "sync.push"
    ) {
        return Ok(());
    }
    require_agent_free_space(vm_state_dir, space_probe)
}

/// The free host disk space that a VM write or a VM start needs
/// (`CODEX_WORKSPACE_AGENT_MIN_FREE_BYTES`, default 5 GiB).
#[cfg(unix)]
pub(crate) fn require_agent_free_space(
    vm_state_dir: &Path,
    space_probe: &dyn GuestSpaceProbe,
) -> Result<(), GuestError> {
    let required = match std::env::var(AGENT_MIN_FREE_BYTES_ENV) {
        Ok(value) => value
            .parse::<u64>()
            .map_err(|_| GuestError::SpacePreflight {
                message: format!("{AGENT_MIN_FREE_BYTES_ENV} must be a non-negative integer."),
            })?,
        Err(std::env::VarError::NotPresent) => DEFAULT_AGENT_MIN_FREE_BYTES,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(GuestError::SpacePreflight {
                message: format!("{AGENT_MIN_FREE_BYTES_ENV} must be a non-negative integer."),
            });
        }
    };
    let free = space_probe
        .free_bytes(vm_state_dir)
        .map_err(|_| GuestError::SpacePreflight {
            message: "Could not check free disk space for the VM.".to_owned(),
        })?;
    if free < required {
        return Err(GuestError::SpacePreflight {
            message: format!(
                "Not enough free disk space for the VM: {free} free, {required} required (bytes)."
            ),
        });
    }
    Ok(())
}

/// Production client for the host-side VM control socket.
#[cfg(unix)]
#[derive(Clone, Debug)]
pub struct UnixVmControlRpc {
    socket_path: PathBuf,
}

#[cfg(unix)]
impl UnixVmControlRpc {
    /// Connect to `<vm_state_dir>/control.sock`.
    pub fn new(vm_state_dir: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: vm_state_dir.into().join("control.sock"),
        }
    }

    /// Use the Python runtime's VM state path: `CODEX_LINUX_VM_STATE_DIR` when set,
    /// otherwise `<runtime_state_dir>/linux-vm`.
    pub fn for_runtime_state(runtime_state_dir: &Path) -> Self {
        let vm_state_dir = std::env::var_os("CODEX_LINUX_VM_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| runtime_state_dir.join("linux-vm"));
        Self::new(vm_state_dir)
    }

    /// The endpoint path, exposed for diagnostics and fake endpoint tests.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

#[cfg(unix)]
impl GuestRpc for UnixVmControlRpc {
    fn stream(
        &self,
        method: &str,
        params: &Value,
        request_id: Option<&str>,
        timeout: Duration,
        on_frame: &mut dyn FnMut(Value) -> Result<(), GuestError>,
    ) -> Result<Value, GuestError> {
        stream_socket(
            &self.socket_path,
            method,
            params,
            request_id,
            timeout,
            on_frame,
        )
    }
}

/// Production client for an application-owned Unix JSON-RPC socket.
#[cfg(unix)]
#[derive(Clone, Debug)]
pub struct UnixSocketGuestRpc {
    socket_path: PathBuf,
}

#[cfg(unix)]
impl UnixSocketGuestRpc {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }
}

#[cfg(unix)]
impl GuestRpc for UnixSocketGuestRpc {
    fn stream(
        &self,
        method: &str,
        params: &Value,
        request_id: Option<&str>,
        timeout: Duration,
        on_frame: &mut dyn FnMut(Value) -> Result<(), GuestError>,
    ) -> Result<Value, GuestError> {
        stream_socket(
            &self.socket_path,
            method,
            params,
            request_id,
            timeout,
            on_frame,
        )
    }
}

#[cfg(unix)]
fn stream_socket(
    socket_path: &Path,
    method: &str,
    params: &Value,
    request_id: Option<&str>,
    timeout: Duration,
    on_frame: &mut dyn FnMut(Value) -> Result<(), GuestError>,
) -> Result<Value, GuestError> {
    use socket2::{Domain, SockAddr, Socket, Type};
    use std::os::unix::net::UnixStream;

    let identity = request_identity(request_id)?;
    validate_request(method, &identity, timeout)?;
    let mut request = Map::new();
    request.insert("id".to_owned(), Value::String(identity.clone()));
    request.insert("method".to_owned(), Value::String(method.to_owned()));
    request.insert(
        "params".to_owned(),
        if params.is_null() {
            Value::Object(Map::new())
        } else {
            params.clone()
        },
    );
    let mut frame =
        serde_json::to_vec(&Value::Object(request)).map_err(|_| GuestError::InvalidRequest)?;
    frame.push(b'\n');
    if frame.len() > GUEST_FRAME_LIMIT_BYTES {
        return Err(GuestError::Protocol {
            message: "request exceeds the frame limit",
            uncertain: false,
        });
    }

    let deadline = Instant::now() + timeout;
    let address = SockAddr::unix(socket_path).map_err(|error| GuestError::Transport {
        message: error.to_string(),
        uncertain: false,
    })?;
    let socket =
        Socket::new(Domain::UNIX, Type::STREAM, None).map_err(|error| GuestError::Transport {
            message: error.to_string(),
            uncertain: false,
        })?;
    let connect_timeout = timeout.min(Duration::from_secs(GUEST_CONNECT_TIMEOUT_SECONDS));
    socket
        .connect_timeout(&address, connect_timeout)
        .map_err(|error| GuestError::Transport {
            message: error.to_string(),
            uncertain: false,
        })?;
    let mut channel: UnixStream = socket.into();
    let write_timeout = remaining(deadline).map_err(|_| GuestError::Protocol {
        message: "request exceeded its timeout before send",
        uncertain: false,
    })?;
    channel
        .set_write_timeout(Some(write_timeout))
        .map_err(|error| GuestError::Transport {
            message: error.to_string(),
            uncertain: false,
        })?;
    channel
        .write_all(&frame)
        .map_err(|error| GuestError::Transport {
            message: error.to_string(),
            uncertain: true,
        })?;

    let mut bytes = Vec::with_capacity(BUFFER_READ_BYTES);
    loop {
        let next = read_line(&mut channel, deadline, &mut bytes)?;
        let response: Value = serde_json::from_slice(&next).map_err(|_| GuestError::Protocol {
            message: "response is not valid JSON",
            uncertain: true,
        })?;
        let Some(fields) = response.as_object() else {
            return Err(GuestError::Protocol {
                message: "response must be an object",
                uncertain: true,
            });
        };
        if fields.get("id").and_then(Value::as_str) != Some(identity.as_str()) {
            return Err(GuestError::Protocol {
                message: "response identity differs from the request",
                uncertain: true,
            });
        }
        if let Some(error) = fields.get("error") {
            let (code, detail) = error_fields(error);
            return Err(GuestError::Rejected {
                uncertain: code.as_deref() == Some("outcome_unknown"),
                code,
                detail,
            });
        }
        on_frame(response.clone()).map_err(|_| GuestError::FrameCallback { uncertain: true })?;
        if let Some(result) = fields.get("result") {
            return Ok(result.clone());
        }
    }
}

#[cfg(unix)]
fn read_line(
    channel: &mut std::os::unix::net::UnixStream,
    deadline: Instant,
    buffer: &mut Vec<u8>,
) -> Result<Vec<u8>, GuestError> {
    // Darwin may reject SO_RCVTIMEO after the peer closes, even when its full
    // response is buffered. Poll an owned nonblocking socket instead, so both
    // buffered EOF responses and incomplete responses obey the same deadline.
    channel
        .set_nonblocking(true)
        .map_err(|error| GuestError::Transport {
            message: error.to_string(),
            uncertain: true,
        })?;
    let mut chunk = [0; BUFFER_READ_BYTES];
    loop {
        if let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
            if end > GUEST_FRAME_LIMIT_BYTES {
                return Err(GuestError::Protocol {
                    message: "response exceeds the frame limit",
                    uncertain: true,
                });
            }
            let remainder = buffer.split_off(end + 1);
            let mut line = std::mem::replace(buffer, remainder);
            line.pop();
            return Ok(line);
        }
        if buffer.len() > GUEST_FRAME_LIMIT_BYTES {
            return Err(GuestError::Protocol {
                message: "response exceeds the frame limit",
                uncertain: true,
            });
        }
        let wait = remaining(deadline).map_err(|_| GuestError::Protocol {
            message: "response exceeded its timeout",
            uncertain: true,
        })?;
        let timeout =
            rustix::event::Timespec::try_from(wait).map_err(|error| GuestError::Transport {
                message: error.to_string(),
                uncertain: true,
            })?;
        let mut socket = rustix::event::PollFd::new(&*channel, rustix::event::PollFlags::IN);
        match rustix::event::poll(std::slice::from_mut(&mut socket), Some(&timeout)) {
            Ok(0) | Err(rustix::io::Errno::INTR) => continue,
            Ok(_) => {}
            Err(error) => {
                return Err(GuestError::Transport {
                    message: error.to_string(),
                    uncertain: true,
                });
            }
        }
        match channel.read(&mut chunk) {
            Ok(0) => {
                return Err(GuestError::Transport {
                    message: "connection closed before a result".to_owned(),
                    uncertain: true,
                });
            }
            Ok(length) => buffer.extend_from_slice(&chunk[..length]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) => {
                return Err(GuestError::Transport {
                    message: error.to_string(),
                    uncertain: true,
                });
            }
        }
    }
}

fn request_identity(request_id: Option<&str>) -> Result<String, GuestError> {
    if let Some(identity) = request_id {
        if identity.is_empty() || identity.chars().count() > GUEST_REQUEST_ID_MAX_CHARS {
            return Err(GuestError::InvalidRequest);
        }
        return Ok(identity.to_owned());
    }
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(|_| GuestError::Identity)?;
    Ok(data_encoding::HEXLOWER.encode(&random))
}

fn validate_request(method: &str, identity: &str, timeout: Duration) -> Result<(), GuestError> {
    if method.is_empty()
        || identity.is_empty()
        || identity.chars().count() > GUEST_REQUEST_ID_MAX_CHARS
        || timeout.is_zero()
    {
        return Err(GuestError::InvalidRequest);
    }
    let maximum = if method == "host.exec" {
        GUEST_EXEC_TIMEOUT_SECONDS
    } else {
        GUEST_RPC_TIMEOUT_SECONDS
    };
    if timeout > Duration::from_secs(maximum) {
        return Err(GuestError::InvalidRequest);
    }
    Ok(())
}

fn remaining(deadline: Instant) -> Result<Duration, GuestError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(GuestError::Protocol {
            message: "request exceeded its timeout",
            uncertain: true,
        })
}

fn error_fields(error: &Value) -> (Option<String>, Option<String>) {
    match error {
        Value::String(detail) => (None, Some(detail.clone())),
        Value::Object(fields) => (
            fields
                .get("code")
                .and_then(Value::as_str)
                .map(str::to_owned),
            fields
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned),
        ),
        _ => (None, None),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;
    use std::thread;

    use serde_json::{Value, json};

    use super::{GuestError, GuestRpc, UnixGuestRpc, UnixVmControlRpc};

    const TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

    #[test]
    fn completed_response_survives_peer_close_before_read() {
        let (mut client, mut server) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        let response = b"{\"id\":\"closed-peer\",\"result\":{}}\n";
        server.write_all(response).expect("complete response");
        drop(server);
        let line = super::read_line(
            &mut client,
            std::time::Instant::now() + TEST_TIMEOUT,
            &mut Vec::new(),
        )
        .expect("a complete buffered response survives peer close");
        assert_eq!(line, &response[..response.len() - 1]);
    }

    #[test]
    fn incomplete_response_after_peer_close_remains_uncertain() {
        let (mut client, mut server) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        server.write_all(b"{\"result\":").expect("partial response");
        drop(server);
        let error = super::read_line(
            &mut client,
            std::time::Instant::now() + TEST_TIMEOUT,
            &mut Vec::new(),
        )
        .expect_err("partial response cannot confirm an operation");
        assert!(matches!(
            error,
            GuestError::Transport {
                uncertain: true,
                ..
            }
        ));
    }

    #[test]
    fn silent_peer_cannot_extend_response_deadline() {
        let (mut client, _server) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        let error = super::read_line(
            &mut client,
            std::time::Instant::now() + std::time::Duration::from_millis(20),
            &mut Vec::new(),
        )
        .expect_err("silent peer times out without closing");
        assert!(matches!(
            error,
            GuestError::Protocol {
                message: "response exceeded its timeout",
                uncertain: true
            }
        ));
    }

    fn fake_guest(response: Value) -> (tempfile::TempDir, UnixGuestRpc, mpsc::Receiver<Value>) {
        let directory = tempfile::tempdir().expect("temporary fake guest directory");
        let rpc = UnixGuestRpc::new(directory.path());
        let listener = UnixListener::bind(rpc.socket_path()).expect("fake endpoint binds");
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let (mut connection, _) = listener.accept().expect("fake guest accepts request");
            connection
                .set_read_timeout(Some(TEST_TIMEOUT))
                .expect("fake endpoint read timeout");
            let mut line = String::new();
            BufReader::new(&mut connection)
                .read_line(&mut line)
                .expect("fake guest reads one frame");
            let request: Value = serde_json::from_str(&line).expect("request frame parses");
            sender
                .send(request.clone())
                .expect("request witness delivered");
            let mut response = serde_json::to_vec(&response).expect("response serializes");
            response.push(b'\n');
            connection
                .write_all(&response)
                .expect("fake guest writes response");
        });
        (directory, rpc, receiver)
    }

    #[test]
    fn call_frames_request_and_matches_response_identity() {
        let response = json!({"id":"request-1","result":{"state":"running"}});
        let (_directory, rpc, request) = fake_guest(response);
        let result = rpc
            .call("host.status", &json!({}), Some("request-1"), TEST_TIMEOUT)
            .expect("RPC result received");
        let sent = request
            .recv_timeout(TEST_TIMEOUT)
            .expect("request witness received");
        assert_eq!(sent["id"], "request-1");
        assert_eq!(sent["method"], "host.status");
        assert_eq!(sent["params"], json!({}));
        assert_eq!(result, json!({"state":"running"}));
    }

    #[test]
    fn guest_error_preserves_code_detail_and_uncertainty() {
        let response = json!({
            "id":"request-2",
            "error":{"code":"outcome_unknown","message":"operation receipt pending"}
        });
        let (_directory, rpc, request) = fake_guest(response);
        let error = rpc
            .call("line.merge", &json!({}), Some("request-2"), TEST_TIMEOUT)
            .expect_err("guest reports uncertain outcome");
        request
            .recv_timeout(TEST_TIMEOUT)
            .expect("request witness received");
        assert_eq!(error.code(), Some("outcome_unknown"), "{error:?}");
        assert_eq!(error.detail(), Some("operation receipt pending"));
        assert!(error.uncertain());
    }

    #[test]
    fn response_with_a_different_identity_is_uncertain() {
        let response = json!({"id":"different","result":{}});
        let (_directory, rpc, request) = fake_guest(response);
        let error = rpc
            .call("health", &json!({}), Some("request-3"), TEST_TIMEOUT)
            .expect_err("mismatched response refused");
        request
            .recv_timeout(TEST_TIMEOUT)
            .expect("request witness received");
        assert!(matches!(
            error,
            GuestError::Protocol {
                uncertain: true,
                ..
            }
        ));
    }

    #[test]
    fn control_rpc_uses_the_host_control_socket() {
        let directory = tempfile::tempdir().expect("temporary fake VM directory");
        let rpc = UnixVmControlRpc::new(directory.path());
        let listener = UnixListener::bind(rpc.socket_path()).expect("fake control endpoint binds");
        let (sender, request) = mpsc::channel();
        thread::spawn(move || {
            let (mut connection, _) = listener.accept().expect("fake control endpoint accepts");
            connection
                .set_read_timeout(Some(TEST_TIMEOUT))
                .expect("fake endpoint read timeout");
            let mut line = String::new();
            BufReader::new(&mut connection)
                .read_line(&mut line)
                .expect("fake control endpoint reads frame");
            let value: Value = serde_json::from_str(&line).expect("request frame parses");
            sender.send(value).expect("request witness delivered");
            connection
                .write_all(br#"{"id":"host-status","result":{"state":"stopped"}}"#)
                .and_then(|()| connection.write_all(b"\n"))
                .expect("fake control endpoint writes response");
        });
        let result = rpc
            .call("host.status", &json!({}), Some("host-status"), TEST_TIMEOUT)
            .expect("control result received");
        let sent = request
            .recv_timeout(TEST_TIMEOUT)
            .expect("request witness received");
        assert_eq!(sent["method"], "host.status");
        assert_eq!(result, json!({"state":"stopped"}));
    }
}
