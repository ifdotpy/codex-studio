//! Agent-list progress contract. Timestamps use Unix seconds.
use serde::{Deserialize, Serialize};

/// A managed command currently starting or running for this agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentCommandProgress {
    pub monitor_id: String,
    /// Human-readable stage: the first line of the managed command.
    pub stage: String,
    pub started_at: f64,
    pub last_output_line: String,
}

/// An idle worker still owns this unsubmitted assignment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentStall {
    pub task_id: String,
    /// Later of the assignment update and latest settled execution, in Unix seconds.
    pub since: f64,
}

/// Schemas consumed by OpenAPI and its generated renderer contract.
pub(crate) fn schemas() -> [(&'static str, serde_json::Value); 2] {
    use serde_json::json;
    [
        (
            "AgentCommandProgress",
            json!({"type":"object","title":"AgentCommandProgress","additionalProperties":false,
            "properties":{"monitorId":{"type":"string","title":"Monitorid"},"stage":{"type":"string","title":"Stage"},
            "startedAt":{"type":"number","title":"Startedat"},"lastOutputLine":{"type":"string","title":"Lastoutputline"}},
            "required":["lastOutputLine","monitorId","stage","startedAt"]}),
        ),
        (
            "AgentStall",
            json!({"type":"object","title":"AgentStall","additionalProperties":false,
            "properties":{"taskId":{"type":"string","title":"Taskid"},"since":{"type":"number","title":"Since"}},
            "required":["since","taskId"]}),
        ),
    ]
}

pub(crate) fn check_schema_parity(openapi: &serde_json::Value) -> Result<(), String> {
    for (name, schema) in schemas() {
        if openapi.pointer(&format!("/components/schemas/{name}")) != Some(&schema) {
            return Err(format!(
                "{name} must match runtime-schema/src/agent_progress.rs"
            ));
        }
    }
    for (field, schema) in [
        (
            "commandProgress",
            serde_json::json!({"type":"array","title":"Commandprogress","default":[],"items":{"$ref":"#/components/schemas/AgentCommandProgress"}}),
        ),
        (
            "stall",
            serde_json::json!({"anyOf":[{"$ref":"#/components/schemas/AgentStall"},{"type":"null"}],"default":null}),
        ),
    ] {
        for consumer in ["AgentEntityDto", "AgentResponse", "BranchResponse"] {
            let mut expected = schema.clone();
            // FastAPI documents null defaults on direct route responses; the
            // shared nested entity schema omits that response metadata.
            if consumer == "AgentEntityDto" && field == "stall" {
                // Both property schemas above are object literals.
                expected
                    .as_object_mut()
                    .expect("property schema is an object")
                    .remove("default");
            }
            if openapi.pointer(&format!(
                "/components/schemas/{consumer}/properties/{field}"
            )) != Some(&expected)
            {
                return Err(format!(
                    "{consumer}.{field} must match runtime-schema/src/agent_progress.rs"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn agent_progress_schema_matches_strict_serde_contract() {
        let examples = [
            serde_json::to_value(AgentCommandProgress {
                monitor_id: "m".into(),
                stage: "test".into(),
                started_at: 1.0,
                last_output_line: "pass".into(),
            })
            .unwrap(),
            serde_json::to_value(AgentStall {
                task_id: "t".into(),
                since: 2.0,
            })
            .unwrap(),
        ];
        for ((_, schema), example) in schemas().into_iter().zip(examples) {
            let validator = jsonschema::validator_for(&schema).unwrap();
            assert!(validator.is_valid(&example));
            assert_eq!(
                schema["required"].as_array().unwrap().len(),
                example.as_object().unwrap().len()
            );
        }
        assert!(
            serde_json::from_str::<AgentStall>(r#"{"taskId":"t","since":1,"extra":true}"#).is_err()
        );
    }
}
