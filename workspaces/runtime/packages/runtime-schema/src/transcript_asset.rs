//! Typed persisted transcript assets and their generated storage schema.
use crate::stored_record::StoredJsonInteger;
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct TranscriptAssetId(pub String);
impl From<String> for TranscriptAssetId {
    fn from(value: String) -> Self {
        Self(value)
    }
}
impl From<&str> for TranscriptAssetId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}
impl AsRef<str> for TranscriptAssetId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TranscriptAssetRecord {
    id: TranscriptAssetId,
    agent: String,
    name: String,
    path: String,
    mime: String,
    image: bool,
    #[schemars(schema_with = "size_schema")]
    size: StoredJsonInteger,
    hash: String,
    created: Number,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}
fn size_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":"integer","x-safe-integer":"unsigned"})
}
impl TranscriptAssetRecord {
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }
    pub fn id(&self) -> &TranscriptAssetId {
        &self.id
    }
    pub fn agent(&self) -> &str {
        &self.agent
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn mime(&self) -> &str {
        &self.mime
    }
    pub fn image(&self) -> bool {
        self.image
    }
    pub fn size(&self) -> &Number {
        self.size.number()
    }
    pub fn hash(&self) -> &str {
        &self.hash
    }
    pub fn created(&self) -> &Number {
        &self.created
    }
    pub fn retarget(&mut self, id: String, agent: String) {
        self.id = TranscriptAssetId(id);
        self.agent = agent;
    }
    pub fn into_value(self) -> Value {
        // Every field is already JSON data or a serializable scalar.
        serde_json::to_value(self).expect("asset fields are JSON serializable")
    }
}
pub(crate) fn schema() -> Value {
    schemars::schema_for!(TranscriptAssetRecord).to_value()
}
pub(crate) fn check_schema_parity() -> Result<(), String> {
    let stored: Value = serde_json::from_slice(include_bytes!(
        "schemas/runtime-transcript-asset.schema.json"
    ))
    .map_err(|error| error.to_string())?;
    if stored != schema() {
        return Err("persisted transcript asset schema must be generated from transcript_asset::TranscriptAssetRecord".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn asset_schema_and_serde_agree_including_integer_aliases() {
        check_schema_parity().unwrap();
        let base = json!({"id":"a","agent":"owner","name":"n","path":"p","mime":"m","image":false,"size":1.0,"hash":"h","created":2,"future":{"keep":true}});
        let mut cases = vec![(base.clone(), true)];
        for (field, value, valid) in [
            ("size", json!(-1), true),
            ("size", json!(u64::MAX), true),
            ("size", json!(1.5), false),
            ("image", json!(null), false),
            ("created", json!("bad"), false),
            ("agent", json!(null), false),
        ] {
            let mut record = base.clone();
            record[field] = value;
            cases.push((record, valid));
        }
        for field in [
            "id", "agent", "name", "path", "mime", "image", "size", "hash", "created",
        ] {
            let mut record = base.clone();
            record.as_object_mut().unwrap().remove(field);
            cases.push((record, false));
        }
        cases.push((json!(["a"]), false));
        for (value, valid) in cases {
            let record = serde_json::from_value::<TranscriptAssetRecord>(value.clone());
            assert_eq!(record.is_ok(), valid, "{value}");
            assert_eq!(
                crate::validate("PersistedTranscriptAsset", &value).unwrap(),
                valid,
                "{value}"
            );
            if let Ok(record) = record {
                assert_eq!(record.into_value(), value);
            }
        }
    }
}
