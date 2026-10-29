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
    fn queue_command_schema_matches_committed_openapi() {
        let openapi =
            serde_json::from_str(include_str!("../../runtime-http/openapi.json")).expect("OpenAPI");
        check_schema_parity(&openapi).expect("queue schemas match their owner");
    }

    #[test]
    fn queue_command_schema_preserves_fixed_legacy_outcomes() {
        let openapi: Value =
            serde_json::from_str(include_str!("../../runtime-http/openapi.json")).expect("OpenAPI");
        let validators = schemas()
            .into_iter()
            .map(|(name, schema)| {
                (
                    name,
                    [
                        jsonschema::validator_for(&schema).expect("owner schema"),
                        jsonschema::validator_for(&openapi["components"]["schemas"][name])
                            .expect("committed schema"),
                    ],
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let check = |name: &str, wire: &Value, expected: bool| {
            for validator in &validators[name] {
                assert_eq!(validator.is_valid(wire), expected, "{name}: {wire}");
            }
        };
        // Expected outcomes are fixed legacy schema behavior, independent of
        // either schema renderer. Handler-only ID/receipt checks stay in HTTP.
        for (name, base, nullable) in [
            (
                "QueueCancelBody",
                json!({"action":"cancel","id":"a","message_id":"fallback"}),
                &[
                    "agent",
                    "id",
                    "message_id",
                    "request_id",
                    "expected_revision",
                    "expectedText",
                ][..],
            ),
            (
                "QueueEditBody",
                json!({"action":"edit","id":"a","text":"text"}),
                &[
                    "agent",
                    "id",
                    "message_id",
                    "request_id",
                    "expected_revision",
                    "expectedText",
                ][..],
            ),
            (
                "QueueFirstBody",
                json!({"action":"first","id":"a"}),
                &[
                    "agent",
                    "id",
                    "message_id",
                    "request_id",
                    "expected_revision",
                    "expectedText",
                ][..],
            ),
            (
                "QueueFirstBody",
                json!({"action":"send_now","id":"a"}),
                &[
                    "agent",
                    "id",
                    "message_id",
                    "request_id",
                    "expected_revision",
                    "expectedText",
                ][..],
            ),
            (
                "QueueReorderBody",
                json!({"action":"reorder","expected_revision":"r","ordered_ids":["a"]}),
                &["agent", "id", "request_id"][..],
            ),
        ] {
            check(name, &base, true);
            for malformed in [Value::Null, json!(true), json!(7), json!("body"), json!([])] {
                check(name, &malformed, false);
            }
            for field in nullable {
                for (value, expected) in [
                    (None, true),
                    (Some(Value::Null), true),
                    (Some(json!("")), true),
                    (Some(json!("value")), true),
                    (Some(json!(7)), false),
                    (Some(json!(false)), false),
                    (Some(json!([])), false),
                    (Some(json!({})), false),
                ] {
                    let mut wire = base.clone();
                    wire.as_object_mut().expect("body").remove(*field);
                    if let Some(value) = value {
                        wire[*field] = value;
                    }
                    check(name, &wire, expected);
                }
            }
            for character in ["x", "🙂"] {
                // Literal lengths deliberately do not read the owner's limit.
                for (length, expected) in [(199, true), (200, true), (201, false)] {
                    let mut wire = base.clone();
                    wire["request_id"] = json!(character.repeat(length));
                    check(name, &wire, expected);
                }
            }
            let mut missing_action = base.clone();
            missing_action
                .as_object_mut()
                .expect("body")
                .remove("action");
            check(name, &missing_action, false);
            for action in [Value::Null, json!("unknown"), json!(1)] {
                let mut wire = base.clone();
                wire["action"] = action;
                check(name, &wire, false);
            }
            let mut unknown = base;
            unknown["extra"] = json!(true);
            check(name, &unknown, false);
        }
        for (wire, expected) in [
            (json!({"action":"cancel"}), false),
            (json!({"action":"cancel","id":null}), false),
            (json!({"action":"cancel","id":""}), false),
            (json!({"action":"cancel","message_id":""}), false),
            (json!({"action":"cancel","message_id":"a"}), true),
            (json!({"action":"cancel","id":null,"message_id":"a"}), true),
            (json!({"action":"cancel","id":"","message_id":"a"}), true),
            (json!({"action":"cancel","id":"a","message_id":null}), true),
            (json!({"action":"cancel","id":"a","message_id":""}), true),
            (json!({"action":"cancel","id":"a","message_id":"b"}), true),
        ] {
            check("QueueCancelBody", &wire, expected);
        }
        for (wire, expected) in [
            (json!({"action":"edit","text":""}), true),
            (json!({"action":"edit","text":"text","id":null}), true),
            (json!({"action":"edit","text":"text","message_id":""}), true),
            (json!({"action":"edit"}), false),
            (json!({"action":"edit","text":null}), false),
            (json!({"action":"edit","text":7}), false),
        ] {
            check("QueueEditBody", &wire, expected);
        }
        for character in ["x", "🙂"] {
            for (length, expected) in [(31999, true), (32000, true), (32001, false)] {
                check(
                    "QueueEditBody",
                    &json!({"action":"edit","text":character.repeat(length)}),
                    expected,
                );
            }
        }
        for action in ["first", "send_now"] {
            for wire in [
                json!({"action":action}),
                json!({"action":action,"id":null}),
                json!({"action":action,"message_id":""}),
            ] {
                check("QueueFirstBody", &wire, true);
            }
        }
        for (wire, expected) in [
            (
                json!({"action":"reorder","expected_revision":"","ordered_ids":[]}),
                true,
            ),
            (
                json!({"action":"reorder","expected_revision":"r","ordered_ids":["","a","a"]}),
                true,
            ),
            (
                json!({"action":"reorder","expected_revision":null,"ordered_ids":[]}),
                false,
            ),
            (json!({"action":"reorder","ordered_ids":[]}), false),
            (json!({"action":"reorder","expected_revision":"r"}), false),
            (
                json!({"action":"reorder","expected_revision":"r","ordered_ids":null}),
                false,
            ),
            (
                json!({"action":"reorder","expected_revision":"r","ordered_ids":["a",7]}),
                false,
            ),
            (
                json!({"action":"reorder","expected_revision":"r","ordered_ids":[],"message_id":"a"}),
                false,
            ),
            (
                json!({"action":"reorder","expected_revision":"r","ordered_ids":[],"expectedText":null}),
                false,
            ),
        ] {
            check("QueueReorderBody", &wire, expected);
        }
    }
}
