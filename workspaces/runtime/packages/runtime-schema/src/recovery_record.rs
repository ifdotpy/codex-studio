//! Typed recovery records. The persisted JSON schema owns wire compatibility;
//! unknown extensions remain opaque and original records remain with callers.
use crate::shared_values::MAX_SAFE_INTEGER;
use crate::stored_record::StoredJsonInteger;
use serde::{Deserialize, Deserializer, de::Error};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(transparent)]
pub struct SafeInteger(StoredJsonInteger);
#[derive(Clone, Debug)]
pub struct NonnegativeInteger(f64);
#[derive(Clone, Debug)]
pub struct NonnegativeNumber(f64);

impl SafeInteger {
    pub fn get(&self) -> f64 {
        // The constructor accepts only JSON numbers within the finite safe range.
        self.0.number().as_f64().expect("validated finite number")
    }
    pub fn as_u64(&self) -> Option<u64> {
        self.0.number().as_u64()
    }
    pub fn number(&self) -> &serde_json::Number {
        self.0.number()
    }
}
impl NonnegativeInteger {
    pub fn get(&self) -> f64 {
        self.0
    }
}
impl NonnegativeNumber {
    pub fn get(&self) -> f64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for SafeInteger {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let n = StoredJsonInteger::deserialize(d)?;
        if n.number()
            .as_f64()
            .is_some_and(|value| (0.0..=MAX_SAFE_INTEGER as f64).contains(&value))
        {
            Ok(Self(n))
        } else {
            Err(D::Error::custom("invalid safe integer"))
        }
    }
}
impl<'de> Deserialize<'de> for NonnegativeInteger {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let n = f64::deserialize(d)?;
        if n >= 0.0 && n.fract() == 0.0 {
            Ok(Self(n))
        } else {
            Err(D::Error::custom("invalid nonnegative integer"))
        }
    }
}
impl<'de> Deserialize<'de> for NonnegativeNumber {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let n = f64::deserialize(d)?;
        if n >= 0.0 {
            Ok(Self(n))
        } else {
            Err(D::Error::custom("invalid nonnegative number"))
        }
    }
}
fn nonempty<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let s = String::deserialize(d)?;
    if s.is_empty() {
        Err(D::Error::custom("empty id"))
    } else {
        Ok(s)
    }
}
fn optional_nonnull<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

fn nullable_string_enum<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    Option::<String>::deserialize(d)?
        .map(|value| {
            crate::string_enum::deserialize(serde::de::value::StringDeserializer::<D::Error>::new(
                value,
            ))
        })
        .transpose()
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum RecoverySource {
    Text(String),
    Fields(Box<RecoveryFields>),
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RequestState {
    Pending,
    Result,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoverySupervisorIdentity {
    pub generation: Option<SafeInteger>,
    pub handle: Option<String>,
    pub state_dir: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryFields {
    pub account_key: Option<String>,
    pub active_at_reservation: Option<bool>,
    pub agent: Option<String>,
    pub auto_wake: Option<bool>,
    pub automatic: Option<bool>,
    pub connection_id: Option<String>,
    pub created: Option<NonnegativeNumber>,
    pub epoch: Option<SafeInteger>,
    pub error: Option<Value>,
    pub events: Option<Vec<String>>,
    pub execution_outcome: Option<String>,
    pub id: Option<String>,
    pub new_thread_id: Option<String>,
    pub observed_turn_id: Option<String>,
    pub outcome: Option<String>,
    pub phase: Option<String>,
    pub source: Option<RecoverySource>,
    pub stage: Option<String>,
    pub start_attempt: Option<Box<RecoveryFields>>,
    pub submitted: Option<bool>,
    pub supervisor_identity: Option<RecoverySupervisorIdentity>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionRecoveryFields {
    pub request_basis: Option<Value>,
    pub request_id: Option<String>,
    pub request_receipt: Option<Value>,
    #[serde(default, deserialize_with = "nullable_string_enum")]
    pub request_state: Option<RequestState>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryAgentRecord {
    pub account_history: Option<Vec<BTreeMap<String, Value>>>,
    pub account_key: Option<String>,
    pub active_at_reservation: Option<bool>,
    pub active_tools: Option<Vec<Value>>,
    pub agent: Option<String>,
    pub auto_wake: Option<bool>,
    pub automatic: Option<bool>,
    pub connection_id: Option<String>,
    pub connection_recovery: Option<ConnectionRecoveryFields>,
    pub context_repair: Option<Box<RecoveryFields>>,
    pub context_repair_wait: Option<Box<RecoveryFields>>,
    pub created: Option<NonnegativeNumber>,
    pub disconnect_recovery: Option<Box<RecoveryFields>>,
    pub epoch: SafeInteger,
    pub error: Option<Value>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub events: Option<NonnegativeInteger>,
    pub execution_outcome: Option<String>,
    #[serde(deserialize_with = "nonempty")]
    id: String,
    pub in_flight: Option<bool>,
    pub native_failure_hold: Option<bool>,
    pub native_thread_block: Option<Box<RecoveryFields>>,
    pub native_turn_error: Option<Box<RecoveryFields>>,
    pub new_thread_id: Option<String>,
    pub observed_turn_id: Option<String>,
    pub outcome: Option<String>,
    pub phase: Option<String>,
    pub provider: Option<String>,
    pub restart_recovery: Option<Box<RecoveryFields>>,
    pub root_id: String,
    pub source: Option<RecoverySource>,
    pub stage: Option<String>,
    pub start_attempt: Option<Box<RecoveryFields>>,
    pub start_outcome_hold: Option<Box<RecoveryFields>>,
    pub status: String,
    pub submitted: Option<bool>,
    pub supervisor_identity: Option<RecoverySupervisorIdentity>,
    pub thread_id: Option<String>,
    pub turn_epoch: Option<SafeInteger>,
    pub turn_id: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl RecoveryAgentRecord {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn extensions(&self) -> &BTreeMap<String, Value> {
        &self.extra
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sha2::{Digest, Sha256};

    #[test]
    fn safe_integer_retains_numeric_encoding() {
        let integer: SafeInteger = serde_json::from_str("1").expect("integer");
        let decimal: SafeInteger = serde_json::from_str("1.0").expect("integer alias");
        assert_eq!(integer.get(), decimal.get());
        assert_eq!(integer.as_u64(), Some(1));
        assert_eq!(decimal.as_u64(), None);
    }

    #[test]
    fn request_state_requires_string_or_null() {
        for spelling in ["pending", "result"] {
            let mut record = json!({"id":"a","rootId":"a","epoch":1,"status":"running",
                "connectionRecovery":{"requestState":spelling}});
            assert!(serde_json::from_value::<RecoveryAgentRecord>(record.clone()).is_ok());
            record["connectionRecovery"]["requestState"] = json!({spelling:null});
            assert!(!crate::validate("RecoveryAgentRecord", &record).expect("schema"));
            assert!(serde_json::from_value::<RecoveryAgentRecord>(record).is_err());
        }
        let mut record = json!({"id":"a","rootId":"a","epoch":1,"status":"running",
            "connectionRecovery":{"requestState":null}});
        assert!(serde_json::from_value::<RecoveryAgentRecord>(record.clone()).is_ok());
        record["connectionRecovery"]
            .as_object_mut()
            .expect("object")
            .remove("requestState");
        assert!(serde_json::from_value::<RecoveryAgentRecord>(record).is_ok());
    }

    #[test]
    fn persisted_schema_and_typed_owner_remain_in_sync() {
        let schema_text = include_str!("schemas/recovery-agent-record.schema.json");
        assert_eq!(
            data_encoding::HEXLOWER.encode(&Sha256::digest(schema_text.as_bytes())),
            "6794f8060eec331c2aca3f7e93101e89db89866b08a1fd3695ff8a51ca620816",
            "recovery-agent-record.schema.json changed: review the typed owner and parity cases"
        );
        let schema: Value = serde_json::from_str(schema_text).expect("schema");
        let samples = [
            json!(null),
            json!(true),
            json!(false),
            json!(""),
            json!("a"),
            json!(-1),
            json!(0),
            json!(1),
            json!(1.0),
            json!(0.5),
            json!(9007199254740991_u64),
            json!(9007199254740992_u64),
            json!([]),
            json!(["a"]),
            json!([1]),
            json!({}),
            json!({"extra":true}),
        ];
        for (schema_object, container) in [
            (&schema, None),
            (&schema["$defs"]["RecoveryFields"], Some("startAttempt")),
            (
                &schema["$defs"]["RecoverySupervisorIdentity"],
                Some("supervisorIdentity"),
            ),
            (
                &schema["$defs"]["ConnectionRecoveryFields"],
                Some("connectionRecovery"),
            ),
        ] {
            for field in schema_object["properties"]
                .as_object()
                .expect("properties")
                .keys()
            {
                for sample in &samples {
                    let mut record = json!({"id":"a", "rootId":"a", "epoch":1, "status":"running"});
                    if let Some(container) = container {
                        record[container] = json!({field:sample});
                    } else {
                        record[field] = sample.clone();
                    }
                    let schema_accepts =
                        crate::validate("RecoveryAgentRecord", &record).expect("validator");
                    let typed_accepts =
                        serde_json::from_value::<RecoveryAgentRecord>(record.clone()).is_ok();
                    assert_eq!(
                        typed_accepts, schema_accepts,
                        "{container:?} {field}: {record}"
                    );
                }
            }
        }
    }
}
