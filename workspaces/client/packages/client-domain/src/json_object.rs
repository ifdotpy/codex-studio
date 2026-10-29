//! Require a JSON object when deserializing a type that also accepts sequences.

use serde::{Deserialize, Serialize};

#[derive(Serialize)]
#[serde(transparent)]
pub struct JsonObject<T>(pub T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for JsonObject<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<T>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for ObjectVisitor<T> {
            type Value = JsonObject<T>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an object")
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                map: M,
            ) -> Result<Self::Value, M::Error> {
                T::deserialize(serde::de::value::MapAccessDeserializer::new(map)).map(JsonObject)
            }
        }
        deserializer.deserialize_map(ObjectVisitor(std::marker::PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize, Serialize)]
    struct Borrowed<'a> {
        value: &'a str,
    }

    #[test]
    fn requires_object_and_preserves_borrowed_fields_and_serialization() {
        let input = r#"{"value":"hello"}"#;
        let JsonObject(value): JsonObject<Borrowed<'_>> =
            serde_json::from_str(input).expect("object");
        assert_eq!(value.value, "hello");
        assert_eq!(value.value.as_ptr(), input[10..].as_ptr());
        assert_eq!(
            serde_json::to_string(&JsonObject(value)).expect("serialize"),
            input
        );
        assert!(serde_json::from_str::<JsonObject<Borrowed<'_>>>(r#"["hello"]"#).is_err());
    }
}
