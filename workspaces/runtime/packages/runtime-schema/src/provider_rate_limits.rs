//! The bridge's SDK-backed consumed rate projection owns this generated schema.
//! Every runtime reader uses this boundary before interpreting raw rate fields.
use serde_json::Value;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateEvidenceError {
    Unsupported,
    InvalidContract,
}

pub fn checked_claude_rate_info(record: &Value) -> Result<Option<&Value>, RateEvidenceError> {
    let Some(info) = record.get("claudeRateLimit") else {
        return Ok(None);
    };
    static VALIDATOR: OnceLock<Result<jsonschema::Validator, RateEvidenceError>> = OnceLock::new();
    let validator = VALIDATOR
        .get_or_init(|| {
            let schema: Value =
                serde_json::from_str(include_str!("schemas/claude-rate-limit.schema.json"))
                    .map_err(|_| RateEvidenceError::InvalidContract)?;
            jsonschema::validator_for(&schema).map_err(|_| RateEvidenceError::InvalidContract)
        })
        .as_ref()
        .map_err(|error| *error)?;
    if validator.is_valid(info) {
        Ok(Some(info))
    } else {
        Err(RateEvidenceError::Unsupported)
    }
}

/// An absent event permits independent plan-window evidence; present invalid
/// evidence cannot make the observation fresh or prove usage/overage allowed.
pub fn rate_evidence_supported(data: &Value) -> bool {
    checked_claude_rate_info(data).is_ok()
        && data
            .get("rateLimits")
            .is_none_or(|bucket| checked_claude_rate_info(bucket).is_ok())
        && data
            .get("rateLimitsByLimitId")
            .and_then(Value::as_object)
            .is_none_or(|buckets| {
                buckets
                    .values()
                    .all(|bucket| checked_claude_rate_info(bucket).is_ok())
            })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn sdk_generated_projection_keeps_absence_known_and_unsupported_distinct() {
        assert_eq!(checked_claude_rate_info(&json!({})), Ok(None));
        let known = json!({"claudeRateLimit":{"status":"rejected", "rateLimitType":"five_hour", "overageStatus":"allowed", "resetsAt":2000}});
        assert_eq!(
            checked_claude_rate_info(&known),
            Ok(Some(&known["claudeRateLimit"]))
        );
        for value in [
            json!({"type":"unsupported", "raw":{"status":"allowed"}}),
            json!({"status":"future"}),
            json!({"status":"allowed", "overageStatus":"future"}),
            json!({"status":"allowed", "resetsAt":"soon"}),
            Value::Null,
        ] {
            assert_eq!(
                checked_claude_rate_info(&json!({"claudeRateLimit":value})),
                Err(RateEvidenceError::Unsupported)
            );
        }
    }
}
