//! Queue mutation wire owner. Presence is preserved because receipts hash the original body.

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

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueTarget {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub agent: Field<String>,
    #[serde(
        default,
        skip_serializing_if = "Field::is_missing",
        rename = "expectedText"
    )]
    #[schemars(with = "Option<String>")]
    pub expected_text: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub expected_revision: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub message_id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub request_id: Field<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueEdit {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub agent: Field<String>,
    #[serde(
        default,
        skip_serializing_if = "Field::is_missing",
        rename = "expectedText"
    )]
    #[schemars(with = "Option<String>")]
    pub expected_text: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub expected_revision: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub message_id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub request_id: Field<String>,
    #[schemars(length(max = crate::shared_values::MAX_MANAGED_MESSAGE_CODEPOINTS))]
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueReorder {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub agent: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub request_id: Field<String>,
    pub expected_revision: String,
    pub ordered_ids: Vec<String>,
}

/// The HTTP and service command. Unknown actions, fields and mistyped payloads fail at ingress.
///
/// ```compile_fail
/// use studio_runtime_schema::queue_command::{Field, QueueCommand, QueueReorder};
/// let command = QueueCommand::Reorder(QueueReorder {
///     agent: Field::Missing,
///     id: Field::Missing,
///     request_id: Field::Missing,
///     ordered_ids: vec![],
///     expected_revision: None, // A reorder always needs a revision string.
/// });
/// ```
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum QueueCommand {
    Cancel(QueueTarget),
    Edit(QueueEdit),
    First(QueueTarget),
    SendNow(QueueTarget),
    Reorder(QueueReorder),
}

impl QueueCommand {
    pub fn agent(&self) -> Option<&str> {
        match self {
            Self::Cancel(body) | Self::First(body) | Self::SendNow(body) => body.agent.value(),
            Self::Edit(body) => body.agent.value(),
            Self::Reorder(body) => body.agent.value(),
        }
        .map(String::as_str)
    }
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Self::Cancel(body) | Self::First(body) | Self::SendNow(body) => body.request_id.value(),
            Self::Edit(body) => body.request_id.value(),
            Self::Reorder(body) => body.request_id.value(),
        }
        .map(String::as_str)
    }
    /// Omission skips the check; explicit null conflicts with every revision.
    pub fn accepts_revision(&self, current: &str) -> bool {
        let field = match self {
            Self::Cancel(body) | Self::First(body) | Self::SendNow(body) => &body.expected_revision,
            Self::Edit(body) => &body.expected_revision,
            Self::Reorder(body) => return body.expected_revision == current,
        };
        match field {
            Field::Missing => true,
            Field::Null => false,
            Field::Value(value) => value == current,
        }
    }
    pub fn target_id(&self) -> Option<&str> {
        let (id, message_id) = match self {
            Self::Cancel(body) | Self::First(body) | Self::SendNow(body) => {
                (&body.id, &body.message_id)
            }
            Self::Edit(body) => (&body.id, &body.message_id),
            Self::Reorder(_) => return None,
        };
        message_id
            .value()
            .filter(|id| !id.is_empty())
            .or_else(|| id.value())
            .map(String::as_str)
    }
    pub fn expected_text(&self) -> Option<&str> {
        match self {
            Self::Cancel(body) | Self::First(body) | Self::SendNow(body) => {
                body.expected_text.value()
            }
            Self::Edit(body) => body.expected_text.value(),
            Self::Reorder(_) => None,
        }
        .map(String::as_str)
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
    let mut cancel = variants.next().expect("cancel");
    let edit = variants.next().expect("edit");
    let mut first = variants.next().expect("first");
    let _send_now = variants.next().expect("send now");
    let reorder = variants.next().expect("reorder");
    first["properties"]["action"] = json!({"type":"string","enum":["first","send_now"]});
    // Cancel historically validates its ID alias in the schema; the other
    // message actions perform this same check at the HTTP handler.
    cancel["if"] = json!({"properties":{"id":{"type":"string","minLength":1}},"required":["id"]});
    cancel["else"] = json!({"properties":{"message_id":{"type":"string","minLength":1}},"required":["message_id"]});
    let mut schemas = vec![
        ("QueueCancelBody", cancel),
        ("QueueEditBody", edit),
        ("QueueFirstBody", first),
        ("QueueReorderBody", reorder),
    ];
    for (_, schema) in &mut schemas {
        schema["properties"]["request_id"] = json!({"anyOf":[{"type":"string","maxLength":QUEUE_REQUEST_ID_MAX_CODEPOINTS},{"type":"null"}]});
    }
    schemas
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
    fn queue_commands_preserve_every_optional_field_and_action() {
        for action in ["cancel", "edit", "first", "send_now", "reorder"] {
            let mut base = json!({"action":action});
            if action == "edit" {
                base["text"] = json!(" edit ");
            }
            if action == "reorder" {
                base["expected_revision"] = json!("revision");
                base["ordered_ids"] = json!(["a", "b"]);
            }
            let fields: &[&str] = if action == "reorder" {
                &["agent", "id", "request_id"]
            } else {
                &[
                    "agent",
                    "id",
                    "message_id",
                    "request_id",
                    "expected_revision",
                    "expectedText",
                ]
            };
            for field in fields {
                for value in [
                    None,
                    Some(Value::Null),
                    Some(json!("")),
                    Some(json!(" value ")),
                ] {
                    let mut wire = base.clone();
                    if let Some(value) = value {
                        wire[*field] = value;
                    }
                    let command: QueueCommand =
                        serde_json::from_value(wire.clone()).expect("typed command");
                    assert_eq!(serde_json::to_value(command).expect("signature body"), wire);
                }
            }
        }
    }

    #[test]
    fn queue_commands_reject_unknown_actions_fields_and_wrong_types() {
        for wire in [
            json!({"action":"unknown"}),
            json!({"action":"cancel","extra":true}),
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
    fn queue_command_schema_preserves_old_acceptance() {
        let old: Value =
            serde_json::from_str(include_str!("../../runtime-http/openapi.json")).expect("OpenAPI");
        for (name, schema) in schemas() {
            let before =
                jsonschema::validator_for(&old["components"]["schemas"][name]).expect("old schema");
            let after = jsonschema::validator_for(&schema).expect("owner schema");
            let action = match name {
                "QueueCancelBody" => "cancel",
                "QueueEditBody" => "edit",
                "QueueFirstBody" => "first",
                "QueueReorderBody" => "reorder",
                _ => unreachable!(),
            };
            let base = match action {
                "edit" => json!({"action":action,"text":"text","id":"a"}),
                "reorder" => json!({"action":action,"expected_revision":"r","ordered_ids":["a"]}),
                _ => json!({"action":action,"id":"a"}),
            };
            for field in [
                "action",
                "agent",
                "id",
                "message_id",
                "request_id",
                "expected_revision",
                "expectedText",
                "text",
                "ordered_ids",
                "extra",
            ] {
                for value in [
                    Value::Null,
                    json!(""),
                    json!("a"),
                    json!(42),
                    json!(true),
                    json!([]),
                    json!(["a"]),
                ] {
                    let mut wire = base.clone();
                    wire[field] = value;
                    assert_eq!(
                        before.is_valid(&wire),
                        after.is_valid(&wire),
                        "{name}: {wire}"
                    );
                }
            }
        }
    }
}
