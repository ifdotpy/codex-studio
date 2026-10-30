//! Checked persisted task shell, with open extension data retained for recovery evidence.
use crate::{queue_command::Field, records::agent_projection::NonnegativeNumber};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct TaskId(pub String);
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
pub struct TaskRecord {
    id: TaskId,
    #[serde(rename = "agent", default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    agent: Field<String>,
    #[serde(
        rename = "completedAtMs",
        default,
        skip_serializing_if = "Field::is_missing"
    )]
    #[schemars(with = "Option<serde_json::Number>", range(min = 0))]
    completed_at_ms: Field<NonnegativeNumber>,
    #[serde(rename = "created", default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<serde_json::Number>", range(min = 0))]
    created: Field<NonnegativeNumber>,
    #[serde(
        rename = "durationMs",
        default,
        skip_serializing_if = "Field::is_missing"
    )]
    #[schemars(with = "Option<serde_json::Number>", range(min = 0))]
    duration_ms: Field<NonnegativeNumber>,
    #[serde(rename = "itemId", default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    item_id: Field<String>,
    #[serde(rename = "kind", default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    kind: Field<String>,
    #[serde(
        rename = "outputTruncated",
        default,
        skip_serializing_if = "Field::is_missing"
    )]
    #[schemars(with = "Option<bool>")]
    output_truncated: Field<bool>,
    #[serde(
        rename = "startedAtMs",
        default,
        skip_serializing_if = "Field::is_missing"
    )]
    #[schemars(with = "Option<serde_json::Number>", range(min = 0))]
    started_at_ms: Field<NonnegativeNumber>,
    #[serde(rename = "status", default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    status: Field<String>,
    #[serde(rename = "tail", default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    tail: Field<String>,
    #[serde(rename = "turnId", default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    turn_id: Field<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    task_type: Field<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}
impl TaskRecord {
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        Self::deserialize(value)
    }
    pub fn id(&self) -> &TaskId {
        &self.id
    }
    pub fn agent(&self) -> Option<&str> {
        self.agent.value().map(String::as_str)
    }
    pub fn completed_at_ms(&self) -> Option<f64> {
        self.completed_at_ms
            .value()
            .and_then(|n| n.number().as_f64())
    }
    pub fn created(&self) -> Option<f64> {
        self.created.value().and_then(|n| n.number().as_f64())
    }
    pub fn duration_ms(&self) -> Option<f64> {
        self.duration_ms.value().and_then(|n| n.number().as_f64())
    }
    pub fn item_id(&self) -> Option<&str> {
        self.item_id.value().map(String::as_str)
    }
    pub fn kind(&self) -> Option<&str> {
        self.kind.value().map(String::as_str)
    }
    pub fn output_truncated(&self) -> Option<bool> {
        self.output_truncated.value().copied()
    }
    pub fn started_at_ms(&self) -> Option<f64> {
        self.started_at_ms.value().and_then(|n| n.number().as_f64())
    }
    pub fn status(&self) -> Option<&str> {
        self.status.value().map(String::as_str)
    }
    pub fn tail(&self) -> Option<&str> {
        self.tail.value().map(String::as_str)
    }
    pub fn turn_id(&self) -> Option<&str> {
        self.turn_id.value().map(String::as_str)
    }
    pub fn task_type(&self) -> Option<&str> {
        self.task_type.value().map(String::as_str)
    }
    pub fn duration_json(&self) -> Option<Value> {
        match &self.duration_ms {
            Field::Missing => None,
            Field::Null => Some(Value::Null),
            Field::Value(number) => Some(Value::Number(number.number().clone())),
        }
    }
    pub fn extra(&self, key: &str) -> Option<&Value> {
        self.extra.get(key)
    }
    pub fn set_status(&mut self, value: &str) {
        self.status = Field::Value(value.into());
    }
    pub fn set_tail(&mut self, value: String) {
        self.tail = Field::Value(value);
    }
    pub fn set_output_truncated(&mut self, value: bool) {
        self.output_truncated = Field::Value(value);
    }
    pub fn set_finished(&mut self, value: Value) {
        self.extra.insert("finished".into(), value);
    }
    pub fn set_error(&mut self, value: Value) {
        self.extra.insert("error".into(), value);
    }
    pub fn set_recovery_hold(&mut self, value: Value) {
        self.extra.insert("recoveryHold".into(), value);
    }
    pub fn remove_error(&mut self) {
        self.extra.remove("error");
    }
    pub fn remove_recovery_hold(&mut self) {
        self.extra.remove("recoveryHold");
    }
    pub fn value(&self) -> Value {
        self.serialize(serde_json::value::Serializer)
            .expect("task fields are JSON serializable")
    }
    pub fn into_value(self) -> Value {
        let mut fields: serde_json::Map<String, Value> = self.extra.into_iter().collect();
        fields.insert("id".into(), Value::String(self.id.0));
        move_field(&mut fields, "agent", self.agent, Value::String);
        move_field(&mut fields, "completedAtMs", self.completed_at_ms, |n| {
            Value::Number(n.number().clone())
        });
        move_field(&mut fields, "created", self.created, |n| {
            Value::Number(n.number().clone())
        });
        move_field(&mut fields, "durationMs", self.duration_ms, |n| {
            Value::Number(n.number().clone())
        });
        move_field(&mut fields, "itemId", self.item_id, Value::String);
        move_field(&mut fields, "kind", self.kind, Value::String);
        move_field(
            &mut fields,
            "outputTruncated",
            self.output_truncated,
            Value::Bool,
        );
        move_field(&mut fields, "startedAtMs", self.started_at_ms, |n| {
            Value::Number(n.number().clone())
        });
        move_field(&mut fields, "status", self.status, Value::String);
        move_field(&mut fields, "tail", self.tail, Value::String);
        move_field(&mut fields, "turnId", self.turn_id, Value::String);
        move_field(&mut fields, "type", self.task_type, Value::String);
        Value::Object(fields)
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

fn move_field<T>(
    fields: &mut serde_json::Map<String, Value>,
    key: &str,
    field: Field<T>,
    convert: impl FnOnce(T) -> Value,
) {
    match field {
        Field::Missing => {}
        Field::Null => {
            fields.insert(key.into(), Value::Null);
        }
        Field::Value(value) => {
            fields.insert(key.into(), convert(value));
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn consuming_projection_preserves_owned_allocations() {
        let record = TaskRecord::deserialize(
            serde_json::json!({"id":"t","tail":"owned-tail","opaque":["owned-extension"]}),
        )
        .unwrap();
        let text = record.tail().unwrap().as_ptr();
        let extension = record.extra["opaque"].as_array().unwrap().as_ptr();
        let expected = serde_json::json!(&record);
        let value = record.into_value();
        assert_eq!(value, expected);
        assert_eq!(value["tail"].as_str().unwrap().as_ptr(), text);
        assert_eq!(value["opaque"].as_array().unwrap().as_ptr(), extension);
    }
    use super::*;
    use serde_json::json;

    #[test]
    fn task_schema_matches_typed_fields_and_preserves_json() {
        check_schema_parity().unwrap();
        let fields = [
            "agent",
            "completedAtMs",
            "created",
            "durationMs",
            "itemId",
            "kind",
            "outputTruncated",
            "startedAtMs",
            "status",
            "tail",
            "turnId",
            "type",
        ];
        let schema = schema();
        assert_eq!(
            schema["properties"].as_object().unwrap().len(),
            fields.len() + 1
        );
        for field in fields {
            for candidate in [
                Value::Null,
                json!("x"),
                json!(true),
                json!(1),
                json!(1.0),
                json!(-1),
                json!({}),
                json!([]),
            ] {
                let mut value = json!({"id":"task", "extra":{"x":[1]}});
                value[field] = candidate;
                let parsed = TaskRecord::from_value(value.clone());
                assert_eq!(
                    parsed.is_ok(),
                    crate::validate("PersistedTaskRecord", &value).unwrap(),
                    "{field}: {value}"
                );
                if let Ok(parsed) = parsed {
                    assert_eq!(parsed.into_value(), value);
                }
            }
        }
        assert!(TaskRecord::from_value(json!([])).is_err());
        assert!(TaskRecord::from_value(json!({})).is_err());
    }
}
