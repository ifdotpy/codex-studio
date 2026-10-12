//! Synchronous process ports and the supervisor v1 client.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const PROTOCOL_VERSION: u64 = 1;
const MAX_FRAME_BYTES: usize = 1024 * 1024;
const IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Backend identity sent in the supervisor handshake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendIdentity {
    pub backend_id: BackendId,
    pub state_dir: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BackendId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SupervisorHandle(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventCursor(pub u64);

/// Stable supervisor handle together with its process generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandleIdentity {
    pub handle: SupervisorHandle,
    pub generation: Generation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Generation(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupervisorAction {
    Health,
    Open,
    Next,
    FinishFallback,
    AdminCloseHandle,
    RetireCommand,
    Write,
    OperationStatus,
    Ack,
    Replay,
    Status,
    Detach,
}

impl SupervisorAction {
    const fn wire_name(self) -> &'static str {
        match self {
            Self::Health => "health",
            Self::Open => "open",
            Self::Next => "next",
            Self::FinishFallback => "finishFallback",
            Self::AdminCloseHandle => "adminCloseHandle",
            Self::RetireCommand => "retireCommand",
            Self::Write => "write",
            Self::OperationStatus => "operationStatus",
            Self::Ack => "ack",
            Self::Replay => "replay",
            Self::Status => "status",
            Self::Detach => "detach",
        }
    }
}

impl fmt::Display for SupervisorAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire_name())
    }
}

/// Errors distinguish a rejected handshake, a lost connection, and a request
/// whose result is unknown because the response did not arrive.
#[derive(Debug)]
pub enum SupervisorError {
    Io(io::Error),
    Json(serde_json::Error),
    ProtocolMismatch {
        expected: u64,
        received: Option<u64>,
        detail: String,
    },
    InvalidFrame(String),
    ConnectionLost,
    UnknownOutcome {
        action: SupervisorAction,
        request_id: u64,
    },
    Remote {
        action: SupervisorAction,
        message: String,
    },
    UnsupportedTransport(&'static str),
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "supervisor transport: {error}"),
            Self::Json(error) => write!(f, "supervisor JSON: {error}"),
            Self::ProtocolMismatch {
                expected,
                received,
                detail,
            } => {
                write!(
                    f,
                    "supervisor protocol mismatch: expected {expected}, received {received:?}: {detail}"
                )
            }
            Self::InvalidFrame(detail) => write!(f, "invalid supervisor frame: {detail}"),
            Self::ConnectionLost => f.write_str("supervisor connection lost during handshake"),
            Self::UnknownOutcome { action, request_id } => {
                write!(
                    f,
                    "supervisor {action} request {request_id} has unknown outcome"
                )
            }
            Self::Remote { action, message } => {
                write!(f, "supervisor {action} rejected: {message}")
            }
            Self::UnsupportedTransport(platform) => {
                write!(f, "supervisor transport is not implemented on {platform}")
            }
        }
    }
}

impl std::error::Error for SupervisorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for SupervisorError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for SupervisorError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Any synchronous byte stream with a bounded frame reader can back this
/// client. Production currently supplies a Unix domain socket.
pub trait SupervisorTransport: Read + Write {}
impl<T: Read + Write> SupervisorTransport for T {}

pub struct SupervisorClient<T: SupervisorTransport> {
    stream: BufReader<T>,
    next_request_id: u64,
    backend_identity: BackendIdentity,
}

impl<T: SupervisorTransport> SupervisorClient<T> {
    /// Perform the protocol-v1 identity handshake over an already-connected
    /// transport. The transport is closed by dropping the client.
    pub fn from_transport(
        transport: T,
        identity: BackendIdentity,
    ) -> Result<Self, SupervisorError> {
        Self::from_transport_with_operator(transport, identity, false)
    }

    fn from_transport_with_operator(
        mut transport: T,
        identity: BackendIdentity,
        operator: bool,
    ) -> Result<Self, SupervisorError> {
        let mut handshake = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "stateDir": identity.state_dir,
            "backendId": identity.backend_id,
        });
        if operator && let Some(object) = handshake.as_object_mut() {
            object.remove("backendId");
            object.insert("operator".into(), Value::Bool(true));
        }
        write_frame(&mut transport, &handshake)?;
        let mut stream = BufReader::new(transport);
        let response = read_frame(&mut stream)?.ok_or(SupervisorError::ConnectionLost)?;
        let value: Value = serde_json::from_str(&response)?;
        if value.get("ok") == Some(&Value::Bool(true)) {
            Ok(Self {
                stream,
                next_request_id: 0,
                backend_identity: identity,
            })
        } else {
            let detail = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unexpected handshake response")
                .to_owned();
            Err(SupervisorError::ProtocolMismatch {
                expected: PROTOCOL_VERSION,
                received: None,
                detail,
            })
        }
    }

    pub fn backend_identity(&self) -> &BackendIdentity {
        &self.backend_identity
    }

    pub fn health(&mut self, request: HealthRequest) -> Result<HealthResult, SupervisorError> {
        self.call(SupervisorAction::Health, request)
    }

    pub fn open(&mut self, request: OpenRequest) -> Result<OpenResult, SupervisorError> {
        self.call(SupervisorAction::Open, request)
    }

    pub fn next(&mut self, request: CursorRequest) -> Result<NextResult, SupervisorError> {
        self.call(SupervisorAction::Next, request)
    }

    pub fn finish_fallback(
        &mut self,
        request: FinishFallbackRequest,
    ) -> Result<FinishedFallback, SupervisorError> {
        self.call(SupervisorAction::FinishFallback, request)
    }

    pub fn admin_close_handle(
        &mut self,
        request: AdminCloseHandleRequest,
    ) -> Result<AdminCloseHandleResult, SupervisorError> {
        self.call(SupervisorAction::AdminCloseHandle, request)
    }

    pub fn retire_command(
        &mut self,
        request: RetireCommandRequest,
    ) -> Result<RetireCommandResult, SupervisorError> {
        self.call(SupervisorAction::RetireCommand, request)
    }

    pub fn write(&mut self, request: WriteRequest) -> Result<WriteResult, SupervisorError> {
        self.call(SupervisorAction::Write, request)
    }

    pub fn operation_status(
        &mut self,
        request: OperationStatusRequest,
    ) -> Result<OperationStatusResult, SupervisorError> {
        self.call(SupervisorAction::OperationStatus, request)
    }

    pub fn ack(&mut self, request: AckRequest) -> Result<AckResult, SupervisorError> {
        self.call(SupervisorAction::Ack, request)
    }

    pub fn replay(&mut self, request: CursorRequest) -> Result<ReplayResult, SupervisorError> {
        self.call(SupervisorAction::Replay, request)
    }

    pub fn status(&mut self, request: HandleRequest) -> Result<StatusResult, SupervisorError> {
        self.call(SupervisorAction::Status, request)
    }

    pub fn detach(&mut self, request: HandleRequest) -> Result<DetachedResult, SupervisorError> {
        self.call(SupervisorAction::Detach, request)
    }

    fn call<R: Serialize, O: for<'de> Deserialize<'de>>(
        &mut self,
        action: SupervisorAction,
        request: R,
    ) -> Result<O, SupervisorError> {
        self.next_request_id = self.next_request_id.checked_add(1).ok_or_else(|| {
            SupervisorError::InvalidFrame("request identity space exhausted".into())
        })?;
        let request_id = self.next_request_id;
        let mut value = serde_json::to_value(request)?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| SupervisorError::InvalidFrame("request must be an object".into()))?;
        object.insert("requestId".into(), Value::from(request_id));
        object.insert("action".into(), Value::from(action.wire_name()));
        if !matches!(
            action,
            SupervisorAction::Health
                | SupervisorAction::FinishFallback
                | SupervisorAction::AdminCloseHandle
        ) && !object.contains_key("handle")
        {
            return Err(SupervisorError::InvalidFrame(format!(
                "{} request requires handle",
                action
            )));
        }
        if let Err(error) = write_frame(self.stream.get_mut(), &value) {
            return match error {
                SupervisorError::Io(_) => {
                    Err(SupervisorError::UnknownOutcome { action, request_id })
                }
                other => Err(other),
            };
        }
        let response = match read_frame(&mut self.stream) {
            Ok(Some(line)) => line,
            Ok(None) | Err(SupervisorError::Io(_)) => {
                return Err(SupervisorError::UnknownOutcome { action, request_id });
            }
            Err(_) => return Err(SupervisorError::UnknownOutcome { action, request_id }),
        };
        let response: Value = match serde_json::from_str(&response) {
            Ok(response) => response,
            Err(_) => return Err(SupervisorError::UnknownOutcome { action, request_id }),
        };
        if response.get("requestId").and_then(Value::as_u64) != Some(request_id) {
            return Err(SupervisorError::UnknownOutcome { action, request_id });
        }
        if let Some(message) = response.get("error").and_then(Value::as_str) {
            return Err(SupervisorError::Remote {
                action,
                message: message.to_owned(),
            });
        }
        let result = response
            .get("result")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        serde_json::from_value(result)
            .map_err(|_| SupervisorError::UnknownOutcome { action, request_id })
    }
}

#[cfg(unix)]
impl SupervisorClient<std::os::unix::net::UnixStream> {
    /// Connect to the supervisor socket in a state directory or to a socket
    /// path directly. The caller owns path validation before connecting.
    pub fn connect(
        path: impl AsRef<Path>,
        identity: BackendIdentity,
    ) -> Result<Self, SupervisorError> {
        let path = path.as_ref();
        let endpoint = if path.is_dir() {
            path.join("supervisor.sock")
        } else {
            path.to_path_buf()
        };
        let stream = std::os::unix::net::UnixStream::connect(endpoint)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        Self::from_transport(stream, identity)
    }

    pub fn connect_operator(
        path: impl AsRef<Path>,
        state_dir: PathBuf,
    ) -> Result<Self, SupervisorError> {
        let path = path.as_ref();
        let endpoint = if path.is_dir() {
            path.join("supervisor.sock")
        } else {
            path.to_path_buf()
        };
        let stream = std::os::unix::net::UnixStream::connect(endpoint)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        Self::from_transport_with_operator(
            stream,
            BackendIdentity {
                backend_id: BackendId(String::new()),
                state_dir,
            },
            true,
        )
    }
}

#[cfg(windows)]
impl SupervisorClient<std::fs::File> {
    /// Windows named-pipe support is intentionally deferred until the supported
    /// server entry points are inventoried and a transport is approved.
    pub fn connect(
        _path: impl AsRef<Path>,
        _identity: BackendIdentity,
    ) -> Result<Self, SupervisorError> {
        Err(SupervisorError::UnsupportedTransport("Windows"))
    }
}

fn write_frame(writer: &mut impl Write, value: &Value) -> Result<(), SupervisorError> {
    let mut frame = serde_json::to_vec(value)?;
    if frame.len() + 1 > MAX_FRAME_BYTES {
        return Err(SupervisorError::InvalidFrame(
            "request exceeds 1 MiB".into(),
        ));
    }
    frame.push(b'\n');
    writer.write_all(&frame)?;
    writer.flush()?;
    Ok(())
}

fn read_frame(reader: &mut impl BufRead) -> Result<Option<String>, SupervisorError> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(SupervisorError::InvalidFrame(
                    "connection ended inside a frame".into(),
                ))
            };
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if frame.len() + take > MAX_FRAME_BYTES {
            return Err(SupervisorError::InvalidFrame(
                "response exceeds 1 MiB".into(),
            ));
        }
        let complete = available.get(take.wrapping_sub(1)) == Some(&b'\n');
        frame.extend_from_slice(&available[..take]);
        reader.consume(take);
        if complete {
            frame.pop();
            return String::from_utf8(frame)
                .map(Some)
                .map_err(|error| SupervisorError::InvalidFrame(error.to_string()));
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct HealthRequest {
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct FinishFallbackRequest {
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HandleRequest {
    pub handle: SupervisorHandle,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Serialize)]
pub struct CursorRequest {
    pub handle: SupervisorHandle,
    pub cursor: EventCursor,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Serialize)]
pub struct OpenRequest {
    pub handle: SupervisorHandle,
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Serialize)]
pub struct WriteRequest {
    pub handle: SupervisorHandle,
    #[serde(rename = "operationId")]
    pub operation_id: OperationId,
    #[serde(rename = "nativeId")]
    pub native_id: Option<u64>,
    pub message: Value,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Serialize)]
pub struct OperationStatusRequest {
    pub handle: SupervisorHandle,
    #[serde(rename = "operationId")]
    pub operation_id: OperationId,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Serialize)]
pub struct AckRequest {
    pub handle: SupervisorHandle,
    pub sequence: EventCursor,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Serialize)]
pub struct RetireCommandRequest {
    pub handle: SupervisorHandle,
    pub generation: Generation,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Serialize)]
pub struct AdminCloseHandleRequest {
    pub handle: SupervisorHandle,
    #[serde(rename = "expectedPid")]
    pub expected_pid: u32,
    #[serde(rename = "expectedStartTime")]
    pub expected_start_time: String,
    #[serde(rename = "expectedSignature")]
    pub expected_signature: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HealthResult {
    pub protocol: u64,
    #[serde(rename = "stateDir")]
    pub state_dir: String,
    pub handles: Vec<HealthHandle>,
    #[serde(rename = "journalLimitBytes")]
    pub journal_limit_bytes: u64,
    #[serde(rename = "outputLimitBytesPerHandle")]
    pub output_limit_bytes_per_handle: u64,
    pub durability: Durability,
    pub recovery: RecoveryStatus,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthHandle {
    pub id: String,
    pub pid: Option<u32>,
    pub signature: String,
    #[serde(rename = "startTime")]
    pub start_time: Option<String>,
    #[serde(rename = "jobIdentity")]
    pub job_identity: Option<String>,
    pub sequence: u64,
    pub acknowledged: u64,
    #[serde(rename = "bufferedBytes")]
    pub buffered_bytes: u64,
    pub backpressure: bool,
    #[serde(rename = "stdoutReaderAlive")]
    pub stdout_reader_alive: bool,
    #[serde(rename = "stdoutReaderError")]
    pub stdout_reader_error: Option<String>,
    #[serde(rename = "persistenceErrors")]
    pub persistence_errors: BTreeMap<String, String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenResult {
    pub resumed: bool,
    #[serde(rename = "initResult")]
    pub init_result: Option<Value>,
    pub acknowledged: u64,
    pub sequence: u64,
    pub generation: Generation,
    #[serde(rename = "returnCode")]
    pub return_code: Option<i32>,
    pub pid: u32,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl OpenResult {
    pub fn handle_identity(&self, handle: SupervisorHandle) -> HandleIdentity {
        HandleIdentity {
            handle,
            generation: self.generation,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NextResult {
    pub event: Option<SupervisorEvent>,
    #[serde(rename = "returnCode")]
    pub return_code: Option<i32>,
    pub backpressure: bool,
    #[serde(rename = "stdoutReaderAlive")]
    pub stdout_reader_alive: bool,
    #[serde(rename = "stderrReaderAlive")]
    pub stderr_reader_alive: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorEvent {
    pub sequence: EventCursor,
    pub kind: EventKind,
    #[serde(
        deserialize_with = "decode_event_payload",
        serialize_with = "encode_event_payload"
    )]
    pub payload: Value,
    pub generation: Generation,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
fn decode_event_payload<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Value, D::Error> {
    let encoded = String::deserialize(deserializer)?;
    serde_json::from_str(&encoded).map_err(serde::de::Error::custom)
}

fn encode_event_payload<S: serde::Serializer>(
    payload: &Value,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&payload.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventKind {
    Stdout,
    Stderr,
    Exit,
    Unknown(String),
}
impl<'de> Deserialize<'de> for EventKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "stdout" => Self::Stdout,
            "stderr" => Self::Stderr,
            "exit" => Self::Exit,
            other => Self::Unknown(other.to_owned()),
        })
    }
}
impl Serialize for EventKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Exit => "exit",
            Self::Unknown(other) => other,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FinishedFallback {
    pub finished: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminCloseHandleResult {
    pub closed: bool,
    pub handle: SupervisorHandle,
    pub pid: u32,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RetireCommandResult {
    pub retired: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WriteResult {
    pub accepted: bool,
    pub duplicate: bool,
    #[serde(rename = "remoteId")]
    pub remote_id: Option<u64>,
    #[serde(rename = "durableMs")]
    pub durable_ms: f64,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationStatusResult {
    pub accepted: bool,
    pub reason: Option<String>,
    #[serde(rename = "nativeId")]
    pub native_id: Option<u64>,
    pub response: Option<Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AckResult {
    pub acknowledged: u64,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayResult {
    pub events: Vec<SupervisorEvent>,
    pub sequence: u64,
    pub acknowledged: u64,
    pub backpressure: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusResult {
    pub sequence: u64,
    pub acknowledged: u64,
    pub backpressure: bool,
    pub pid: u32,
    #[serde(rename = "returnCode")]
    pub return_code: Option<i32>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DetachedResult {
    pub detached: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Durability {
    SqliteFullSyncPerRequest,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryStatus {
    pub degraded: bool,
    #[serde(rename = "fallbackReady")]
    pub fallback_ready: bool,
    pub blocked: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// User terminal port. Implementations belong in a later platform adapter.
pub trait PtyHost: Send + Sync {
    type Session: Send;
    type Error: std::error::Error + Send + Sync + 'static;
    fn open(
        &self,
        command: &[String],
        cwd: &Path,
        env: &[(String, String)],
    ) -> Result<Self::Session, Self::Error>;
    fn resize(
        &self,
        session: &mut Self::Session,
        columns: u16,
        rows: u16,
    ) -> Result<(), Self::Error>;
    fn write(&self, session: &mut Self::Session, bytes: &[u8]) -> Result<(), Self::Error>;
    fn read(&self, session: &mut Self::Session, buffer: &mut [u8]) -> Result<usize, Self::Error>;
    fn wait(&self, session: &mut Self::Session) -> Result<Option<i32>, Self::Error>;
    fn close(&self, session: Self::Session) -> Result<(), Self::Error>;
}

/// Progress-file watch port. Callbacks are delivered on the implementation's
/// worker thread and must not block event delivery.
pub trait ProgressWatch: Send + Sync {
    type Watch: Send;
    type Error: std::error::Error + Send + Sync + 'static;
    fn watch(
        &self,
        path: &Path,
        changed: Box<dyn Fn() + Send + Sync>,
    ) -> Result<Self::Watch, Self::Error>;
    fn unwatch(&self, watch: Self::Watch) -> Result<(), Self::Error>;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_codec_round_trips_compact_json_and_preserves_extensions() {
        let value = serde_json::json!({"action":"status","handle":"h","future":42});
        let mut encoded = Vec::new();
        write_frame(&mut encoded, &value).unwrap();
        let mut reader = Cursor::new(encoded);
        let decoded: Value =
            serde_json::from_str(&read_frame(&mut reader).unwrap().unwrap()).unwrap();
        assert_eq!(decoded, value);
        let result: DetachedResult =
            serde_json::from_value(serde_json::json!({"detached":true,"newField":7})).unwrap();
        assert_eq!(result.extra.get("newField"), Some(&Value::from(7)));
        let encoded_again = serde_json::to_value(result).unwrap();
        assert_eq!(encoded_again["newField"], 7);
    }

    #[test]
    fn event_payload_and_unknown_kind_round_trip() {
        let event: SupervisorEvent = serde_json::from_value(serde_json::json!({
            "sequence": 2, "kind": "future-event", "payload": "{\"x\":1}", "generation": 4, "extension": true
        })).unwrap();
        assert_eq!(event.kind, EventKind::Unknown("future-event".into()));
        assert_eq!(event.payload["x"], 1);
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["kind"], "future-event");
        assert_eq!(value["extension"], true);
    }

    #[test]
    fn frame_reader_rejects_oversized_and_incomplete_frames() {
        let mut too_large = Cursor::new(vec![b'x'; MAX_FRAME_BYTES + 1]);
        assert!(matches!(
            read_frame(&mut too_large),
            Err(SupervisorError::InvalidFrame(_))
        ));
        let mut partial = Cursor::new(b"{}".to_vec());
        assert!(matches!(
            read_frame(&mut partial),
            Err(SupervisorError::InvalidFrame(_))
        ));
    }
}
