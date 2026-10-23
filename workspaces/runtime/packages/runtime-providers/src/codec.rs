use serde::{Deserialize, Serialize, Serializer, ser::SerializeMap};
use serde_json::{Map, Value};
use std::borrow::Cow;
use thiserror::Error;

/// Largest accepted UTF-8 JSON payload, excluding its LF or CRLF delimiter.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Maximum wire bytes occupied by a frame's LF or CRLF delimiter.
pub const MAX_FRAME_DELIMITER_BYTES: usize = 2;

/// Count payload bytes in a complete wire frame, excluding LF or CRLF when present.
#[must_use]
pub fn frame_payload_bytes(frame: &[u8]) -> usize {
    frame
        .strip_suffix(b"\n")
        .map_or(frame, |without_lf| {
            without_lf.strip_suffix(b"\r").unwrap_or(without_lf)
        })
        .len()
}

/// Count payload bytes in a partial transport line, deferring a trailing CR until the next read.
#[must_use]
pub fn frame_prefix_payload_bytes(prefix: &[u8]) -> usize {
    prefix.strip_suffix(b"\r").unwrap_or(prefix).len()
}

/// Numeric Codex IDs and string Claude bridge IDs are distinct on the wire.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(u64),
    String(String),
}

/// Decode an ID straight from the token, without the buffered `untagged`
/// detour; any other JSON type, including negative numbers, is invalid.
impl<'de> Deserialize<'de> for RequestId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct IdVisitor;
        impl serde::de::Visitor<'_> for IdVisitor {
            type Value = RequestId;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a non-negative integer or string request id")
            }
            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<RequestId, E> {
                Ok(RequestId::Number(value))
            }
            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<RequestId, E> {
                u64::try_from(value)
                    .map(RequestId::Number)
                    .map_err(|_| E::invalid_value(serde::de::Unexpected::Signed(value), &self))
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<RequestId, E> {
                Ok(RequestId::String(value.to_owned()))
            }
            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<RequestId, E> {
                Ok(RequestId::String(value))
            }
        }
        deserializer.deserialize_any(IdVisitor)
    }
}

/// Define the single closed set of method names understood by this crate.
/// Unknown methods are kept losslessly by [`Frame::UnknownMethod`].
macro_rules! define_methods {
    ($( $variant:ident => $wire_name:literal, )+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub enum Method {
            $( $variant, )+
        }

        impl Method {
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $wire_name, )+
                }
            }

            fn from_str(value: &str) -> Option<Self> {
                match value {
                    $( $wire_name => Some(Self::$variant), )+
                    _ => None,
                }
            }

            /// Convert a method name from a wire frame when it belongs to the known set.
            #[must_use]
            pub fn from_wire(value: &str) -> Option<Self> {
                Self::from_str(value)
            }

            /// Return every method name the runtime currently understands.
            #[must_use]
            pub const fn all() -> &'static [Self] {
                &[$(Self::$variant),+]
            }
        }
    };
}

define_methods! {
    Initialize => "initialize",
    Initialized => "initialized",
    Ack => "ack",
    AccountLoginCancel => "account/login/cancel",
    AccountLoginStart => "account/login/start",
    AccountRateLimitResetCreditConsume => "account/rateLimitResetCredit/consume",
    ThreadLoadedList => "thread/loaded/list",
    ConfigRead => "config/read",
    SkillsExtraRootsSet => "skills/extraRoots/set",
    ModelList => "model/list",
    AccountRead => "account/read",
    AccountRateLimitsRead => "account/rateLimits/read",
    ThreadStart => "thread/start",
    ThreadRealtimeStart => "thread/realtime/start",
    ThreadRealtimeStop => "thread/realtime/stop",
    ThreadRealtimeAppendSpeech => "thread/realtime/appendSpeech",
    ThreadRealtimeItemStarted => "thread/realtime/item/started",
    ThreadRealtimeTranscriptDelta => "thread/realtime/transcript/delta",
    ThreadRealtimeTranscriptDone => "thread/realtime/transcript/done",
    ThreadRealtimeSdp => "thread/realtime/sdp",
    ThreadRealtimeError => "thread/realtime/error",
    ThreadRealtimeClosed => "thread/realtime/closed",
    ThreadResume => "thread/resume",
    ThreadRead => "thread/read",
    ThreadTurnsList => "thread/turns/list",
    TurnStart => "turn/start",
    TurnSteer => "turn/steer",
    TurnInterrupt => "turn/interrupt",
    ThreadList => "thread/list",
    ThreadArchive => "thread/archive",
    ThreadFork => "thread/fork",
    ThreadNameSet => "thread/name/set",
    ThreadMetadataUpdate => "thread/metadata/update",
    ThreadSettingsUpdate => "thread/settings/update",
    ThreadCompactStart => "thread/compact/start",
    ThreadRollback => "thread/rollback",
    ThreadItemsList => "thread/items/list",
    ThreadGoalSet => "thread/goal/set",
    ThreadGoalUpdated => "thread/goal/updated",
    ThreadUnarchive => "thread/unarchive",
    ThreadUnsubscribe => "thread/unsubscribe",
    ProcessSpawn => "process/spawn",
    ProcessWriteStdin => "process/writeStdin",
    ProcessKill => "process/kill",
    ProcessResizePty => "process/resizePty",
    CommandExec => "command/exec",
    CommandExecWrite => "command/exec/write",
    CommandExecResize => "command/exec/resize",
    CommandExecTerminate => "command/exec/terminate",
    ThreadBackgroundTerminalsList => "thread/backgroundTerminals/list",
    ThreadBackgroundTerminalsTerminate => "thread/backgroundTerminals/terminate",
    ThreadBackgroundTerminalsClean => "thread/backgroundTerminals/clean",
    ThreadCommandExecutionBackground => "thread/commandExecution/background",
    ThreadQueueList => "thread/queue/list",
    ClaudeState => "claude/state",
    ClaudeRepairReport => "claude/repairReport",
    ClaudeFileBackups => "claude/fileBackups",
    ClaudeSettings => "claude/settings",
    ClaudeActivity => "claude/activity",
    ClaudeStopTask => "claude/stopTask",
    ClaudeDiagnostics => "claude/diagnostics",
    ClaudeMoveVersions => "claude/moveVersions",
    McpServerReconnect => "mcpServer/reconnect",
    McpServerStatusList => "mcpServerStatus/list",
    SkillsList => "skills/list",
    OperationStatus => "operationStatus",
    ReviewStart => "review/start",
    BrowserAnswer => "browser.answer",
    BrowserCallback => "browser.callback",
    BrowserClose => "browser.close",
    BrowserWait => "browser.wait",
    ClaudeCommands => "claude/commands",
    ClaudeMoveExport => "claude/moveExport",
    ClaudeMoveIdentity => "claude/moveIdentity",
    ClaudeMovePreflight => "claude/movePreflight",
    ClaudeMoveProof => "claude/moveProof",
    Detach => "detach",
    EventRead => "eventRead",
    Exec => "exec",
    Generate => "generate",
    Health => "health",
    Info => "info",
    LayrHealth => "layr.health",
    Next => "next",
    Open => "open",
    ProjectEnsure => "project.ensure",
    ProjectExec => "project.exec",
    ProjectImport => "project.import",
    ProviderList => "provider.list",
    Replay => "replay",
    ResponseStatus => "responseStatus",
    RetireCommand => "retireCommand",
    SecretsList => "secrets.list",
    SecretsRemove => "secrets.remove",
    SecretsSet => "secrets.set",
    ShareConfigure => "share.configure",
    ShareName => "share.name",
    ShareStatus => "share.status",
    Sign => "sign",
    Status => "status",
    SyncMacApply => "sync.mac.apply",
    SyncMacHandoff => "sync.mac.handoff",
    SyncMacHashes => "sync.mac.hashes",
    SyncMacHead => "sync.mac.head",
    SyncMacRead => "sync.mac.read",
    SyncMacWorktree => "sync.mac.worktree",
    VaultConfigure => "vault.configure",
    VaultPush => "vault.push",
    VaultStatus => "vault.status",
    UploadBegin => "upload.begin",
    UploadChunk => "upload.chunk",
    UploadCommit => "upload.commit",
    Verify => "verify",
    Write => "write",
    CurrentTimeRead => "currentTime/read",
    ItemToolCall => "item/tool/call",
    ItemCommandExecutionRequestApproval => "item/commandExecution/requestApproval",
    ItemFileChangeRequestApproval => "item/fileChange/requestApproval",
    ItemPermissionsRequestApproval => "item/permissions/requestApproval",
    McpServerElicitationRequest => "mcpServer/elicitation/request",
    ItemToolRequestUserInput => "item/tool/requestUserInput",
    ItemStarted => "item/started",
    ItemCompleted => "item/completed",
    ItemAgentMessageDelta => "item/agentMessage/delta",
    ItemReasoningTextDelta => "item/reasoning/textDelta",
    ItemCommandExecutionOutputDelta => "item/commandExecution/outputDelta",
    CommandExecOutputDelta => "command/exec/outputDelta",
    TurnStarted => "turn/started",
    TurnCompleted => "turn/completed",
    TurnPlanUpdated => "turn/plan/updated",
    TurnDiffUpdated => "turn/diff/updated",
    ThreadClosed => "thread/closed",
    ThreadStatusChanged => "thread/status/changed",
    ThreadTokenUsageUpdated => "thread/tokenUsage/updated",
    AccountLoginCompleted => "account/login/completed",
    AccountUpdated => "account/updated",
    AccountRateLimitsUpdated => "account/rateLimits/updated",
    ModelSafetyBufferingUpdated => "model/safetyBuffering/updated",
    ProviderGenerationStarted => "provider/generationStarted",
    ProtocolError => "error",
}

/// Provider response payload. Error bodies are kept as JSON because provider
/// error fields vary by CLI release.
#[derive(Clone, Debug, PartialEq)]
pub enum ResponseBody {
    Result(Value),
    Error(Value),
}

/// A recognized request frame. `fields` retains extension fields other than
/// `id`, `method`, and `params`.
#[derive(Clone, Debug, PartialEq)]
pub struct RequestFrame {
    pub id: RequestId,
    pub method: Method,
    pub params: Option<Value>,
    pub fields: Map<String, Value>,
}

/// A recognized notification frame without a request ID.
#[derive(Clone, Debug, PartialEq)]
pub struct NotificationFrame {
    pub method: Method,
    pub params: Option<Value>,
    pub fields: Map<String, Value>,
}

/// An unrecognized method, retaining the complete JSON object unchanged in
/// meaning so callers can forward newer provider messages.
#[derive(Clone, Debug, PartialEq)]
pub struct UnknownMethodFrame {
    pub method: String,
    pub fields: Map<String, Value>,
}

/// One decoded JSON-RPC line.
#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    Request(RequestFrame),
    Notification(NotificationFrame),
    UnknownMethod(UnknownMethodFrame),
    Response {
        id: RequestId,
        body: ResponseBody,
        fields: Map<String, Value>,
    },
}

/// The decoded contents of one supervisor stdout journal entry.
#[derive(Debug, PartialEq)]
pub enum DecodedJournalPayload {
    Frame(Frame),
    /// The payload was not a protocol frame. Its bytes and decoding error stay
    /// attached to the journal cursor so a caller can settle or expose it.
    RawStdout {
        bytes: Vec<u8>,
        error: CodecError,
    },
}

/// A decoded stdout record, retaining the journal identity on both paths.
#[derive(Debug, PartialEq)]
pub struct DecodedJournalEntry {
    pub cursor: u64,
    pub generation: u64,
    pub payload: DecodedJournalPayload,
}

#[derive(Debug, Error, PartialEq)]
pub enum CodecError {
    #[error("invalid JSON frame: {0}")]
    MalformedJson(String),
    #[error("invalid protocol frame: {0}")]
    MalformedFrame(&'static str),
    #[error("protocol frame exceeds {limit} bytes")]
    FrameTooLarge { limit: usize },
    #[error("stream ended with {buffered} bytes of an incomplete frame")]
    TruncatedFrame { buffered: usize },
    #[error("cannot serialize protocol frame: {0}")]
    Serialization(String),
}

/// Incremental newline-delimited JSON decoder shared by both stdio clients.
#[derive(Default)]
pub struct LineDecoder {
    buffer: Vec<u8>,
}

impl LineDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, CodecError> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        let mut consumed = 0;
        let mut decode_error = None;
        while let Some(relative_end) = self.buffer[consumed..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let end = consumed + relative_end;
            let frame_end = end + 1;
            if frame_payload_bytes(&self.buffer[consumed..frame_end]) > MAX_FRAME_BYTES {
                consumed = frame_end;
                decode_error = Some(CodecError::FrameTooLarge {
                    limit: MAX_FRAME_BYTES,
                });
                break;
            }
            let start = consumed;
            consumed = end + 1;
            let mut line_end = end;
            if line_end > start && self.buffer[line_end - 1] == b'\r' {
                line_end -= 1;
            }
            match decode_frame(&self.buffer[start..line_end]) {
                Ok(frame) => frames.push(frame),
                Err(error) => {
                    decode_error = Some(error);
                    break;
                }
            }
        }
        if consumed > 0 {
            let remaining = self.buffer.len() - consumed;
            self.buffer.copy_within(consumed.., 0);
            self.buffer.truncate(remaining);
        }
        if let Some(error) = decode_error {
            return Err(error);
        }
        if frame_prefix_payload_bytes(&self.buffer) > MAX_FRAME_BYTES {
            return Err(CodecError::FrameTooLarge {
                limit: MAX_FRAME_BYTES,
            });
        }
        Ok(frames)
    }

    pub fn finish(&mut self) -> Result<(), CodecError> {
        if self.buffer.is_empty() {
            Ok(())
        } else if frame_payload_bytes(&self.buffer) > MAX_FRAME_BYTES {
            Err(CodecError::FrameTooLarge {
                limit: MAX_FRAME_BYTES,
            })
        } else {
            Err(CodecError::TruncatedFrame {
                buffered: self.buffer.len(),
            })
        }
    }
}

fn decode_frame(line: &[u8]) -> Result<Frame, CodecError> {
    serde_json::from_slice::<WireObject<'_>>(line)
        .map_err(|error| CodecError::MalformedJson(error.to_string()))?
        .into_frame()
}

/// One provider wire object. Serde splits the envelope from the extension
/// members while it parses, so a frame is built from a single JSON tree with
/// no member clones or removals.
#[derive(Deserialize)]
struct WireObject<'a> {
    #[serde(borrow)]
    method: Option<Cow<'a, str>>,
    id: Option<RequestId>,
    #[serde(default, deserialize_with = "present")]
    params: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    result: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    error: Option<Value>,
    #[serde(flatten)]
    fields: Map<String, Value>,
}

/// Keep an explicit JSON `null` member as `Some(Value::Null)`; only an absent
/// member is `None`, so a frame re-encodes to the object it was decoded from.
fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

impl WireObject<'_> {
    fn into_frame(self) -> Result<Frame, CodecError> {
        let Self {
            method,
            id,
            params,
            result,
            error,
            mut fields,
        } = self;
        let mut keep = |key: &str, value: Option<Value>| {
            if let Some(value) = value {
                fields.insert(key.to_owned(), value);
            }
        };
        let Some(method) = method else {
            let id = id.ok_or(CodecError::MalformedFrame(
                "expected a method or response id",
            ))?;
            keep("params", params);
            let body = match (error, result) {
                (Some(error), _) => ResponseBody::Error(error),
                (None, result) => {
                    ResponseBody::Result(result.unwrap_or_else(|| Value::Object(Map::new())))
                }
            };
            return Ok(Frame::Response { id, body, fields });
        };
        keep("result", result);
        keep("error", error);
        let Some(known) = Method::from_str(&method) else {
            keep("params", params);
            keep("id", id.map(Value::from));
            keep("method", Some(Value::String(method.clone().into_owned())));
            return Ok(Frame::UnknownMethod(UnknownMethodFrame {
                method: method.into_owned(),
                fields,
            }));
        };
        Ok(match id {
            Some(id) => Frame::Request(RequestFrame {
                id,
                method: known,
                params,
                fields,
            }),
            None => Frame::Notification(NotificationFrame {
                method: known,
                params,
                fields,
            }),
        })
    }
}

impl From<RequestId> for Value {
    fn from(id: RequestId) -> Self {
        match id {
            RequestId::Number(number) => Self::from(number),
            RequestId::String(text) => Self::String(text),
        }
    }
}

/// Decode a supervisor event payload that arrived already parsed with the
/// same wire schema as stdout lines, without re-encoding it. Only a malformed
/// payload is encoded, to keep its bytes for the journal.
pub fn decode_journal_value(cursor: u64, generation: u64, payload: &Value) -> DecodedJournalEntry {
    let decoded = WireObject::deserialize(payload)
        .map_err(|error| CodecError::MalformedJson(error.to_string()))
        .and_then(WireObject::into_frame);
    let payload = match decoded {
        Ok(frame) => DecodedJournalPayload::Frame(frame),
        Err(error) => DecodedJournalPayload::RawStdout {
            bytes: serde_json::to_vec(payload).unwrap_or_default(),
            error,
        },
    };
    DecodedJournalEntry {
        cursor,
        generation,
        payload,
    }
}

/// Decode a complete supervisor stdout journal record without losing its
/// cursor when its payload is not valid protocol JSON.
pub fn decode_journal_entry(cursor: u64, generation: u64, bytes: Vec<u8>) -> DecodedJournalEntry {
    let decoded = decode_journal_frame(&bytes);
    let payload = match decoded {
        Ok(frame) => DecodedJournalPayload::Frame(frame),
        Err(error) => DecodedJournalPayload::RawStdout { bytes, error },
    };
    DecodedJournalEntry {
        cursor,
        generation,
        payload,
    }
}

/// Decode a complete stdout record by borrowing its buffer. Malformed bytes are
/// copied into the returned journal entry; valid frame bytes are not retained.
pub fn decode_journal_entry_borrowed(
    cursor: u64,
    generation: u64,
    bytes: &[u8],
) -> DecodedJournalEntry {
    let payload = match decode_journal_frame(bytes) {
        Ok(frame) => DecodedJournalPayload::Frame(frame),
        Err(error) => DecodedJournalPayload::RawStdout {
            bytes: bytes.to_vec(),
            error,
        },
    };
    DecodedJournalEntry {
        cursor,
        generation,
        payload,
    }
}

fn decode_journal_frame(bytes: &[u8]) -> Result<Frame, CodecError> {
    let mut line = bytes;
    if let Some(without_newline) = line.strip_suffix(b"\n") {
        line = without_newline;
        if let Some(without_carriage_return) = line.strip_suffix(b"\r") {
            line = without_carriage_return;
        }
    }
    if frame_payload_bytes(bytes) > MAX_FRAME_BYTES {
        Err(CodecError::FrameTooLarge {
            limit: MAX_FRAME_BYTES,
        })
    } else {
        decode_frame(line)
    }
}

fn serialize_error(error: serde_json::Error) -> CodecError {
    CodecError::Serialization(error.to_string())
}

/// Serialize a frame with the provider stdio newline delimiter.
pub fn encode_line(frame: &Frame) -> Result<Vec<u8>, CodecError> {
    let mut output = Vec::new();
    encode_line_into(frame, &mut output)?;
    Ok(output)
}

/// Serialize into a caller-owned buffer, retaining its allocation for reuse.
pub fn encode_line_into(frame: &Frame, output: &mut Vec<u8>) -> Result<(), CodecError> {
    output.clear();
    let view = FrameView::new(frame);
    if let Err(error) = serde_json::to_writer(&mut *output, &view) {
        output.clear();
        return Err(serialize_error(error));
    }
    if output.len() > MAX_FRAME_BYTES {
        output.clear();
        return Err(CodecError::FrameTooLarge {
            limit: MAX_FRAME_BYTES,
        });
    }
    output.push(b'\n');
    Ok(())
}

#[derive(Clone, Copy)]
enum FrameValue<'a> {
    Json(&'a Value),
    Id(&'a RequestId),
    Text(&'a str),
}

impl Serialize for FrameValue<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Json(value) => value.serialize(serializer),
            Self::Id(value) => value.serialize(serializer),
            Self::Text(value) => serializer.serialize_str(value),
        }
    }
}

struct FrameView<'a> {
    entries: Vec<(&'a str, FrameValue<'a>)>,
}

impl<'a> FrameView<'a> {
    fn new(frame: &'a Frame) -> Self {
        let (fields, overrides): (&Map<String, Value>, Vec<(&str, FrameValue<'a>)>) = match frame {
            Frame::Request(method) => {
                let mut overrides = vec![
                    ("id", FrameValue::Id(&method.id)),
                    ("method", FrameValue::Text(method.method.as_str())),
                ];
                if let Some(params) = &method.params {
                    overrides.push(("params", FrameValue::Json(params)));
                }
                (&method.fields, overrides)
            }
            Frame::Notification(method) => {
                let mut overrides = vec![("method", FrameValue::Text(method.method.as_str()))];
                if let Some(params) = &method.params {
                    overrides.push(("params", FrameValue::Json(params)));
                }
                (&method.fields, overrides)
            }
            Frame::UnknownMethod(unknown) => (&unknown.fields, Vec::new()),
            Frame::Response { id, body, fields } => {
                let mut overrides = vec![("id", FrameValue::Id(id))];
                match body {
                    ResponseBody::Result(value) => {
                        overrides.push(("result", FrameValue::Json(value)))
                    }
                    ResponseBody::Error(value) => {
                        overrides.push(("error", FrameValue::Json(value)))
                    }
                }
                (fields, overrides)
            }
        };
        let mut entries = fields
            .iter()
            .map(|(key, value)| (key.as_str(), FrameValue::Json(value)))
            .collect::<Vec<_>>();
        for (key, value) in overrides {
            match entries.binary_search_by(|(existing, _)| existing.cmp(&key)) {
                Ok(index) => entries[index].1 = value,
                Err(index) => entries.insert(index, (key, value)),
            }
        }
        Self { entries }
    }
}

impl Serialize for FrameView<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.entries.len()))?;
        for (key, value) in &self.entries {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

/// Validate provider initialization parameters before opening a child process.
/// The request ID used here is the shortest wire-valid ID; `SessionState`
/// validates the final generated ID again before it emits the initialize write.
pub fn validate_initialize_params(params: &Value) -> Result<(), CodecError> {
    encode_line(&Frame::Request(RequestFrame {
        id: RequestId::Number(1),
        method: Method::Initialize,
        params: Some(params.clone()),
        fields: Map::new(),
    }))
    .map(|_| ())
}
