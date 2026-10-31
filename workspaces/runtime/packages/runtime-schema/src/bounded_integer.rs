//! Bounded integral values for persisted records.
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct BoundedInteger<const MIN: i64, const MAX: u64>(i64);
impl<const MIN: i64, const MAX: u64> BoundedInteger<MIN, MAX> {
    pub fn get(self) -> i64 {
        self.0
    }
}
impl<const MIN: i64, const MAX: u64> TryFrom<i64> for BoundedInteger<MIN, MAX> {
    type Error = &'static str;
    fn try_from(n: i64) -> Result<Self, Self::Error> {
        if n >= MIN && (n as i128) <= MAX as i128 {
            Ok(Self(n))
        } else {
            Err("integer outside bounds")
        }
    }
}
impl<'de, const MIN: i64, const MAX: u64> Deserialize<'de> for BoundedInteger<MIN, MAX> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::try_from(i64::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
impl<const MIN: i64, const MAX: u64> schemars::JsonSchema for BoundedInteger<MIN, MAX> {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        format!("BoundedInteger{MIN}_{MAX}").into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type":"integer","minimum":MIN,"maximum":MAX})
    }
}
