//! Typed persisted complaints and their generated schema.

use crate::queue_command::Field;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ComplaintRecord {
    id: String,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    author: Field<String>,
    #[serde(rename = "leadId", default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    lead_id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    status: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    text: Field<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl ComplaintRecord {
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }
    pub fn id(&self) -> &String {
        &self.id
    }
    pub fn author(&self) -> Option<&str> {
        self.author.value().map(String::as_str)
    }
    pub fn lead_id(&self) -> Option<&str> {
        self.lead_id.value().map(String::as_str)
    }
    pub fn status(&self) -> Option<&str> {
        self.status.value().map(String::as_str)
    }
    pub fn text(&self) -> Option<&str> {
        self.text.value().map(String::as_str)
    }
    pub fn into_value(self) -> Value {
        // Every member is already JSON data or a serializable scalar.
        serde_json::to_value(self).expect("complaint fields are JSON serializable")
    }
}

pub(crate) fn schema() -> Value {
    schemars::schema_for!(ComplaintRecord).to_value()
}
pub(crate) fn check_schema_parity() -> Result<(), String> {
    let stored: Value = serde_json::from_slice(include_bytes!(
        "schemas/persisted-complaint-record.schema.json"
    ))
    .map_err(|error| error.to_string())?;
    if stored != schema() {
        return Err(
            "persisted complaint schema must be generated from complaint_record::ComplaintRecord"
                .into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn complaint_schema_and_serde_preserve_legacy_shapes() {
        check_schema_parity().unwrap();
        for (value, valid) in [
            (json!({"id":"c"}), true),
            (
                json!({"id":"c","author":null,"leadId":"l","text":null,"status":"future","extra":[1]}),
                true,
            ),
            (json!({"id":"c","status":null}), true),
            (json!({"id":"c","author":1}), false),
            (json!({"id":"c","status":false}), false),
            (json!(["c"]), false),
            (json!({}), false),
        ] {
            let parsed = serde_json::from_value::<ComplaintRecord>(value.clone());
            assert_eq!(parsed.is_ok(), valid, "{value}");
            assert_eq!(
                crate::validate("PersistedComplaintRecord", &value).unwrap(),
                valid,
                "{value}"
            );
            if let Ok(parsed) = parsed {
                assert_eq!(parsed.into_value(), value);
            }
        }
    }
}
