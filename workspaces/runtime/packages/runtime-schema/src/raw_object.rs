//! Single-pass JSON object decoding with opaque extension values.
use std::{collections::BTreeMap, fmt, marker::PhantomData};

use serde::de::{Error, MapAccess, Visitor};
use serde_json::value::RawValue;

/// Builder for named fields; return false without consuming an unknown value.
pub trait RawObjectFields<'de>: Default {
    type Output;
    fn field<M: MapAccess<'de>>(&mut self, name: &str, map: &mut M) -> Result<bool, M::Error>;
    fn finish<E: Error>(
        self,
        extensions: BTreeMap<String, Box<RawValue>>,
    ) -> Result<Self::Output, E>;
}

/// Decode known fields directly and retain extension values as exact JSON bytes.
pub fn deserialize<'de, D, B>(deserializer: D) -> Result<B::Output, D::Error>
where
    D: serde::Deserializer<'de>,
    B: RawObjectFields<'de>,
{
    struct Object<B>(PhantomData<B>);
    impl<'de, B: RawObjectFields<'de>> Visitor<'de> for Object<B> {
        type Value = B::Output;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a JSON object")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut fields = B::default();
            let mut extensions = BTreeMap::new();
            while let Some(name) = map.next_key::<String>()? {
                if !fields.field(&name, &mut map)? {
                    let value = map.next_value::<Box<RawValue>>()?;
                    if extensions.insert(name, value).is_some() {
                        return Err(M::Error::custom("duplicate extension field"));
                    }
                }
            }
            fields.finish(extensions)
        }
    }
    deserializer.deserialize_map(Object::<B>(PhantomData))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    struct Object {
        id: String,
        extensions: BTreeMap<String, Box<RawValue>>,
    }
    #[derive(Default)]
    struct Fields {
        id: Option<String>,
    }
    impl<'de> RawObjectFields<'de> for Fields {
        type Output = Object;
        fn field<M: MapAccess<'de>>(&mut self, name: &str, map: &mut M) -> Result<bool, M::Error> {
            if name == "id" {
                if self.id.is_some() {
                    return Err(M::Error::duplicate_field("id"));
                }
                self.id = Some(map.next_value()?);
                return Ok(true);
            }
            Ok(false)
        }
        fn finish<E: Error>(
            self,
            extensions: BTreeMap<String, Box<RawValue>>,
        ) -> Result<Object, E> {
            Ok(Object {
                id: self.id.ok_or_else(|| E::missing_field("id"))?,
                extensions,
            })
        }
    }
    impl<'de> Deserialize<'de> for Object {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            super::deserialize::<D, Fields>(deserializer)
        }
    }
    #[test]
    fn decodes_known_fields_and_preserves_extension_bytes() {
        let object: Object =
            serde_json::from_str(r#"{"id":"i","provider":{"z":  1,"a":2}}"#).unwrap();
        assert_eq!(object.id, "i");
        assert_eq!(object.extensions["provider"].get(), r#"{"z":  1,"a":2}"#);
        assert!(serde_json::from_str::<Object>(r#"{"id":"i","id":"j"}"#).is_err());
        assert!(serde_json::from_str::<Object>(r#"{"id":"i","x":1,"x":2}"#).is_err());
    }
}
