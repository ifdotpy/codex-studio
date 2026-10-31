//! Typed persisted rules, including open extension fields.
use crate::stored_record::StoredJsonInteger;
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use std::borrow::Cow;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RuleInteger<const MIN: i64, const MAX: u64>(StoredJsonInteger);
impl<const MIN: i64, const MAX: u64> RuleInteger<MIN, MAX> {
    pub fn number(&self) -> &Number {
        self.0.number()
    }
}
impl<'de, const MIN: i64, const MAX: u64> Deserialize<'de> for RuleInteger<MIN, MAX> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let n = StoredJsonInteger::deserialize(d)?;
        let valid = if let Some(v) = n.number().as_i64() {
            (v as i128) >= MIN as i128 && (v as i128) <= MAX as i128
        } else if let Some(v) = n.number().as_u64() {
            (v as i128) >= MIN as i128 && v <= MAX
        } else {
            n.number()
                .as_f64()
                .is_some_and(|v| (v as i128) >= MIN as i128 && (v as i128) <= MAX as i128)
        };
        if valid {
            Ok(Self(n))
        } else {
            Err(serde::de::Error::custom("rule integer outside bounds"))
        }
    }
}
impl<const MIN: i64, const MAX: u64> schemars::JsonSchema for RuleInteger<MIN, MAX> {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        format!("RuleInteger{MIN}_{MAX}").into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type":"integer","minimum":MIN,"maximum":MAX})
    }
}
pub use crate::bounded_integer::BoundedInteger as StrictRuleInteger;
pub type RuleText<const MAX: usize> = crate::bounded_text::BoundedText<0, MAX>;
fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum RuleKind {
    Interval,
    Once,
    File {
        path: String,
    },
    Event,
    LowWorkers {
        minimum_workers: StrictRuleInteger<1, 255>,
        duration_minutes: StrictRuleInteger<1, 525600>,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuleStatus {
    Active,
    Paused,
    Completed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OwnerHold {
    FailedInputHeld,
    NativeFailureHold,
    Archived,
}
fn nullable_string_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type": ["string", "null"]})
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LaunchIntent {
    #[serde(deserialize_with = "Option::<String>::deserialize")]
    #[schemars(required, schema_with = "nullable_string_schema")]
    pub account_key: Option<String>,
    pub agent_id: String,
    pub check_number: StrictRuleInteger<0, 9007199254740991>,
    pub command: RuleText<12000>,
    pub epoch: StrictRuleInteger<0, 9007199254740991>,
    pub liveness: bool,
    pub monitor_id: String,
    pub rule_id: String,
    #[serde(deserialize_with = "Option::<String>::deserialize")]
    #[schemars(required, schema_with = "nullable_string_schema")]
    pub thread_id: Option<String>,
    pub timeout_ms: StrictRuleInteger<1000, 86400000>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestartCheck {
    pub epoch: u64,
    pub checks: u64,
    pub monitor_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuleRecord {
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "StrictRuleInteger<0, 9007199254740991>")]
    pub active_workers: Option<StrictRuleInteger<0, 9007199254740991>>,
    pub agent: String,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "bool")]
    pub alerted: Option<bool>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Number")]
    pub at: Option<Number>,
    pub checks: StrictRuleInteger<0, 9007199254740991>,
    pub command: RuleText<12000>,
    pub created: Number,
    pub epoch: StrictRuleInteger<0, 9007199254740991>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub event: String,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "RuleText<12000>")]
    pub event_text: Option<RuleText<12000>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Number")]
    pub file_activity_at: Option<Number>,
    pub file_generation: StrictRuleInteger<0, 9007199254740991>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<(i64, u64, u64)>,
    pub id: String,
    pub in_flight: bool,
    pub interval_seconds: StrictRuleInteger<10, 31536000>,
    #[serde(flatten)]
    pub kind: RuleKind,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Number")]
    pub last_at: Option<Number>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "String")]
    pub last_event: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit_code: Option<i64>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Number")]
    pub last_finished: Option<Number>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "RuleText<12000>")]
    pub last_output: Option<RuleText<12000>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_stall_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_stall_exit_code: Option<i64>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Number")]
    pub last_stall_finished: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_intent: Option<LaunchIntent>,
    pub liveness_command: RuleText<12000>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low_since: Option<Number>,
    pub name: RuleText<120>,
    pub next_at: Number,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_hold: Option<OwnerHold>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_check: Option<RestartCheck>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "String")]
    pub root_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "String")]
    pub stall_event_key: Option<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "bool")]
    pub stall_probe: Option<bool>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "String")]
    pub stall_text: Option<String>,
    pub stall_timeout_seconds: StrictRuleInteger<0, 31536000>,
    pub stall_wake_generation: StrictRuleInteger<-1, 9007199254740991>,
    pub status: RuleStatus,
    pub text: String,
    pub wakes: StrictRuleInteger<0, 9007199254740991>,
}
impl RuleRecord {
    pub fn id(&self) -> &String {
        &self.id
    }
}
pub(crate) fn schema() -> Value {
    schemars::schema_for!(RuleRecord).to_value()
}
pub(crate) fn check_schema_parity() -> Result<(), String> {
    let stored: Value =
        serde_json::from_slice(include_bytes!("schemas/persisted-rule-record.schema.json"))
            .map_err(|e| e.to_string())?;
    if stored != schema() {
        return Err("persisted rule schema must match rule_record::RuleRecord".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_kind_requires_its_specific_fields() {
        assert!(serde_json::from_str::<RuleKind>(r#"{"kind":"file"}"#).is_err());
        assert!(
            serde_json::from_str::<RuleKind>(r#"{"kind":"low_workers","minimumWorkers":1}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<RuleKind>(r#"{"kind":"file","path":"fixture-file"}"#).is_ok()
        );
        assert!(
            serde_json::from_str::<RuleKind>(
                r#"{"kind":"low_workers","minimumWorkers":1,"durationMinutes":5}"#
            )
            .is_ok()
        );
    }
    #[test]
    fn strict_rule_requires_owner_and_discards_extensions() {
        assert!(serde_json::from_str::<RuleRecord>(r#"{"id":"r"}"#).is_err());
        let raw = r#"{"id":"r","agent":"a","epoch":1,"name":"n","kind":"interval","status":"active","intervalSeconds":60,"nextAt":1,"created":1,"command":"","text":"","checks":0,"wakes":0,"inFlight":false,"fileGeneration":0,"stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"livenessCommand":"","event":"","unknown":true}"#;
        let record = serde_json::from_str::<RuleRecord>(raw).expect("rule with extension");
        assert!(!serde_json::to_string(&record).unwrap().contains("unknown"));
        let valid = r#"{"id":"r","agent":"a","epoch":1,"name":"n","kind":"file","path":"fixture-file","status":"active","intervalSeconds":60,"nextAt":1,"created":1,"command":"","text":"","checks":0,"wakes":0,"inFlight":false,"fileGeneration":0,"stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"livenessCommand":"","event":""}"#;
        serde_json::from_str::<RuleRecord>(valid).expect("valid file rule");
    }
    #[test]
    fn stored_rule_schema_matches_type() {
        check_schema_parity().unwrap();
    }
    #[test]
    fn launch_intent_requires_nullable_identity_fields() {
        let raw = r#"{"ruleId":"r","checkNumber":1,"agentId":"a","epoch":2,"accountKey":null,"threadId":null,"monitorId":"m","command":"run","timeoutMs":1000,"liveness":false}"#;
        let intent: LaunchIntent = serde_json::from_str(raw).unwrap();
        assert_eq!((intent.account_key, intent.thread_id), (None, None));
        assert!(
            serde_json::from_str::<LaunchIntent>(&raw.replace(",\"threadId\":null", "")).is_err()
        );
    }
}
