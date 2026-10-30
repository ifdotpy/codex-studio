//! Checked persisted transcript shells; provider extension fields stay opaque.
use crate::queue_command::Field;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct TranscriptItemId(pub String);
impl From<String> for TranscriptItemId {
    fn from(value: String) -> Self {
        Self(value)
    }
}
impl From<&str> for TranscriptItemId {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}
impl AsRef<str> for TranscriptItemId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Inline text or a legacy item whose content is held in the full-text store.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum TranscriptText {
    Inline(String),
    #[default]
    InFulltextMissing,
    InFulltextNull,
}

/// The stored transcript item roles.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptRole {
    Assistant,
    User,
    System,
    Output,
}
impl TranscriptRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Assistant => "assistant",
            Self::User => "user",
            Self::System => "system",
            Self::Output => "output",
        }
    }
}
impl TranscriptText {
    fn is_missing(&self) -> bool {
        matches!(self, Self::InFulltextMissing)
    }
    pub fn value(&self) -> Option<&String> {
        match self {
            Self::Inline(text) => Some(text),
            Self::InFulltextMissing | Self::InFulltextNull => None,
        }
    }
}
impl<'de> Deserialize<'de> for TranscriptText {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Option::<String>::deserialize(deserializer)? {
            Some(text) => Self::Inline(text),
            None => Self::InFulltextNull,
        })
    }
}
impl Serialize for TranscriptText {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Inline(text) => serializer.serialize_str(text),
            Self::InFulltextMissing | Self::InFulltextNull => serializer.serialize_none(),
        }
    }
}

#[derive(Clone, Debug, Serialize, schemars::JsonSchema)]
#[schemars(rename = "PersistedTranscriptInput")]
pub struct TranscriptInput {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<crate::persistence::EventKind>")]
    kind: Field<crate::persistence::EventKind>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    text: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<bool>")]
    truncated: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<f64>")]
    at: Field<serde_json::Number>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<Vec<Value>>")]
    assets: Field<Vec<Box<RawValue>>>,
    #[serde(rename = "turnId", default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    turn_id: Field<String>,
    #[serde(
        rename = "clientMessageId",
        default,
        skip_serializing_if = "Field::is_missing"
    )]
    #[schemars(with = "Option<String>")]
    client_message_id: Field<String>,
    #[serde(flatten)]
    #[schemars(with = "BTreeMap<String, Value>")]
    extra: BTreeMap<String, Box<RawValue>>,
}
#[derive(Clone, Debug, Serialize, schemars::JsonSchema)]
#[schemars(
    rename = "PersistedTranscriptRecord",
    description = "Legacy parent text is hydrated from fulltext before response validation."
)]
pub struct TranscriptItemRecord {
    id: TranscriptItemId,
    role: TranscriptRole,
    #[serde(default, skip_serializing_if = "TranscriptText::is_missing")]
    #[schemars(with = "Option<String>")]
    text: TranscriptText,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<Vec<TranscriptInput>>")]
    inputs: Field<Vec<TranscriptInput>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<serde_json::Number>")]
    at: Field<serde_json::Number>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    title: Field<String>,
    #[serde(
        rename = "afterRestore",
        default,
        skip_serializing_if = "Field::is_missing"
    )]
    #[schemars(with = "Option<String>")]
    after_restore: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<bool>")]
    truncated: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<bool>")]
    streaming: Field<bool>,
    #[serde(rename = "turnId", default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    turn_id: Field<String>,
    #[serde(
        rename = "clientMessageId",
        default,
        skip_serializing_if = "Field::is_missing"
    )]
    #[schemars(with = "Option<String>")]
    client_message_id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<Vec<Value>>")]
    assets: Field<Vec<Box<RawValue>>>,
    #[serde(flatten)]
    #[schemars(with = "BTreeMap<String, Value>")]
    extra: BTreeMap<String, Box<RawValue>>,
}

macro_rules! read_field {
    ($name:ident, $map:ident, $fields:ident, $wire:literal, $member:ident) => {
        if $name == $wire {
            if $fields.$member.is_some() {
                return Err(serde::de::Error::duplicate_field($wire));
            }
            $fields.$member = Some($map.next_value()?);
            return Ok(true);
        }
    };
}

#[derive(Default)]
struct ItemFields {
    id: Option<TranscriptItemId>,
    role: Option<TranscriptRole>,
    text: Option<TranscriptText>,
    inputs: Option<Field<Vec<TranscriptInput>>>,
    at: Option<Field<serde_json::Number>>,
    title: Option<Field<String>>,
    after_restore: Option<Field<String>>,
    truncated: Option<Field<bool>>,
    streaming: Option<Field<bool>>,
    turn_id: Option<Field<String>>,
    client_message_id: Option<Field<String>>,
    assets: Option<Field<Vec<Box<RawValue>>>>,
}
impl<'de> crate::raw_object::RawObjectFields<'de> for ItemFields {
    type Output = TranscriptItemRecord;
    fn field<M: serde::de::MapAccess<'de>>(
        &mut self,
        name: &str,
        map: &mut M,
    ) -> Result<bool, M::Error> {
        read_field!(name, map, self, "id", id);
        read_field!(name, map, self, "role", role);
        read_field!(name, map, self, "text", text);
        read_field!(name, map, self, "inputs", inputs);
        read_field!(name, map, self, "at", at);
        read_field!(name, map, self, "title", title);
        read_field!(name, map, self, "afterRestore", after_restore);
        read_field!(name, map, self, "truncated", truncated);
        read_field!(name, map, self, "streaming", streaming);
        read_field!(name, map, self, "turnId", turn_id);
        read_field!(name, map, self, "clientMessageId", client_message_id);
        read_field!(name, map, self, "assets", assets);
        Ok(false)
    }
    fn finish<E: serde::de::Error>(
        self,
        extra: BTreeMap<String, Box<RawValue>>,
    ) -> Result<Self::Output, E> {
        Ok(TranscriptItemRecord {
            id: self.id.ok_or_else(|| E::missing_field("id"))?,
            role: self.role.ok_or_else(|| E::missing_field("role"))?,
            text: self.text.unwrap_or(TranscriptText::InFulltextMissing),
            inputs: self.inputs.unwrap_or_default(),
            at: self.at.unwrap_or_default(),
            title: self.title.unwrap_or_default(),
            after_restore: self.after_restore.unwrap_or_default(),
            truncated: self.truncated.unwrap_or_default(),
            streaming: self.streaming.unwrap_or_default(),
            turn_id: self.turn_id.unwrap_or_default(),
            client_message_id: self.client_message_id.unwrap_or_default(),
            assets: self.assets.unwrap_or_default(),
            extra,
        })
    }
}
impl<'de> Deserialize<'de> for TranscriptItemRecord {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        crate::raw_object::deserialize::<D, ItemFields>(deserializer)
    }
}

#[derive(Default)]
struct InputFields {
    id: Option<Field<String>>,
    kind: Option<Field<crate::persistence::EventKind>>,
    text: Option<Field<String>>,
    truncated: Option<Field<bool>>,
    at: Option<Field<serde_json::Number>>,
    assets: Option<Field<Vec<Box<RawValue>>>>,
    turn_id: Option<Field<String>>,
    client_message_id: Option<Field<String>>,
}
impl<'de> crate::raw_object::RawObjectFields<'de> for InputFields {
    type Output = TranscriptInput;
    fn field<M: serde::de::MapAccess<'de>>(
        &mut self,
        name: &str,
        map: &mut M,
    ) -> Result<bool, M::Error> {
        read_field!(name, map, self, "id", id);
        read_field!(name, map, self, "kind", kind);
        read_field!(name, map, self, "text", text);
        read_field!(name, map, self, "truncated", truncated);
        read_field!(name, map, self, "at", at);
        read_field!(name, map, self, "assets", assets);
        read_field!(name, map, self, "turnId", turn_id);
        read_field!(name, map, self, "clientMessageId", client_message_id);
        Ok(false)
    }
    fn finish<E: serde::de::Error>(
        self,
        extra: BTreeMap<String, Box<RawValue>>,
    ) -> Result<Self::Output, E> {
        Ok(TranscriptInput {
            id: self.id.unwrap_or_default(),
            kind: self.kind.unwrap_or_default(),
            text: self.text.unwrap_or_default(),
            truncated: self.truncated.unwrap_or_default(),
            at: self.at.unwrap_or_default(),
            assets: self.assets.unwrap_or_default(),
            turn_id: self.turn_id.unwrap_or_default(),
            client_message_id: self.client_message_id.unwrap_or_default(),
            extra,
        })
    }
}
impl<'de> Deserialize<'de> for TranscriptInput {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        crate::raw_object::deserialize::<D, InputFields>(deserializer)
    }
}
impl TranscriptItemRecord {
    pub fn id(&self) -> &TranscriptItemId {
        &self.id
    }
    pub fn role(&self) -> TranscriptRole {
        self.role
    }
    pub fn text(&self) -> &TranscriptText {
        &self.text
    }
    pub fn set_inline_text(&mut self, text: String) {
        self.text = TranscriptText::Inline(text);
    }
    /// New writes require the complete current input shape.
    pub fn has_complete_inputs(&self) -> bool {
        match &self.inputs {
            Field::Missing => true,
            Field::Null => false,
            Field::Value(inputs) => inputs.iter().all(|input| {
                input.id().is_some()
                    && input.kind().is_some()
                    && input.text().is_some()
                    && input.truncated().is_some()
                    && input.at().is_some_and(f64::is_finite)
                    && matches!(input.assets(), Field::Value(_))
            }),
        }
    }
    pub fn inputs(&self) -> &Field<Vec<TranscriptInput>> {
        &self.inputs
    }
    pub fn title(&self) -> Option<&str> {
        self.title.value().map(String::as_str)
    }
    pub fn after_restore(&self) -> Option<&str> {
        self.after_restore.value().map(String::as_str)
    }
    pub fn after_restore_field(&self) -> &Field<String> {
        &self.after_restore
    }
    pub fn at(&self) -> &Field<serde_json::Number> {
        &self.at
    }
    pub fn set_after_restore(&mut self, value: String) {
        self.after_restore = Field::Value(value);
    }
    pub fn truncated(&self) -> Option<bool> {
        self.truncated.value().copied()
    }
    pub fn truncated_field(&self) -> &Field<bool> {
        &self.truncated
    }
    pub fn streaming(&self) -> &Field<bool> {
        &self.streaming
    }
    pub fn assets(&self) -> &Field<Vec<Box<RawValue>>> {
        &self.assets
    }
    pub fn turn_id(&self) -> &Field<String> {
        &self.turn_id
    }
    pub fn client_message_id(&self) -> &Field<String> {
        &self.client_message_id
    }
    pub fn into_value(self) -> Value {
        self.value()
    }
    pub fn value(&self) -> Value {
        self.serialize(serde_json::value::Serializer)
            .expect("transcript fields serialize")
    }
}
impl TranscriptInput {
    pub fn id(&self) -> Option<&str> {
        self.id.value().map(String::as_str)
    }
    pub fn kind(&self) -> Option<&str> {
        self.kind
            .value()
            .copied()
            .map(crate::persistence::EventKind::as_str)
    }
    /// A legacy input with no kind inherits the parent role.
    pub fn projected_role(&self, parent: TranscriptRole) -> TranscriptRole {
        if self.kind.value() == Some(&crate::persistence::EventKind::User) {
            TranscriptRole::User
        } else {
            parent
        }
    }
    pub fn text(&self) -> Option<&str> {
        self.text.value().map(String::as_str)
    }
    /// Event text takes precedence; legacy absent or null text projects as empty.
    pub fn projected_text(&self, event: Option<String>) -> String {
        match event {
            Some(text) => text,
            None => match &self.text {
                Field::Value(text) => text.clone(),
                Field::Missing | Field::Null => String::new(),
            },
        }
    }
    pub fn truncated(&self) -> Option<bool> {
        self.truncated.value().copied()
    }
    /// Legacy missing or null input truncation means the preview was complete.
    pub fn effective_truncated(&self) -> bool {
        matches!(self.truncated, Field::Value(true))
    }
    pub fn at(&self) -> Option<f64> {
        self.at.value().and_then(serde_json::Number::as_f64)
    }
    pub fn assets(&self) -> &Field<Vec<Box<RawValue>>> {
        &self.assets
    }
    pub fn turn_id(&self) -> &Field<String> {
        &self.turn_id
    }
    pub fn client_message_id(&self) -> &Field<String> {
        &self.client_message_id
    }
    pub fn value(&self) -> Value {
        self.serialize(serde_json::value::Serializer)
            .expect("transcript input fields serialize")
    }
}
pub(crate) fn schema() -> Value {
    schemars::schema_for!(TranscriptItemRecord).to_value()
}
pub(crate) fn check_schema_parity() -> Result<(), String> {
    let stored: Value = serde_json::from_slice(include_bytes!(
        "schemas/persisted-transcript-record.schema.json"
    ))
    .map_err(|e| e.to_string())?;
    if stored != schema() {
        return Err("persisted transcript schema must be generated from transcript_record::TranscriptItemRecord".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn transcript_current_record_requires_role_and_keeps_raw_extensions() {
        assert!(serde_json::from_str::<TranscriptItemRecord>(r#"{"id":"i","text":"x"}"#).is_err());
        let raw = r#"{"id":"i","role":"user","text":"x","provider":{"z":  1, "a":2}}"#;
        let record: TranscriptItemRecord = serde_json::from_str(raw).expect("record");
        let encoded = serde_json::to_string(&record).expect("record serialization");
        assert!(encoded.contains(r#"{"z":  1, "a":2}"#), "{encoded}");
    }
    #[test]
    fn transcript_hash_projection_keeps_canonical_numbers_and_legacy_text_presence() {
        for raw in [
            r#"{"id":"i","role":"user","inputs":[{"id":"x","kind":"user","text":"x","at":1,"assets":[],"truncated":false}],"provider":{"z": 1,"a":2}}"#,
            r#"{"id":"i","role":"assistant","text":null}"#,
            r#"{"id":"i","role":"assistant"}"#,
        ] {
            let original: Value = serde_json::from_str(raw).unwrap();
            let record: TranscriptItemRecord = serde_json::from_str(raw).unwrap();
            assert_eq!(record.value().to_string(), original.to_string());
        }
    }
    #[test]
    fn transcript_shell_and_input_match_schema_preserving_extensions() {
        check_schema_parity().unwrap();
        for (nested, fields) in [
            (false, &["id", "role", "text", "inputs"][..]),
            (true, &["id", "kind", "text", "truncated"][..]),
        ] {
            for field in fields {
                for candidate in [
                    Value::Null,
                    json!("value"),
                    json!(true),
                    json!(1),
                    json!({}),
                    json!([]),
                ] {
                    let mut value = json!({"id":"i", "extra":{"x":[1]}});
                    if nested {
                        value["inputs"] = json!([{}]);
                        value["inputs"][0][*field] = candidate;
                    } else {
                        value[*field] = candidate;
                    }
                    let parsed: Result<TranscriptItemRecord, _> =
                        serde_json::from_str(&value.to_string());
                    assert_eq!(
                        parsed.is_ok(),
                        crate::validate("PersistedTranscriptRecord", &value).unwrap(),
                        "{field}: {value}"
                    );
                    if let Ok(parsed) = parsed {
                        assert_eq!(parsed.into_value(), value);
                    }
                }
            }
        }
        for value in [
            json!({"id":"i"}),
            json!({"id":"i","role":null,"text":null,"inputs":null,"future":[1]}),
            json!({"id":"i","role":"output","inputs":[{"id":null,"kind":"user","text":"t","truncated":false,"extra":{"x":1}}]}),
            json!([]),
            json!({}),
            json!({"id":"i","text":1}),
            json!({"id":"i","inputs":[null]}),
            json!({"id":"i","inputs":[{"id":1}]}),
            json!({"id":"i","inputs":[{"truncated":"yes"}]}),
        ] {
            let parsed: Result<TranscriptItemRecord, _> = serde_json::from_str(&value.to_string());
            assert_eq!(
                parsed.is_ok(),
                crate::validate("PersistedTranscriptRecord", &value).unwrap(),
                "{value}"
            );
            if let Ok(parsed) = parsed {
                assert_eq!(parsed.into_value(), value);
            }
        }
    }
}
