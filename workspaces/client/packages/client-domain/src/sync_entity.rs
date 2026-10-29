//! Entity change envelopes shared by sync transport and the local cache.
use crate::MAX_SAFE_INTEGER;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

/// Maximum entity records requested per sync pull page.
pub const SYNC_ENTITY_PAGE_LIMIT: i64 = 500;

/// Maximum UTF-8 bytes in a local entity projection key.
pub const MAX_ENTITY_SCOPE_BYTES: usize = 512;

#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct EntityChange(EntityChangeKind);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum EntityChangeKind {
    Upsert {
        collection: Collection,
        id: EntityId,
        seq: EntitySequence,
        value: Map<String, Value>,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    Delete {
        collection: Collection,
        id: EntityId,
        seq: EntitySequence,
        #[serde(default, deserialize_with = "reject_value", skip_serializing)]
        value: (),
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
}
fn reject_value<'de, D: Deserializer<'de>>(_deserializer: D) -> Result<(), D::Error> {
    Err(serde::de::Error::custom("delete must omit value"))
}

#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct Collection(String);
impl<'de> Deserialize<'de> for Collection {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value.is_empty() || value.contains(':') {
            Err(serde::de::Error::custom("invalid collection"))
        } else {
            Ok(Self(value))
        }
    }
}
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct EntityId(String);
impl<'de> Deserialize<'de> for EntityId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value.is_empty() {
            Err(serde::de::Error::custom("invalid identity"))
        } else {
            Ok(Self(value))
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(transparent)]
pub struct EntitySequence(u64);
impl<'de> Deserialize<'de> for EntitySequence {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = u64::deserialize(deserializer)?;
        if value > MAX_SAFE_INTEGER as u64 {
            Err(serde::de::Error::custom("invalid sequence"))
        } else {
            Ok(Self(value))
        }
    }
}
impl<'de> Deserialize<'de> for EntityChange {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let change = Self(EntityChangeKind::deserialize(deserializer)?);
        if change.scope().len() > MAX_ENTITY_SCOPE_BYTES {
            return Err(serde::de::Error::custom("invalid projection identity"));
        }
        Ok(change)
    }
}
impl EntityChange {
    pub fn collection(&self) -> &str {
        match &self.0 {
            EntityChangeKind::Upsert { collection, .. }
            | EntityChangeKind::Delete { collection, .. } => &collection.0,
        }
    }
    pub fn id(&self) -> &str {
        match &self.0 {
            EntityChangeKind::Upsert { id, .. } | EntityChangeKind::Delete { id, .. } => &id.0,
        }
    }
    pub fn seq(&self) -> u64 {
        match &self.0 {
            EntityChangeKind::Upsert { seq, .. } | EntityChangeKind::Delete { seq, .. } => seq.0,
        }
    }
    pub fn value(&self) -> Option<&Map<String, Value>> {
        match &self.0 {
            EntityChangeKind::Upsert { value, .. } => Some(value),
            EntityChangeKind::Delete { .. } => None,
        }
    }
    pub fn scope(&self) -> String {
        format!("entity:{}:{}", self.collection(), self.id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn entity_changes_keep_opaque_payload_and_validate_boundaries() {
        for value in [
            json!({"op":"upsert","collection":"future","id":"a:b","seq":1,"value":{"nested":[null,{"x":true}]},"future":42}),
            json!({"op":"delete","collection":"future","id":"a","seq":2,"future":true}),
        ] {
            let change: EntityChange = serde_json::from_value(value.clone()).expect("valid change");
            assert_eq!(serde_json::to_value(&change).expect("serialize"), value);
            assert!(!change.scope().is_empty());
        }
        for value in [
            json!({"op":"delete","collection":"a","id":"b","seq":1,"value":null}),
            json!({"op":"upsert","collection":"a","id":"b","seq":1,"value":[]}),
            json!({"op":"upsert","collection":"a:b","id":"b","seq":1,"value":{}}),
            json!({"op":"delete","collection":"a","id":"","seq":1}),
            json!({"op":"delete","collection":"a","id":"b","seq":1.0}),
            json!({"op":"delete","collection":"a","id":"b","seq":MAX_SAFE_INTEGER as u64 + 1}),
        ] {
            assert!(
                serde_json::from_value::<EntityChange>(value.clone()).is_err(),
                "{value}"
            );
        }
        assert!(serde_json::from_value::<EntityChange>(json!({"op":"delete","collection":"a","id":"x".repeat(MAX_ENTITY_SCOPE_BYTES),"seq":0})).is_err());
    }

    #[test]
    fn shared_wire_fields_match_openapi_owner() {
        let openapi: Value = serde_json::from_str(include_str!(
            "../../../../runtime/packages/runtime-http/openapi.json"
        ))
        .expect("openapi");
        let schema = &openapi["components"]["schemas"]["SyncEntity"];
        assert_eq!(schema["required"], json!(["op", "collection", "id", "seq"]));
        assert_eq!(
            schema["properties"]["op"]["enum"],
            json!(["upsert", "delete"])
        );
        assert_eq!(schema["properties"]["collection"]["type"], "string");
        assert_eq!(schema["properties"]["id"]["type"], "string");
        assert_eq!(schema["properties"]["seq"]["type"], "integer");
        assert_eq!(schema["properties"]["seq"]["minimum"], 0);
        assert_eq!(schema["properties"]["seq"]["maximum"], MAX_SAFE_INTEGER);
        assert!(
            schema["oneOf"].as_array().expect("variants").iter().any(
                |v| v["properties"]["op"]["const"] == "upsert"
                    && v["required"]
                        .as_array()
                        .is_some_and(|r| r.contains(&json!("value")))
            )
        );
        assert!(
            schema["oneOf"]
                .as_array()
                .expect("variants")
                .iter()
                .any(|v| v["properties"]["op"]["const"] == "delete")
        );
    }
}
