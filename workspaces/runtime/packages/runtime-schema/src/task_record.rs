//! Closed persisted task record, retaining opaque recovery evidence.
pub use crate::stored_record::NonnegativeNumber;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct TaskId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TaskKind {
    Command,
    Tool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TaskStatus {
    Starting,
    Approval,
    Pending,
    Unknown,
    Running,
    Completed,
    Failed,
    Interrupted,
    Lost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TaskType {
    CommandExecution,
    DynamicToolCall,
    McpToolCall,
    WebSearch,
    FileChange,
    ContextCompaction,
}

/// Numeric IDs remain numeric because process-control identities depend on their original shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum ProcessId {
    Text(String),
    Signed(i64),
    Unsigned(u64),
}
impl ProcessId {
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text(value) => Some(value),
            Self::Signed(_) | Self::Unsigned(_) => None,
        }
    }
    pub fn normalized(&self) -> String {
        match self {
            Self::Text(value) => value.clone(),
            Self::Signed(value) => value.to_string(),
            Self::Unsigned(value) => value.to_string(),
        }
    }
    pub fn truthy(&self) -> bool {
        match self {
            Self::Text(value) => !value.is_empty(),
            Self::Signed(value) => *value != 0,
            Self::Unsigned(value) => *value != 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskRecoveryHold {
    pub stage: RecoveryHoldStage,
    pub at: f64,
    pub reason: String,
    pub outcome: RecoveryHoldOutcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryHoldStage {
    Held,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryHoldOutcome {
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
struct Opaque(Box<serde_json::value::RawValue>);
impl PartialEq for Opaque {
    fn eq(&self, other: &Self) -> bool {
        self.0.get() == other.0.get()
    }
}
impl TaskType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CommandExecution => "commandExecution",
            Self::DynamicToolCall => "dynamicToolCall",
            Self::McpToolCall => "mcpToolCall",
            Self::WebSearch => "webSearch",
            Self::FileChange => "fileChange",
            Self::ContextCompaction => "contextCompaction",
        }
    }
}
impl From<String> for TaskId {
    fn from(value: String) -> Self {
        Self(value)
    }
}
impl From<&str> for TaskId {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}
impl AsRef<str> for TaskId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(rename = "PersistedTaskRecord")]
#[serde(deny_unknown_fields)]
pub struct TaskRecord {
    id: TaskId,
    agent: String,
    #[serde(rename = "completedAtMs", skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Number>", range(min = 0))]
    pub completed_at_ms: Option<NonnegativeNumber>,
    #[schemars(with = "serde_json::Number", range(min = 0))]
    created: NonnegativeNumber,
    #[serde(rename = "durationMs", skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Number>", range(min = 0))]
    pub duration_ms: Option<NonnegativeNumber>,
    #[serde(rename = "itemId")]
    item_id: String,
    kind: TaskKind,
    #[serde(rename = "outputTruncated", skip_serializing_if = "Option::is_none")]
    pub output_truncated: Option<bool>,
    #[serde(rename = "startedAtMs", skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Number>", range(min = 0))]
    pub started_at_ms: Option<NonnegativeNumber>,
    status: TaskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tail: Option<String>,
    #[serde(rename = "turnId")]
    turn_id: String,
    #[serde(rename = "type")]
    task_type: TaskType,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(rename = "exitCode", default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    interactive: Option<bool>,
    #[serde(
        rename = "outputFile",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub output_file: Option<String>,
    #[serde(
        rename = "outputPreviewOnly",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    output_preview_only: Option<bool>,
    #[serde(
        rename = "ownerEpoch",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub owner_epoch: Option<i64>,
    #[serde(rename = "processId", default, skip_serializing_if = "Option::is_none")]
    pub process_id: Option<ProcessId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(
        rename = "sourceAccountKey",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub source_account_key: Option<String>,
    #[serde(
        rename = "sourceConnectionId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub source_connection_id: Option<String>,
    #[serde(
        rename = "sourceProvider",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub source_provider: Option<crate::ProviderKind>,
    #[serde(
        rename = "sourceThreadId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub source_thread_id: Option<String>,
    #[serde(
        rename = "reattachRecovery",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<Value>")]
    reattach_recovery: Option<Opaque>,
    #[serde(
        rename = "recoveryHold",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    recovery_hold: Option<TaskRecoveryHold>,
}
impl TaskRecord {
    pub fn new(
        agent: String,
        item_id: String,
        turn_id: String,
        created: NonnegativeNumber,
        task_type: TaskType,
        name: String,
    ) -> Self {
        Self {
            id: TaskId(format!("{agent}:{item_id}")),
            agent,
            item_id,
            turn_id,
            created,
            task_type,
            name,
            kind: if task_type == TaskType::CommandExecution {
                TaskKind::Command
            } else {
                TaskKind::Tool
            },
            status: TaskStatus::Running,
            completed_at_ms: None,
            duration_ms: None,
            output_truncated: None,
            started_at_ms: None,
            tail: None,
            arguments: None,
            command: None,
            cwd: None,
            error: None,
            exit_code: None,
            finished: None,
            interactive: None,
            output_file: None,
            output_preview_only: None,
            owner_epoch: None,
            process_id: None,
            query: None,
            server: None,
            tool: None,
            source_account_key: None,
            source_connection_id: None,
            source_provider: None,
            source_thread_id: None,
            reattach_recovery: None,
            recovery_hold: None,
        }
    }
    pub fn id(&self) -> &TaskId {
        &self.id
    }
    pub fn agent(&self) -> &str {
        &self.agent
    }
    pub fn completed_at_ms(&self) -> Option<f64> {
        self.completed_at_ms
            .as_ref()
            .and_then(|n| n.number().as_f64())
    }
    pub fn created(&self) -> f64 {
        // NonnegativeNumber already validates that the number is representable as f64.
        self.created
            .number()
            .as_f64()
            .expect("validated task timestamp")
    }
    pub fn duration_ms(&self) -> Option<f64> {
        self.duration_ms.as_ref().and_then(|n| n.number().as_f64())
    }
    pub fn item_id(&self) -> &str {
        &self.item_id
    }
    pub fn kind(&self) -> TaskKind {
        self.kind
    }
    pub fn output_truncated(&self) -> Option<bool> {
        self.output_truncated
    }
    pub fn started_at_ms(&self) -> Option<f64> {
        self.started_at_ms
            .as_ref()
            .and_then(|n| n.number().as_f64())
    }
    pub fn status(&self) -> TaskStatus {
        self.status
    }
    pub fn tail(&self) -> Option<&str> {
        self.tail.as_deref()
    }
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }
    pub fn task_type(&self) -> TaskType {
        self.task_type
    }
    pub fn duration_json(&self) -> Option<Value> {
        self.duration_ms
            .as_ref()
            .map(|number| Value::Number(number.number().clone()))
    }
    pub fn process_id(&self) -> Option<&ProcessId> {
        self.process_id.as_ref()
    }
    pub fn source_thread_id(&self) -> Option<&str> {
        self.source_thread_id.as_deref()
    }
    pub fn source_account_key(&self) -> Option<&str> {
        self.source_account_key.as_deref()
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn has_recovery_hold(&self) -> bool {
        self.recovery_hold.is_some()
    }
    pub fn set_status(&mut self, value: TaskStatus) {
        self.status = value;
    }
    pub fn set_tail(&mut self, value: String) {
        self.tail = Some(value);
    }
    pub fn set_output_truncated(&mut self, value: bool) {
        self.output_truncated = Some(value);
    }
    pub fn set_finished(&mut self, value: f64) {
        self.finished = Some(value);
    }
    pub fn set_error(&mut self, value: String) {
        self.error = Some(value);
    }
    pub fn set_recovery_hold(&mut self, value: TaskRecoveryHold) {
        self.recovery_hold = Some(value);
    }
    pub fn remove_error(&mut self) {
        self.error = None;
    }
    pub fn remove_recovery_hold(&mut self) {
        self.recovery_hold = None;
    }
}

pub(crate) fn schema() -> Value {
    schemars::schema_for!(TaskRecord).to_value()
}
pub(crate) fn check_schema_parity() -> Result<(), String> {
    let stored: Value =
        serde_json::from_slice(include_bytes!("schemas/persisted-task-record.schema.json"))
            .map_err(|e| e.to_string())?;
    if stored != schema() {
        return Err("persisted task schema must be generated from task_record::TaskRecord".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn stored_task_requires_its_identity_and_lifecycle_fields() {
        let incomplete = r#"{"id":"task","agent":"agent","kind":"command","status":"running"}"#;
        assert!(serde_json::from_str::<super::TaskRecord>(incomplete).is_err());
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn task_schema_matches_closed_shape() {
        check_schema_parity().unwrap();
        let value = json!({"id":"t","agent":"a","created":1,"itemId":"i","turnId":"u","kind":"command","status":"running","type":"commandExecution","name":"commandExecution"});
        let parsed: TaskRecord = serde_json::from_str(&value.to_string()).unwrap();
        assert_eq!(
            parsed.serialize(serde_json::value::Serializer).unwrap(),
            value
        );
        assert!(
            serde_json::from_str::<TaskRecord>(
                &value
                    .to_string()
                    .replace("\"status\":\"running\"", "\"status\":\"future\"")
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<TaskRecord>(
                &value
                    .to_string()
                    .replace("\"id\":\"t\"", "\"unknown\":true,\"id\":\"t\"")
            )
            .is_err()
        );
    }
}
