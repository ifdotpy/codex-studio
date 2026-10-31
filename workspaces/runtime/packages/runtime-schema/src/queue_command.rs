//! Queue mutation wire owner.

use crate::monitor_record::MonitorText;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};

/// A nullable wire field whose omission must survive request signature serialization.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Field<T> {
    #[default]
    Missing,
    Null,
    Value(T),
}

impl<T> Field<T> {
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Missing | Self::Null => None,
            Self::Value(value) => Some(value),
        }
    }
    pub fn as_ref(&self) -> Option<&T> {
        self.value()
    }
    pub fn is_some(&self) -> bool {
        matches!(self, Self::Value(_))
    }
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for Field<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Option::<T>::deserialize(deserializer).map(|value| match value {
            None => Self::Null,
            Some(value) => Self::Value(value),
        })
    }
}
impl<T: Serialize> Serialize for Field<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Missing | Self::Null => serializer.serialize_none(),
            Self::Value(value) => value.serialize(serializer),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ExpectedRevision {
    #[default]
    Unchecked,
    MustBeAbsent,
    Is(String),
}

impl<'de> Deserialize<'de> for ExpectedRevision {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Option::<String>::deserialize(deserializer)? {
            None => Self::MustBeAbsent,
            Some(value) => Self::Is(value),
        })
    }
}

impl Serialize for ExpectedRevision {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Unchecked | Self::MustBeAbsent => serializer.serialize_none(),
            Self::Is(value) => value.serialize(serializer),
        }
    }
}

impl ExpectedRevision {
    fn unchecked(&self) -> bool {
        matches!(self, Self::Unchecked)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "expectedText"
    )]
    pub expected_text: Option<String>,
    #[serde(default, skip_serializing_if = "ExpectedRevision::unchecked")]
    #[schemars(with = "Option<String>")]
    pub expected_revision: ExpectedRevision,
    #[schemars(with = "String", length(min = 1))]
    pub id: MonitorText<1, { usize::MAX }>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<MonitorText<0, QUEUE_REQUEST_ID_MAX_CODEPOINTS>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueEdit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "expectedText"
    )]
    pub expected_text: Option<String>,
    #[serde(default, skip_serializing_if = "ExpectedRevision::unchecked")]
    #[schemars(with = "Option<String>")]
    pub expected_revision: ExpectedRevision,
    #[schemars(with = "String", length(min = 1))]
    pub id: MonitorText<1, { usize::MAX }>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<MonitorText<0, QUEUE_REQUEST_ID_MAX_CODEPOINTS>>,
    pub text: MonitorText<0, { crate::shared_values::MAX_MANAGED_MESSAGE_CODEPOINTS }>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueReorder {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub expected_revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub ordered_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<MonitorText<0, QUEUE_REQUEST_ID_MAX_CODEPOINTS>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueSendNow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "expectedText"
    )]
    pub expected_text: Option<String>,
    #[serde(default, skip_serializing_if = "ExpectedRevision::unchecked")]
    #[schemars(with = "Option<String>")]
    pub expected_revision: ExpectedRevision,
    #[schemars(with = "String", length(min = 1))]
    pub id: MonitorText<1, { usize::MAX }>,
    pub request_id: MonitorText<1, QUEUE_REQUEST_ID_MAX_CODEPOINTS>,
}

/// The HTTP and service command. Unknown actions, fields and mistyped payloads fail at ingress.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum QueueCommand {
    Edit(QueueEdit),
    First(QueueTarget),
    SendNow(QueueSendNow),
    Reorder(QueueReorder),
}

impl QueueCommand {
    pub fn agent(&self) -> Option<&str> {
        match self {
            Self::First(body) => body.agent.as_deref(),
            Self::SendNow(body) => body.agent.as_deref(),
            Self::Edit(body) => body.agent.as_deref(),
            Self::Reorder(body) => body.agent.as_deref(),
        }
    }
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Self::First(body) => body.request_id.as_ref().map(MonitorText::as_str),
            Self::SendNow(body) => Some(body.request_id.as_str()),
            Self::Edit(body) => body.request_id.as_ref().map(MonitorText::as_str),
            Self::Reorder(body) => body.request_id.as_ref().map(MonitorText::as_str),
        }
    }
    pub fn accepts_revision(&self, current: &str) -> bool {
        let field = match self {
            Self::First(body) => &body.expected_revision,
            Self::SendNow(body) => &body.expected_revision,
            Self::Edit(body) => &body.expected_revision,
            Self::Reorder(body) => return body.expected_revision == current,
        };
        match field {
            ExpectedRevision::Unchecked => true,
            ExpectedRevision::MustBeAbsent => false,
            ExpectedRevision::Is(value) => value == current,
        }
    }
    pub fn target_id(&self) -> Option<&str> {
        match self {
            Self::First(body) => Some(body.id.as_str()),
            Self::Edit(body) => Some(body.id.as_str()),
            Self::SendNow(body) => Some(body.id.as_str()),
            Self::Reorder(_) => None,
        }
    }
    pub fn expected_text(&self) -> Option<&str> {
        match self {
            Self::First(body) => body.expected_text.as_deref(),
            Self::SendNow(body) => body.expected_text.as_deref(),
            Self::Edit(body) => body.expected_text.as_deref(),
            Self::Reorder(_) => None,
        }
    }
}

/// Derive the existing four OpenAPI components from the single tagged command owner.
pub(crate) fn schemas() -> Vec<(&'static str, Value)> {
    let schema = schemars::generate::SchemaSettings::draft2020_12()
        .with(|settings| settings.inline_subschemas = true)
        .for_deserialize()
        .into_generator()
        .into_root_schema_for::<QueueCommand>()
        .to_value();
    let mut variants = schema["oneOf"]
        .as_array()
        .expect("tagged enum has variants")
        .clone()
        .into_iter();
    let edit = variants.next().expect("edit");
    let first = variants.next().expect("first");
    let send_now = variants.next().expect("send now");
    let reorder = variants.next().expect("reorder");
    vec![
        ("QueueEditBody", edit),
        ("QueueFirstBody", json!({"oneOf": [first, send_now]})),
        ("QueueReorderBody", reorder),
    ]
}

/// Maximum raw queue receipt identifier length, in Unicode code points.
pub const QUEUE_REQUEST_ID_MAX_CODEPOINTS: usize = 200;

pub(crate) fn check_schema_parity(openapi: &Value) -> Result<(), String> {
    for (name, schema) in schemas() {
        if openapi.pointer(&format!("/components/schemas/{name}")) != Some(&schema) {
            return Err(format!(
                "{name} must match runtime-schema/src/queue_command.rs"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_commands_reject_unknown_actions_fields_and_wrong_types() {
        for wire in [
            json!({"action":"unknown"}),
            json!({"action":"cancel","id":"a"}),
            json!({"action":"first","id":"a","extra":true}),
            json!({"action":"edit"}),
            json!({"action":"edit","text":42}),
            json!({"action":"first","id":42}),
            json!({"action":"send_now","request_id":false}),
            json!({"action":"reorder","expected_revision":null,"ordered_ids":[]}),
            json!({"action":"reorder","expected_revision":"r","ordered_ids":[42]}),
        ] {
            assert!(
                serde_json::from_value::<QueueCommand>(wire.clone()).is_err(),
                "{wire}"
            );
        }
    }

    #[test]
    fn queue_command_schema_matches_committed_openapi() {
        let openapi =
            serde_json::from_str(include_str!("../../runtime-http/openapi.json")).expect("OpenAPI");
        check_schema_parity(&openapi).expect("queue schemas match their owner");
    }

    #[test]
    fn queue_schema_and_typed_input_agree() {
        let schemas = schemas()
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>();
        for (name, wire, accepted) in [
            (
                "QueueEditBody",
                json!({"action":"edit","id":"a","text":"x"}),
                true,
            ),
            (
                "QueueEditBody",
                json!({"action":"edit","id":"a","text":"x".repeat(32001)}),
                false,
            ),
            (
                "QueueFirstBody",
                json!({"action":"send_now","id":"a"}),
                false,
            ),
            (
                "QueueFirstBody",
                json!({"action":"send_now","id":"a","request_id":"r"}),
                true,
            ),
            (
                "QueueReorderBody",
                json!({"action":"reorder","expected_revision":"r","ordered_ids":[]}),
                true,
            ),
        ] {
            assert_eq!(
                jsonschema::validator_for(&schemas[name])
                    .expect("schema")
                    .is_valid(&wire),
                accepted,
                "{wire}"
            );
            assert_eq!(
                serde_json::from_value::<QueueCommand>(wire.clone()).is_ok(),
                accepted,
                "{wire}"
            );
        }
    }
}
