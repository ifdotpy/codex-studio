//! Claude failure metadata. The JSON schema owns allowed values and bounds.
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{collections::BTreeMap, sync::OnceLock};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeFailure {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_error_status: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_error_origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<u64>,
}

pub fn schema() -> Value {
    serde_json::from_str(include_str!("schemas/claude-failure.schema.json"))
        .expect("Claude failure schema")
}

impl ClaudeFailure {
    /// Drop unknown or invalid fields before retaining the typed record.
    pub fn from_value(value: &Value) -> Option<Self> {
        static FIELDS: OnceLock<BTreeMap<String, crate::Validator>> = OnceLock::new();
        let validators = FIELDS.get_or_init(|| {
            schema()["properties"]
                .as_object()
                .expect("properties")
                .iter()
                .map(|(key, rule)| {
                    (
                        key.clone(),
                        crate::Validator::new(rule).expect("field schema"),
                    )
                })
                .collect()
        });
        let clean: Map<String, Value> = value
            .as_object()?
            .iter()
            .filter(|(key, value)| {
                validators
                    .get(*key)
                    .is_some_and(|validator| validator.is_valid(value))
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if clean.is_empty() {
            return None;
        }
        serde_json::from_value(Value::Object(clean)).ok()
    }
}

/// Sanitize the extension independently of the existing error and policy fields.
pub fn sanitize_error(error: &mut Value) {
    let Some(fields) = error.as_object_mut() else {
        return;
    };
    if let Some(raw) = fields.remove("claudeFailure")
        && let Some(record) = ClaudeFailure::from_value(&raw)
    {
        fields.insert(
            "claudeFailure".into(),
            serde_json::to_value(record).expect("failure record"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schema_fields_match_the_typed_record_and_enforce_bounds() {
        let document = schema();
        let valid: Map<String, Value> = document["properties"]
            .as_object()
            .expect("schema properties")
            .iter()
            .map(|(key, rule)| {
                let sample = if let Some(values) = rule["enum"].as_array() {
                    values[0].clone()
                } else if rule["type"] == "integer" {
                    rule["maximum"].clone()
                } else {
                    json!("server_gate")
                };
                (key.clone(), sample)
            })
            .collect();
        let valid = Value::Object(valid);
        assert_eq!(
            serde_json::to_value(
                ClaudeFailure::from_value(&valid).expect("schema fields match Rust fields")
            )
            .expect("typed failure serializes"),
            valid
        );
        for field in ["promptTokens", "maxContextTokens", "apiErrorStatus"] {
            for number in [
                json!(-1),
                json!(9007199254740992_u64),
                json!("123"),
                json!(1.5),
            ] {
                assert!(ClaudeFailure::from_value(&json!({field:number})).is_none());
            }
        }
        for code in ["Bearer secret", "sk-secret", &"x".repeat(65)] {
            assert!(ClaudeFailure::from_value(&json!({"apiErrorCode":code})).is_none());
        }
        assert!(
            ClaudeFailure::from_value(
                &json!({"message":"private","error_details":"secret","apiError":"new_unknown"})
            )
            .is_none()
        );
        assert!(ClaudeFailure::from_value(&json!([])).is_none());
    }
}
