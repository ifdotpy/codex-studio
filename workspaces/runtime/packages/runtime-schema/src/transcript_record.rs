//! Checked persisted transcript shells; provider extension fields stay opaque.
use crate::queue_command::Field;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct TranscriptItemId(pub String);
impl From<String> for TranscriptItemId {
    fn from(value: String) -> Self {
        Self(value)
    }
}
impl From<&str> for TranscriptItemId {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}
impl AsRef<str> for TranscriptItemId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(rename = "PersistedTranscriptInput")]
pub struct TranscriptInput {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    kind: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    text: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<bool>")]
    truncated: Field<bool>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(
    rename = "PersistedTranscriptRecord",
    description = "Legacy parent text is hydrated from fulltext before response validation."
)]
pub struct TranscriptItemRecord {
    id: TranscriptItemId,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    role: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    text: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<Vec<TranscriptInput>>")]
    inputs: Field<Vec<TranscriptInput>>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}
impl TranscriptItemRecord {
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        Self::deserialize(value)
    }
    pub fn id(&self) -> &TranscriptItemId {
        &self.id
    }
    pub fn role(&self) -> Option<&str> {
        self.role.value().map(String::as_str)
    }
    pub fn text(&self) -> &Field<String> {
        &self.text
    }
    pub fn inputs(&self) -> &Field<Vec<TranscriptInput>> {
        &self.inputs
    }
    pub fn extra(&self, key: &str) -> Option<&Value> {
        self.extra.get(key)
    }
    pub fn set_after_restore(&mut self, value: Value) {
        self.extra.insert("afterRestore".into(), value);
    }
    pub fn into_value(self) -> Value {
        let mut fields: serde_json::Map<String, Value> = self.extra.into_iter().collect();
        fields.insert("id".into(), Value::String(self.id.0));
        insert_field(&mut fields, "role", self.role, Value::String);
        insert_field(&mut fields, "text", self.text, Value::String);
        insert_field(&mut fields, "inputs", self.inputs, |inputs| {
            Value::Array(
                inputs
                    .into_iter()
                    .map(TranscriptInput::into_value)
                    .collect(),
            )
        });
        Value::Object(fields)
    }
    pub fn value(&self) -> Value {
        self.serialize(serde_json::value::Serializer)
            .expect("transcript fields serialize")
    }
}
impl TranscriptInput {
    pub fn id(&self) -> Option<&str> {
        self.id.value().map(String::as_str)
    }
    pub fn kind(&self) -> Option<&str> {
        self.kind.value().map(String::as_str)
    }
    pub fn text(&self) -> Option<&str> {
        self.text.value().map(String::as_str)
    }
    pub fn truncated(&self) -> Option<bool> {
        self.truncated.value().copied()
    }
    pub fn extra(&self, key: &str) -> Option<&Value> {
        self.extra.get(key)
    }
    fn into_value(self) -> Value {
        let mut fields: serde_json::Map<String, Value> = self.extra.into_iter().collect();
        insert_field(&mut fields, "id", self.id, Value::String);
        insert_field(&mut fields, "kind", self.kind, Value::String);
        insert_field(&mut fields, "text", self.text, Value::String);
        insert_field(&mut fields, "truncated", self.truncated, Value::Bool);
        Value::Object(fields)
    }
    pub fn value(&self) -> Value {
        self.serialize(serde_json::value::Serializer)
            .expect("transcript input fields serialize")
    }
}
fn insert_field<T>(
    fields: &mut serde_json::Map<String, Value>,
    name: &str,
    field: Field<T>,
    project: impl FnOnce(T) -> Value,
) {
    let value = match field {
        Field::Missing => return,
        Field::Null => Value::Null,
        Field::Value(value) => project(value),
    };
    fields.insert(name.into(), value);
}

pub(crate) fn schema() -> Value {
    schemars::schema_for!(TranscriptItemRecord).to_value()
}
pub(crate) fn check_schema_parity() -> Result<(), String> {
    let stored: Value = serde_json::from_slice(include_bytes!(
        "schemas/persisted-transcript-record.schema.json"
    ))
    .map_err(|e| e.to_string())?;
    if stored != schema() {
        return Err("persisted transcript schema must be generated from transcript_record::TranscriptItemRecord".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn transcript_shell_and_input_match_schema_preserving_extensions() {
        check_schema_parity().unwrap();
        for (nested, fields) in [
            (false, &["id", "role", "text", "inputs"][..]),
            (true, &["id", "kind", "text", "truncated"][..]),
        ] {
            for field in fields {
                for candidate in [
                    Value::Null,
                    json!("value"),
                    json!(true),
                    json!(1),
                    json!({}),
                    json!([]),
                ] {
                    let mut value = json!({"id":"i", "extra":{"x":[1]}});
                    if nested {
                        value["inputs"] = json!([{}]);
                        value["inputs"][0][*field] = candidate;
                    } else {
                        value[*field] = candidate;
                    }
                    let parsed = TranscriptItemRecord::from_value(value.clone());
                    assert_eq!(
                        parsed.is_ok(),
                        crate::validate("PersistedTranscriptRecord", &value).unwrap(),
                        "{field}: {value}"
                    );
                    if let Ok(parsed) = parsed {
                        assert_eq!(parsed.into_value(), value);
                    }
                }
            }
        }
        for value in [
            json!({"id":"i"}),
            json!({"id":"i","role":null,"text":null,"inputs":null,"future":[1]}),
            json!({"id":"i","role":"output","inputs":[{"id":null,"kind":"user","text":"t","truncated":false,"extra":{"x":1}}]}),
            json!([]),
            json!({}),
            json!({"id":"i","text":1}),
            json!({"id":"i","inputs":[null]}),
            json!({"id":"i","inputs":[{"id":1}]}),
            json!({"id":"i","inputs":[{"truncated":"yes"}]}),
        ] {
            let parsed = TranscriptItemRecord::from_value(value.clone());
            assert_eq!(
                parsed.is_ok(),
                crate::validate("PersistedTranscriptRecord", &value).unwrap(),
                "{value}"
            );
            if let Ok(parsed) = parsed {
                assert_eq!(parsed.into_value(), value);
            }
        }
    }
}
