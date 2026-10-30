//! The permissive stored usage-resume envelope. Decision and cancellation
//! callers apply their own stricter contracts to these historical records.
use crate::queue_command::Field;
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename = "PersistedUsageResumeRecord")]
pub struct UsageResumeRecord {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub cause: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub status: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<Number>")]
    pub failed_at: Field<Number>,
    // Persistence historically accepts any account evidence; recovery only
    // selects its string form after SQLite has applied the recovery predicate.
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Value")]
    pub account_key: Field<Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

pub(crate) fn schema() -> Value {
    schemars::schema_for!(UsageResumeRecord).to_value()
}

pub(crate) fn check_schema_parity() -> Result<(), String> {
    let stored: Value = serde_json::from_slice(include_bytes!(
        "schemas/persisted-usage-resume-record.schema.json"
    ))
    .map_err(|error| error.to_string())?;
    if stored != schema() {
        return Err("persisted usage resume schema must be generated from usage_resume_record::UsageResumeRecord".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn persisted_schema_keeps_permissive_evidence_and_presence() {
        check_schema_parity().unwrap();
        for (value, valid) in [
            (json!({}), true),
            (
                json!({"status":null,"cause":"future","failedAt":1.0,"accountKey":[],"epoch":null,"taskClaims":{"future":true}}),
                true,
            ),
            (json!({"status":false}), false),
            (json!({"cause":1}), false),
            (json!({"failedAt":"1"}), false),
            (json!([]), false),
        ] {
            let parsed = UsageResumeRecord::deserialize(value.clone());
            assert_eq!(parsed.is_ok(), valid, "{value}");
            assert_eq!(
                crate::validate("PersistedUsageResumeRecord", &value).unwrap(),
                valid,
                "{value}"
            );
            if let Ok(record) = parsed {
                assert_eq!(json!(record), value);
            }
        }
    }
}
