//! Typed persisted complaints and their generated schema.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComplaintStatus {
    Open,
    InProgress,
    Resolved,
    Declined,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComplaintRecipient {
    User,
    Lead,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComplaintSource {
    Message,
    UserTask,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ComplaintResponse {
    pub at: f64,
    pub author: String,
    pub id: String,
    pub status: crate::complaints::ComplaintResponseStatus,
    pub text: String,
}

fn required_nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    Option::deserialize(d)
}

fn nullable_string(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":["string","null"]})
}
fn nullable_number(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":["number","null"]})
}
fn nullable_integer(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":["integer","null"]})
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ComplaintRecord {
    pub id: String,
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(required, schema_with = "nullable_string")]
    pub author: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(required, schema_with = "nullable_string")]
    pub lead_id: Option<String>,
    pub status: ComplaintStatus,
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(required, schema_with = "nullable_string")]
    pub text: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(required, schema_with = "nullable_number")]
    pub created: Option<f64>,
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(required, schema_with = "nullable_number")]
    pub read_at: Option<f64>,
    pub recipient: ComplaintRecipient,
    pub responses: Vec<ComplaintResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_type: Option<ComplaintSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(required, schema_with = "nullable_number")]
    pub updated: Option<f64>,
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(required, schema_with = "nullable_integer")]
    pub version: Option<i64>,
}

impl ComplaintRecord {
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        Self::deserialize(value)
    }
    pub fn id(&self) -> &String {
        &self.id
    }
    pub fn author(&self) -> Option<&str> {
        self.author.as_deref()
    }
    pub fn lead_id(&self) -> Option<&str> {
        self.lead_id.as_deref()
    }
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }
    pub fn into_value(self) -> Value {
        self.serialize(serde_json::value::Serializer)
            .expect("complaint fields are JSON serializable")
    }
}

pub(crate) fn schema() -> Value {
    schemars::schema_for!(ComplaintRecord).to_value()
}
pub(crate) fn check_schema_parity() -> Result<(), String> {
    let stored: Value = serde_json::from_slice(include_bytes!(
        "schemas/persisted-complaint-record.schema.json"
    ))
    .map_err(|error| error.to_string())?;
    if stored != schema() {
        return Err(
            "persisted complaint schema must be generated from complaint_record::ComplaintRecord"
                .into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_complaint_rejects_missing_fields_and_unknown_status() {
        let raw = r#"{"id":"c","leadId":"l","author":"a","text":"raw text","recipient":"lead","version":1,"status":"open","created":1.0,"updated":1.0,"readAt":null,"responses":[]}"#;
        assert!(serde_json::from_str::<ComplaintRecord>(raw).is_ok());
        assert!(serde_json::from_str::<ComplaintRecord>(r#"{"id":"c"}"#).is_err());
        assert!(serde_json::from_str::<ComplaintRecord>(&raw.replace("open", "unknown")).is_err());
    }

    #[test]
    fn complaint_receipt_serialization_keeps_historical_field_order() {
        let raw = r#"{"id":"c","leadId":"l","author":"user","text":"hello","recipient":"lead","version":1,"status":"open","created":1.0,"updated":1.0,"readAt":null,"responses":[]}"#;
        let record: ComplaintRecord = serde_json::from_str(raw).unwrap();
        assert_eq!(
            serde_json::to_string(&record).unwrap(),
            r#"{"id":"c","author":"user","leadId":"l","status":"open","text":"hello","created":1.0,"readAt":null,"recipient":"lead","responses":[],"updated":1.0,"version":1}"#
        );
        let raw = r#"{"id":"c","leadId":"l","author":"user","text":"hello","recipient":"lead","version":1,"status":"open","created":1.0,"updated":1.0,"readAt":null,"responses":[{"id":"r","author":"a","text":"done","status":"resolved","at":2.0}]}"#;
        let record: ComplaintRecord = serde_json::from_str(raw).unwrap();
        assert_eq!(
            serde_json::to_string(&record).unwrap(),
            r#"{"id":"c","author":"user","leadId":"l","status":"open","text":"hello","created":1.0,"readAt":null,"recipient":"lead","responses":[{"at":2.0,"author":"a","id":"r","status":"resolved","text":"done"}],"updated":1.0,"version":1}"#
        );
    }

    #[test]
    fn complaint_schema_and_serde_match() {
        check_schema_parity().unwrap();
        let good = json!({"id":"c","author":null,"leadId":"l","text":null,"status":"open","recipient":"lead","version":1,"created":1.0,"updated":1.0,"readAt":null,"responses":[]});
        for (value, valid) in [
            (good.clone(), true),
            (json!({"id":"c"}), false),
            (
                {
                    let mut value = good.clone();
                    value["status"] = json!("future");
                    value
                },
                false,
            ),
            (
                {
                    let mut value = good;
                    value["future"] = json!(true);
                    value
                },
                false,
            ),
        ] {
            let parsed: Result<ComplaintRecord, _> = serde_json::from_str(&value.to_string());
            assert_eq!(parsed.is_ok(), valid, "{value}");
            assert_eq!(
                crate::validate("PersistedComplaintRecord", &value).unwrap(),
                valid,
                "{value}"
            );
            if let Ok(parsed) = parsed {
                assert_eq!(parsed.into_value(), value);
            }
        }
    }
}
