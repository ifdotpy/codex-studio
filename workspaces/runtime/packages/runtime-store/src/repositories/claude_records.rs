//! Open execution-attempt record for the shared typed table and read-only audit.
use crate::{JsonRecord, RecordKeyMode, RecordQueryShape};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use studio_runtime_schema::claude_permissions::PermissionMode;
use studio_runtime_schema::queue_command::Field;

fn frozen_permission(source: &Map<String, Value>) -> Result<Option<PermissionMode>, ()> {
    let mode = source
        .get("permissionMode")
        .filter(|value| !value.is_null())
        .map(|value| PermissionMode::deserialize(value).map_err(|_| ()))
        .transpose()?;
    // work-1793466115683291392-20: frozen receipts retain their legacy field.
    let legacy = source
        .get("yoloMode")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_bool()
                .map(|enabled| {
                    if enabled {
                        PermissionMode::BypassPermissions
                    } else {
                        PermissionMode::Default
                    }
                })
                .ok_or(())
        })
        .transpose()?;
    if mode.is_some_and(|mode| {
        legacy.is_some_and(|legacy| {
            (mode == PermissionMode::BypassPermissions)
                != (legacy == PermissionMode::BypassPermissions)
        })
    }) {
        return Err(());
    }
    Ok(mode.or(legacy))
}

fn frozen_input_matches(left: &Value, right: &Value) -> bool {
    if left == right {
        return true;
    }
    let (Some(left), Some(right)) = (left.as_object(), right.as_object()) else {
        return false;
    };
    if left.len() != right.len()
        || left
            .iter()
            .any(|(key, value)| key != "source" && right.get(key) != Some(value))
    {
        return false;
    }
    let (Some(left), Some(right)) = (
        left.get("source").and_then(Value::as_object),
        right.get("source").and_then(Value::as_object),
    ) else {
        return false;
    };
    let permission = |key: &&String| matches!(key.as_str(), "permissionMode" | "yoloMode");
    left.iter().filter(|(key, _)| !permission(key)).count()
        == right.iter().filter(|(key, _)| !permission(key)).count()
        && left
            .iter()
            .filter(|(key, _)| !permission(key))
            .all(|(key, value)| right.get(key) == Some(value))
        && matches!((frozen_permission(left), frozen_permission(right)), (Ok(Some(a)), Ok(Some(b))) if a == b)
}
#[derive(Clone, Deserialize, Serialize)]
pub struct ClaudeAttemptRecord {
    id: String,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    events: Field<Vec<String>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    submitted: Field<bool>,
    #[serde(flatten)]
    fields: Map<String, Value>,
}
impl ClaudeAttemptRecord {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn events(&self) -> Option<&[String]> {
        self.events.value().map(Vec::as_slice)
    }
    pub fn submitted(&self) -> Option<bool> {
        self.submitted.value().copied()
    }
    pub fn extension(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }

    // The live typed record canonicalizes absent optional fields and null alike;
    // the original receipt remains unchanged in storage.
    pub fn matches_field(&self, key: &str, value: Option<&Value>) -> bool {
        match key {
            "id" => value.and_then(Value::as_str) == Some(self.id.as_str()),
            "events" => match &self.events {
                Field::Missing => value.is_none(),
                Field::Null => value == Some(&Value::Null),
                Field::Value(events) => value.and_then(Value::as_array).is_some_and(|values| {
                    values.len() == events.len()
                        && values
                            .iter()
                            .zip(events)
                            .all(|(v, e)| v.as_str() == Some(e))
                }),
            },
            "submitted" => match &self.submitted {
                Field::Missing => value.is_none(),
                Field::Null => value == Some(&Value::Null),
                Field::Value(submitted) => value.and_then(Value::as_bool) == Some(*submitted),
            },
            "claudeInputRequest" => match (self.fields.get(key), value) {
                (Some(left), Some(right)) => frozen_input_matches(left, right),
                (left, right) => left.filter(|v| !v.is_null()) == right.filter(|v| !v.is_null()),
            },
            _ => self.fields.get(key).filter(|v| !v.is_null()) == value.filter(|v| !v.is_null()),
        }
    }
    pub fn set_frozen_input(&mut self, value: Value) {
        self.fields.insert("claudeInputRequest".into(), value);
    }
    pub fn set_transport(&mut self, connection_id: Value, supervisor_identity: Value) {
        self.fields.insert("connectionId".into(), connection_id);
        self.fields
            .insert("supervisorIdentity".into(), supervisor_identity);
    }
}
impl JsonRecord for ClaudeAttemptRecord {
    type Id = String;
    const TABLE: &'static str = "runtime_execution_attempts";
    const SCHEMA: &'static str = "PersistedClaudeAttemptRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::None;
    fn record_key(&self) -> Option<&String> {
        Some(&self.id)
    }
    // Remove this override once the record is closed and typed.
    fn encode_stored(&self, _: &rusqlite::Connection, row_id: &str) -> crate::StoreResult<String> {
        crate::records::encode_open_record(self, row_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frozen_input_matches_legacy_and_canonical_permission_modes() {
        let frozen = json!({"id":"attempt","claudeInputRequest":{"input":[],"source":{"yoloMode":true,"cwd":"/project"}}});
        let record = ClaudeAttemptRecord::deserialize(frozen.clone()).unwrap();
        let canonical =
            json!({"input":[],"source":{"permissionMode":"bypassPermissions","cwd":"/project"}});
        assert!(
            studio_runtime_schema::start_attempt::ClaudeInputSource::deserialize(
                &canonical["source"]
            )
            .is_ok()
        );
        assert!(record.matches_field("claudeInputRequest", Some(&canonical)));
        let different = json!({"input":[],"source":{"permissionMode":"default","cwd":"/project"}});
        assert!(!record.matches_field("claudeInputRequest", Some(&different)));
        let unknown = json!({"input":[],"source":{"permissionMode":"unknown","cwd":"/project"}});
        assert!(!record.matches_field("claudeInputRequest", Some(&unknown)));
        let other_input = json!({"input":["changed"],"source":{"permissionMode":"bypassPermissions","cwd":"/project"}});
        assert!(!record.matches_field("claudeInputRequest", Some(&other_input)));
        let conflicting = json!({"input":[],"source":{"permissionMode":"bypassPermissions","yoloMode":false,"cwd":"/project"}});
        assert!(!record.matches_field("claudeInputRequest", Some(&conflicting)));
        assert_eq!(
            serde_json::from_str::<Value>(&serde_json::to_string(&record).unwrap()).unwrap(),
            frozen
        );
    }

    #[test]
    fn claude_attempt_owner_preserves_nullable_receipt_fields_and_extensions() {
        for input in [
            json!({"id":"attempt","future":{"values":[1,null]}}),
            json!({"id":"attempt","events":null,"submitted":null}),
            json!({"id":"attempt","events":[],"submitted":false}),
            json!({"id":"attempt","events":["event"],"submitted":true}),
        ] {
            let record = ClaudeAttemptRecord::deserialize(input.clone()).unwrap();
            let encoded = serde_json::to_string(&record).unwrap();
            let saved: Value = serde_json::from_str(&encoded).unwrap();
            assert_eq!(saved, input);
            for field in ["id", "events", "submitted", "future"] {
                assert!(record.matches_field(field, input.get(field)));
            }
        }
        let receipt: ClaudeAttemptRecord =
            serde_json::from_str(r#"{"id":"attempt","supervisorIdentity":null}"#).unwrap();
        assert!(receipt.matches_field("supervisorIdentity", None));
        assert!(!receipt.matches_field("supervisorIdentity", Some(&json!({"handle":"other"}))));
        let missing: ClaudeAttemptRecord = serde_json::from_str(r#"{"id":"attempt"}"#).unwrap();
        let null: ClaudeAttemptRecord =
            serde_json::from_str(r#"{"id":"attempt","events":null,"submitted":null}"#).unwrap();
        assert!(!missing.matches_field("events", Some(&Value::Null)));
        assert!(!null.matches_field("events", None));
        assert!(!missing.matches_field("submitted", Some(&Value::Null)));
        assert!(!null.matches_field("submitted", None));
    }
}
