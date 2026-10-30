//! Persisted request envelope; method-specific payloads remain open JSON.
use crate::queue_command::Field;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename = "PersistedRequestRecord")]
pub struct RequestRecord {
    id: String,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    agent: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    connection_id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    method: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    status: Field<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl RequestRecord {
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        Self::deserialize(value)
    }
    pub fn id(&self) -> &String {
        &self.id
    }
    pub fn agent(&self) -> Option<&str> {
        self.agent.value().map(String::as_str)
    }
    pub fn connection_id(&self) -> Option<&str> {
        self.connection_id.value().map(String::as_str)
    }
    pub fn method(&self) -> Option<&str> {
        self.method.value().map(String::as_str)
    }
    pub fn status(&self) -> Option<&str> {
        self.status.value().map(String::as_str)
    }
    pub fn set_status(&mut self, status: &str) {
        self.status = Field::Value(status.to_owned());
    }
    pub fn extension(&self, key: &str) -> Option<&Value> {
        self.extra.get(key)
    }
    pub fn set_extension(&mut self, key: &str, value: Value) -> Result<(), serde_json::Error> {
        if matches!(key, "id" | "agent" | "connectionId" | "method" | "status") {
            return Err(serde::de::Error::custom(
                "request extension conflicts with a known field",
            ));
        }
        self.extra.insert(key.to_owned(), value);
        Ok(())
    }
    pub fn value(&self) -> Value {
        self.clone().into_value()
    }
    pub fn into_value(self) -> Value {
        let mut object: Map<String, Value> = self.extra.into_iter().collect();
        object.insert("id".into(), Value::String(self.id));
        for (key, field) in [
            ("agent", self.agent),
            ("connectionId", self.connection_id),
            ("method", self.method),
            ("status", self.status),
        ] {
            match field {
                Field::Missing => {}
                Field::Null => {
                    object.insert(key.into(), Value::Null);
                }
                Field::Value(value) => {
                    object.insert(key.into(), Value::String(value));
                }
            }
        }
        Value::Object(object)
    }
}

pub(crate) fn schema() -> Value {
    schemars::schema_for!(RequestRecord).to_value()
}
pub(crate) fn check_schema_parity() -> Result<(), String> {
    let stored: Value = serde_json::from_slice(include_bytes!(
        "schemas/persisted-request-record.schema.json"
    ))
    .map_err(|e| e.to_string())?;
    if stored != schema() {
        return Err(
            "persisted request schema must be generated from request_record::RequestRecord".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schema_and_owner_preserve_open_fields_and_presence() {
        check_schema_parity().unwrap();
        for (value, valid) in [
            (json!({"id":"r"}), true),
            (
                json!({"id":"r","agent":null,"connectionId":"","method":"future/method","status":"future","params":{"opaque":[1,null]},"count":1.0}),
                true,
            ),
            (json!({"id":"r","status":false}), false),
            (json!({"id":"r","method":1}), false),
            (json!({"id":"r","agent":[]}), false),
            (json!({"id":"r","connectionId":{}}), false),
            (json!({}), false),
            (json!(["r"]), false),
        ] {
            let decoded = RequestRecord::from_value(value.clone());
            assert_eq!(decoded.is_ok(), valid, "{value}");
            assert_eq!(
                crate::validate("PersistedRequestRecord", &value).unwrap(),
                valid,
                "{value}"
            );
            if let Ok(record) = decoded {
                assert_eq!(record.into_value(), value);
            }
        }
    }

    #[test]
    fn consuming_projection_moves_opaque_payloads() {
        let record =
            RequestRecord::from_value(json!({"id":"r","params":{"nested":["retained"]}})).unwrap();
        let before = record.extension("params").unwrap()["nested"][0]
            .as_str()
            .unwrap()
            .as_ptr();
        let output = record.into_value();
        assert_eq!(
            before,
            output["params"]["nested"][0].as_str().unwrap().as_ptr()
        );
    }

    #[test]
    fn extension_writes_cannot_shadow_any_schema_field() {
        let fields = schema()["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut record = RequestRecord::from_value(json!({"id":"r"})).unwrap();
        for field in fields {
            assert!(
                record.set_extension(&field, json!("shadow")).is_err(),
                "{field}"
            );
        }
        record.set_extension("future", json!([1, null])).unwrap();
        assert_eq!(record.extension("future"), Some(&json!([1, null])));
    }
}
