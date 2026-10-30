//! Wire fields shared by native and guest Layr evidence replies.
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceResponse<T> {
    #[serde(default)]
    pub state_id: T,
    #[serde(default)]
    pub project_id: T,
    #[serde(default)]
    pub line: T,
}

/// A string only when the wire value is a string; other JSON remains invalid.
#[derive(Default)]
pub struct EvidenceString(pub Option<String>);

impl<'de> Deserialize<'de> for EvidenceString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(
            Value::deserialize(deserializer)?
                .as_str()
                .map(str::to_owned),
        ))
    }
}

impl EvidenceResponse<EvidenceString> {
    pub fn from_value(value: &Value) -> Option<Self> {
        if !value.is_object() {
            return None;
        }
        Self::deserialize(value).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_response_matches_guest_and_native_field_admission() {
        for text in [
            r#"{"stateId":"s","projectId":"p","line":"l"}"#,
            r#"{"stateId":"wrong","stateId":"s","projectId":"","line":"","extra":[]}"#,
            r#"{"stateId":null,"projectId":3,"line":{}}"#,
            r#"{"stateId":"s","line":false}"#,
            r#"["s","p","l"]"#,
            "null",
            "{",
            r#"{"stateId":"s","ignored":1e999}"#,
        ] {
            let Ok(value) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            let expected = (
                value.get("stateId").and_then(Value::as_str),
                value.get("projectId").and_then(Value::as_str),
                value.get("line").and_then(Value::as_str),
            );
            let result = EvidenceResponse::from_value(&value);
            let observed = (
                result.as_ref().and_then(|v| v.state_id.0.as_deref()),
                result.as_ref().and_then(|v| v.project_id.0.as_deref()),
                result.as_ref().and_then(|v| v.line.0.as_deref()),
            );
            assert_eq!(observed, expected, "{text}");
        }
    }
}
