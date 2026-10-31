//! Typed persisted continuation snapshot. The snapshot schema owns the wire shape.
use crate::queue_command::Field;
use crate::{
    json_object::JsonObject, provider_kind::ProviderKind, recovery_record::SafeInteger,
    shared_values::MAX_SAFE_INTEGER,
};
use serde::{Deserialize, Deserializer, Serialize, de::Error};
use serde_json::{Number, Value};
use std::{borrow::Cow, collections::BTreeMap};

/// A JSON string limited in Unicode code points.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct BoundedString<const MIN: usize, const MAX: usize>(String);
impl<const MIN: usize, const MAX: usize> BoundedString<MIN, MAX> {
    pub fn new(value: String) -> Option<Self> {
        (MIN..=MAX)
            .contains(&value.chars().count())
            .then_some(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn as_string(&self) -> &String {
        &self.0
    }
}
impl<const MIN: usize, const MAX: usize> schemars::JsonSchema for BoundedString<MIN, MAX> {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        format!("ContinuationString{MIN}_{MAX}").into()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        <crate::bounded_text::BoundedText<MIN, MAX> as schemars::JsonSchema>::json_schema(generator)
    }
}
impl<'de, const MIN: usize, const MAX: usize> Deserialize<'de> for BoundedString<MIN, MAX> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(d)?)
            .ok_or_else(|| D::Error::custom("string length out of range"))
    }
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct BoundedVec<T, const MAX: usize>(Vec<T>);
impl<T, const MAX: usize> BoundedVec<T, MAX> {
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }
}
impl<T: schemars::JsonSchema, const MAX: usize> schemars::JsonSchema for BoundedVec<T, MAX> {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        format!("ContinuationVec{MAX}_{}", T::schema_name()).into()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type":"array","maxItems":MAX,"items":generator.subschema_for::<T>()})
    }
}
impl<'de, T: Deserialize<'de>, const MAX: usize> Deserialize<'de> for BoundedVec<T, MAX> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Vec::<T>::deserialize(d)?;
        if v.len() <= MAX {
            Ok(Self(v))
        } else {
            Err(D::Error::custom("too many items"))
        }
    }
}
/// Nonnegative time, bounded to the shared safe integer maximum.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct BoundedNumber(Number);
impl BoundedNumber {
    pub fn get(&self) -> f64 {
        self.0.as_f64().expect("bounded JSON number")
    }

    pub fn from_number(number: Number) -> Option<Self> {
        number
            .as_f64()
            .filter(|n| (0.0..=MAX_SAFE_INTEGER as f64).contains(n))
            .map(|_| Self(number))
    }

    pub fn from_f64(value: f64) -> Option<Self> {
        Self::from_number(Number::from_f64(value)?)
    }
}
impl schemars::JsonSchema for BoundedNumber {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        "ContinuationNumber".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type":"number","minimum":0,"maximum":MAX_SAFE_INTEGER})
    }
}
impl<'de> Deserialize<'de> for BoundedNumber {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::from_number(Number::deserialize(d)?)
            .ok_or_else(|| D::Error::custom("number out of range"))
    }
}
fn optional_nonnull<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
fn object<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    JsonObject::<T>::deserialize(d).map(|value| value.0)
}
fn optional_string_enum<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    crate::string_enum::deserialize(d).map(Some)
}
pub type ContinuationSnapshotRecord = JsonObject<ContinuationSnapshot>;
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContinuationRecord {
    Usage(UsageContinuation),
    Capacity(CapacityContinuation),
}
impl ContinuationRecord {
    pub fn id(&self) -> &str {
        match self {
            Self::Usage(record) => record.id.as_str(),
            Self::Capacity(record) => record.id.as_str(),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContinuationSnapshot {
    pub agent: ContinuationAgent,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub ancestors: Option<BoundedVec<ContinuationAgent, 64>>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub claims: Option<BoundedVec<BoundedString<1, 4096>, 4096>>,
    pub record: Option<ContinuationRecord>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapacityContinuation {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<BoundedString<1, 4096>>")]
    pub accepted_turn_id: Field<BoundedString<1, 4096>>,
    pub account_key: BoundedString<1, 4096>,
    #[schemars(with = "crate::rule_record::RuleInteger<0, 9007199254740991>")]
    pub attempt: SafeInteger,
    #[serde(deserialize_with = "crate::string_enum::deserialize")]
    pub cause: CapacityContinuationCause,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<BoundedNumber>")]
    pub claimed_at: Field<BoundedNumber>,
    pub cwd: BoundedString<0, 1048576>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<BoundedNumber>")]
    pub due_at: Field<BoundedNumber>,
    #[schemars(with = "crate::rule_record::RuleInteger<0, 9007199254740991>")]
    pub epoch: SafeInteger,
    pub id: BoundedString<1, 4096>,
    #[schemars(with = "crate::rule_record::RuleInteger<0, 9007199254740991>")]
    pub max_attempts: SafeInteger,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<BoundedString<0, 1048576>>")]
    pub reason: Field<BoundedString<0, 1048576>>,
    #[serde(deserialize_with = "object")]
    pub settings: PreparationSettings,
    #[serde(deserialize_with = "crate::string_enum::deserialize")]
    pub status: CapacityContinuationStatus,
    #[serde(
        default,
        deserialize_with = "optional_nonnull",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "BoundedVec<BoundedString<1, 4096>, 4096>")]
    pub task_claims: Option<BoundedVec<BoundedString<1, 4096>, 4096>>,
    pub thread_id: BoundedString<1, 4096>,
    pub turn_id: BoundedString<1, 4096>,
    #[serde(
        default,
        deserialize_with = "optional_nonnull",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "crate::rule_record::RuleInteger<0, 9007199254740991>")]
    pub waits: Option<SafeInteger>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<BoundedNumber>")]
    pub updated_at: Field<BoundedNumber>,
}

/// Exact settings captured for a capacity retry.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreparationSettings {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub model: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub effort: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub native_effort: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<bool>")]
    pub fast_mode: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<bool>")]
    pub yolo_mode: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub profile_instructions: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub role: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<bool>")]
    pub daybreak_enabled: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub cyber_access_program: Field<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuationAgent {
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub account_key: Option<BoundedString<1, 4096>>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub auth_resume_attempt: Option<SafeInteger>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub auto_wake: Option<bool>,
    pub capacity_retry: Option<CapacityContinuation>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub capacity_retry_count: Option<SafeInteger>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub cwd: Option<BoundedString<0, 1048576>>,
    pub deleted_at: Option<BoundedNumber>,
    pub epoch: SafeInteger,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub excluded_account_keys: Option<BoundedVec<BoundedString<1, 4096>, 4096>>,
    pub id: BoundedString<1, 4096>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub in_flight: Option<bool>,
    pub last_completed_turn: Option<BoundedString<1, 4096>>,
    pub last_completed_turn_status: Option<String>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub native_failure_hold: Option<bool>,
    pub parent_id: Option<BoundedString<1, 4096>>,
    #[serde(deserialize_with = "crate::string_enum::deserialize")]
    pub provider: ProviderKind,
    pub root_id: BoundedString<1, 4096>,
    pub status: BoundedString<1, 4096>,
    pub thread_id: Option<BoundedString<1, 4096>>,
    pub turn_epoch: Option<SafeInteger>,
    pub turn_id: Option<BoundedString<1, 4096>>,
    pub usage_resume: Option<UsageContinuation>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub usage_resume_enabled: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageContinuation {
    pub account_key: BoundedString<1, 4096>,
    #[serde(
        default,
        deserialize_with = "optional_nonnull",
        skip_serializing_if = "Option::is_none"
    )]
    pub auth_attempt: Option<SafeInteger>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub auth_refresh_marker: Field<BoundedString<0, 1048576>>,
    #[serde(
        default,
        deserialize_with = "optional_string_enum",
        skip_serializing_if = "Option::is_none"
    )]
    pub cause: Option<UsageContinuationCause>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub due_at: Field<BoundedNumber>,
    pub epoch: SafeInteger,
    pub failed_at: BoundedNumber,
    pub id: BoundedString<1, 4096>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub planned_at: Field<BoundedNumber>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub proof_at: Field<BoundedNumber>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub reason: Field<BoundedString<0, 1048576>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub reset_at: Field<BoundedNumber>,
    #[serde(deserialize_with = "crate::string_enum::deserialize")]
    pub status: UsageContinuationStatus,
    #[serde(
        default,
        deserialize_with = "optional_nonnull",
        skip_serializing_if = "Option::is_none"
    )]
    pub task_claims: Option<BoundedVec<BoundedString<1, 4096>, 4096>>,
    pub thread_id: BoundedString<1, 4096>,
    pub turn_id: BoundedString<1, 4096>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub claimed_at: Field<BoundedNumber>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub accepted_turn_id: Field<BoundedString<1, 4096>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waits: Option<SafeInteger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<PreparationSettings>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub started_at: Field<BoundedNumber>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub updated_at: Field<BoundedNumber>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub last_checked_at: Field<BoundedNumber>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for_auth: Option<bool>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum CapacityContinuationCause {
    #[serde(rename = "serverOverloaded")]
    ServerOverloaded,
    #[serde(rename = "internalServerError")]
    InternalServerError,
    #[serde(rename = "httpConnectionFailed")]
    HttpConnectionFailed,
    #[serde(rename = "responseStreamConnectionFailed")]
    ResponseStreamConnectionFailed,
    #[serde(rename = "responseStreamDisconnected")]
    ResponseStreamDisconnected,
    #[serde(rename = "responseTooManyFailedAttempts")]
    ResponseTooManyFailedAttempts,
    #[serde(rename = "diskFull")]
    DiskFull,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum CapacityContinuationStatus {
    #[serde(rename = "scheduled")]
    Scheduled,
    #[serde(rename = "starting")]
    Starting,
    #[serde(rename = "finished")]
    Finished,
    #[serde(rename = "cancelled")]
    Cancelled,
    #[serde(rename = "unknown")]
    Unknown,
    #[serde(rename = "exhausted")]
    Exhausted,
    #[serde(rename = "failed")]
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, Deserialize)]
pub enum UsageContinuationCause {
    #[serde(rename = "usage_limit")]
    UsageLimit,
    #[serde(rename = "rate_limit")]
    RateLimit,
    #[serde(rename = "auth")]
    Auth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, Deserialize)]
pub enum UsageContinuationStatus {
    #[serde(rename = "scheduled")]
    Scheduled,
    #[serde(rename = "started")]
    Started,
    #[serde(rename = "cancelled")]
    Cancelled,
    #[serde(rename = "unknown")]
    Unknown,
}

pub(crate) fn capacity_schema() -> Value {
    schemars::schema_for!(CapacityContinuation).to_value()
}
pub(crate) fn check_capacity_schema_parity() -> Result<(), String> {
    let stored: Value = serde_json::from_slice(include_bytes!(
        "schemas/persisted-capacity-continuation.schema.json"
    ))
    .map_err(|e| e.to_string())?;
    if stored != capacity_schema() {
        return Err("capacity schema must be generated from CapacityContinuation".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn continuation_record_owners_preserve_numbers_and_refuse_capacity_extensions() {
        let usage = json!({"id":"u","accountKey":"a","threadId":"t","epoch":1.0,"turnId":"v","status":"scheduled","cause":"auth","failedAt":100,"dueAt":null,"proofAt":3.0,"extension":{"a":[1,null]}});
        let capacity = json!({"id":"c","accountKey":"a","threadId":"t","epoch":1.0,"turnId":"v","status":"scheduled","cause":"serverOverloaded","cwd":"","settings":{"model":"example","fastMode":false},"attempt":1.0,"maxAttempts":3,"claimedAt":null});
        let u = UsageContinuation::deserialize(usage.clone()).unwrap();
        let c = CapacityContinuation::deserialize(capacity.clone()).unwrap();
        assert_eq!(json!(u), usage);
        assert_eq!(json!(c), capacity);
        let mut invalid = capacity;
        invalid["extension"] = json!({"a":[1,null]});
        assert!(CapacityContinuation::deserialize(invalid).is_err());
    }

    #[test]
    fn continuation_schema_parity() {
        let text = include_str!("schemas/persisted-continuation-snapshot.schema.json");
        assert_eq!(
            capacity_schema(),
            serde_json::from_str::<Value>(include_str!(
                "schemas/persisted-capacity-continuation.schema.json"
            ))
            .expect("capacity schema")
        );
        let openapi: Value = serde_json::from_slice(crate::OPENAPI).expect("openapi");
        let registry = crate::registry_for_document(&openapi).expect("schema registry");
        let schema: Value = serde_json::from_str(text).expect("schema");
        let samples = [
            json!(null),
            json!(true),
            json!(false),
            json!(""),
            json!("x"),
            json!(-1),
            json!(0),
            json!(1),
            json!(1.0),
            json!(0.5),
            json!(9007199254740991_u64),
            json!(9007199254740992_u64),
            json!([]),
            json!(["x"]),
            json!([1]),
            json!({}),
            json!({"extra":true}),
        ];
        type Parse = fn(Value) -> bool;
        let cases: Vec<(&str, Value, Parse)> = vec![
            (
                "ROOT",
                json!({"agent":{"id":"x","rootId":"x","epoch":0,"status":"x","provider":"codex"}}),
                |v| serde_json::from_value::<ContinuationSnapshotRecord>(v).is_ok(),
            ),
            (
                "CapacityContinuation",
                json!({"id":"x","accountKey":"x","threadId":"x","epoch":0,"turnId":"x","status":"scheduled","cause":"serverOverloaded","cwd":"","settings":{},"attempt":0,"maxAttempts":0}),
                |v| serde_json::from_value::<CapacityContinuation>(v).is_ok(),
            ),
            (
                "ContinuationAgent",
                json!({"id":"x","rootId":"x","epoch":0,"status":"x","provider":"codex"}),
                |v| serde_json::from_value::<ContinuationAgent>(v).is_ok(),
            ),
            (
                "UsageContinuation",
                json!({"id":"x","accountKey":"x","threadId":"x","epoch":0,"turnId":"x","status":"scheduled","failedAt":0}),
                |v| serde_json::from_value::<UsageContinuation>(v).is_ok(),
            ),
        ];
        let mut enum_objects_accepted = Vec::new();
        for (name, base, parse) in cases {
            let target = if name == "ROOT" {
                schema.clone()
            } else if name == "CapacityContinuation" {
                capacity_schema()
            } else {
                json!({"$defs":schema["$defs"], "$ref":format!("#/$defs/{name}")})
            };
            let validator = jsonschema::options()
                .with_registry(&registry)
                .build(&target)
                .expect("validator");
            assert!(
                validator.is_valid(&base),
                "invalid test baseline {name}: {base}"
            );
            assert!(
                parse(base.clone()),
                "typed baseline rejected {name}: {base}"
            );
            let props = if name == "ROOT" {
                &schema["properties"]
            } else if name == "CapacityContinuation" {
                &target["properties"]
            } else {
                &schema["$defs"][name]["properties"]
            };
            for key in props.as_object().expect("properties").keys() {
                let mut absent = base.clone();
                absent.as_object_mut().expect("object").remove(key);
                assert_eq!(
                    parse(absent.clone()),
                    validator.is_valid(&absent),
                    "{name} missing {key}: {absent}"
                );
                for sample in &samples {
                    let mut input = base.clone();
                    input[key] = sample.clone();
                    assert_eq!(
                        parse(input.clone()),
                        validator.is_valid(&input),
                        "{name} {key}: {input}"
                    );
                }
                let variants = props[key]["enum"].as_array().or_else(|| {
                    (props[key]["$ref"] == "#/$defs/ProviderKind")
                        .then(|| schema["$defs"]["ProviderKind"]["enum"].as_array())
                        .flatten()
                });
                if let Some(variants) = variants {
                    for variant in variants {
                        let mut input = base.clone();
                        input[key] = variant.clone();
                        assert!(validator.is_valid(&input));
                        assert!(parse(input.clone()), "{name} {key}: {input}");
                        let spelling = variant.as_str().expect("string enum spelling");
                        input[key] = json!({spelling:null});
                        assert!(!validator.is_valid(&input));
                        if parse(input.clone()) {
                            enum_objects_accepted.push(format!("{name}.{key}: {input}"));
                        }
                    }
                }
                if let Some(spelling) = props[key]["const"].as_str() {
                    let mut input = base.clone();
                    input[key] = json!({spelling:null});
                    assert!(!validator.is_valid(&input));
                    if parse(input.clone()) {
                        enum_objects_accepted.push(format!("{name}.{key}: {input}"));
                    }
                }
            }
            for input in [json!([]), json!({"unknown":[1,null]}), json!(null)] {
                assert_eq!(
                    parse(input.clone()),
                    validator.is_valid(&input),
                    "{name}: {input}"
                );
            }
        }
        assert!(
            enum_objects_accepted.is_empty(),
            "accepted enum objects: {enum_objects_accepted:?}"
        );
    }
}
