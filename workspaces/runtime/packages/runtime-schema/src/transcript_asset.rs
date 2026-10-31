//! Typed persisted transcript assets and their generated storage schema.
use crate::raw_object::{self, RawObjectFields};
use crate::stored_record::StoredJsonInteger;
use serde::de::{Error, MapAccess};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
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

#[derive(Clone, Debug, Serialize, schemars::JsonSchema)]
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
    #[schemars(with = "BTreeMap<String, Value>")]
    extra: BTreeMap<String, Box<RawValue>>,
}

#[derive(Default)]
struct AssetFields {
    id: Option<TranscriptAssetId>,
    agent: Option<String>,
    name: Option<String>,
    path: Option<String>,
    mime: Option<String>,
    image: Option<bool>,
    size: Option<StoredJsonInteger>,
    hash: Option<String>,
    created: Option<Number>,
}

macro_rules! asset_field {
    ($self:ident, $map:ident, $name:ident, $($wire:literal => $field:ident),+ $(,)?) => {
        match $name {
            $( $wire => {
                if $self.$field.is_some() { return Err(serde::de::Error::duplicate_field($wire)); }
                $self.$field = Some($map.next_value()?);
                Ok(true)
            }, )+
            _ => Ok(false),
        }
    };
}

impl<'de> RawObjectFields<'de> for AssetFields {
    type Output = TranscriptAssetRecord;
    fn field<M: MapAccess<'de>>(&mut self, name: &str, map: &mut M) -> Result<bool, M::Error> {
        asset_field!(self, map, name, "id" => id, "agent" => agent, "name" => name,
            "path" => path, "mime" => mime, "image" => image, "size" => size,
            "hash" => hash, "created" => created)
    }
    fn finish<E: Error>(self, extra: BTreeMap<String, Box<RawValue>>) -> Result<Self::Output, E> {
        Ok(TranscriptAssetRecord {
            id: self.id.ok_or_else(|| E::missing_field("id"))?,
            agent: self.agent.ok_or_else(|| E::missing_field("agent"))?,
            name: self.name.ok_or_else(|| E::missing_field("name"))?,
            path: self.path.ok_or_else(|| E::missing_field("path"))?,
            mime: self.mime.ok_or_else(|| E::missing_field("mime"))?,
            image: self.image.ok_or_else(|| E::missing_field("image"))?,
            size: self.size.ok_or_else(|| E::missing_field("size"))?,
            hash: self.hash.ok_or_else(|| E::missing_field("hash"))?,
            created: self.created.ok_or_else(|| E::missing_field("created"))?,
            extra,
        })
    }
}

impl<'de> Deserialize<'de> for TranscriptAssetRecord {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        raw_object::deserialize::<D, AssetFields>(deserializer)
    }
}

/// Public asset metadata, excluding its storage path.
#[derive(Clone, Debug, Serialize)]
pub struct TranscriptAssetView {
    id: TranscriptAssetId,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<StoredJsonInteger>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created: Option<Number>,
    #[serde(flatten)]
    extra: BTreeMap<String, Box<RawValue>>,
}

#[derive(Default)]
struct ViewFields {
    id: Option<TranscriptAssetId>,
    agent: Option<String>,
    name: Option<String>,
    mime: Option<String>,
    image: Option<bool>,
    size: Option<StoredJsonInteger>,
    hash: Option<String>,
    created: Option<Number>,
    seen: std::collections::BTreeSet<String>,
}

impl<'de> RawObjectFields<'de> for ViewFields {
    type Output = TranscriptAssetView;
    fn field<M: MapAccess<'de>>(&mut self, name: &str, map: &mut M) -> Result<bool, M::Error> {
        if !matches!(
            name,
            "id" | "agent" | "name" | "mime" | "image" | "size" | "hash" | "created"
        ) {
            return Ok(false);
        }
        if !self.seen.insert(name.to_owned()) {
            return Err(M::Error::custom(format!("duplicate field {name}")));
        }
        match name {
            "id" => self.id = Some(map.next_value()?),
            "agent" => self.agent = Some(map.next_value()?),
            "name" => self.name = Some(map.next_value()?),
            "mime" => self.mime = Some(map.next_value()?),
            "image" => self.image = Some(map.next_value()?),
            "size" => self.size = Some(map.next_value()?),
            "hash" => self.hash = Some(map.next_value()?),
            "created" => self.created = Some(map.next_value()?),
            _ => unreachable!(),
        }
        Ok(true)
    }
    fn finish<E: Error>(self, extra: BTreeMap<String, Box<RawValue>>) -> Result<Self::Output, E> {
        Ok(TranscriptAssetView {
            id: self.id.ok_or_else(|| E::missing_field("id"))?,
            agent: self.agent,
            name: self.name,
            mime: self.mime,
            image: self.image,
            size: self.size,
            hash: self.hash,
            created: self.created,
            extra,
        })
    }
}

impl<'de> Deserialize<'de> for TranscriptAssetView {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        raw_object::deserialize::<D, ViewFields>(deserializer)
    }
}

impl TranscriptAssetView {
    pub fn id(&self) -> &str {
        self.id.as_ref()
    }
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
    pub fn into_view(self) -> TranscriptAssetView {
        TranscriptAssetView {
            id: self.id,
            agent: Some(self.agent),
            name: Some(self.name),
            mime: Some(self.mime),
            image: Some(self.image),
            size: Some(self.size),
            hash: Some(self.hash),
            created: Some(self.created),
            extra: self.extra,
        }
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
    fn asset_rejects_duplicate_opaque_fields() {
        let raw = r#"{"id":"a","agent":"owner","name":"n","path":"p","mime":"m","image":false,"size":1,"hash":"h","created":2,"future":1,"future":2}"#;
        assert!(serde_json::from_str::<TranscriptAssetRecord>(raw).is_err());
    }

    #[test]
    fn asset_views_keep_accepted_json_fields() {
        for raw in [
            r#"{"id":"a","future":{"z":  1}}"#,
            r#"{"id":"a","agent":"owner","name":"n","mime":"m","image":false,"size":1,"hash":"h","created":2}"#,
        ] {
            let view: TranscriptAssetView = serde_json::from_str(raw).unwrap();
            let before: Value = serde_json::from_str(raw).unwrap();
            let after: Value =
                serde_json::from_str(&serde_json::to_string(&view).unwrap()).unwrap();
            assert_eq!(before, after);
        }
        assert!(serde_json::from_str::<TranscriptAssetView>(r#"{"id":"a","name":null}"#).is_err());
    }

    #[test]
    fn asset_view_excludes_path_and_keeps_opaque_bytes() {
        let raw = r#"{"id":"a","agent":"owner","name":"n","path":"private","mime":"m","image":false,"size":1,"hash":"h","created":2,"future":{"x":  1}}"#;
        let record: TranscriptAssetRecord = serde_json::from_str(raw).unwrap();
        let view = serde_json::to_string(&record.into_view()).unwrap();
        assert!(!view.contains("private"));
        assert!(view.contains(r#""future":{"x":  1}"#));
    }

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
