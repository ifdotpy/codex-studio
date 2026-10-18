//! Pure protocol-v1 admission and journal decisions. No I/O or clock access.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const PROTOCOL: u64 = 1;
pub const HANDLE_LIMIT: u64 = 256 * 1024 * 1024;
pub const MAX_HANDLE_CHARACTERS: usize = 180;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct Refusal(pub &'static str);

/// The provider JSON-RPC message id of one supervisor write: the `nativeId` request field,
/// the `remoteId` result field, and the `operations.native_id` journal column.
///
/// This type owns that contract for every Rust reader and writer. Codex app-server ids are
/// integers. The Claude bridge gives its server requests string ids (`claude:<uuid>`), and a
/// reply to one carries that string. The Python supervisor
/// (`codex_process_supervisor.Child.write`) journals the id it receives without conversion,
/// so one journal can hold integer, text and null ids. Only an integer is an RPC identity
/// that the supervisor renumbers and that `operationStatus` can prove. The parity check is
/// `supervisor_python_written_text_native_ids_stay_exact` in the runtime-services tests.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(untagged)]
pub enum NativeId {
    Integer(i64),
    Text(String),
}
impl NativeId {
    /// The integer RPC identity, or `None` for a text id.
    pub fn rpc(&self) -> Option<i64> {
        match self {
            Self::Integer(id) => Some(*id),
            Self::Text(_) => None,
        }
    }
}
impl From<i64> for NativeId {
    fn from(id: i64) -> Self {
        Self::Integer(id)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handshake {
    pub protocol: u64,
    pub state_dir: String,
    pub backend_id: Option<String>,
    #[serde(default)]
    pub probe: bool,
    #[serde(default)]
    pub operator: bool,
}

#[derive(Default)]
pub struct Ownership {
    owner: Option<String>,
    connections: usize,
}
impl Ownership {
    pub fn attach(&mut self, first: &Handshake, root: &str) -> Result<Option<String>, Refusal> {
        if first.protocol != PROTOCOL || first.state_dir != root {
            return Err(Refusal("Supervisor protocol or state directory mismatch"));
        }
        let owner = if first.probe {
            None
        } else {
            first.backend_id.clone()
        };
        if let Some(id) = &owner {
            if self.connections > 0 && self.owner.as_ref() != Some(id) {
                return Err(Refusal("Another backend is attached to this supervisor"));
            }
            self.owner = Some(id.clone());
            self.connections += 1;
        }
        Ok(owner)
    }
    pub fn detach(&mut self, owner: Option<&str>) {
        if owner.is_some() && self.owner.as_deref() == owner {
            self.connections = self.connections.saturating_sub(1);
            if self.connections == 0 {
                self.owner = None;
            }
        }
    }
}

#[derive(Deserialize)]
pub struct Request {
    #[serde(rename = "requestId")]
    pub request_id: Option<Value>,
    #[serde(flatten)]
    pub command: Command,
}

#[derive(Deserialize)]
#[serde(tag = "action")]
pub enum Command {
    #[serde(rename = "health")]
    Health,
    #[serde(rename = "finishFallback")]
    FinishFallback,
    #[serde(rename = "open")]
    Open {
        handle: String,
        command: Vec<String>,
        env: BTreeMap<String, String>,
        #[serde(deserialize_with = "Option::<String>::deserialize")]
        cwd: Option<String>,
    },
    #[serde(rename = "write")]
    Write {
        handle: String,
        #[serde(rename = "operationId")]
        operation_id: String,
        #[serde(rename = "nativeId")]
        native_id: Option<NativeId>,
        message: serde_json::Map<String, Value>,
    },
    #[serde(rename = "next")]
    Next { handle: String, cursor: u64 },
    #[serde(rename = "replay")]
    Replay { handle: String, cursor: u64 },
    #[serde(rename = "ack")]
    Ack { handle: String, sequence: u64 },
    #[serde(rename = "operationStatus")]
    OperationStatus {
        handle: String,
        #[serde(rename = "operationId")]
        operation_id: String,
    },
    #[serde(rename = "status")]
    Status { handle: String },
    #[serde(rename = "detach")]
    Detach { handle: String },
    #[serde(rename = "retireCommand")]
    RetireCommand { handle: String, generation: u64 },
    #[serde(rename = "adminCloseHandle")]
    AdminCloseHandle {
        handle: String,
        #[serde(rename = "expectedPid")]
        expected_pid: u32,
        #[serde(rename = "expectedStartTime")]
        expected_start_time: String,
        #[serde(rename = "expectedSignature")]
        expected_signature: String,
    },
}
impl Command {
    pub fn handle(&self) -> Option<&str> {
        match self {
            Self::Health | Self::FinishFallback => None,
            Self::Open { handle, .. }
            | Self::Write { handle, .. }
            | Self::Next { handle, .. }
            | Self::Replay { handle, .. }
            | Self::Ack { handle, .. }
            | Self::OperationStatus { handle, .. }
            | Self::Status { handle }
            | Self::Detach { handle }
            | Self::RetireCommand { handle, .. }
            | Self::AdminCloseHandle { handle, .. } => Some(handle),
        }
    }
    pub fn validate(&self) -> Result<(), Refusal> {
        if self
            .handle()
            .is_some_and(|h| h.is_empty() || h.chars().count() > MAX_HANDLE_CHARACTERS)
        {
            return Err(Refusal("Invalid supervisor handle"));
        }
        if let Self::Open { command, .. } = self
            && (command.is_empty() || command.iter().any(|s| s.contains('\0')))
        {
            return Err(Refusal("Invalid supervisor command"));
        }
        Ok(())
    }
}

/// Construct only from checked journal records, including Python-written rows.
#[derive(Deserialize)]
struct HandleRecord {
    sequence: u64,
    acknowledged: u64,
    rpc_sequence: u64,
    generation: u64,
}
pub struct HandleSnapshot {
    record: HandleRecord,
}
impl HandleSnapshot {
    pub fn from_record(record: Value) -> Result<Self, Refusal> {
        let record: HandleRecord = serde_json::from_value(record)
            .map_err(|_| Refusal("Invalid supervisor handles record"))?;
        let snapshot = Self { record };
        if snapshot.record.acknowledged > snapshot.record.sequence
            || snapshot.record.sequence >= i64::MAX as u64
            || snapshot.record.rpc_sequence >= i64::MAX as u64
            || snapshot.record.generation >= i64::MAX as u64
        {
            return Err(Refusal("Invalid supervisor handles sequence range"));
        }
        Ok(snapshot)
    }
    pub fn sequence(&self) -> u64 {
        self.record.sequence
    }
    pub fn acknowledged(&self) -> u64 {
        self.record.acknowledged
    }
    pub fn generation(&self) -> u64 {
        self.record.generation
    }
    pub fn cursor(&self, cursor: u64, ack: bool) -> Result<(), Refusal> {
        if cursor < self.record.acknowledged || cursor > self.record.sequence {
            return Err(Refusal(if ack {
                "Supervisor ACK cursor is outside the saved sequence range"
            } else {
                "Supervisor replay cursor is stale or ahead of the journal"
            }));
        }
        Ok(())
    }
    pub fn retire(
        &self,
        handle: &str,
        generation: u64,
        exited: bool,
        readers_done: bool,
    ) -> Result<(), Refusal> {
        if !handle.starts_with("server-command:") {
            return Err(Refusal(
                "Only completed server command handles can be retired",
            ));
        }
        if self.record.generation != generation
            || !exited
            || !readers_done
            || self.record.sequence != self.record.acknowledged
        {
            return Err(Refusal("The command adapter is not ready for retirement"));
        }
        Ok(())
    }
    pub fn write(
        &self,
        prior: Option<&ReceiptSnapshot>,
        digest: &str,
        native_id: Option<&NativeId>,
        message: &Value,
        buffered: u64,
    ) -> Result<WriteDecision, Refusal> {
        if let Some(prior) = prior {
            if prior.record.digest != digest {
                return Err(Refusal(
                    "Supervisor operation id was reused with different content",
                ));
            }
            return Ok(WriteDecision::Duplicate {
                remote_id: prior.record.native_id.clone(),
            });
        }
        if buffered >= HANDLE_LIMIT {
            return Err(Refusal(
                "Supervisor output journal is full; waiting for backend replay acknowledgement",
            ));
        }
        // Python renumbers only an integer id; a text id is journaled and written unchanged.
        let rpc = message.get("method").is_some() && native_id.and_then(NativeId::rpc).is_some();
        if rpc && self.record.rpc_sequence + 1 >= i64::MAX as u64 {
            return Err(Refusal("Supervisor RPC sequence exhausted"));
        }
        let remote_id = if rpc {
            // The bound above keeps the next sequence inside i64.
            Some(NativeId::Integer((self.record.rpc_sequence + 1) as i64))
        } else {
            native_id.cloned()
        };
        let mut outbound = message.clone();
        if rpc {
            outbound["id"] = serde_json::to_value(&remote_id)
                .map_err(|_| Refusal("Invalid supervisor native id"))?;
        }
        Ok(WriteDecision::Accept {
            remote_id,
            rpc_sequence: if rpc {
                self.record.rpc_sequence + 1
            } else {
                self.record.rpc_sequence
            },
            outbound,
        })
    }
}
#[derive(Deserialize)]
struct ReceiptRecord {
    digest: String,
    native_id: Option<NativeId>,
    generation: Option<u64>,
    response: Option<String>,
    response_sequence: Option<u64>,
}
pub struct ReceiptSnapshot {
    record: ReceiptRecord,
}
impl ReceiptSnapshot {
    pub fn from_record(record: Value) -> Result<Self, Refusal> {
        let record: ReceiptRecord = serde_json::from_value(record)
            .map_err(|_| Refusal("Invalid supervisor operations record"))?;
        if record.generation.is_some_and(|id| id > i64::MAX as u64)
            || record
                .response_sequence
                .is_some_and(|id| id > i64::MAX as u64)
        {
            return Err(Refusal("Invalid supervisor operations integer range"));
        }
        if let Some(raw) = &record.response {
            serde_json::from_str::<Value>(raw)
                .map_err(|_| Refusal("Invalid supervisor operations response"))?;
        }
        Ok(Self { record })
    }
    pub fn status(&self, current: &HandleSnapshot) -> Result<Value, Refusal> {
        use serde_json::json;
        if self.record.generation != Some(current.record.generation) {
            return Ok(json!({"accepted":false,"reason":"native_generation_changed"}));
        }
        // Like Python, only an integer id proves the RPC identity of a monitor request.
        let Some(native_id) = self.record.native_id.as_ref().and_then(NativeId::rpc) else {
            return Ok(json!({"accepted":false,"reason":"rpc_identity_missing"}));
        };
        let response = match (&self.record.response, self.record.response_sequence) {
            (Some(raw), Some(seq)) if seq <= current.record.acknowledged => {
                serde_json::from_str(raw)
                    .map_err(|_| Refusal("Invalid supervisor operations response"))?
            }
            _ => Value::Null,
        };
        Ok(json!({"accepted":true,"nativeId":native_id,"response":response}))
    }
}
pub enum WriteDecision {
    Duplicate {
        remote_id: Option<NativeId>,
    },
    Accept {
        remote_id: Option<NativeId>,
        rpc_sequence: u64,
        outbound: Value,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum StartMatch {
    Gone,
    Exact,
    Legacy,
    Different,
}

/// Prefixes of the kernel start identities the Rust supervisor records:
/// `linux:<boot_id>:<start ticks>` and `macos:<boot session uuid>:<secs>.<usecs>`.
/// Any other recorded value is a legacy Python token.
pub const NATIVE_START_PREFIXES: [&str; 2] = ["linux:", "macos:"];

/// Whether a recorded start identity is a native kernel identity that may
/// authorize a signal, as opposed to a legacy token that needs an upgrade first.
pub fn is_native_start(value: &str) -> bool {
    NATIVE_START_PREFIXES
        .iter()
        .any(|prefix| value.starts_with(prefix))
}

/// Legacy equality permits one durable upgrade; only kernel identity authorizes signals.
pub fn compare_start(expected: &str, actual: Option<&str>, legacy: Option<&str>) -> StartMatch {
    let Some(actual) = actual else {
        return StartMatch::Gone;
    };
    if is_native_start(expected) {
        if expected == actual {
            StartMatch::Exact
        } else {
            StartMatch::Different
        }
    } else if legacy == Some(expected) {
        StartMatch::Legacy
    } else {
        StartMatch::Different
    }
}

#[cfg(test)]
mod start_identity_tests {
    use super::{StartMatch, compare_start, is_native_start};

    const MAC: &str = "macos:1CCC2169-B973-447A-B702-3F226EECA807:1790895321.190147";

    #[test]
    fn native_tokens_match_only_exactly() {
        assert!(is_native_start(MAC));
        assert!(is_native_start("linux:boot:12"));
        assert!(!is_native_start("1790895321.190147"));
        assert_eq!(compare_start(MAC, Some(MAC), None), StartMatch::Exact);
        let other = "macos:1CCC2169-B973-447A-B702-3F226EECA807:1790895321.190148";
        assert_eq!(compare_start(MAC, Some(other), None), StartMatch::Different);
        // A matching legacy value never upgrades a native expectation.
        assert_eq!(
            compare_start(MAC, Some(other), Some(MAC)),
            StartMatch::Different
        );
        assert_eq!(compare_start(MAC, None, None), StartMatch::Gone);
    }

    #[test]
    fn python_macos_token_is_legacy_only_when_the_live_value_matches() {
        let python = "1790895321.190147";
        assert_eq!(
            compare_start(python, Some(MAC), Some("1790895321.190147")),
            StartMatch::Legacy
        );
        assert_eq!(
            compare_start(python, Some(MAC), Some("1790895321.190148")),
            StartMatch::Different
        );
        assert_eq!(
            compare_start(python, Some(MAC), None),
            StartMatch::Different
        );
    }
}
