//! Origin of each `turn/start` or `turn/steer` input item sent to the Claude bridge.
//! `inputOrigin` is Studio provenance, not text supplied by an agent. Only stored
//! `user` events and delegated assignments retain task authority: work decisions,
//! ready work, and followups with a server-stamped senderId. Reports, system
//! followups, context, and monitor output are Studio events. The Human variant
//! means this preserved task authority, not proof of keyboard input.
//! Omission preserves older bridge callers.
//! Codex inputs omit this extension and retain their existing wire shape.
//! `contracts` generates the bridge constants from this owner.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderInputOrigin {
    Human,
    StudioEvent,
}

impl ProviderInputOrigin {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::StudioEvent => "studio_event",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Map<String, serde_json::Value>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}
