//! Typed persisted monitor rows and their open transport evidence.
use crate::queue_command::Field;
use crate::stored_record::StoredJsonInteger;
use crate::studio_errors::MonitorStatus;
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use std::{borrow::Cow, collections::BTreeMap};

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct MonitorInteger<const MIN: i64, const MAX: u64>(StoredJsonInteger);
impl<const MIN: i64, const MAX: u64> MonitorInteger<MIN, MAX> {
    pub fn number(&self) -> &Number {
        self.0.number()
    }
}
impl<'de, const MIN: i64, const MAX: u64> Deserialize<'de> for MonitorInteger<MIN, MAX> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let n = StoredJsonInteger::deserialize(d)?;
        let value = n.number();
        let valid = if let Some(v) = value.as_i64() {
            (v as i128) >= MIN as i128 && (v as i128) <= MAX as i128
        } else if let Some(v) = value.as_u64() {
            (v as i128) >= MIN as i128 && v <= MAX
        } else {
            value
                .as_f64()
                .is_some_and(|v| (v as i128) >= MIN as i128 && (v as i128) <= MAX as i128)
        };
        if valid {
            Ok(Self(n))
        } else {
            Err(serde::de::Error::custom("monitor integer outside bounds"))
        }
    }
}
impl<const MIN: i64, const MAX: u64> schemars::JsonSchema for MonitorInteger<MIN, MAX> {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        format!("MonitorInteger{MIN}_{MAX}").into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type":"integer","minimum":MIN,"maximum":MAX})
    }
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct MonitorText<const MIN: usize, const MAX: usize>(String);
impl<const MIN: usize, const MAX: usize> MonitorText<MIN, MAX> {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de, const MIN: usize, const MAX: usize> Deserialize<'de> for MonitorText<MIN, MAX> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let size = s.chars().count();
        if (MIN..=MAX).contains(&size) {
            Ok(Self(s))
        } else {
            Err(serde::de::Error::custom("invalid monitor text length"))
        }
    }
}
impl<const MIN: usize, const MAX: usize> schemars::JsonSchema for MonitorText<MIN, MAX> {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        format!("MonitorText{MIN}_{MAX}").into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        if MIN == 0 {
            schemars::json_schema!({"type":"string","maxLength":MAX})
        } else {
            schemars::json_schema!({"type":"string","minLength":MIN,"maxLength":MAX})
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct MonitorTime(Number);
impl MonitorTime {
    pub fn number(&self) -> &Number {
        &self.0
    }
}
impl<'de> Deserialize<'de> for MonitorTime {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let n = Number::deserialize(d)?;
        if n.as_f64().is_some_and(|v| v >= 0.0) {
            Ok(Self(n))
        } else {
            Err(serde::de::Error::custom("negative monitor time"))
        }
    }
}
impl schemars::JsonSchema for MonitorTime {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        "MonitorTime".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type":"number","minimum":0})
    }
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct MonitorExitCodes(Vec<MonitorInteger<0, 255>>);
impl MonitorExitCodes {
    pub fn values(&self) -> &[MonitorInteger<0, 255>] {
        &self.0
    }
}
impl<'de> Deserialize<'de> for MonitorExitCodes {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let items = Vec::deserialize(d)?;
        if (1..=16).contains(&items.len()) {
            Ok(Self(items))
        } else {
            Err(serde::de::Error::custom("invalid exit code count"))
        }
    }
}
impl schemars::JsonSchema for MonitorExitCodes {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        "MonitorExitCodes".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type":"array","minItems":1,"maxItems":16,"items":{"type":"integer","minimum":0,"maximum":255}})
    }
}
fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
fn present_operation<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Field<MonitorOperationRecord>, D::Error> {
    Field::<crate::json_object::JsonObject<MonitorOperationRecord>>::deserialize(d).map(|v| match v
    {
        Field::Missing => Field::Missing,
        Field::Null => Field::Null,
        Field::Value(value) => Field::Value(value.0),
    })
}
fn status_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let status =
        crate::studio_errors::schemas().expect("monitor status schema")["MonitorState"].clone();
    schemars::json_schema!({"anyOf": [status, {"type":"null"}]})
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MonitorWakeOn {
    Exit,
    Failure,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MonitorOperationRecord {
    pub account_key: MonitorText<1, 200>,
    pub agent: MonitorText<1, 200>,
    pub connection_id: MonitorText<1, 200>,
    pub epoch: MonitorInteger<0, 9007199254740991>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MonitorRecord {
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "MonitorTime")]
    pub activity_at: Option<MonitorTime>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "MonitorInteger<0, 9007199254740991>")]
    pub activity_generation: Option<MonitorInteger<0, 9007199254740991>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub agent: Field<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "MonitorInteger<0, 9007199254740991>")]
    pub bytes: Option<MonitorInteger<0, 9007199254740991>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "bool")]
    pub cancel_requested: Option<bool>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "MonitorText<0, 12000>")]
    pub command: Option<MonitorText<0, 12000>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "bool")]
    pub configuration_pending: Option<bool>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "MonitorTime")]
    pub created: Option<MonitorTime>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "String")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<MonitorInteger<0, 9007199254740991>>")]
    pub epoch: Field<MonitorInteger<0, 9007199254740991>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<MonitorText<0, 12000>>")]
    pub error: Field<MonitorText<0, 12000>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<StoredJsonInteger>")]
    pub exit_code: Field<StoredJsonInteger>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<MonitorTime>")]
    pub finished: Field<MonitorTime>,
    pub id: String,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<BTreeMap<String, Value>>")]
    pub input_attempt: Field<BTreeMap<String, Value>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "bool")]
    pub interactive: Option<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<StoredJsonInteger>")]
    pub last_exit_code: Field<StoredJsonInteger>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "MonitorText<0, 12000>")]
    pub liveness_command: Option<MonitorText<0, 12000>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "String")]
    pub log: Option<String>,
    #[serde(
        default,
        deserialize_with = "present_operation",
        skip_serializing_if = "Field::is_missing"
    )]
    #[schemars(with = "Option<MonitorOperationRecord>")]
    pub operation: Field<MonitorOperationRecord>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub rule_id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<MonitorInteger<0, 9007199254740991>>")]
    pub stall_probe_generation: Field<MonitorInteger<0, 9007199254740991>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<MonitorTime>")]
    pub stall_probe_started: Field<MonitorTime>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "MonitorInteger<0, 31536000>")]
    pub stall_timeout_seconds: Option<MonitorInteger<0, 31536000>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "MonitorInteger<-1, 9007199254740991>")]
    pub stall_wake_generation: Option<MonitorInteger<-1, 9007199254740991>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(schema_with = "status_schema")]
    pub status: Field<MonitorStatus>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub stdin_close_requested: Field<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "bool")]
    pub stdin_closed: Option<bool>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "MonitorExitCodes")]
    pub success_exit_codes: Option<MonitorExitCodes>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "MonitorText<0, 12000>")]
    pub tail: Option<MonitorText<0, 12000>>,
    #[serde(rename = "timeout_ms")]
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "MonitorInteger<1000, 86400000>")]
    pub timeout_ms: Option<MonitorInteger<1000, 86400000>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "MonitorWakeOn")]
    pub wake_on: Option<MonitorWakeOn>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
impl MonitorRecord {
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }
    pub fn id(&self) -> &String {
        &self.id
    }
    pub fn value(&self) -> Value {
        serde_json::to_value(self).expect("monitor fields are JSON serializable")
    }
    pub fn into_value(self) -> Value {
        self.value()
    }
    pub fn agent(&self) -> Option<&str> {
        self.agent.value().map(String::as_str)
    }
    pub fn status(&self) -> Option<MonitorStatus> {
        self.status.value().copied()
    }
}
pub(crate) fn schema() -> Value {
    schemars::schema_for!(MonitorRecord).to_value()
}
pub(crate) fn check_schema_parity() -> Result<(), String> {
    let stored: Value = serde_json::from_slice(include_bytes!(
        "schemas/persisted-monitor-record.schema.json"
    ))
    .map_err(|e| e.to_string())?;
    if stored != schema() {
        return Err("persisted monitor schema must match monitor_record::MonitorRecord".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn stored_monitor_schema_matches_all_field_shapes() {
        check_schema_parity().unwrap();
        let schema: Value = serde_json::from_slice(include_bytes!(
            "schemas/persisted-monitor-record.schema.json"
        ))
        .unwrap();
        let candidates = vec![
            json!(null),
            json!(true),
            json!(false),
            json!(""),
            json!("running"),
            json!("exit"),
            json!(0),
            json!(-1),
            json!(1),
            json!(1.0),
            json!(1.5),
            json!(9007199254740991u64),
            json!(9007199254740992u64),
            json!([]),
            json!([0, 255]),
            json!({}),
        ];
        for field in schema["properties"].as_object().unwrap().keys() {
            for candidate in &candidates {
                let mut value = json!({"id":"m"});
                value[field] = candidate.clone();
                let expected = crate::validate("PersistedMonitorRecord", &value).unwrap();
                let decoded = serde_json::from_value::<MonitorRecord>(value.clone());
                assert_eq!(decoded.is_ok(), expected, "{field}: {candidate}");
                if let Ok(record) = decoded {
                    assert_eq!(record.value(), value);
                }
            }
        }
    }
    #[test]
    fn operation_keeps_arbitrary_transport_evidence() {
        let value = json!({"id":"m","operation":{"agent":"a","epoch":1.0,"accountKey":"x","connectionId":"c","providerRequest":{"future":true},"threadId":17},"future":[1]});
        assert!(crate::validate("PersistedMonitorRecord", &value).unwrap());
        assert_eq!(
            serde_json::from_value::<MonitorRecord>(value.clone())
                .unwrap()
                .value(),
            value
        );
        for (key, candidate) in [
            ("agent", json!("")),
            ("connectionId", json!("x".repeat(201))),
            ("epoch", json!(-1)),
            ("epoch", json!(9007199254740992u64)),
        ] {
            let mut invalid = value.clone();
            invalid["operation"][key] = candidate;
            assert!(!crate::validate("PersistedMonitorRecord", &invalid).unwrap());
            assert!(serde_json::from_value::<MonitorRecord>(invalid).is_err());
        }
    }
}
