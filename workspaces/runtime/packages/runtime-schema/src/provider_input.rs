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

/// Numeric Codex IDs and string Claude bridge IDs are distinct on the wire.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, schemars::JsonSchema)]
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

impl From<RequestId> for serde_json::Value {
    fn from(id: RequestId) -> Self {
        match id {
            RequestId::Number(number) => Self::from(number),
            RequestId::String(text) => Self::String(text),
        }
    }
}
