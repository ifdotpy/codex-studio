//! Typed persisted terminal record and its schema owner.
use crate::queue_command::Field;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::stored_record::StoredJsonInteger;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TerminalRecord {
    id: String,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    agent: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    status: Field<String>,
    #[serde(
        rename = "exitCode",
        default,
        skip_serializing_if = "Field::is_missing"
    )]
    #[schemars(with = "Option<StoredJsonInteger>")]
    exit_code: Field<StoredJsonInteger>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl TerminalRecord {
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }
    pub fn id(&self) -> &String {
        &self.id
    }
    pub fn status(&self) -> Option<&str> {
        self.status.value().map(String::as_str)
    }
    pub fn set_status(&mut self, status: &str) {
        self.status = Field::Value(status.to_owned());
    }
    pub fn set_exit_code(&mut self, code: Option<i64>) {
        self.exit_code = code.map_or(Field::Null, |value| {
            Field::Value(StoredJsonInteger::from_i64(value))
        });
    }
    pub fn exit_code_value(&self) -> Value {
        self.exit_code
            .value()
            .map_or(Value::Null, |value| Value::Number(value.number().clone()))
    }
    pub fn value(&self) -> Value {
        // Every member is already JSON data and the typed scalars are serializable.
        serde_json::to_value(self).expect("terminal fields are JSON serializable")
    }
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.extra.get(key)
    }
    pub fn set(&mut self, key: impl Into<String>, value: Value) -> Result<(), serde_json::Error> {
        let key = key.into();
        if ["id", "agent", "status", "exitCode"].contains(&key.as_str()) {
            return Err(serde::de::Error::custom(
                "terminal extension conflicts with a typed field",
            ));
        }
        self.extra.insert(key, value);
        Ok(())
    }
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        self.extra.remove(key)
    }
}

pub(crate) fn check_schema_parity() -> Result<(), String> {
    let generated = schema();
    let stored: Value = serde_json::from_slice(include_bytes!(
        "schemas/persisted-terminal-record.schema.json"
    ))
    .map_err(|error| error.to_string())?;
    if generated != stored {
        return Err(
            "persisted terminal schema must be generated from terminal_record::TerminalRecord"
                .into(),
        );
    }
    Ok(())
}

pub(crate) fn schema() -> Value {
    schemars::schema_for!(TerminalRecord).to_value()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn generated_schema_and_serde_agree_on_terminal_shapes() {
        check_schema_parity().unwrap();
        for (value, valid) in [
            (json!({"id":"t"}), true),
            (
                json!({"id":"t","agent":null,"status":null,"exitCode":null,"extension":[1]}),
                true,
            ),
            (json!({"id":"t","exitCode":1.0}), true),
            (json!({"id":"t","exitCode":u64::MAX}), true),
            (json!({"id":"t","exitCode":1.5}), false),
            (json!({"id":"t","agent":3}), false),
            (json!({"id":"t","status":false}), false),
            (json!(["t"]), false),
            (json!({}), false),
        ] {
            assert_eq!(
                serde_json::from_value::<TerminalRecord>(value.clone()).is_ok(),
                valid,
                "{value}"
            );
            assert_eq!(
                crate::validate("PersistedTerminalRecord", &value).unwrap(),
                valid,
                "{value}"
            );
        }
    }
}
