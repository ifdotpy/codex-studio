//! Persisted queue metadata. Unknown fields remain open for future producers.
use crate::{delivery::DeliveryMoment, queue_command::Field, raw_json::RawJson};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Retains the accepted historical spelling while using the delivery owner for policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct StoredDelivery(String);
impl StoredDelivery {
    pub fn moment(&self) -> DeliveryMoment {
        DeliveryMoment::parse_legacy(&self.0).expect("validated delivery")
    }
    pub fn canonical(moment: DeliveryMoment) -> Self {
        Self(moment.as_str().to_owned())
    }
}
impl<'de> Deserialize<'de> for StoredDelivery {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        DeliveryMoment::parse_legacy(&raw).map_err(serde::de::Error::custom)?;
        Ok(Self(raw))
    }
}
impl schemars::JsonSchema for StoredDelivery {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "StoredDelivery".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"$ref": format!("{}#/components/schemas/DeliveryMomentInput", crate::OPENAPI_URI)})
    }
    fn inline_schema() -> bool {
        true
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename = "RuntimeEventMetadataRecord")]
pub struct EventMetadataFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Vec<Value>")]
    pub assets: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<StoredDelivery>")]
    pub delivery: Field<StoredDelivery>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<StoredDelivery>")]
    pub requested_delivery: Field<StoredDelivery>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<Map<String, Value>>")]
    pub timing: Field<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub transcript_item_id: Field<String>,
    #[serde(flatten)]
    #[schemars(with = "BTreeMap<String, Value>")]
    pub extensions: BTreeMap<String, RawJson>,
}

#[derive(Default)]
struct MetadataFields {
    assets: Option<Vec<Value>>,
    delivery: Option<Field<StoredDelivery>>,
    requested_delivery: Option<Field<StoredDelivery>>,
    timing: Option<Field<Map<String, Value>>>,
    transcript_item_id: Option<Field<String>>,
}
impl<'de> crate::raw_object::RawObjectFields<'de> for MetadataFields {
    type Output = EventMetadataRecord;
    fn field<M: serde::de::MapAccess<'de>>(
        &mut self,
        name: &str,
        map: &mut M,
    ) -> Result<bool, M::Error> {
        macro_rules! field {
            ($wire:literal, $member:ident) => {
                if name == $wire {
                    if self.$member.is_some() {
                        return Err(serde::de::Error::duplicate_field($wire));
                    }
                    self.$member = Some(map.next_value()?);
                    return Ok(true);
                }
            };
        }
        field!("assets", assets);
        field!("delivery", delivery);
        field!("requestedDelivery", requested_delivery);
        field!("timing", timing);
        field!("transcriptItemId", transcript_item_id);
        Ok(false)
    }
    fn finish<E: serde::de::Error>(
        self,
        extensions: BTreeMap<String, Box<serde_json::value::RawValue>>,
    ) -> Result<Self::Output, E> {
        Ok(EventMetadataRecord(EventMetadataFields {
            assets: self.assets,
            delivery: self.delivery.unwrap_or_default(),
            requested_delivery: self.requested_delivery.unwrap_or_default(),
            timing: self.timing.unwrap_or_default(),
            transcript_item_id: self.transcript_item_id.unwrap_or_default(),
            extensions: extensions
                .into_iter()
                .map(|(key, value)| (key, RawJson(value)))
                .collect(),
        }))
    }
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct EventMetadataRecord(pub EventMetadataFields);
impl<'de> Deserialize<'de> for EventMetadataRecord {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        crate::raw_object::deserialize::<D, MetadataFields>(d)
    }
}
impl std::ops::Deref for EventMetadataRecord {
    type Target = EventMetadataFields;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for EventMetadataRecord {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl EventMetadataRecord {
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        let encoded = value.to_string();
        drop(value);
        serde_json::from_str(&encoded)
    }
    pub fn into_value(self) -> Value {
        let EventMetadataFields {
            assets,
            delivery,
            requested_delivery,
            timing,
            transcript_item_id,
            extensions,
        } = self.0;
        let mut fields = Map::new();
        if let Some(assets) = assets {
            fields.insert("assets".into(), Value::Array(assets));
        }
        move_field(&mut fields, "delivery", delivery, |value| {
            Value::String(value.0)
        });
        move_field(
            &mut fields,
            "requestedDelivery",
            requested_delivery,
            |value| Value::String(value.0),
        );
        move_field(&mut fields, "timing", timing, Value::Object);
        move_field(
            &mut fields,
            "transcriptItemId",
            transcript_item_id,
            Value::String,
        );
        fields.extend(extensions.into_iter().map(|(key, value)| {
            // RawJson contains valid JSON from the shared object decoder.
            let value = value
                .serialize(serde_json::value::Serializer)
                .expect("raw JSON serializes");
            (key, value)
        }));
        Value::Object(fields)
    }
}
fn move_field<T>(
    fields: &mut Map<String, Value>,
    name: &str,
    field: Field<T>,
    into_value: impl FnOnce(T) -> Value,
) {
    match field {
        Field::Missing => {}
        Field::Null => {
            fields.insert(name.into(), Value::Null);
        }
        Field::Value(value) => {
            fields.insert(name.into(), into_value(value));
        }
    }
}
pub(crate) fn schema() -> Value {
    schemars::schema_for!(EventMetadataRecord).to_value()
}
pub(crate) fn check_schema_parity() -> Result<(), String> {
    let stored: Value =
        serde_json::from_slice(include_bytes!("schemas/runtime-event-metadata.schema.json"))
            .map_err(|e| e.to_string())?;
    if schema() != stored {
        return Err("runtime event metadata schema must be generated from event_metadata::EventMetadataRecord".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn metadata_rejects_duplicate_extensions_and_preserves_raw_bytes() {
        let raw = r#"{"future":{"z":  1,"a":2}}"#;
        let record: EventMetadataRecord = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_string(&record).unwrap(), raw);
        assert!(serde_json::from_str::<EventMetadataRecord>(r#"{"future":1,"future":2}"#).is_err());
    }

    #[test]
    fn consuming_projection_moves_owned_payloads() {
        let record = EventMetadataRecord::from_value(json!({
            "assets":[{"payload":"asset-value"}],
            "delivery":"after_turn", "requestedDelivery":"steer",
            "timing":{"evidence":["timing-value"]},
            "transcriptItemId":"transcript-id", "future":["extension-value"]
        }))
        .unwrap();
        let assets = record.assets.as_ref().unwrap().as_ptr();
        let delivery = record.delivery.value().unwrap().0.as_ptr();
        let requested = record.requested_delivery.value().unwrap().0.as_ptr();
        let timing = record.timing.value().unwrap()["evidence"]
            .as_array()
            .unwrap()
            .as_ptr();
        let transcript = record.transcript_item_id.value().unwrap().as_ptr();
        let expected = json!(&record);
        let result = record.into_value();
        assert_eq!(result, expected);
        assert_eq!(result["assets"].as_array().unwrap().as_ptr(), assets);
        assert_eq!(result["delivery"].as_str().unwrap().as_ptr(), delivery);
        assert_eq!(
            result["requestedDelivery"].as_str().unwrap().as_ptr(),
            requested
        );
        assert_eq!(
            result["timing"]["evidence"].as_array().unwrap().as_ptr(),
            timing
        );
        assert_eq!(
            result["transcriptItemId"].as_str().unwrap().as_ptr(),
            transcript
        );
    }

    #[test]
    fn schema_and_typed_owner_agree_on_presence_legacy_and_extensions() {
        check_schema_parity().unwrap();
        let samples = [
            json!({}),
            json!({"assets":[]}),
            json!({"assets":null}),
            json!({"assets":[null, {"future":true}]}),
            json!({"delivery":null}),
            json!({"delivery":"after_turn", "requestedDelivery":"steer"}),
            json!({"delivery":"unknown"}),
            json!({"timing":null}),
            json!({"timing":[]}),
            json!({"timing":{"enqueuedAt":1234567890123456789u64}, "transcriptItemId":null, "future":[1,null]}),
            json!({"transcriptItemId":3}),
            json!([]),
        ];
        for value in samples {
            let decoded = EventMetadataRecord::from_value(value.clone());
            assert_eq!(
                decoded.is_ok(),
                crate::validate("RuntimeEventMetadataRecord", &value).unwrap(),
                "{value}"
            );
            if let Ok(decoded) = decoded {
                assert_eq!(decoded.into_value(), value);
            }
        }
        for (name, expected) in DeliveryMoment::ALL
            .map(|v| (v.as_str(), v))
            .into_iter()
            .chain(crate::delivery::LEGACY_DELIVERY_MOMENTS)
        {
            let decoded = EventMetadataRecord::from_value(json!({"delivery":name})).unwrap();
            assert_eq!(decoded.delivery.value().unwrap().moment(), expected);
            assert_eq!(decoded.into_value()["delivery"], name);
        }
    }
}
