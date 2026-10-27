//! Task mutation ingress owner. Receipt signatures continue to use the original
//! transport JSON, so decoding never rewrites decimal numbers, omission or null.

use crate::queue_command::Field;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Task title length, in Unicode code points.
pub const MAX_TITLE_CODEPOINTS: usize = 160;
/// Task description length, in Unicode code points (the existing HTTP limit).
pub const MAX_DESCRIPTION_CODEPOINTS: usize = 64_000;
/// Task cancellation reason length, in Unicode code points.
pub const MAX_CANCEL_REASON_CODEPOINTS: usize = 12_000;
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
        #[schemars(length(max = MAX_DESCRIPTION_CODEPOINTS))]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
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
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(length(max = MAX_DESCRIPTION_CODEPOINTS))]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        #[schemars(with = "Option<String>")]
        owner: Field<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<InitialStatus>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(length(max = MAX_DEPENDENCIES))]
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
        #[schemars(length(min = 1, max = MAX_CANCEL_REASON_CODEPOINTS))]
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
        // The generated schema is compiled and checked by the contract witnesses.
        let (schema, validator) = CONTRACT.get_or_init(|| {
            let schema = schema();
            let validator = crate::Validator::new(&schema).expect("task command schema");
            (schema, validator)
        });
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

/// Derive the HTTP request and tool mutation variants from serde's tagged owner.
pub fn schema() -> Value {
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
    schema
}

fn command_properties(variants: &[Value]) -> serde_json::Map<String, Value> {
    let mut fields = std::collections::BTreeMap::<String, Vec<Value>>::new();
    for variant in variants {
        for (field, schema) in variant["properties"]
            .as_object()
            .expect("command properties")
        {
            let mut optional = schema.clone();
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

/// Tool reads retain their pagination contract; every mutation field and its
/// action-specific requirements are generated from the command owner.
pub fn apply_tool_contract(name: &str, input: &mut serde_json::Map<String, Value>) {
    if name != "orchestration_task" {
        return;
    }
    let variants = schema()["oneOf"]
        .as_array()
        .expect("command variants")
        .clone();
    for (field, schema) in command_properties(&variants) {
        if !matches!(field.as_str(), "action" | "agent" | "id") {
            input["properties"][field] = schema;
        }
    }
    input.insert("allOf".into(), Value::Array(variants.into_iter().map(|variant| {
        json!({"if":{"properties":{"action":variant["properties"]["action"]},"required":["action"]},
            "then":variant})
    }).collect()));
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
            json!({"action":"create","title":"Task","description":"x".repeat(64000)}),
            json!({"action":"create","title":"Task","checks":null,"task_id":null,"reason":null,"revision":null,"files":null}),
            json!({"action":"create","title":"Task","reason":""}),
            json!({"action":"cancel","task_id":"task","reason":"done","title":""}),
            json!({"action":"submit","task_id":"task","result":{"answer":null},"checks":"checked","revision":"abc"}),
            json!({"action":"update","task_id":"task","version":2.0,"owner":null}),
        ] {
            assert!(validator.is_valid(&raw), "{raw}");
            assert!(TaskCommand::parse(&raw).is_ok());
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
            assert!(!validator.is_valid(&raw), "{raw}");
            assert!(TaskCommand::parse(&raw).is_err());
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
}
