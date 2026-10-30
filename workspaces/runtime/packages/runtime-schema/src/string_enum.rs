//! Deserialize a string-only enum using the enum's own wire names.
use serde::{Deserialize, Deserializer};

pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    let value = String::deserialize(d)?;
    T::deserialize(serde::de::value::StringDeserializer::<D::Error>::new(value))
}
