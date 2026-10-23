//! Acceptance gate input contract. This module owns its timeout and text bounds.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Default acceptance gate deadline, in milliseconds.
pub const DEFAULT_TIMEOUT_MS: u64 = 600_000;
/// Maximum acceptance gate deadline, in milliseconds.
pub const MAX_TIMEOUT_MS: u64 = 3_600_000;
/// Maximum command or override reason length, in Unicode code points.
pub const MAX_TEXT_CODEPOINTS: usize = 32_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Gate {
    pub command: String,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}
fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

pub fn parse(value: &Value) -> Result<Option<Gate>, &'static str> {
    if value.is_null() {
        return Ok(None);
    }
    let gate: Gate = if let Some(command) = value.as_str() {
        Gate {
            command: command.into(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    } else {
        serde_json::from_value(value.clone()).map_err(|_| "Invalid acceptance gate")?
    };
    if gate.command.trim().is_empty()
        || gate.command.chars().count() > MAX_TEXT_CODEPOINTS
        || gate.timeout_ms == 0
        || gate.timeout_ms > MAX_TIMEOUT_MS
    {
        return Err("Invalid acceptance gate command or timeout_ms");
    }
    Ok(Some(gate))
}

pub fn input_schema() -> Value {
    let command = json!({"type":"string","minLength":1,"maxLength":MAX_TEXT_CODEPOINTS});
    json!({"anyOf":[command, {"type":"object","additionalProperties":false,
        "properties":{"command":command,"timeout_ms":{"type":"integer","minimum":1,
        "maximum":MAX_TIMEOUT_MS,"default":DEFAULT_TIMEOUT_MS}},"required":["command"]}, {"type":"null"}],
        "description":"Acceptance command. Runs in the clean submitter workspace at the full submitted Git commit. Null disables the gate. Omit on create to inherit the lead default."})
}

pub fn apply_tool_contract(name: &str, input: &mut serde_json::Map<String, Value>) {
    if name != "orchestration_task" {
        return;
    }
    let Some(properties) = input.get_mut("properties") else {
        return;
    };
    properties["gate"] = input_schema();
    properties["override_reason"] = json!({"type":"string","minLength":1,"maxLength":MAX_TEXT_CODEPOINTS,
        "description":"Lead reason to accept despite a failed, missing, or unknown gate. Saved in the decision."});
    if let Some(actions) = properties["action"]["enum"].as_array_mut()
        && !actions.iter().any(|v| v == "set_default_gate")
    {
        actions.push(json!("set_default_gate"));
    }
}

/// OpenAPI fields consumed by the web. The generator checks these exact fields against this owner.
pub fn apply_api_contract(document: &mut Value) {
    let schemas = &mut document["components"]["schemas"];
    schemas["TaskAcceptanceGate"] = input_schema();
    schemas["TaskGateDefaultResponse"] = json!({"type":"object","properties":{
        "id":{"type":"string"},"defaultGate":{"$ref":"#/components/schemas/TaskAcceptanceGate"}},
        "required":["id","defaultGate"]});
    schemas["TaskGateEvidence"] = json!({"type":"object","additionalProperties":false,
        "properties":{
            "status":{"enum":["not_run","running","passed","failed","unknown"]},
            "config":{"$ref":"#/components/schemas/TaskAcceptanceGate"},
            "cwd":{"type":["string","null"]},"revision":{"type":"string"},
            "monitorId":{"type":["string","null"]},"exitCode":{"type":["integer","null"]},
            "output":{"type":"string","maxLength":crate::shared_values::MONITOR_OUTPUT_TAIL_CODEPOINTS},
            "log":{"type":["string","null"]},"error":{"type":["string","null"]},"finished":{"type":"number"}
        },"required":["status","config","cwd","revision","monitorId","exitCode","output","log"]});
    let input = schemas["WorkBody"]
        .as_object_mut()
        .expect("WorkBody schema");
    apply_tool_contract("orchestration_task", input);
    for name in ["WorkItem", "WorkViewRecordResponse"] {
        schemas[name]["properties"]["gate"] =
            json!({"$ref":"#/components/schemas/TaskAcceptanceGate"});
    }
    for name in [
        "WorkResultDto",
        "WorkResultRecord",
        "WorkResultRecordResponse",
    ] {
        schemas[name]["properties"]["gate"] =
            json!({"$ref":"#/components/schemas/TaskGateEvidence"});
    }
    for name in [
        "WorkDecisionDto",
        "WorkDecisionRecord",
        "WorkDecisionRecordResponse",
    ] {
        schemas[name]["properties"]["gateOverrideReason"] =
            json!({"type":"string","minLength":1,"maxLength":MAX_TEXT_CODEPOINTS});
    }
    let responses = document
        .pointer_mut("/paths/~1api~1work/post/responses/200/content/application~1json/schema/anyOf")
        .and_then(Value::as_array_mut)
        .expect("work response variants");
    let default = json!({"$ref":"#/components/schemas/TaskGateDefaultResponse"});
    if !responses.contains(&default) {
        responses.push(default);
    }
}

pub fn check_api_contract(document: &Value) -> Result<(), String> {
    let mut expected = document.clone();
    apply_api_contract(&mut expected);
    if expected != *document {
        return Err("Acceptance gate schema differs from runtime-schema/src/task_gate.rs".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn acceptance_gate_schema_and_parser_agree() {
        let validator =
            crate::Validator::new(&input_schema()).expect("acceptance gate schema compiles");
        for value in [
            json!("true"),
            json!({"command":"true"}),
            json!({"command":"true","timeout_ms":MAX_TIMEOUT_MS}),
            Value::Null,
        ] {
            assert!(validator.is_valid(&value));
            assert!(parse(&value).is_ok());
        }
        for value in [
            json!(""),
            json!({"command":"true","timeout_ms":0}),
            json!({"command":"true","timeout_ms":MAX_TIMEOUT_MS+1}),
            json!({"command":"true","cwd":std::env::temp_dir()}),
        ] {
            assert!(!validator.is_valid(&value));
            assert!(parse(&value).is_err());
        }
        assert!(parse(&json!("  ")).is_err());
    }
}
