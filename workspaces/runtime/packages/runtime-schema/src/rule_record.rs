//! Typed persisted rules, including open extension fields.
use crate::stored_record::{StoredJsonInteger, present_nullable};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use std::{borrow::Cow, collections::BTreeMap};

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
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RuleText<const MAX: usize>(String);
impl<const MAX: usize> RuleText<MAX> {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de, const MAX: usize> Deserialize<'de> for RuleText<MAX> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s.chars().count() <= MAX {
            Ok(Self(s))
        } else {
            Err(serde::de::Error::custom("rule text too long"))
        }
    }
}
impl<const MAX: usize> schemars::JsonSchema for RuleText<MAX> {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        format!("RuleText{MAX}").into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type":"string","maxLength":MAX})
    }
}
fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
fn present_launch<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<LaunchIntent>>, D::Error> {
    Option::<crate::json_object::JsonObject<LaunchIntent>>::deserialize(d)
        .map(|v| Some(v.map(|o| o.0)))
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    Interval,
    Once,
    File,
    Event,
    LowWorkers,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuleStatus {
    Active,
    Paused,
    Completed,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
    pub check_number: RuleInteger<0, 9007199254740991>,
    pub command: RuleText<12000>,
    pub epoch: RuleInteger<0, 9007199254740991>,
    pub liveness: bool,
    pub monitor_id: String,
    pub rule_id: String,
    #[serde(deserialize_with = "Option::<String>::deserialize")]
    #[schemars(required, schema_with = "nullable_string_schema")]
    pub thread_id: Option<String>,
    pub timeout_ms: RuleInteger<1000, 86400000>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuleRecord {
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "RuleInteger<0, 9007199254740991>")]
    pub active_workers: Option<RuleInteger<0, 9007199254740991>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "String")]
    pub agent: Option<String>,
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
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "RuleInteger<0, 9007199254740991>")]
    pub checks: Option<RuleInteger<0, 9007199254740991>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "RuleText<12000>")]
    pub command: Option<RuleText<12000>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Number")]
    pub created: Option<Number>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "RuleInteger<1, 525600>")]
    pub duration_minutes: Option<RuleInteger<1, 525600>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "RuleInteger<0, 9007199254740991>")]
    pub epoch: Option<RuleInteger<0, 9007199254740991>>,
    #[serde(
        default,
        deserialize_with = "present_nullable",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<String>")]
    pub error: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "String")]
    pub event: Option<String>,
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
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "RuleInteger<0, 9007199254740991>")]
    pub file_generation: Option<RuleInteger<0, 9007199254740991>>,
    #[serde(
        default,
        deserialize_with = "present_nullable",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(
        with = "Option<(RuleInteger<-9223372036854775808, 9223372036854775807>, RuleInteger<0, 18446744073709551615>, RuleInteger<0, 18446744073709551615>)>"
    )]
    pub fingerprint: Option<
        Option<(
            RuleInteger<-9223372036854775808, 9223372036854775807>,
            RuleInteger<0, 18446744073709551615>,
            RuleInteger<0, 18446744073709551615>,
        )>,
    >,
    pub id: String,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "bool")]
    pub in_flight: Option<bool>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "RuleInteger<10, 31536000>")]
    pub interval_seconds: Option<RuleInteger<10, 31536000>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "RuleKind")]
    pub kind: Option<RuleKind>,
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
    #[serde(
        default,
        deserialize_with = "present_nullable",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<StoredJsonInteger>")]
    pub last_exit_code: Option<Option<StoredJsonInteger>>,
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
    #[serde(
        default,
        deserialize_with = "present_nullable",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<String>")]
    pub last_stall_error: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "present_nullable",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<StoredJsonInteger>")]
    pub last_stall_exit_code: Option<Option<StoredJsonInteger>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Number")]
    pub last_stall_finished: Option<Number>,
    #[serde(
        default,
        deserialize_with = "present_launch",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<LaunchIntent>")]
    pub launch_intent: Option<Option<LaunchIntent>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "RuleText<12000>")]
    pub liveness_command: Option<RuleText<12000>>,
    #[serde(
        default,
        deserialize_with = "present_nullable",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<Number>")]
    pub low_since: Option<Option<Number>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "RuleInteger<1, 255>")]
    pub minimum_workers: Option<RuleInteger<1, 255>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "RuleText<120>")]
    pub name: Option<RuleText<120>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Number")]
    pub next_at: Option<Number>,
    #[serde(
        default,
        deserialize_with = "present_nullable",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<OwnerHold>")]
    pub owner_hold: Option<Option<OwnerHold>>,
    #[serde(
        default,
        deserialize_with = "present_nullable",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<String>")]
    pub path: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "present_nullable",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<BTreeMap<String, Value>>")]
    pub restart_check: Option<Option<BTreeMap<String, Value>>>,
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
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "RuleInteger<0, 31536000>")]
    pub stall_timeout_seconds: Option<RuleInteger<0, 31536000>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "RuleInteger<-1, 9007199254740991>")]
    pub stall_wake_generation: Option<RuleInteger<-1, 9007199254740991>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "RuleStatus")]
    pub status: Option<RuleStatus>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "String")]
    pub text: Option<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "RuleInteger<0, 9007199254740991>")]
    pub wakes: Option<RuleInteger<0, 9007199254740991>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
impl RuleRecord {
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }
    pub fn id(&self) -> &String {
        &self.id
    }
    pub fn value(&self) -> Value {
        serde_json::to_value(self).expect("rule fields are JSON serializable")
    }
    pub fn into_value(self) -> Value {
        self.value()
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
    use serde_json::json;

    #[test]
    fn typed_rule_matches_stored_schema_on_each_field_and_bounds() {
        let schema: Value =
            serde_json::from_slice(include_bytes!("schemas/persisted-rule-record.schema.json"))
                .unwrap();
        let fields = schema["properties"].as_object().unwrap();
        let candidates = vec![
            json!(null),
            json!(true),
            json!(false),
            json!(""),
            json!("active"),
            json!("interval"),
            json!(0),
            json!(-1),
            json!(1),
            json!(10.0),
            json!(1.5),
            json!(9007199254740991u64),
            json!(9007199254740992u64),
            json!(9223372036854775808u64),
            json!(u64::MAX),
            json!(18446744073709551616.0),
            json!([]),
            json!([0, 1, 2]),
            json!([9223372036854775808.0, 1, 2]),
            json!([0, 18446744073709551616.0, 2]),
            json!({}),
        ];
        for field in fields.keys() {
            for candidate in &candidates {
                let mut value = json!({"id":"r"});
                value[field] = candidate.clone();
                let schema_valid = crate::validate("PersistedRuleRecord", &value).unwrap();
                let decoded = serde_json::from_value::<RuleRecord>(value.clone());
                assert_eq!(decoded.is_ok(), schema_valid, "field {field}: {candidate}");
                if let Ok(decoded) = decoded {
                    assert_eq!(decoded.value(), value, "field {field}");
                }
            }
        }
    }

    #[test]
    fn launch_requires_nullable_fields_and_preserves_null() {
        let mut launch = json!({"ruleId":"r","checkNumber":1.0,"agentId":"a","epoch":2,"accountKey":null,"threadId":null,"monitorId":"m","command":"run","timeoutMs":1000,"liveness":false});
        let value = json!({"id":"r","launchIntent":launch});
        let record: RuleRecord = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(record.value(), value);
        assert!(crate::validate("PersistedRuleRecord", &value).unwrap());
        launch.as_object_mut().unwrap().remove("threadId");
        let missing = json!({"id":"r","launchIntent":launch});
        assert!(serde_json::from_value::<RuleRecord>(missing.clone()).is_err());
        assert!(!crate::validate("PersistedRuleRecord", &missing).unwrap());
    }
}
