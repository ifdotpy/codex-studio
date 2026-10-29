//! Typed persisted annotations and their generated schema.
use crate::stored_record::{StoredJsonInteger, present_nullable};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AnnotationRecord {
    id: String,
    #[serde(
        default,
        deserialize_with = "present_nullable",
        skip_serializing_if = "Option::is_none"
    )]
    agent: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "present_nullable",
        skip_serializing_if = "Option::is_none"
    )]
    path: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "present_nullable",
        skip_serializing_if = "Option::is_none"
    )]
    text: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "present_nullable",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(schema_with = "line_schema")]
    line: Option<Option<StoredJsonInteger>>,
    #[serde(
        default,
        deserialize_with = "present_nullable",
        skip_serializing_if = "Option::is_none"
    )]
    created: Option<Option<Number>>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

fn line_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"anyOf":[{"type":"integer","x-safe-integer":"unsigned"},{"type":"null"}]})
}

impl AnnotationRecord {
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }
    pub fn id(&self) -> &String {
        &self.id
    }
    pub fn value(&self) -> Value {
        // Every field is already serializable JSON data.
        serde_json::to_value(self).expect("annotation fields are JSON serializable")
    }
    pub fn into_value(self) -> Value {
        self.value()
    }
}

pub(crate) fn schema() -> Value {
    schemars::schema_for!(AnnotationRecord).to_value()
}
pub(crate) fn check_schema_parity() -> Result<(), String> {
    let stored: Value = serde_json::from_slice(include_bytes!(
        "schemas/persisted-annotation-record.schema.json"
    ))
    .map_err(|error| error.to_string())?;
    if stored != schema() {
        return Err("persisted annotation schema must be generated from annotation_record::AnnotationRecord".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schema_and_typed_annotations_accept_the_same_shapes() {
        check_schema_parity().unwrap();
        for (value, valid) in [
            (json!({"id":"a"}), true),
            (
                json!({"id":"a","line":-1.0,"created":null,"extra":[1]}),
                true,
            ),
            (json!({"id":"a","line":u64::MAX}), true),
            (json!({"id":"a","line":1.5}), false),
            (json!({"id":"a","created":"bad"}), false),
            (json!(["a"]), false),
        ] {
            assert_eq!(
                serde_json::from_value::<AnnotationRecord>(value.clone()).is_ok(),
                valid,
                "{value}"
            );
            assert_eq!(
                crate::validate("PersistedAnnotationRecord", &value).unwrap(),
                valid,
                "{value}"
            );
        }
    }
}
