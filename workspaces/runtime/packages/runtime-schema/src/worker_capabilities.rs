//! Worker capability contract.
//!
//! Profiles and spawn requests use the five camelCase fields below. Missing or
//! null fields inherit the profile value. In profiles they mean the existing
//! provider default. Empty arrays are explicit: no allowed tools, no additional
//! denied tools, no user MCP servers, or no skill preload, respectively.
//! Spawn overrides must narrow profile limits. Tool patterns contain `*` as the
//! only wildcard. Deny patterns accumulate. MCP server and skill names are exact.
//!
//! The resolved object is persisted as `capabilityProfile` on the worker and sent
//! unchanged as `capabilityProfile` in Claude thread/start and thread/resume.
//! Claude must select only named user MCP configurations, fail before prompt
//! submission if any required server is absent or disconnected, and preload each
//! named skill file. Preflight errors identify the field and server/skill name.
//! `allowedTools` is a restriction, not merely SDK permission preapproval.
//! `disallowedTools` always includes Studio's native Agent prohibition.
//!
//! Studio counts completed provider turns in `completedWorkerTurns`. The cap
//! stops automatic continuation with `worker_max_turns` as `terminalReason`, a
//! visible error message, and the ordinary parent completion event. Only an
//! explicit lead instruction resets the count. SDK maxTurns is not this count.
//! Root agent creation rejects maxTurns because only child workers have this
//! counter. Codex rejects allowedTools, disallowedTools and skills, including
//! empty lists. Codex MCP selection reads the native merged config and marks
//! selected servers required. Codex VM workers reject mcpServers because Studio
//! cannot read their native configuration through the host session.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Limits for one worker. `max_turns` measures completed provider turns.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disallowed_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
}

/// Terminal reason for a worker whose completed turn allowance is exhausted.
pub const TURN_LIMIT_REASON: &str = "worker_max_turns";

/// Fewest completed turns allowed by a worker profile.
pub const MIN_WORKER_TURNS: u32 = 1;
/// The storage type bounds the counter and the shared JSON contract.
pub const MAX_WORKER_TURNS: u32 = u32::MAX;

impl WorkerCapabilities {
    pub fn validate(&self) -> Result<(), String> {
        for (field, values, patterns) in [
            ("allowedTools", &self.allowed_tools, true),
            ("disallowedTools", &self.disallowed_tools, true),
            ("mcpServers", &self.mcp_servers, false),
            ("skills", &self.skills, false),
        ] {
            if let Some(values) = values {
                for value in values {
                    if value.is_empty()
                        || !value.chars().all(|c| {
                            c.is_ascii_alphanumeric()
                                || "_-.:/".contains(c)
                                || (patterns && c == '*')
                        })
                        || value.split('/').any(|part| part == "..")
                    {
                        return Err(format!("Invalid {field} name: {value}"));
                    }
                }
            }
        }
        if self.max_turns.is_some_and(|turns| turns < MIN_WORKER_TURNS) {
            return Err("maxTurns must be a positive integer".into());
        }
        Ok(())
    }
}

/// Property schemas shared by persistence, HTTP, and the managed spawn catalog.
pub fn properties() -> serde_json::Map<String, Value> {
    let mut fields = serde_json::Map::new();
    for field in ["allowedTools", "disallowedTools", "mcpServers", "skills"] {
        let pattern = if matches!(field, "allowedTools" | "disallowedTools") {
            "^[A-Za-z0-9_.:/*-]+$"
        } else {
            "^[A-Za-z0-9_.:/-]+$"
        };
        fields.insert(
            field.into(),
            json!({"anyOf":[
                {"type":"array","items":{"type":"string","minLength":1,"pattern":pattern}},
                {"type":"null"}
            ]}),
        );
    }
    fields.insert(
        "maxTurns".into(),
        json!({"anyOf":[
            {"type":"integer","minimum":MIN_WORKER_TURNS,"maximum":MAX_WORKER_TURNS},
            {"type":"null"}
        ]}),
    );
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_fields_validate_and_preserve_empty_lists() {
        for field in ["allowedTools", "disallowedTools", "mcpServers", "skills"] {
            let empty: WorkerCapabilities = serde_json::from_value(json!({field:[]})).unwrap();
            empty.validate().unwrap();
            assert_eq!(serde_json::to_value(empty).unwrap()[field], json!([]));
            let bad: WorkerCapabilities = serde_json::from_value(json!({field:[""]})).unwrap();
            assert!(bad.validate().unwrap_err().contains(field));
        }
        assert!(serde_json::from_value::<WorkerCapabilities>(json!({"maxTurns":-1})).is_err());
        let zero: WorkerCapabilities = serde_json::from_value(json!({"maxTurns":0})).unwrap();
        assert!(zero.validate().is_err());
        let one: WorkerCapabilities = serde_json::from_value(json!({"maxTurns":1})).unwrap();
        one.validate().unwrap();
    }

    #[test]
    fn capability_schema_copies_match_owner() {
        let openapi: Value =
            serde_json::from_str(include_str!("../../runtime-http/openapi.json")).unwrap();
        let stored: Value =
            serde_json::from_str(include_str!("schemas/persisted-profile-record.schema.json"))
                .unwrap();
        for (field, schema) in properties() {
            assert_eq!(stored["properties"][&field], schema, "persisted {field}");
            for name in [
                "ProfileRecord",
                "ProfileBody",
                "ProfileMutation",
                "CreateAgentRequest",
                "WorkerCapabilitiesDto",
            ] {
                assert_eq!(
                    openapi["components"]["schemas"][name]["properties"][&field], schema,
                    "{name}.{field}"
                );
            }
        }
    }
}
