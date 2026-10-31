//! Length-checked text shared by persisted and ingress contracts.
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct BoundedText<const MIN: usize, const MAX: usize>(String);
impl<const MIN: usize, const MAX: usize> BoundedText<MIN, MAX> {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<const MIN: usize, const MAX: usize> TryFrom<String> for BoundedText<MIN, MAX> {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if (MIN..=MAX).contains(&value.chars().count()) {
            Ok(Self(value))
        } else {
            Err("invalid bounded text length")
        }
    }
}
impl<'de, const MIN: usize, const MAX: usize> Deserialize<'de> for BoundedText<MIN, MAX> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::try_from(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
impl<const MIN: usize, const MAX: usize> schemars::JsonSchema for BoundedText<MIN, MAX> {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        format!("BoundedText{MIN}_{MAX}").into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        if MIN == 0 {
            schemars::json_schema!({"type":"string","maxLength":MAX})
        } else {
            schemars::json_schema!({"type":"string","minLength":MIN,"maxLength":MAX})
        }
    }
}
