//! Fresh Claude bridge output only. Historical session items remain open JSON.
//! Native tool arguments, results and compaction metadata belong to the SDK;
//! this contract owns the Studio envelope without copying those vendor shapes.

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Map, Value};

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum FreshToolStatus {
    InProgress,
    Completed,
    Failed,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FreshAgentPhase {
    Commentary,
    FinalAnswer,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FreshInputDeliveryStatus {
    Preparing,
    Submitted,
    Delivered,
    NotApplied,
    Cancelled,
}

#[derive(Serialize, JsonSchema)]
pub enum FreshInputDelivery {
    #[serde(rename = "steer")]
    Steer,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FreshToolFields {
    pub id: String,
    pub status: FreshToolStatus,
    pub tool: String,
    pub arguments: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "FreshToolResult")]
    pub result: Option<FreshToolResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "f64")]
    pub elapsed_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "bool")]
    pub stalled: Option<bool>,
}

#[derive(Serialize, JsonSchema)]
pub struct FreshToolResult {
    // A tool result can omit content; preserve that omission rather than null.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Value")]
    pub content: Option<Value>,
}

#[derive(Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum FreshFileChangeKind {
    Add,
    Update,
}

#[derive(Serialize, JsonSchema)]
pub struct FreshFileChange {
    // Unvalidated native tool payloads are forwarded without invented strings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<Value>,
    pub kind: FreshFileChangeKind,
    pub diff: Value,
}

#[derive(Serialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum FreshItem {
    UserMessage {
        id: String,
        content: Vec<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        native_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "Vec<String>")]
        native_ids: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        native_submission_attempt_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "FreshInputDelivery")]
        delivery: Option<FreshInputDelivery>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "FreshInputDeliveryStatus")]
        delivery_status: Option<FreshInputDeliveryStatus>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        client_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        error: Option<String>,
    },
    AgentMessage {
        id: String,
        text: String,
        phase: FreshAgentPhase,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        native_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "bool")]
        thinking: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "bool")]
        native_subagent: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "bool")]
        studio_notice: Option<bool>,
    },
    CommandExecution {
        #[serde(flatten)]
        tool_fields: FreshToolFields,
        #[serde(skip_serializing_if = "Option::is_none")]
        command: Option<Value>,
        cwd: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        aggregated_output: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<Option<i32>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "bool")]
        can_move_to_background: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        background_task_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "bool")]
        backgrounded_by_user: Option<bool>,
    },
    FileChange {
        #[serde(flatten)]
        tool_fields: FreshToolFields,
        changes: Vec<FreshFileChange>,
    },
    McpToolCall {
        #[serde(flatten)]
        tool_fields: FreshToolFields,
        server: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        output_file: Option<String>,
    },
    ContextCompaction {
        id: String,
        status: FreshCompactionStatus,
        compaction_metadata: Value,
    },
}

#[derive(Serialize, JsonSchema)]
pub enum FreshCompactionStatus {
    #[serde(rename = "completed")]
    Completed,
}
