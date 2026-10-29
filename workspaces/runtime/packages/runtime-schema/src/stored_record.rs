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

pub(crate) fn present_nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}
