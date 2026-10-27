//! Agent-list progress contract. Timestamps use Unix seconds.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Persisted wait field shared by admission owners, status, and sync.
pub const START_WAIT_FIELD: &str = "startWait";

/// One epoch-scoped wait. `until` is a retry deadline in Unix seconds.
///
/// ```compile_fail
/// use studio_runtime_schema::agent_progress::AgentStartWait;
/// let _ = AgentStartWait::Slot { epoch: 1 };
/// ```
/// ```compile_fail
/// use studio_runtime_schema::agent_progress::AgentStartWait;
/// let _ = AgentStartWait::Pause { epoch: 1 };
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "reason",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum AgentStartWait {
    Slot {
        #[schemars(range(min = 0, max = crate::shared_values::MAX_SAFE_INTEGER))]
        epoch: i64,
        #[schemars(range(min = 0, max = crate::shared_values::MAX_SAFE_INTEGER))]
        running_turns: u64,
        #[schemars(range(min = 0, max = crate::shared_values::MAX_SAFE_INTEGER))]
        max_running_turns: u64,
    },
    Pause {
        #[schemars(range(min = 0, max = crate::shared_values::MAX_SAFE_INTEGER))]
        epoch: i64,
        until: f64,
    },
    StartQueue {
        #[schemars(range(min = 0, max = crate::shared_values::MAX_SAFE_INTEGER))]
        epoch: i64,
    },
    Disk {
        #[schemars(range(min = 0, max = crate::shared_values::MAX_SAFE_INTEGER))]
        epoch: i64,
    },
    ModelCatalog {
        #[schemars(range(min = 0, max = crate::shared_values::MAX_SAFE_INTEGER))]
        epoch: i64,
    },
    ProviderSession {
        #[schemars(range(min = 0, max = crate::shared_values::MAX_SAFE_INTEGER))]
        epoch: i64,
    },
    Dependency {
        #[schemars(range(min = 0, max = crate::shared_values::MAX_SAFE_INTEGER))]
        epoch: i64,
    },
    DispatchPending {
        #[schemars(range(min = 0, max = crate::shared_values::MAX_SAFE_INTEGER))]
        epoch: i64,
    },
    Paused {
        #[schemars(range(min = 0, max = crate::shared_values::MAX_SAFE_INTEGER))]
        epoch: i64,
    },
    NativeFailure {
        #[schemars(range(min = 0, max = crate::shared_values::MAX_SAFE_INTEGER))]
        epoch: i64,
    },
    AccountTransfer {
        #[schemars(range(min = 0, max = crate::shared_values::MAX_SAFE_INTEGER))]
        epoch: i64,
    },
}

impl AgentStartWait {
    pub fn epoch(&self) -> i64 {
        match self {
            Self::Slot { epoch, .. } => *epoch,
            Self::Pause { epoch, .. } => *epoch,
            Self::StartQueue { epoch, .. } => *epoch,
            Self::Disk { epoch, .. } => *epoch,
            Self::ModelCatalog { epoch, .. } => *epoch,
            Self::ProviderSession { epoch, .. } => *epoch,
            Self::Dependency { epoch, .. } => *epoch,
            Self::DispatchPending { epoch, .. } => *epoch,
            Self::Paused { epoch, .. } => *epoch,
            Self::NativeFailure { epoch, .. } => *epoch,
            Self::AccountTransfer { epoch, .. } => *epoch,
        }
    }
}

/// Decode stored waits from older writers that emitted null for unused details.
/// Incomplete evidence and unknown fields remain errors.
pub fn decode_stored_wait(
    mut value: serde_json::Value,
) -> Result<AgentStartWait, serde_json::Error> {
    if let Some(object) = value.as_object_mut() {
        for key in ["runningTurns", "maxRunningTurns", "until"] {
            if object.get(key).is_some_and(serde_json::Value::is_null) {
                object.remove(key);
            }
        }
    }
    serde_json::from_value(value)
}

/// A managed command currently starting or running for this agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentCommandProgress {
    pub monitor_id: String,
    /// Human-readable stage: the first line of the managed command.
    pub stage: String,
    pub started_at: f64,
    pub last_output_line: String,
}

/// An idle worker still owns this unsubmitted assignment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentStall {
    pub task_id: String,
    /// Later of the assignment update and latest settled execution, in Unix seconds.
    pub since: f64,
}

/// Derive the closed reason set from the tagged wait owner.
fn reason_schema(generator: &mut schemars::SchemaGenerator) -> serde_json::Value {
    let wait = AgentStartWait::json_schema(generator).to_value();
    let values: Vec<_> = wait["oneOf"]
        .as_array()
        .expect("tagged wait variants")
        .iter()
        .map(|variant| {
            serde_json::Value::String(
                variant["properties"]["reason"]["const"]
                    .as_str()
                    .expect("tagged reason is a string")
                    .to_owned(),
            )
        })
        .collect();
    serde_json::json!({"type": "string", "enum": values})
}

/// Schemas consumed by OpenAPI and its generated renderer contract.
pub(crate) fn schemas() -> [(&'static str, serde_json::Value); 5] {
    let mut generator = schemars::generate::SchemaSettings::openapi3()
        .for_serialize()
        .into_generator();
    [
        (
            "AgentEntityStartWaitDto",
            AgentStartWait::json_schema(&mut generator).to_value(),
        ),
        (
            "StartWaitRecordResponse",
            AgentStartWait::json_schema(&mut generator).to_value(),
        ),
        ("StartWaitReason", reason_schema(&mut generator)),
        (
            "AgentCommandProgress",
            AgentCommandProgress::json_schema(&mut generator).to_value(),
        ),
        (
            "AgentStall",
            AgentStall::json_schema(&mut generator).to_value(),
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
    fn agent_progress_openapi_fragments_derive_from_serde_owners() {
        fn fragment<T: schemars::JsonSchema>() -> serde_json::Value {
            T::json_schema(
                &mut schemars::generate::SchemaSettings::openapi3()
                    .for_serialize()
                    .into_generator(),
            )
            .to_value()
        }
        let openapi: serde_json::Value =
            serde_json::from_str(include_str!("../../runtime-http/openapi.json"))
                .expect("OpenAPI source parses");
        for (name, schema) in [
            ("AgentCommandProgress", fragment::<AgentCommandProgress>()),
            ("AgentStall", fragment::<AgentStall>()),
        ] {
            assert_eq!(
                openapi.pointer(&format!("/components/schemas/{name}")),
                Some(&schema)
            );
        }
    }

    #[test]
    fn start_wait_requires_variant_evidence() {
        for value in [
            r#"{"reason":"slot","epoch":3}"#,
            r#"{"reason":"slot","epoch":3,"runningTurns":1}"#,
            r#"{"reason":"pause","epoch":3}"#,
            r#"{"reason":"pause","epoch":3,"until":null}"#,
        ] {
            assert!(serde_json::from_str::<AgentStartWait>(value).is_err());
        }
    }

    #[test]
    fn wait_schema_preserves_safe_integer_bounds() {
        let schema = schemas()[0].1.clone();
        for variant in schema["oneOf"].as_array().expect("wait variants") {
            let properties = variant["properties"].as_object().expect("properties");
            for field in ["epoch", "runningTurns", "maxRunningTurns"] {
                if let Some(number) = properties.get(field) {
                    assert_eq!(number["minimum"], 0);
                    assert_eq!(number["maximum"], crate::shared_values::MAX_SAFE_INTEGER);
                }
            }
        }
    }

    #[test]
    fn stored_wait_accepts_legacy_nulls_without_inventing_evidence() {
        assert_eq!(decode_stored_wait(serde_json::json!({"reason":"slot", "epoch":3, "runningTurns":1, "maxRunningTurns":2, "until":null})).unwrap(), AgentStartWait::Slot { epoch:3, running_turns:1, max_running_turns:2 });
        assert!(
            decode_stored_wait(
                serde_json::json!({"reason":"slot", "epoch":3, "runningTurns":null})
            )
            .is_err()
        );
        assert!(
            decode_stored_wait(serde_json::json!({"reason":"disk", "epoch":3, "extra":null}))
                .is_err()
        );
    }

    #[test]
    fn agent_progress_schema_matches_strict_serde_contract() {
        for wait in [
            AgentStartWait::Slot {
                epoch: 3,
                running_turns: 1,
                max_running_turns: 2,
            },
            AgentStartWait::Pause {
                epoch: 3,
                until: 5.0,
            },
            AgentStartWait::Disk { epoch: 3 },
        ] {
            let value = serde_json::to_value(&wait).expect("typed wait");
            assert_eq!(
                serde_json::from_value::<AgentStartWait>(value).expect("wait round trip"),
                wait
            );
        }
        assert!(
            serde_json::from_str::<AgentStartWait>(r#"{"reason":"unknown","epoch":3}"#).is_err()
        );
        assert!(
            serde_json::from_str::<AgentStartWait>(r#"{"reason":"disk","epoch":3,"guess":true}"#)
                .is_err()
        );

        let examples = [
            serde_json::to_value(AgentCommandProgress {
                monitor_id: "m".into(),
                stage: "test".into(),
                started_at: 1.0,
                last_output_line: "pass".into(),
            })
            .expect("test fixture value"),
            serde_json::to_value(AgentStall {
                task_id: "t".into(),
                since: 2.0,
            })
            .expect("test fixture value"),
        ];
        for ((_, schema), example) in schemas().into_iter().skip(3).zip(examples) {
            let validator = jsonschema::validator_for(&schema).expect("test fixture value");
            assert!(validator.is_valid(&example));
            assert_eq!(
                schema["required"]
                    .as_array()
                    .expect("test fixture value")
                    .len(),
                example.as_object().expect("test fixture value").len()
            );
        }
        assert!(
            serde_json::from_str::<AgentStall>(r#"{"taskId":"t","since":1,"extra":true}"#).is_err()
        );
    }
}
