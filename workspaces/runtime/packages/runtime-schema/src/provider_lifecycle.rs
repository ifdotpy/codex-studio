//! Read-only lifecycle decisions. Raw provider/history documents remain the
//! receipt and storage owners; an unrecognized status supplies no decision.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Studio terminal outcomes shared by native Codex and the Claude bridge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TerminalTurnStatus {
    Completed,
    Interrupted,
    Failed,
}
impl TerminalTurnStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct TerminalTurn {
    pub id: String,
    pub status: TerminalTurnStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadIdentity {
    pub thread_id: String,
}

pub use crate::codex_generated::{
    CodexThreadActiveFlag, CodexThreadActivity as ThreadActivity, CodexThreadStatus,
    CodexTurnItemsView, CodexTurnStatus,
};

impl CodexTurnStatus {
    pub fn observe(turn: &Value) -> Option<Self> {
        Self::deserialize(turn.get("status")?).ok()
    }
    pub const fn terminal(self) -> Option<TerminalTurnStatus> {
        match self {
            Self::Completed => Some(TerminalTurnStatus::Completed),
            Self::Interrupted => Some(TerminalTurnStatus::Interrupted),
            Self::Failed => Some(TerminalTurnStatus::Failed),
            Self::InProgress => None,
        }
    }
}

impl CodexTurnItemsView {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotLoaded => "notLoaded",
            Self::Summary => "summary",
            Self::Full => "full",
        }
    }
}

/// The shared decision needs only activity, never invented Codex active flags.
/// Claude's mirror status has no activeFlags; it is not a CodexThreadStatus.
impl ThreadActivity {
    pub fn observe(status: &Value) -> Option<Self> {
        Self::deserialize(status).ok()
    }
    pub const fn is_idle(self) -> bool {
        match self {
            Self::Idle | Self::NotLoaded => true,
            Self::SystemError | Self::Active => false,
        }
    }
}

/// A read observation, not a stored thread or a vendor thread replacement.
/// Unknown activity retains the identity but cannot prove idle or active.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadLifecycle<'a> {
    pub thread_id: &'a str,
    pub activity: Option<ThreadActivity>,
}
impl<'a> ThreadLifecycle<'a> {
    pub fn observe(thread: &'a Value) -> Option<Self> {
        Some(Self {
            thread_id: thread.get("id")?.as_str().filter(|id| !id.is_empty())?,
            activity: ThreadActivity::observe(&thread["status"]),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnPhase {
    Running,
    Terminal(TerminalTurnStatus),
}

/// Borrowed identities are the Studio thread and provider turn identities.
/// Claude native session IDs and SDK message UUIDs remain separate fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TurnLifecycle<'a> {
    pub thread_id: &'a str,
    pub turn_id: &'a str,
    pub phase: TurnPhase,
}
impl<'a> TurnLifecycle<'a> {
    pub fn observe(thread_id: &'a str, turn: &'a Value) -> Option<Self> {
        let turn_id = turn.get("id")?.as_str().filter(|id| !id.is_empty())?;
        if thread_id.is_empty() {
            return None;
        }
        let phase = CodexTurnStatus::observe(turn)?
            .terminal()
            .map_or(TurnPhase::Running, TurnPhase::Terminal);
        Some(Self {
            thread_id,
            turn_id,
            phase,
        })
    }
}

/// Capability name of Studio's retained provider lifecycle protocol: boolean proof
/// that the loaded Claude bridge bounds missing steer receipts and reconciles exact
/// input identities before completing a turn.
pub const CLAUDE_STEER_COMPLETION_RECEIPTS: &str = "steerCompletionReceipts";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projection_keeps_raw_payload_and_unknown_outcomes_unsettled() {
        for status in [json!("uncertain"), json!("future"), Value::Null, json!(3)] {
            let turn = json!({"id":"turn","status":status,"future":{"receipt":42}});
            let original = serde_json::to_vec(&turn).expect("receipt");
            assert!(TurnLifecycle::observe("mirror-thread", &turn).is_none());
            assert_eq!(serde_json::to_vec(&turn).expect("receipt"), original);
        }
        let running = json!({"id":"turn","status":"inProgress"});
        assert_eq!(
            TurnLifecycle::observe("mirror-thread", &running)
                .expect("running")
                .phase,
            TurnPhase::Running
        );
        assert!(TurnLifecycle::observe("", &running).is_none());
        assert!(TurnLifecycle::observe("thread", &json!({"status":"completed"})).is_none());
        for status in [
            TerminalTurnStatus::Completed,
            TerminalTurnStatus::Failed,
            TerminalTurnStatus::Interrupted,
        ] {
            let turn = json!({"id":"turn","status":status});
            assert_eq!(
                TurnLifecycle::observe("mirror-thread", &turn),
                Some(TurnLifecycle {
                    thread_id: "mirror-thread",
                    turn_id: "turn",
                    phase: TurnPhase::Terminal(status)
                })
            );
        }
    }

    #[test]
    fn claude_activity_does_not_fabricate_codex_active_flags() {
        let claude = json!({"type":"active"});
        assert_eq!(
            ThreadActivity::observe(&claude),
            Some(ThreadActivity::Active)
        );
        assert!(serde_json::from_value::<CodexThreadStatus>(claude).is_err());
        assert!(ThreadActivity::observe(&json!({"type":"future"})).is_none());
        let thread =
            json!({"id":"mirror-thread","status":{"type":"future"},"nativeId":"native-session"});
        assert_eq!(
            ThreadLifecycle::observe(&thread),
            Some(ThreadLifecycle {
                thread_id: "mirror-thread",
                activity: None
            })
        );
        assert!(
            ThreadActivity::observe(&json!({"type":"idle"}))
                .expect("idle")
                .is_idle()
        );
    }

    #[test]
    fn lifecycle_mirrors_match_selected_codex_generated_schema_leaves() {
        fn normalize(value: &mut Value) {
            if let Some(map) = value.as_object_mut() {
                for key in ["$schema", "title", "description"] {
                    map.remove(key);
                }
                if let Some(value) = map.remove("const") {
                    map.insert("enum".into(), json!([value]));
                }
                if let Some(Value::Array(required)) = map.get_mut("required") {
                    required.sort_by_key(Value::to_string);
                }
                if let Some(Value::Array(variants)) = map.get("oneOf")
                    && variants.iter().all(|v| {
                        v["type"] == "string" && v["enum"].as_array().is_some_and(|v| v.len() == 1)
                    })
                {
                    let values: Vec<_> = variants.iter().map(|v| v["enum"][0].clone()).collect();
                    map.remove("oneOf");
                    map.insert("type".into(), json!("string"));
                    map.insert("enum".into(), json!(values));
                }
                for value in map.values_mut() {
                    normalize(value);
                }
            } else if let Some(values) = value.as_array_mut() {
                for value in values {
                    normalize(value);
                }
            }
        }
        fn schema<T: JsonSchema>() -> Value {
            schemars::generate::SchemaSettings::draft07()
                .into_generator()
                .into_root_schema_for::<T>()
                .to_value()
        }
        let vendor: Value =
            serde_json::from_str(include_str!("schemas/codex/lifecycle.schema.json"))
                .expect("vendor leaves");
        for (name, mut generated) in [
            ("TurnStatus", schema::<CodexTurnStatus>()),
            ("ThreadStatus", schema::<CodexThreadStatus>()),
            ("ThreadActiveFlag", schema::<CodexThreadActiveFlag>()),
            ("TurnItemsView", schema::<CodexTurnItemsView>()),
        ] {
            generated
                .as_object_mut()
                .expect("schema")
                .remove("definitions");
            let mut expected = vendor["definitions"][name].clone();
            normalize(&mut generated);
            normalize(&mut expected);
            assert_eq!(generated, expected, "mirror of vendor leaf {name}");
        }
    }
}
