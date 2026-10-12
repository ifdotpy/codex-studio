use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

/// Largest accepted UTF-8 JSON payload, excluding its newline delimiter.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Numeric Codex IDs and string Claude bridge IDs are distinct on the wire.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(u64),
    String(String),
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
    ThreadQueueList => "thread/queue/list",
    ClaudeState => "claude/state",
    ClaudeSettings => "claude/settings",
    ClaudeActivity => "claude/activity",
    ClaudeStopTask => "claude/stopTask",
    ClaudeDiagnostics => "claude/diagnostics",
    ClaudeMoveVersions => "claude/moveVersions",
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
        while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            if end > MAX_FRAME_BYTES {
                self.buffer.drain(..=end);
                return Err(CodecError::FrameTooLarge {
                    limit: MAX_FRAME_BYTES,
                });
            }
            let mut line = self.buffer.drain(..=end).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            frames.push(decode_frame(&line)?);
        }
        if self.buffer.len() > MAX_FRAME_BYTES {
            return Err(CodecError::FrameTooLarge {
                limit: MAX_FRAME_BYTES,
            });
        }
        Ok(frames)
    }

    pub fn finish(&mut self) -> Result<(), CodecError> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(CodecError::TruncatedFrame {
                buffered: self.buffer.len(),
            })
        }
    }
}

fn decode_frame(line: &[u8]) -> Result<Frame, CodecError> {
    let mut object: Map<String, Value> = serde_json::from_slice(line)
        .map_err(|error| CodecError::MalformedJson(error.to_string()))?;
    let Some(object_method) = object.get("method") else {
        let Some(id_value) = object.remove("id") else {
            return Err(CodecError::MalformedFrame(
                "expected a method or response id",
            ));
        };
        let id = serde_json::from_value(id_value).map_err(|_| {
            CodecError::MalformedFrame("response id must be a string or unsigned integer")
        })?;
        let result = object.remove("result");
        let error = object.remove("error");
        let body = if let Some(value) = error {
            ResponseBody::Error(value)
        } else {
            ResponseBody::Result(result.unwrap_or_else(|| Value::Object(Map::new())))
        };
        return Ok(Frame::Response {
            id,
            body,
            fields: object,
        });
    };
    let Some(method_name) = object_method.as_str() else {
        return Err(CodecError::MalformedFrame("method must be a string"));
    };
    let method_name = method_name.to_owned();
    if let Some(method) = Method::from_str(&method_name) {
        let id = object.remove("id");
        let params = object.remove("params");
        object.remove("method");
        return if let Some(id) = id {
            let id = serde_json::from_value(id).map_err(|_| {
                CodecError::MalformedFrame("request id must be a string or unsigned integer")
            })?;
            Ok(Frame::Request(RequestFrame {
                id,
                method,
                params,
                fields: object,
            }))
        } else {
            Ok(Frame::Notification(NotificationFrame {
                method,
                params,
                fields: object,
            }))
        };
    }
    Ok(Frame::UnknownMethod(UnknownMethodFrame {
        method: method_name,
        fields: object,
    }))
}

fn serialize_error(error: serde_json::Error) -> CodecError {
    CodecError::Serialization(error.to_string())
}

/// Serialize a frame with the provider stdio newline delimiter.
pub fn encode_line(frame: &Frame) -> Result<Vec<u8>, CodecError> {
    let fields = match frame {
        Frame::Request(method) => {
            let mut fields = method.fields.clone();
            fields.insert(
                "id".into(),
                serde_json::to_value(&method.id).map_err(serialize_error)?,
            );
            fields.insert(
                "method".into(),
                Value::String(method.method.as_str().to_owned()),
            );
            if let Some(params) = &method.params {
                fields.insert("params".into(), params.clone());
            }
            fields
        }
        Frame::Notification(method) => {
            let mut fields = method.fields.clone();
            fields.insert(
                "method".into(),
                Value::String(method.method.as_str().to_owned()),
            );
            if let Some(params) = &method.params {
                fields.insert("params".into(), params.clone());
            }
            fields
        }
        Frame::UnknownMethod(unknown) => unknown.fields.clone(),
        Frame::Response { id, body, fields } => {
            let mut fields = fields.clone();
            fields.insert(
                "id".into(),
                serde_json::to_value(id).map_err(serialize_error)?,
            );
            match body {
                ResponseBody::Result(value) => {
                    fields.insert("result".into(), value.clone());
                }
                ResponseBody::Error(value) => {
                    fields.insert("error".into(), value.clone());
                }
            }
            fields
        }
    };
    let mut output = serde_json::to_vec(&fields).map_err(serialize_error)?;
    if output.len() > MAX_FRAME_BYTES {
        return Err(CodecError::FrameTooLarge {
            limit: MAX_FRAME_BYTES,
        });
    }
    output.push(b'\n');
    Ok(output)
}
