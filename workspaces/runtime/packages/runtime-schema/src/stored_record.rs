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
