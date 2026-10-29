//! Open execution-attempt record for the shared typed table and read-only audit.
use crate::{JsonRecord, RecordKeyMode, RecordQueryShape};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use studio_runtime_schema::queue_command::Field;
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

    // Compare both presence and value: omission and explicit null are distinct
    // receipt evidence even though neither is a usable event list or bool.
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
            _ => self.fields.get(key) == value,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        let missing: ClaudeAttemptRecord = serde_json::from_str(r#"{"id":"attempt"}"#).unwrap();
        let null: ClaudeAttemptRecord =
            serde_json::from_str(r#"{"id":"attempt","events":null,"submitted":null}"#).unwrap();
        assert!(!missing.matches_field("events", Some(&Value::Null)));
        assert!(!null.matches_field("events", None));
        assert!(!missing.matches_field("submitted", Some(&Value::Null)));
        assert!(!null.matches_field("submitted", None));
    }
}
