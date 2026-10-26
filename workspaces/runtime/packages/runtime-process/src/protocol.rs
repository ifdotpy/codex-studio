use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};
use strum::Display;
pub use studio_runtime_providers::supervisor_protocol::NativeId;

pub use studio_runtime_providers::supervisor_protocol::SUPERVISOR_PROTOCOL_V2;
pub const SUPERVISOR_NEXT_BATCH_MAX_EVENTS: usize = 64;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SupervisorHandshakeV2 {
    pub protocol: u64,
    pub state_dir: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_id: Option<BackendId>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub probe: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub operator: bool,
}

impl SupervisorHandshakeV2 {
    pub(crate) fn connection_identity(
        &self,
    ) -> Result<
        studio_runtime_providers::supervisor_protocol::ConnectionIdentity<'_>,
        studio_runtime_providers::supervisor_protocol::Refusal,
    > {
        use studio_runtime_providers::supervisor_protocol::{ConnectionIdentity, Refusal};
        match (self.probe, self.operator, self.backend_id.as_ref()) {
            (true, false, None) => Ok(ConnectionIdentity::Probe),
            // Older retained guests include backendId on operator requests. It
            // never acquires backend ownership or grants backend capabilities.
            (false, true, _) => Ok(ConnectionIdentity::Operator),
            (false, false, Some(backend)) if !backend.0.is_empty() => {
                Ok(ConnectionIdentity::Backend(&backend.0))
            }
            _ => Err(Refusal(
                "Supervisor handshake requires exactly one connection identity",
            )),
        }
    }
}

fn is_false(value: &bool) -> bool {
    !value
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

#[derive(Clone, Copy, Debug, Deserialize, Display, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[strum(serialize_all = "camelCase")]
pub enum SupervisorAction {
    Health,
    Open,
    Next,
    NextBatch,
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
    pub native_id: Option<NativeId>,
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
pub struct NextBatchRequest {
    pub handle: SupervisorHandle,
    pub cursor: EventCursor,
    pub generation: Generation,
    pub limit: usize,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NextBatchRequestEnvelope {
    pub action: SupervisorAction,
    #[serde(rename = "requestId")]
    pub request_id: Option<u64>,
    #[serde(flatten)]
    pub request: NextBatchRequest,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NextBatchResult {
    pub events: Vec<SupervisorEventV2>,
    #[serde(rename = "returnCode")]
    pub return_code: Option<i32>,
    pub backpressure: bool,
    #[serde(rename = "stdoutReaderAlive")]
    pub stdout_reader_alive: bool,
    #[serde(rename = "stderrReaderAlive")]
    pub stderr_reader_alive: bool,
}

/// Protocol-v2 event payload is the journal's original JSON value, embedded
/// directly in the frame rather than quoted as protocol-v1's JSON string.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorEventV2 {
    pub sequence: EventCursor,
    pub generation: Generation,
    pub kind: EventKind,
    pub payload: Box<RawValue>,
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
    pub remote_id: Option<NativeId>,
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
    pub native_id: Option<NativeId>,
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

pub use studio_runtime_providers::supervisor_contract::{
    Durability, HealthHandle, HealthResult, RecoveryStatus, SupervisorHandshakeV2Result,
};
