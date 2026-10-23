//! Owner of the task result contract and its resource limits.
use crate::{Draft, Validator};
use serde_json::{Value, json};

/// Maximum serialized UTF-8 bytes in a task's result schema.
pub const MAX_RESULT_SCHEMA_BYTES: usize = 32_768;
/// Maximum schema nesting depth, in subschemas.
pub const MAX_RESULT_SCHEMA_DEPTH: usize = 32;
/// Maximum failed submissions per task version and provider attempt.
pub const MAX_FAILED_RESULT_SUBMITS_PER_ATTEMPT: u64 = 3;
/// Maximum serialized UTF-8 bytes in a structured result.
pub const MAX_STRUCTURED_RESULT_BYTES: usize = 128_000;
/// Maximum validation errors returned for one submission.
pub const MAX_RESULT_VALIDATION_ERRORS: usize = 32;
/// Existing HTTP text-result bound, in Unicode code points.
pub const MAX_HTTP_RESULT_TEXT_CODEPOINTS: usize = 12_000;
/// Maximum message length for one validation error, in Unicode code points.
pub const MAX_RESULT_ERROR_MESSAGE_CODEPOINTS: usize = 1_024;
pub const RESULT_SCHEMA_DRAFT: &str = "https://json-schema.org/draft/2020-12/schema";

/// Compile a bounded, self-contained draft 2020-12 subset. References, regex,
/// custom vocabularies and unknown keywords are refused, never silently ignored.
pub fn compile(schema: &Value) -> Result<Validator, String> {
    if schema.to_string().len() > MAX_RESULT_SCHEMA_BYTES {
        return Err(format!(
            "result_schema exceeds {MAX_RESULT_SCHEMA_BYTES} UTF-8 bytes"
        ));
    }
    check_subset(schema, "", 0)?;
    Validator::options()
        .with_draft(Draft::Draft202012)
        .build(schema)
        .map_err(|error| {
            format!(
                "Invalid result_schema at {}: {error}",
                error.instance_path()
            )
        })
}

fn check_subset(schema: &Value, path: &str, depth: usize) -> Result<(), String> {
    if depth > MAX_RESULT_SCHEMA_DEPTH {
        return Err(format!(
            "Invalid result_schema at {path}: nesting exceeds {MAX_RESULT_SCHEMA_DEPTH}"
        ));
    }
    if schema.is_boolean() {
        return Ok(());
    }
    let object = schema
        .as_object()
        .ok_or_else(|| format!("Invalid result_schema at {path}: expected an object or boolean"))?;
    for (key, value) in object {
        let path = format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"));
        match key.as_str() {
            "$schema" if value.as_str() == Some(RESULT_SCHEMA_DRAFT) => {}
            "title" | "description" | "$comment" | "default" | "examples" | "type" | "enum"
            | "const" | "required" | "dependentRequired" | "minimum" | "maximum"
            | "exclusiveMinimum" | "exclusiveMaximum" | "multipleOf" | "minLength"
            | "maxLength" | "minItems" | "maxItems" | "uniqueItems" | "minProperties"
            | "maxProperties" | "minContains" | "maxContains" => {}
            "properties" | "dependentSchemas" => {
                let entries = value.as_object().ok_or_else(|| {
                    format!("Invalid result_schema at {path}: expected an object")
                })?;
                for (name, child) in entries {
                    check_subset(
                        child,
                        &format!("{path}/{}", name.replace('~', "~0").replace('/', "~1")),
                        depth + 1,
                    )?;
                }
            }
            "items"
            | "additionalProperties"
            | "propertyNames"
            | "contains"
            | "not"
            | "if"
            | "then"
            | "else" => check_subset(value, &path, depth + 1)?,
            "allOf" | "anyOf" | "oneOf" | "prefixItems" => {
                let children = value
                    .as_array()
                    .ok_or_else(|| format!("Invalid result_schema at {path}: expected an array"))?;
                for (index, child) in children.iter().enumerate() {
                    check_subset(child, &format!("{path}/{index}"), depth + 1)?;
                }
            }
            _ => {
                return Err(format!(
                    "Invalid result_schema at {path}: unsupported keyword or draft"
                ));
            }
        }
    }
    Ok(())
}

/// `result` can be a JSON value or JSON text. A directly valid string stays a string.
pub fn validate_result(schema: &Value, input: &Value) -> Result<Value, Vec<Value>> {
    let error = |message: String| vec![json!({"path":"", "message":message})];
    if input.to_string().len() > MAX_STRUCTURED_RESULT_BYTES {
        return Err(error(format!(
            "result exceeds {MAX_STRUCTURED_RESULT_BYTES} UTF-8 bytes"
        )));
    }
    let validator = compile(schema).map_err(error)?;
    let value = if validator.is_valid(input) {
        input.clone()
    } else if let Some(text) = input.as_str() {
        serde_json::from_str(text).map_err(|e| error(format!("result must be valid JSON: {e}")))?
    } else {
        input.clone()
    };
    let errors: Vec<_> = validator
        .iter_errors(&value)
        .take(MAX_RESULT_VALIDATION_ERRORS)
        .map(|e| json!({"path":e.instance_path().to_string(), "message":e.to_string().chars().take(MAX_RESULT_ERROR_MESSAGE_CODEPOINTS).collect::<String>()}))
        .collect();
    if errors.is_empty() {
        Ok(value)
    } else {
        Err(errors)
    }
}

pub fn schema_input() -> Value {
    json!({"type":["object","boolean"],"additionalProperties":true,"description":format!("Optional draft 2020-12 result contract for create/update (maximum {MAX_RESULT_SCHEMA_BYTES} UTF-8 bytes). Supports types, properties, required, additionalProperties, arrays, bounds, enum/const and combinators. References, regex and unknown keywords are refused. A new task version resets the submit cap.")})
}

/// Apply the same contract to native and legacy provider tool definitions.
pub fn apply_tool_contract(name: &str, input: &mut serde_json::Map<String, Value>) {
    if name == "orchestration_task" {
        input["properties"]["result_schema"] = schema_input();
        input["properties"]["result"] = json!({"description":"For submit with result_schema, supply a JSON value or JSON text that validates. Without a schema, supply text. For accept/reject, supply the review decision as text."});
    } else if name == "orchestration_spawn" {
        input["properties"]["agents"]["items"]["properties"]["task_id"]["description"] = json!(
            "Assign this task to the worker. Its result_schema is included in the assignment and enforced at submit."
        );
    }
}

pub fn assignment_text(work: &Value) -> String {
    work.get("resultSchema").map(|schema| format!("\n\n[Task result contract]\nSubmit result as JSON that validates against this draft 2020-12 schema:\n{schema}\nInvalid submissions leave the task unchanged. After {MAX_FAILED_RESULT_SUBMITS_PER_ATTEMPT} failures in one attempt, further submissions are refused and the lead receives an event.")).unwrap_or_default()
}

/// OpenAPI result fields use the same JSON value contract as task receipts.
pub fn result_field() -> Value {
    json!({"$ref":"#/components/schemas/JsonValue"})
}

pub fn result_input_field() -> Value {
    json!({"$ref":"#/components/schemas/JsonValue", "maxLength":MAX_HTTP_RESULT_TEXT_CODEPOINTS})
}

pub fn check_schema_parity(openapi: &Value) -> Result<(), String> {
    if openapi["components"]["schemas"]["WorkBody"]["properties"]["result"] != result_input_field()
    {
        return Err(
            "WorkBody.result must use task_results::result_input_field from runtime-schema".into(),
        );
    }
    for name in [
        "WorkResultDto",
        "WorkResultRecord",
        "WorkResultRecordResponse",
    ] {
        if openapi["components"]["schemas"][name]["properties"]["result"] != result_field() {
            return Err(format!(
                "{name}.result must use task_results::result_field from runtime-schema"
            ));
        }
    }
    if openapi["components"]["schemas"]["WorkBody"]["properties"]["result_schema"] != schema_input()
    {
        return Err(
            "WorkBody.result_schema must use task_results::schema_input from runtime-schema".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_result_schema_rejects_invalid_and_oversized_contracts() {
        assert!(compile(&json!({"type":"object","properties":{"count":{"type":"integer"}},"required":["count"]})).is_ok());
        for schema in [
            json!({"type":"invalid"}),
            json!({"required":true}),
            json!({"$ref":"https://example.com/schema"}),
            json!({"typo":true}),
            json!({"properties":{"$ref":{"type":"string"}} ,"type":"object","minimum":true}),
        ] {
            assert!(compile(&schema).is_err(), "{schema}");
        }
        assert!(
            compile(&json!({"description":"x".repeat(MAX_RESULT_SCHEMA_BYTES)}))
                .expect_err("oversized schema must fail")
                .contains("bytes")
        );
    }
    #[test]
    fn task_result_schema_openapi_copies_match_owner() {
        let document: Value =
            serde_json::from_str(include_str!("../../runtime-http/openapi.json")).expect("OpenAPI");
        check_schema_parity(&document).expect("task contract parity");
    }

    #[test]
    fn task_result_schema_reports_instance_paths_and_preserves_json() {
        let schema =
            json!({"type":"object","properties":{"count":{"type":"integer"}},"required":["count"]});
        assert_eq!(
            validate_result(&json!({"type":"string"}), &json!("hello"))
                .expect("valid string result"),
            json!("hello")
        );
        assert!(compile(&json!(true)).is_ok());
        assert!(compile(&json!(false)).is_ok());
        assert_eq!(
            validate_result(&json!(true), &Value::Null).expect("unrestricted schema accepts null"),
            Value::Null
        );
        assert!(validate_result(&json!(false), &Value::Null).is_err());
        assert_eq!(
            validate_result(&schema, &json!({"count":3})).expect("valid object result"),
            json!({"count":3})
        );
        assert_eq!(
            validate_result(&schema, &json!("{\"count\":3}")).expect("valid JSON object string"),
            json!({"count":3})
        );
        assert_eq!(
            validate_result(&schema, &json!({"count":"bad"})).expect_err("invalid count must fail")
                [0]["path"],
            "/count"
        );
        assert!(
            validate_result(&schema, &json!("not JSON")).expect_err("invalid JSON must fail")[0]["message"]
                .as_str()
                .expect("validation message is text")
                .contains("valid JSON")
        );
    }
}
