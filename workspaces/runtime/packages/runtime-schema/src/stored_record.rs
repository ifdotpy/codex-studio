//! Shared serde shapes for persisted records.
use serde::{Deserialize, Serialize};

/// A JSON Schema integer, preserving integral decimal spellings and large values.
#[derive(Clone, Debug, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct StoredJsonInteger(#[schemars(schema_with = "integer_schema")] serde_json::Number);

impl StoredJsonInteger {
    pub(crate) fn from_i64(value: i64) -> Self {
        Self(value.into())
    }

    pub(crate) fn number(&self) -> &serde_json::Number {
        &self.0
    }
}

fn integer_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type": "integer"})
}

impl<'de> Deserialize<'de> for StoredJsonInteger {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let number = serde_json::Number::deserialize(deserializer)?;
        if number.is_i64()
            || number.is_u64()
            || number.as_f64().is_some_and(|value| value.fract() == 0.0)
        {
            Ok(Self(number))
        } else {
            Err(serde::de::Error::custom("expected an integer"))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct AgentCounter(StoredJsonInteger);
impl AgentCounter {
    /// Original numeric spelling, retained for persisted wire compatibility.
    pub fn as_number(&self) -> &serde_json::Number {
        self.0.number()
    }
    /// Integer value after safe-range validation.
    pub fn as_i64(&self) -> i64 {
        // AgentCounter accepts only finite JSON numbers within the safe-integer range.
        self.0.number().as_f64().expect("finite agent counter") as i64
    }
}
impl<'de> Deserialize<'de> for AgentCounter {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let number = StoredJsonInteger::deserialize(deserializer)?;
        if number.number().as_f64().is_some_and(|value| {
            (0.0..=crate::shared_values::MAX_SAFE_INTEGER as f64).contains(&value)
        }) {
            Ok(Self(number))
        } else {
            Err(serde::de::Error::custom("integer outside safe range"))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct NonnegativeNumber(serde_json::Number);
impl NonnegativeNumber {
    pub fn number(&self) -> &serde_json::Number {
        &self.0
    }
}
impl<'de> Deserialize<'de> for NonnegativeNumber {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let number = serde_json::Number::deserialize(deserializer)?;
        if number.as_f64().is_some_and(|value| value >= 0.0) {
            Ok(Self(number))
        } else {
            Err(serde::de::Error::custom("number must be nonnegative"))
        }
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};

    #[test]
    fn presence_round_trips_missing_null_and_value_across_record_owners() {
        fn check<T: for<'de> Deserialize<'de> + Serialize>(
            mut base: Value,
            field: &str,
            item: Value,
        ) {
            for value in [None, Some(Value::Null), Some(item)] {
                let mut input = base.clone();
                if let Some(value) = value {
                    input[field] = value;
                }
                let record = T::deserialize(input.clone()).unwrap();
                assert_eq!(
                    record.serialize(serde_json::value::Serializer).unwrap(),
                    input
                );
            }
            base[field] = json!(false);
            assert!(T::deserialize(base).is_err());
        }
        for (field, value) in [
            ("agent", json!("a")),
            ("path", json!("p")),
            ("text", json!("t")),
            ("line", json!(1.0)),
            ("created", json!(1.5)),
        ] {
            check::<crate::annotation_record::AnnotationRecord>(json!({"id":"a"}), field, value);
        }
        for (field, value) in [("agent", json!("a")), ("created", json!(1.5))] {
            check::<crate::checkpoint_record::CheckpointRecord>(json!({"id":"c"}), field, value);
        }
        for field in ["author", "leadId", "status", "text"] {
            check::<crate::complaint_record::ComplaintRecord>(json!({"id":"c"}), field, json!("a"));
        }
        for (field, value) in [("agent", json!("a")), ("native", json!({"plan":[]}))] {
            check::<crate::plan_record::PlanRecord>(
                json!({"id":"p","rootId":"r","text":"","version":1,"updated":null}),
                field,
                value,
            );
        }
        for (field, value) in [
            ("agent", json!("a")),
            ("status", json!("running")),
            ("exitCode", json!(1.0)),
        ] {
            check::<crate::terminal_record::TerminalRecord>(json!({"id":"t"}), field, value);
        }
    }
}
