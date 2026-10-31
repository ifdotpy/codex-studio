//! Typed persisted plans and their generated schema.
use crate::queue_command::Field;
use crate::stored_record::StoredJsonInteger;
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use std::collections::BTreeMap;

/// A plan version is an integral JSON number within the safe-integer domain.
#[derive(Clone, Debug, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(transparent)]
struct PlanVersion(#[schemars(schema_with = "version_schema")] StoredJsonInteger);

fn version_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":"integer", "minimum":0, "maximum":crate::shared_values::MAX_SAFE_INTEGER})
}

fn updated_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"anyOf":[{"type":"integer","x-safe-integer":"unsigned"},{"type":"number"},{"type":"null"}]})
}

fn steps_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":"array","items":{}})
}

impl<'de> Deserialize<'de> for PlanVersion {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = StoredJsonInteger::deserialize(deserializer)?;
        let number = value
            .number()
            .as_f64()
            .ok_or_else(|| serde::de::Error::custom("invalid plan version"))?;
        if (0.0..=crate::shared_values::MAX_SAFE_INTEGER as f64).contains(&number) {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(
                "plan version outside safe-integer range",
            ))
        }
    }
}

fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PlanRecord {
    id: String,
    #[serde(rename = "rootId")]
    root_id: String,
    text: String,
    version: PlanVersion,
    #[serde(deserialize_with = "Option::<Number>::deserialize")]
    #[schemars(required, schema_with = "updated_schema")]
    updated: Option<Number>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    agent: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<BTreeMap<String, Value>>")]
    native: Field<BTreeMap<String, Value>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(schema_with = "steps_schema")]
    steps: Option<Vec<Value>>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl PlanRecord {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }
    pub fn value(&self) -> Value {
        // Every member is already JSON data and the typed scalars are serializable.
        serde_json::to_value(self).expect("plan fields are JSON serializable")
    }
    pub fn into_value(self) -> Value {
        self.value()
    }
    pub fn update_native(&mut self, native: Value, updated: f64) -> Result<(), serde_json::Error> {
        let steps = native
            .get("plan")
            .map(Deserialize::deserialize)
            .transpose()?
            .unwrap_or_default();
        let native: Option<BTreeMap<String, Value>> = Deserialize::deserialize(native)?;
        self.native = native.map_or(Field::Null, Field::Value);
        self.steps = Some(steps);
        self.updated = Number::from_f64(updated);
        Ok(())
    }
}

pub(crate) fn schema() -> Value {
    schemars::schema_for!(PlanRecord).to_value()
}
pub(crate) fn check_schema_parity() -> Result<(), String> {
    let stored: Value =
        serde_json::from_slice(include_bytes!("schemas/persisted-plan-record.schema.json"))
            .map_err(|error| error.to_string())?;
    if stored != schema() {
        return Err("persisted plan schema must be generated from plan_record::PlanRecord".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_plan_updates_keep_opaque_data_and_require_array_steps() {
        let mut record = PlanRecord::from_value(
            json!({"id":"a","rootId":"r","text":"x","version":1,"updated":null}),
        )
        .unwrap();
        let native = json!({"plan":[{"future":[1.0,null]}],"extension":{"keep":true}});
        record.update_native(native.clone(), 2.0).unwrap();
        assert_eq!(record.value()["native"], native);
        assert_eq!(record.value()["steps"], json!([{"future":[1.0,null]}]));
        assert!(record.update_native(json!({"plan":null}), 3.0).is_err());
        record
            .update_native(json!({"extension":true}), 4.0)
            .unwrap();
        assert_eq!(record.value()["steps"], json!([]));
    }

    #[test]
    fn schema_and_typed_plan_agree_on_shapes_and_version_bounds() {
        check_schema_parity().unwrap();
        let base = json!({"id":"a","rootId":"r","text":"x","version":1.0,"updated":null});
        let mut cases = vec![(base.clone(), true)];
        for (field, value, valid) in [
            ("version", json!(-1), false),
            (
                "version",
                json!(crate::shared_values::MAX_SAFE_INTEGER),
                true,
            ),
            (
                "version",
                json!(crate::shared_values::MAX_SAFE_INTEGER + 1),
                false,
            ),
            ("version", json!(1.5), false),
            ("steps", Value::Null, false),
            ("steps", json!([{"future":true}]), true),
            ("native", Value::Null, true),
            ("native", json!([]), false),
        ] {
            let mut record = base.clone();
            record[field] = value;
            cases.push((record, valid));
        }
        let mut missing = base;
        missing.as_object_mut().unwrap().remove("updated");
        cases.push((missing, false));
        cases.push((json!(["a"]), false));
        for (value, valid) in cases {
            assert_eq!(
                serde_json::from_value::<PlanRecord>(value.clone()).is_ok(),
                valid,
                "{value}"
            );
            assert_eq!(
                crate::validate("PersistedPlanRecord", &value).unwrap(),
                valid,
                "{value}"
            );
        }
    }
}
