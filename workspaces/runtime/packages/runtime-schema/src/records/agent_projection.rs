//! Complete persisted-agent projection, retaining wire presence and opaque extensions.
use crate::json_object::JsonObject;
use crate::queue_command::Field;
use crate::stored_record::StoredJsonInteger;
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProjection {
    pub id: String,
    pub root_id: String,
    pub epoch: RootAgentEpoch,
    pub status: String,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub workspace_mode: Field<StringWorkspaceMode>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub account_key: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub active_tools: Field<Vec<JsonObject<ActiveTool>>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub activity: Field<JsonObject<NotificationStatus>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub agent_archive: Field<JsonObject<AgentArchive>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub approval_policy: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub auto_wake: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub cleaned_image_workspace: Field<BTreeMap<String, Value>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub cleaned_worktree: Field<BTreeMap<String, Value>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub compactions: Field<AgentCounter>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub cwd: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub deleted_at: Field<DeletionMarker>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub events: Field<AgentCounter>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub execution_mode: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub in_flight: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub is_lead: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub model: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub name: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub native_response_turn: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub native_safety_buffering: Field<JsonObject<NotificationStatus>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub native_status: Field<JsonObject<NotificationStatus>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub parent_id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub parked_event: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub sandbox: Field<BTreeMap<String, Value>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub thread_id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub turn_id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub worktree_cleanup: Field<BTreeMap<String, Value>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub worktree_ready: Field<bool>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveTool {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub name: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub r#type: Field<String>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentArchive {
    pub at: Number,
    pub epoch: AgentCounter,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub cleanup_pending: Field<bool>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationStatus {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub at: Field<NonnegativeNumber>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub dismissed: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub phase: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub response_started: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub show_buffering_ui: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub turn_id: Field<String>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum DeletionMarker {
    Boolean(bool),
    Number(Number),
}

/// Root fencing epochs retain the historical integer-only JSON representation.
#[derive(Serialize)]
#[serde(transparent)]
pub struct RootAgentEpoch(AgentCounter);
impl RootAgentEpoch {
    pub fn as_i64(&self) -> i64 {
        self.0.as_i64()
    }
}
impl<'de> Deserialize<'de> for RootAgentEpoch {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let counter = AgentCounter::deserialize(deserializer)?;
        if counter.0.number().is_i64() || counter.0.number().is_u64() {
            Ok(Self(counter))
        } else {
            Err(serde::de::Error::custom(
                "root epoch must use an integer representation",
            ))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct AgentCounter(StoredJsonInteger);
impl AgentCounter {
    /// Original numeric spelling, retained for persisted wire compatibility.
    pub fn as_number(&self) -> &Number {
        self.0.number()
    }
    /// Integer value after safe-range validation.
    pub fn as_i64(&self) -> i64 {
        // AgentCounter accepts only finite JSON numbers within the safe-integer range.
        self.0.number().as_f64().expect("finite agent counter") as i64
    }
}
impl<'de> Deserialize<'de> for AgentCounter {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let number = StoredJsonInteger::deserialize(deserializer)?;
        if number.number().as_f64().is_some_and(|value| {
            (0.0..=crate::shared_values::MAX_SAFE_INTEGER as f64).contains(&value)
        }) {
            Ok(Self(number))
        } else {
            Err(serde::de::Error::custom("integer outside safe range"))
        }
    }
}

#[derive(Serialize)]
#[serde(transparent)]
pub struct NonnegativeNumber(Number);
impl<'de> Deserialize<'de> for NonnegativeNumber {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let number = Number::deserialize(deserializer)?;
        if number.as_f64().is_some_and(|value| value >= 0.0) {
            Ok(Self(number))
        } else {
            Err(serde::de::Error::custom("number must be nonnegative"))
        }
    }
}

#[derive(Serialize)]
#[serde(transparent)]
pub struct StringWorkspaceMode(super::WorkspaceMode);
impl<'de> Deserialize<'de> for StringWorkspaceMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::IntoDeserializer;
        let text = String::deserialize(deserializer)?;
        super::WorkspaceMode::deserialize(text.into_deserializer()).map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projection_matches_schema_and_preserves_wire_values() {
        let base = json!({"id":"a","rootId":"r","epoch":1,"status":"future-status",
            "provider":"future", "titleJob":{"malformed":"opaque"}, "startOutcomeHold":[1,null], "vmLaunch":false,
            "accountKey":null,"activity":{"at":0.0,"phase":null,"dismissed":false,"responseStarted":true,"showBufferingUi":false,"turnId":"turn","vendor":[1]},
            "agentArchive":{"at":2.0,"epoch":1.0,"cleanupPending":null,"vendor":true},
            "activeTools":[{"id":null,"name":"tool","type":"call","vendor":{} }], "workspaceMode":"worktree"});
        let candidates = [
            json!(null),
            json!(true),
            json!(-1),
            json!(1.0),
            json!(1.5),
            json!(crate::shared_values::MAX_SAFE_INTEGER),
            json!(crate::shared_values::MAX_SAFE_INTEGER + 1),
            json!("x"),
            json!([]),
            json!({}),
        ];
        let mut records = vec![
            base.clone(),
            json!([]),
            json!({"id":"a","rootId":"r","epoch":0,"status":"idle"}),
        ];
        for field in [
            "id",
            "rootId",
            "epoch",
            "status",
            "workspaceMode",
            "accountKey",
            "activeTools",
            "activity",
            "agentArchive",
            "approvalPolicy",
            "autoWake",
            "cleanedImageWorkspace",
            "cleanedWorktree",
            "compactions",
            "cwd",
            "deletedAt",
            "events",
            "executionMode",
            "inFlight",
            "isLead",
            "model",
            "name",
            "nativeResponseTurn",
            "nativeSafetyBuffering",
            "nativeStatus",
            "parentId",
            "parkedEvent",
            "sandbox",
            "threadId",
            "turnId",
            "worktreeCleanup",
            "worktreeReady",
        ] {
            for candidate in &candidates {
                let mut record = base.clone();
                record[field] = candidate.clone();
                records.push(record);
            }
        }
        for pointer in [
            "/activity/at",
            "/activity/dismissed",
            "/activity/phase",
            "/activity/responseStarted",
            "/activity/showBufferingUi",
            "/activity/turnId",
            "/agentArchive/at",
            "/agentArchive/epoch",
            "/agentArchive/cleanupPending",
            "/activeTools/0/id",
            "/activeTools/0/name",
            "/activeTools/0/type",
        ] {
            for candidate in &candidates {
                let mut record = base.clone();
                *record.pointer_mut(pointer).expect("path") = candidate.clone();
                records.push(record);
            }
        }
        for field in ["id", "rootId", "epoch", "status"] {
            let mut record = base.clone();
            record.as_object_mut().expect("object").remove(field);
            records.push(record);
        }
        for field in ["at", "epoch"] {
            let mut record = base.clone();
            record["agentArchive"]
                .as_object_mut()
                .expect("archive")
                .remove(field);
            records.push(record);
        }
        for value in records {
            let valid = crate::first_error("PersistedAgentRecord", &value)
                .expect("schema")
                .is_none()
                && value.get("epoch").and_then(Value::as_i64).is_some();
            let parsed = serde_json::from_value::<JsonObject<AgentProjection>>(value.clone());
            assert_eq!(parsed.is_ok(), valid, "{value}");
            if let Ok(parsed) = parsed {
                assert_eq!(serde_json::to_value(parsed).expect("serialize"), value);
            }
        }
    }

    #[test]
    fn projection_fields_track_persisted_schema() {
        let schema: Value = serde_json::from_str(include_str!(
            "../schemas/persisted-agent-record.schema.json"
        ))
        .expect("schema");
        let expected = "id rootId epoch status workspaceMode accountKey activeTools activity agentArchive approvalPolicy autoWake cleanedImageWorkspace cleanedWorktree compactions cwd deletedAt events executionMode inFlight isLead model name nativeResponseTurn nativeSafetyBuffering nativeStatus parentId parkedEvent sandbox threadId turnId worktreeCleanup worktreeReady".split_whitespace().collect::<std::collections::BTreeSet<_>>();
        for (definition, names) in [
            ("PersistedActiveTool", "id name type"),
            ("PersistedAgentArchive", "at cleanupPending epoch"),
            (
                "PersistedNotificationStatus",
                "at dismissed phase responseStarted showBufferingUi turnId",
            ),
        ] {
            let expected = names
                .split_whitespace()
                .collect::<std::collections::BTreeSet<_>>();
            let actual = schema["$defs"][definition]["properties"]
                .as_object()
                .expect("nested properties");
            assert_eq!(
                actual
                    .keys()
                    .map(String::as_str)
                    .collect::<std::collections::BTreeSet<_>>(),
                expected,
                "{definition}"
            );
        }
        let fields = schema["properties"].as_object().expect("properties");
        assert_eq!(
            fields
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            expected
        );
    }
}
