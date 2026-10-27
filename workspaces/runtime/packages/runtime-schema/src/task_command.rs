//! Task mutation ingress owner. Receipt signatures continue to use the original
//! transport JSON, so decoding never rewrites decimal numbers, omission or null.

use crate::queue_command::Field;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Task title length, in Unicode code points.
pub const MAX_TITLE_CODEPOINTS: usize = 160;
/// HTTP task description length, in Unicode code points.
pub const MAX_HTTP_DESCRIPTION_CODEPOINTS: usize = 64_000;
/// Agent-tool task description length, in Unicode code points.
pub const MAX_TOOL_DESCRIPTION_CODEPOINTS: usize = 32_000;
/// HTTP cancellation reason length, in Unicode code points.
pub const MAX_HTTP_CANCEL_REASON_CODEPOINTS: usize = 12_000;
/// Agent-tool cancellation reason length, in Unicode code points.
pub const MAX_TOOL_CANCEL_REASON_CODEPOINTS: usize = 32_000;
/// Submission checks length, in Unicode code points.
pub const MAX_CHECKS_CODEPOINTS: usize = 12_000;
/// Review decision length, in Unicode code points.
pub const MAX_REVIEW_CODEPOINTS: usize = 32_000;
/// Submission revision identifier length, in Unicode code points.
pub const MAX_REVISION_CODEPOINTS: usize = 200;
/// Maximum dependency identifiers in a task command.
pub const MAX_DEPENDENCIES: usize = 100;
/// Maximum submitted file paths at command ingress (the existing HTTP budget).
/// The separate service manifest owner retains its broader stored-path budget.
pub const MAX_FILES: usize = 50;
pub const REVIEW_RESULT_REQUIRED: &str =
    "Supply result with 1 to 32000 characters for accept or reject";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InitialStatus {
    Ready,
    Blocked,
}

/// JSON Schema integers include integral decimal numbers. Retain that ingress
/// compatibility while carrying an actual integer in the service.
#[derive(Clone, Copy, Debug, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct Version(#[schemars(range(min = 0, max = crate::shared_values::MAX_SAFE_INTEGER))] i64);
impl Version {
    pub fn get(self) -> i64 {
        self.0
    }
}
impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let number = serde_json::Number::deserialize(deserializer)?;
        let value = number
            .as_i64()
            .or_else(|| {
                let value = number.as_f64()?;
                (value.is_finite()
                    && value.fract() == 0.0
                    && value >= 0.0
                    && value <= crate::shared_values::MAX_SAFE_INTEGER as f64)
                    .then_some(value as i64)
            })
            .ok_or_else(|| serde::de::Error::custom("Task version must be an integer"))?;
        if !(0..=crate::shared_values::MAX_SAFE_INTEGER).contains(&value) {
            return Err(serde::de::Error::custom(
                "Task version is outside the supported range",
            ));
        }
        Ok(Self(value))
    }
}

/// One tagged owner consumed by both HTTP and agent mutation adapters.
///
/// ```compile_fail
/// use studio_runtime_schema::task_command::TaskCommand;
/// let command = TaskCommand::Cancel { task_id: "task".into(), version: None };
/// // A cancellation always carries its reason.
/// ```
///
/// ```compile_fail
/// use studio_runtime_schema::task_command::Version;
/// let version = Version(-1); // Ingress validation cannot be bypassed.
/// ```
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TaskCommand {
    List,
    Create {
        #[schemars(length(min = 1, max = MAX_TITLE_CODEPOINTS))]
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(length(max = MAX_HTTP_DESCRIPTION_CODEPOINTS))]
        #[schemars(transform = tool_non_null)]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(transform = tool_non_null)]
        status: Option<InitialStatus>,
        #[serde(default)]
        #[schemars(length(max = MAX_DEPENDENCIES))]
        dependencies: Vec<String>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        #[schemars(with = "Option<Value>")]
        result_schema: Field<Value>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        #[schemars(with = "Option<crate::task_gate::GateInput>")]
        gate: Field<crate::task_gate::GateInput>,
    },
    Update {
        task_id: String,
        #[serde(default)]
        version: Option<Version>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(length(min = 1, max = MAX_TITLE_CODEPOINTS))]
        #[schemars(transform = tool_non_null)]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(length(max = MAX_HTTP_DESCRIPTION_CODEPOINTS))]
        #[schemars(transform = tool_non_null)]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        #[schemars(with = "Option<String>")]
        owner: Field<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(transform = tool_non_null)]
        status: Option<InitialStatus>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(length(max = MAX_DEPENDENCIES))]
        #[schemars(transform = tool_non_null)]
        dependencies: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        #[schemars(with = "Option<Value>")]
        result_schema: Field<Value>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        #[schemars(with = "Option<crate::task_gate::GateInput>")]
        gate: Field<crate::task_gate::GateInput>,
    },
    Cancel {
        task_id: String,
        #[serde(default)]
        version: Option<Version>,
        #[schemars(length(min = 1, max = MAX_TOOL_CANCEL_REASON_CODEPOINTS))]
        reason: String,
    },
    Reject {
        task_id: String,
        #[serde(default)]
        version: Option<Version>,
        #[schemars(length(min = 1, max = MAX_REVIEW_CODEPOINTS))]
        result: String,
    },
    Claim {
        task_id: String,
        #[serde(default)]
        version: Option<Version>,
        #[serde(default)]
        owner: Option<String>,
    },
    Submit {
        task_id: String,
        #[serde(default)]
        version: Option<Version>,
        result: Value,
        #[schemars(length(max = MAX_CHECKS_CODEPOINTS))]
        checks: String,
        #[schemars(length(max = MAX_REVISION_CODEPOINTS))]
        revision: String,
        #[serde(default)]
        #[schemars(length(max = MAX_FILES))]
        files: Vec<String>,
    },
    Accept {
        task_id: String,
        #[serde(default)]
        version: Option<Version>,
        #[schemars(length(min = 1, max = MAX_REVIEW_CODEPOINTS))]
        result: String,
        #[serde(default)]
        #[schemars(length(min = 1, max = crate::task_gate::MAX_TEXT_CODEPOINTS))]
        override_reason: Option<String>,
    },
    SetDefaultGate {
        #[schemars(required)]
        gate: Option<crate::task_gate::GateInput>,
    },
}

impl TaskCommand {
    pub fn parse(value: &Value) -> Result<Self, String> {
        static CONTRACT: std::sync::OnceLock<(Value, crate::Validator)> =
            std::sync::OnceLock::new();
        Self::parse_contract(value, CONTRACT.get_or_init(|| compiled_contract(schema())))
    }

    /// The agent transport retains its historical description and reason budgets.
    pub fn parse_tool(value: &Value) -> Result<Self, String> {
        static CONTRACT: std::sync::OnceLock<(Value, crate::Validator)> =
            std::sync::OnceLock::new();
        Self::parse_contract(
            value,
            CONTRACT.get_or_init(|| compiled_contract(tool_schema())),
        )
    }

    fn parse_contract(
        value: &Value,
        (schema, validator): &(Value, crate::Validator),
    ) -> Result<Self, String> {
        let action = value["action"].as_str().ok_or("Invalid task action")?;
        let variant = schema["oneOf"]
            .as_array()
            .expect("tagged command variants")
            .iter()
            .find(|variant| variant["properties"]["action"]["const"] == action)
            .ok_or("Unknown task action")?;
        // Derive required-field diagnostics from the same generated owner.
        for field in variant["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !value
                .as_object()
                .is_some_and(|object| object.contains_key(field))
            {
                return Err(
                    if field == "result" && matches!(action, "accept" | "reject") {
                        REVIEW_RESULT_REQUIRED.to_owned()
                    } else {
                        format!("Supply {field} as text for this task action")
                    },
                );
            }
        }
        let command = Self::deserialize(value).map_err(|error| error.to_string())?;
        if !validator.is_valid(value) {
            return Err("Invalid task command field or bound".into());
        }
        Ok(command)
    }
}

fn compiled_contract(schema: Value) -> (Value, crate::Validator) {
    // Schemars derives this schema from the command owner; parity witnesses compile it.
    let validator = crate::Validator::new(&schema).expect("task command schema");
    (schema, validator)
}

/// Derive HTTP mutation variants from the shared payload owner.
pub fn schema() -> Value {
    schema_with_bounds(
        MAX_HTTP_DESCRIPTION_CODEPOINTS,
        MAX_HTTP_CANCEL_REASON_CODEPOINTS,
        false,
    )
}

/// Derive agent-tool mutation variants from the same payload owner.
pub fn tool_schema() -> Value {
    schema_with_bounds(
        MAX_TOOL_DESCRIPTION_CODEPOINTS,
        MAX_TOOL_CANCEL_REASON_CODEPOINTS,
        true,
    )
}

fn schema_with_bounds(
    description_max_codepoints: usize,
    cancel_reason_max_codepoints: usize,
    reject_consumed_null: bool,
) -> Value {
    let mut schema = schemars::generate::SchemaSettings::draft2020_12()
        .with(|settings| settings.inline_subschemas = true)
        .for_deserialize()
        .into_generator()
        .into_root_schema_for::<TaskCommand>()
        .to_value();
    for variant in schema["oneOf"]
        .as_array_mut()
        .expect("tagged command variants")
    {
        let properties = variant["properties"]
            .as_object_mut()
            .expect("command properties");
        for field in ["agent", "id"] {
            properties.insert(field.into(), json!({"type":["string","null"]}));
        }
        if let Some(reason) = properties.get_mut("reason") {
            reason["maxLength"] = json!(cancel_reason_max_codepoints);
        }
        if properties.contains_key("gate") {
            properties.insert("gate".into(), crate::task_gate::input_schema());
        }
        if properties.contains_key("result_schema") {
            properties.insert("result_schema".into(), crate::task_results::schema_input());
        }
        if variant["properties"]["action"]["const"] == "submit" {
            variant["properties"]["result"] =
                json!({"maxLength":crate::task_results::MAX_HTTP_RESULT_TEXT_CODEPOINTS});
        }
    }
    // Known unused fields remain valid for older multi-action callers. Serde
    // consumes only this action's payload; the original JSON owns its receipt.
    schema["properties"] = Value::Object(command_properties(
        schema["oneOf"].as_array().expect("command variants"),
    ));
    schema["additionalProperties"] = json!(false);
    // The shared flat envelope keeps unused legacy fields. Only an action that
    // consumes a field applies its transport's narrower budget.
    for variant in schema["oneOf"].as_array_mut().expect("command variants") {
        for property in variant["properties"]
            .as_object_mut()
            .expect("command properties")
            .values_mut()
        {
            let non_null = property
                .as_object_mut()
                .expect("property schema")
                .remove(TOOL_NON_NULL_EXTENSION)
                .is_some();
            if reject_consumed_null && non_null {
                if let Some(types) = property.get_mut("type").and_then(Value::as_array_mut) {
                    types.retain(|value| value != "null");
                }
                if let Some(values) = property.get_mut("enum").and_then(Value::as_array_mut) {
                    values.retain(|value| !value.is_null());
                }
            }
        }
        if let Some(description) = variant["properties"].get_mut("description") {
            description["maxLength"] = json!(description_max_codepoints);
        }
    }
    schema
}

const TOOL_NON_NULL_EXTENSION: &str = "x-studio-tool-non-null";

fn tool_non_null(schema: &mut schemars::Schema) {
    schema.insert(TOOL_NON_NULL_EXTENSION.into(), json!(true));
}

fn command_properties(variants: &[Value]) -> serde_json::Map<String, Value> {
    let mut fields = std::collections::BTreeMap::<String, Vec<Value>>::new();
    for variant in variants {
        for (field, schema) in variant["properties"]
            .as_object()
            .expect("command properties")
        {
            let mut optional = schema.clone();
            optional
                .as_object_mut()
                .expect("property schema")
                .remove(TOOL_NON_NULL_EXTENSION);
            // A required action's nonempty text constraint does not apply when
            // that field is unused. Its action variant still enforces it.
            optional
                .as_object_mut()
                .expect("property schema")
                .remove("minLength");
            let alternatives = fields.entry(field.clone()).or_default();
            if !alternatives.contains(&optional) {
                alternatives.push(optional);
            }
        }
    }
    fields
        .into_iter()
        .map(|(field, mut alternatives)| {
            let mut schema = if alternatives.len() == 1 {
                alternatives.remove(0)
            } else {
                json!({"anyOf":alternatives})
            };
            // Historical flat optional scalar/list fields accepted null even
            // on actions which did not consume them. Named object contracts
            // (including result_schema) retain their owner's exact schema.
            if matches!(
                schema.get("type").and_then(Value::as_str),
                Some("string" | "integer" | "number" | "array")
            ) {
                schema = json!({"anyOf":[schema,{"type":"null"}]});
            }
            (field, schema)
        })
        .collect()
}

pub fn apply_api_contract(document: &mut Value) {
    let mut body = schema();
    for variant in body["oneOf"].as_array_mut().expect("command variants") {
        if variant["properties"]["action"]["const"] == "submit" {
            variant["properties"]["result"] = crate::task_results::result_input_field();
        }
    }
    document["components"]["schemas"]["WorkBody"] = body;
}

pub fn check_api_contract(document: &Value) -> Result<(), String> {
    let mut expected = document.clone();
    apply_api_contract(&mut expected);
    if expected["components"]["schemas"]["WorkBody"]
        != document["components"]["schemas"]["WorkBody"]
    {
        return Err("WorkBody must match runtime-schema/src/task_command.rs".into());
    }
    Ok(())
}

/// The SDK requires a flat object advertisement. Field schemas come from this
/// owner; action-specific requirements remain enforced by `TaskCommand::parse`.
/// Read actions retain the existing tool catalog's filters and pagination.
pub fn apply_tool_contract(name: &str, input: &mut serde_json::Map<String, Value>) {
    if name != "orchestration_task" {
        return;
    }
    // A flat advertisement cannot distinguish used from unused fields. Keep
    // nullable legacy fields; action-specific tool ingress enforces null refusal.
    let variants = schema_with_bounds(
        MAX_TOOL_DESCRIPTION_CODEPOINTS,
        MAX_TOOL_CANCEL_REASON_CODEPOINTS,
        false,
    )["oneOf"]
        .as_array()
        .expect("command variants")
        .clone();
    for (field, mut schema) in command_properties(&variants) {
        if !matches!(field.as_str(), "action" | "agent" | "id") {
            // SDK conversion applies JSON Schema defaults. Preserve omission
            // of optional arguments; defaults inside supplied objects still
            // belong to those objects' owners (for example gate deadlines).
            schema
                .as_object_mut()
                .expect("field schema")
                .remove("default");
            if let Some(alternatives) = schema.get_mut("anyOf").and_then(Value::as_array_mut) {
                for alternative in alternatives {
                    alternative
                        .as_object_mut()
                        .expect("field alternative")
                        .remove("default");
                }
            }
            input["properties"][field] = schema;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_command_schema_matches_owner_and_transport_variants() {
        let document: Value =
            serde_json::from_str(include_str!("../../runtime-http/openapi.json")).expect("OpenAPI");
        check_api_contract(&document).expect("generated request owner");
        let mut tool = json!({"type":"object","properties":{"action":{"type":"string"}},"required":["action"]}).as_object().expect("object").clone();
        apply_tool_contract("orchestration_task", &mut tool);
        assert_eq!(
            tool["properties"]["result_schema"],
            crate::task_results::schema_input(),
            "a single named schema keeps its owner's exact representation"
        );
        let validator = crate::Validator::new(&Value::Object(tool)).expect("tool variants");
        for raw in [
            json!({"action":"create","title":"Task","description":"x".repeat(32000)}),
            json!({"action":"create","title":"Task","checks":null,"task_id":null,"reason":null,"revision":null,"files":null}),
            json!({"action":"create","title":"Task","reason":""}),
            json!({"action":"cancel","task_id":"task","reason":"done","title":""}),
            json!({"action":"submit","task_id":"task","result":{"answer":null},"checks":"checked","revision":"abc"}),
            json!({"action":"update","task_id":"task","version":2.0,"owner":null}),
        ] {
            assert!(validator.is_valid(&raw), "{raw}");
            assert!(TaskCommand::parse_tool(&raw).is_ok());
        }
        for raw in [
            json!({"action":"create"}),
            json!({"action":"cancel","task_id":"task"}),
            json!({"action":"create","title":null}),
            json!({"action":"create","title":""}),
            json!({"action":"cancel","task_id":"task","reason":null}),
            json!({"action":"cancel","task_id":"task","reason":""}),
            json!({"action":"cancel","task_id":null,"reason":"done"}),
            json!({"action":"submit","task_id":"task","result":"done","checks":null,"revision":"abc"}),
            json!({"action":"submit","task_id":"task","result":"done","checks":"checked","revision":null}),
            json!({"action":"submit","task_id":"task","result":"done","checks":"checked","revision":"abc","files":null}),
            json!({"action":"update","task_id":"task","owner":42}),
            json!({"action":"update","task_id":"task","version":2.5}),
        ] {
            assert!(TaskCommand::parse_tool(&raw).is_err());
        }
        // The advertisement describes fields; the server requires the payload
        // selected by the action even when the SDK accepts an incomplete object.
        let incomplete = json!({"action":"cancel","task_id":"task"});
        assert!(validator.is_valid(&incomplete));
        assert!(TaskCommand::parse_tool(&incomplete).is_err());
        for malformed in [
            json!({"action":"update","task_id":"task","owner":42}),
            json!({"action":"update","task_id":"task","version":2.5}),
        ] {
            assert!(!validator.is_valid(&malformed));
        }
    }
    #[test]
    fn task_commands_validate_variants_and_keep_presence_and_opaque_results() {
        let TaskCommand::Update {
            owner,
            result_schema,
            version,
            ..
        } = TaskCommand::parse(&json!({
            "action":"update","task_id":"task","version":2.0,"owner":null,"result_schema":false
        }))
        .expect("integral decimal and nullable owner")
        else {
            panic!("update command")
        };
        assert!(matches!(owner, Field::Null));
        assert!(matches!(result_schema, Field::Value(Value::Bool(false))));
        assert_eq!(version.expect("version").get(), 2);
        let TaskCommand::Update {
            owner,
            result_schema,
            ..
        } = TaskCommand::parse(&json!({"action":"update","task_id":"task"}))
            .expect("omitted fields")
        else {
            panic!("update command")
        };
        assert!(matches!(owner, Field::Missing));
        assert!(matches!(result_schema, Field::Missing));
        assert!(TaskCommand::parse(&json!({"action":"submit","task_id":"task","result":{"nested":[null,true]},"checks":"checked","revision":"abc"})).is_ok());
        for value in [
            json!({"action":"cancel","task_id":"task"}),
            json!({"action":"update","task_id":"task","version":2.5}),
            json!({"action":"create","title":"x","status":"accepted"}),
            json!({"action":"update","task_id":"task","dependencies":[42]}),
            json!({"action":"create","title":"x","description":"🙂".repeat(64001)}),
        ] {
            assert!(TaskCommand::parse(&value).is_err(), "{value}");
        }
        assert!(
            TaskCommand::parse(
                &json!({"action":"create","title":"x","description":"🙂".repeat(64000)})
            )
            .is_ok()
        );
    }
    #[test]
    fn task_transport_text_bounds_and_consumed_nulls_remain_distinct() {
        for token in ["x", "🙂"] {
            for action in ["create", "update"] {
                for (size, valid) in [(160, true), (161, false)] {
                    let value =
                        json!({"action":action,"task_id":"task","title":token.repeat(size)});
                    assert_eq!(TaskCommand::parse(&value).is_ok(), valid);
                    assert_eq!(TaskCommand::parse_tool(&value).is_ok(), valid);
                }
                for (size, http, tool) in [
                    (32000, true, true),
                    (32001, true, false),
                    (64000, true, false),
                    (64001, false, false),
                ] {
                    let value = json!({"action":action,"task_id":"task","title":"Task","description":token.repeat(size)});
                    assert_eq!(TaskCommand::parse(&value).is_ok(), http);
                    assert_eq!(TaskCommand::parse_tool(&value).is_ok(), tool);
                }
                for field in ["description", "status"] {
                    let mut value = json!({"action":action,"task_id":"task","title":"Task"});
                    value[field] = Value::Null;
                    assert!(TaskCommand::parse(&value).is_ok());
                    assert!(TaskCommand::parse_tool(&value).is_err());
                }
            }
            for (size, http, tool) in [
                (12000, true, true),
                (12001, false, true),
                (32000, false, true),
                (32001, false, false),
            ] {
                let value = json!({"action":"cancel","task_id":"task","reason":token.repeat(size)});
                assert_eq!(TaskCommand::parse(&value).is_ok(), http);
                assert_eq!(TaskCommand::parse_tool(&value).is_ok(), tool);
            }
        }
        for value in [
            json!({"action":"cancel","task_id":"task","reason":"done","description":null,"status":null}),
            json!({"action":"cancel","task_id":"task","reason":"done","description":"x".repeat(32001)}),
            json!({"action":"create","title":"Task","reason":null}),
        ] {
            assert!(TaskCommand::parse(&value).is_ok());
            assert!(TaskCommand::parse_tool(&value).is_ok());
        }
    }
}
