//! Structured persisted tool results. Receipt payloads outside this contract stay opaque.
use crate::queue_command::Field;
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(rename = "PersistedToolContent")]
pub struct ToolContent {
    #[serde(rename = "type", default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    kind: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    text: Field<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolResultRecord {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<bool>")]
    success: Field<bool>,
    #[serde(
        rename = "contentItems",
        default,
        skip_serializing_if = "Field::is_missing"
    )]
    #[schemars(with = "Option<Vec<ToolContent>>")]
    content_items: Field<Vec<ToolContent>>,
    #[serde(
        rename = "serverOutputExpiresAt",
        default,
        skip_serializing_if = "Field::is_missing"
    )]
    #[schemars(with = "Option<Number>")]
    server_output_expires_at: Field<Number>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl ToolResultRecord {
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        Self::deserialize(value)
    }
    pub fn text(success: bool, text: String) -> Self {
        Self {
            success: Field::Value(success),
            content_items: Field::Value(vec![ToolContent::text(text)]),
            server_output_expires_at: Field::Missing,
            extra: BTreeMap::new(),
        }
    }
    pub fn push_text(&mut self, text: String) -> Result<(), &'static str> {
        match &mut self.content_items {
            Field::Value(items) => {
                items.push(ToolContent::text(text));
                Ok(())
            }
            Field::Missing | Field::Null => Err("Invalid tool result"),
        }
    }
    pub fn set_expiry(&mut self, expiry: f64) {
        self.server_output_expires_at = Number::from_f64(expiry).map_or(Field::Null, Field::Value);
    }
    pub fn success(&self) -> Option<bool> {
        self.success.value().copied()
    }
    pub fn into_value(self) -> Value {
        let mut fields = self.extra.into_iter().collect();
        move_field(&mut fields, "success", self.success, Value::Bool);
        move_field(&mut fields, "contentItems", self.content_items, |items| {
            Value::Array(items.into_iter().map(ToolContent::into_value).collect())
        });
        move_field(
            &mut fields,
            "serverOutputExpiresAt",
            self.server_output_expires_at,
            Value::Number,
        );
        Value::Object(fields)
    }
}
impl ToolContent {
    fn into_value(self) -> Value {
        let mut fields = self.extra.into_iter().collect();
        move_field(&mut fields, "type", self.kind, Value::String);
        move_field(&mut fields, "text", self.text, Value::String);
        Value::Object(fields)
    }

    fn text(text: String) -> Self {
        Self {
            kind: Field::Value("inputText".into()),
            text: Field::Value(text),
            extra: BTreeMap::new(),
        }
    }
}

// Both supported schema encodings describe the same nullable JSON types.
fn normalize_schema(value: &Value) -> Value {
    match value {
        Value::Bool(true) => serde_json::json!({}),
        Value::Array(items) => Value::Array(items.iter().map(normalize_schema).collect()),
        Value::Object(fields) => {
            if fields.len() == 1
                && fields.get("$ref").and_then(Value::as_str) == Some("#/$defs/JsonValue")
            {
                return serde_json::json!({});
            }
            let mut out: serde_json::Map<String, Value> = fields
                .iter()
                .filter(|(key, _)| *key != "title")
                .map(|(key, value)| (key.clone(), normalize_schema(value)))
                .collect();
            if let Some(Value::Array(variants)) = out.get("anyOf")
                && variants.len() == 2
                && variants[1] == serde_json::json!({"type":"null"})
                && let Some(mut first) = variants[0].as_object().cloned()
                && let Some(kind) = first.remove("type")
            {
                first.insert("type".into(), serde_json::json!([kind, "null"]));
                out.remove("anyOf");
                out.extend(first);
            }
            Value::Object(out)
        }
        Value::Null | Value::Bool(false) | Value::Number(_) | Value::String(_) => value.clone(),
    }
}

/// Compare both schema consumers with the field contract owned by this type.
pub(crate) fn check_schema_parity() -> Result<(), String> {
    let stored: Value =
        serde_json::from_slice(include_bytes!("schemas/persisted-tool-result.schema.json"))
            .map_err(|e| e.to_string())?;
    let request: Value = serde_json::from_slice(include_bytes!(
        "schemas/persisted-tool-request-record.schema.json"
    ))
    .map_err(|e| e.to_string())?;
    let typed = schemars::schema_for!(ToolResultRecord).to_value();
    for (actual, expected) in [
        (&stored["type"], &typed["type"]),
        (&stored["required"], &typed["required"]),
        (&stored["properties"], &typed["properties"]),
        (
            &stored["additionalProperties"],
            &typed["additionalProperties"],
        ),
        (
            &stored["$defs"]["PersistedToolContent"]["type"],
            &typed["$defs"]["PersistedToolContent"]["type"],
        ),
        (
            &stored["$defs"]["PersistedToolContent"]["required"],
            &typed["$defs"]["PersistedToolContent"]["required"],
        ),
        (
            &stored["$defs"]["PersistedToolContent"]["properties"],
            &typed["$defs"]["PersistedToolContent"]["properties"],
        ),
        (
            &stored["$defs"]["PersistedToolContent"]["additionalProperties"],
            &typed["$defs"]["PersistedToolContent"]["additionalProperties"],
        ),
        (
            &request["$defs"]["PersistedToolResult"]["type"],
            &stored["type"],
        ),
        (
            &request["$defs"]["PersistedToolResult"]["required"],
            &stored["required"],
        ),
        (
            &request["$defs"]["PersistedToolResult"]["additionalProperties"],
            &stored["additionalProperties"],
        ),
        (
            &request["$defs"]["PersistedToolResult"]["properties"],
            &stored["properties"],
        ),
        (
            &request["$defs"]["PersistedToolContent"],
            &stored["$defs"]["PersistedToolContent"],
        ),
    ] {
        if normalize_schema(actual) != normalize_schema(expected) {
            return Err(format!(
                "persisted tool result schemas drifted: actual={actual} expected={expected}"
            ));
        }
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
        let record = ToolResultRecord::deserialize(serde_json::json!({"contentItems":[{"text":"owned-text"}],"opaque":["owned-extension"]})).unwrap();
        let text = record.content_items.value().unwrap()[0]
            .text
            .value()
            .unwrap()
            .as_ptr();
        let extension = record.extra["opaque"].as_array().unwrap().as_ptr();
        let expected = serde_json::json!(&record);
        let value = record.into_value();
        assert_eq!(value, expected);
        assert_eq!(
            value["contentItems"][0]["text"].as_str().unwrap().as_ptr(),
            text
        );
        assert_eq!(value["opaque"].as_array().unwrap().as_ptr(), extension);
    }
    use super::*;
    use serde_json::json;
    #[test]
    fn shell_matches_schema_and_preserves_presence_and_extensions() {
        check_schema_parity().unwrap();
        for value in [
            json!({}),
            json!({"success":null,"contentItems":null,"serverOutputExpiresAt":null,"other":[1]}),
            json!({"success":true,"contentItems":[{"type":null,"text":"x","data":{"a":1}}],"serverOutputExpiresAt":1.0}),
            json!([]),
            json!({"success":1}),
            json!({"contentItems":[null]}),
            json!({"contentItems":[{"text":1}]}),
            json!({"serverOutputExpiresAt":"1"}),
        ] {
            let decoded = ToolResultRecord::from_value(value.clone());
            assert_eq!(
                decoded.is_ok(),
                crate::validate("PersistedToolResult", &value).unwrap(),
                "{value}"
            );
            if let Ok(decoded) = decoded {
                assert_eq!(decoded.into_value(), value);
            }
        }
    }
}
