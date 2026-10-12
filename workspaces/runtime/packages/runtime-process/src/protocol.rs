use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Reader for a supervisor-backed provider byte stream.
///
/// Cursors become available only after the bytes for a complete JSONL frame
/// have been read. The session associates each cursor with its decoded frame;
/// its consumer acknowledges the cursor after durable settlement.
pub trait ProviderProcessReader: Send {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize>;
    fn take_cursors(&mut self) -> Vec<u64>;
}

/// Supervisor-owned process stream consumed by runtime-services.
pub trait ProviderProcess: Send {
    fn split(&mut self) -> io::Result<(Box<dyn ProviderProcessReader>, Box<dyn Write + Send>)>;
    fn acknowledged_cursor(&self) -> u64;
    fn generation(&self) -> u64;
    fn acknowledge(&mut self, cursor: u64) -> io::Result<()>;
    fn stop(&mut self) -> io::Result<()>;
}

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
    pub(crate) const fn wire_name(self) -> &'static str {
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
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("supervisor transport: {0}")]
    Io(#[from] io::Error),
    #[error("supervisor JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("supervisor protocol mismatch: expected {expected}, received {received:?}: {detail}")]
    ProtocolMismatch {
        expected: u64,
        received: Option<u64>,
        detail: String,
    },
    #[error("invalid supervisor frame: {0}")]
    InvalidFrame(String),
    #[error("supervisor connection lost during handshake")]
    ConnectionLost,
    #[error("supervisor {action} request {request_id} has unknown outcome")]
    UnknownOutcome {
        action: SupervisorAction,
        request_id: u64,
    },
    #[error("supervisor {action} rejected: {message}")]
    Remote {
        action: SupervisorAction,
        message: String,
    },
    #[error("supervisor transport is not implemented on {0}")]
    UnsupportedTransport(&'static str),
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
    /// Journal append sequence; independent stdout and stderr readers can
    /// interleave in either order.
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
