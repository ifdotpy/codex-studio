//! Presentation-only JSON ordering. Never changes workspace serde_json features.
use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{collections::HashMap, fmt};

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
pub(crate) enum OrderedJson {
    Object(OrderedObject),
    Array(Vec<OrderedJson>),
    Scalar(serde_json::Value),
}

pub(crate) struct OrderedObject(Vec<(String, OrderedJson)>);

impl<'de> Deserialize<'de> for OrderedObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor;
        impl<'de> Visitor<'de> for ObjectVisitor {
            type Value = OrderedObject;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a JSON object")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut entries: Vec<(String, OrderedJson)> = Vec::new();
                let mut positions: HashMap<String, usize> = HashMap::new();
                while let Some((key, value)) = map.next_entry::<String, OrderedJson>()? {
                    if let Some(&index) = positions.get(&key) {
                        entries[index].1 = value;
                    } else {
                        positions.insert(key.clone(), entries.len());
                        entries.push((key, value));
                    }
                }
                Ok(OrderedObject(entries))
            }
        }
        deserializer.deserialize_map(ObjectVisitor)
    }
}

impl Serialize for OrderedObject {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in &self.0 {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::OrderedJson;

    #[test]
    fn preserves_nested_order_and_last_duplicate_without_global_map_features() {
        let value: OrderedJson = serde_json::from_str(
            r#"{"z":[{"b":1,"a":2,"b":3}],"a":null,"z":[{"y":true,"x":1.5}]}"#,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"{"z":[{"y":true,"x":1.5}],"a":null}"#
        );
        let nested: OrderedJson = serde_json::from_str(r#"{"z":{"b":1,"a":2,"b":3}}"#).unwrap();
        assert_eq!(
            serde_json::to_string(&nested).unwrap(),
            r#"{"z":{"b":3,"a":2}}"#
        );
    }
}
