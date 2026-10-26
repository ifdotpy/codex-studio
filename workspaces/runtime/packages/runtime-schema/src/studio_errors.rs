//! Safe, server-owned incident contract. This is diagnostic evidence, never permission to retry.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
mod monitor;
pub use monitor::{
    MonitorOperationIdentity, MonitorRecoveryEvidence, MonitorRecoveryProof, MonitorRecoveryReason,
    MonitorRecoveryStatus, MonitorStatus,
};

pub(crate) const MONITOR_RECORD_SCHEMA_PATH: &str =
    "workspaces/runtime/packages/runtime-schema/src/schemas/persisted-monitor-record.schema.json";

/// Why work needs attention. The service owns classification and publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "StudioErrorKind")]
pub enum Kind {
    MonitorOutcomeLost,
    MonitorNotRecorded,
    InputUnconfirmed,
    InputUncertain,
    InputStuck,
    TurnDisconnected,
    WorkspaceFailed,
    HttpFailure,
    RecoveryFailed,
    StoredRecordInvalid,
    UnclassifiedFailure,
}

impl Kind {
    pub fn reason(self) -> &'static str {
        match self {
            Self::MonitorOutcomeLost => {
                "Studio lost the result of this command. The command may have run. Studio does not run it again."
            }
            Self::MonitorNotRecorded => {
                "Studio did not run this command, or did not record its end."
            }
            Self::InputUnconfirmed => {
                "The agent did not confirm this message. It may have read it. Studio does not send it again."
            }
            Self::InputUncertain => "Studio does not know if the provider received this message.",
            Self::InputStuck => "This message waits because of a Studio error.",
            Self::TurnDisconnected => "The provider connection ended during the turn.",
            Self::WorkspaceFailed => "Studio could not prepare the workspace of this agent.",
            Self::HttpFailure => {
                "The server could not complete this request. Inspect the diagnostic details before retrying."
            }
            Self::RecoveryFailed => {
                "Studio could not verify retained work during recovery. Its outcome remains uncertain; inspect the diagnostic details."
            }
            Self::StoredRecordInvalid => {
                "A retained record does not match its contract. Inspect its source identity and validation details."
            }
            Self::UnclassifiedFailure => {
                "This work failed without a recognized classification. Its outcome remains uncertain; inspect the source details."
            }
        }
    }

    pub fn certainty(self) -> EffectCertainty {
        match self {
            Self::MonitorOutcomeLost
            | Self::MonitorNotRecorded
            | Self::InputUnconfirmed
            | Self::InputUncertain
            | Self::InputStuck
            | Self::TurnDisconnected
            | Self::WorkspaceFailed
            | Self::HttpFailure
            | Self::RecoveryFailed
            | Self::StoredRecordInvalid
            | Self::UnclassifiedFailure => EffectCertainty::Unknown,
        }
    }

    pub fn target(self, agent_id: String, agent_name: String) -> Target {
        match self {
            Self::InputUncertain | Self::InputStuck | Self::TurnDisconnected => Target::Resume {
                agent_id,
                agent_name,
            },
            Self::MonitorOutcomeLost
            | Self::MonitorNotRecorded
            | Self::InputUnconfirmed
            | Self::WorkspaceFailed
            | Self::UnclassifiedFailure => Target::Check {
                agent_id,
                agent_name,
            },
            Self::HttpFailure | Self::RecoveryFailed | Self::StoredRecordInvalid => Target::Details,
        }
    }
}

/// An action always carries its real destination. Server incidents have no invented agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "action",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
#[schemars(rename = "StudioErrorTarget")]
pub enum Target {
    Resume {
        agent_id: String,
        agent_name: String,
    },
    Check {
        agent_id: String,
        agent_name: String,
    },
    Details,
}

/// Evidence about effects, distinct from provider delivery ("written" is not applied).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "StudioErrorEffectCertainty")]
pub enum EffectCertainty {
    ReadOnly,
    NotApplied,
    Unknown,
}

/// Safe HTTP classifications, shared with the existing operator log.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HttpErrorKind {
    HandlerPanic,
    StoreConfiguration,
    StoreData,
    StoreDatabase,
    StoreIo,
    StoreOwnership,
    StoreSchema,
    StoreConcurrency,
    ResponseBodyRead,
    CostsComputation,
    CostsState,
    Unclassified,
}

impl HttpErrorKind {
    pub fn log_class(self) -> &'static str {
        match self {
            Self::HandlerPanic => "handler_panic",
            Self::StoreConfiguration => "store_configuration",
            Self::StoreData => "store_data",
            Self::StoreDatabase => "store_database",
            Self::StoreIo => "store_io",
            Self::StoreOwnership => "store_ownership",
            Self::StoreSchema => "store_schema",
            Self::StoreConcurrency => "store_concurrency",
            Self::ResponseBodyRead => "response_body_read",
            Self::CostsComputation => "costs_computation",
            Self::CostsState => "costs_state",
            Self::Unclassified => "unclassified",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "source",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
#[schemars(rename = "StudioErrorContext")]
pub enum Context {
    Monitor {
        request_id: Option<String>,
        turn_id: Option<String>,
        epoch: Option<i64>,
    },
    Input {
        turn_id: Option<String>,
        epoch: i64,
    },
    Agent {
        turn_id: Option<String>,
        epoch: Option<i64>,
    },
    Http {
        route: String,
        request_id: Option<String>,
        status: u16,
        class: HttpErrorKind,
    },
    Recovery {
        account: String,
        phase: RecoveryPhase,
        agent_id: Option<String>,
    },
    /// The incident timestamp is the server observation time; the source's invalid values stay private in that row.
    Stored {
        object: String,
        id: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPhase {
    Scan,
    History,
}

/// Only facts extracted by typed adapters belong here; never arbitrary error strings or payloads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
#[schemars(rename = "StudioErrorCause")]
pub enum Cause {
    Monitor {
        status: MonitorStatus,
        operation: Option<MonitorOperationIdentity>,
        recovery: Option<MonitorRecoveryEvidence>,
    },
    SchemaViolation {
        pointer: String,
        keyword: String,
    },
    Unclassified {
        sha256: String,
    },
    Migration {
        id: String,
        target: MigrationTarget,
        phase: MigrationPhase,
        rollback: MigrationRollbackOutcome,
    },
    StoredRecord {
        object: String,
        id: String,
        pointer: Option<String>,
    },
    Sqlite {
        primary_code: i32,
        extended_code: i32,
    },
    Json {
        line: usize,
        column: usize,
        category: JsonCategory,
    },
    Io {
        os_code: Option<i32>,
    },
    SchemaObject {
        object: String,
    },
    Runtime {
        class: HttpErrorKind,
    },
}

/// Migration execution stays in runtime-store; these facts are shared operator and UI evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MigrationTarget {
    Canvas,
    Analytics,
    AccountRegistry,
    Supervisor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MigrationPhase {
    Validate,
    Apply,
    Commit,
    Rollback,
}

/// A failed step's rollback, never a claim about previously committed migration steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MigrationRollbackOutcome {
    NotRequired,
    RolledBack,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "StudioErrorJsonCategory")]
pub enum JsonCategory {
    Io,
    Syntax,
    Data,
    Eof,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(rename = "StudioErrorDiagnostics")]
pub struct Diagnostics {
    pub context: Context,
    pub certainty: EffectCertainty,
    /// Ordered from the owning boundary to the underlying safe machine facts.
    pub causes: Vec<Cause>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StudioError {
    pub id: String,
    pub kind: Kind,
    pub reason: String,
    #[serde(flatten)]
    pub target: Target,
    pub detail: String,
    pub subject: String,
    /// Unix seconds.
    #[schemars(range(min = 0))]
    pub at: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    pub diagnostics: Diagnostics,
}

/// Derive all referenced components from this owner, including flattened action variants.
pub(crate) fn schemas() -> Result<serde_json::Map<String, Value>, String> {
    // The API is OpenAPI 3.1 and the runtime validates JSON Schema: nullable
    // evidence must stay an actual null type, not OpenAPI 3.0's nullable annotation.
    let mut generator = schemars::generate::SchemaSettings::draft2020_12()
        .with(|settings| settings.definitions_path = "/components/schemas".into())
        .for_serialize()
        .into_generator();
    let schema = StudioError::json_schema(&mut generator).to_value();
    let mut schemas = generator.take_definitions(true);
    schemas.insert("StudioError".into(), schema);
    Ok(schemas)
}

pub(crate) fn check_schema_parity(openapi: &Value) -> Result<(), String> {
    for (name, schema) in schemas()? {
        if openapi.pointer(&format!("/components/schemas/{name}")) != Some(&schema) {
            return Err(format!(
                "{name} must match runtime-schema/src/studio_errors.rs"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn incident_actions_require_real_agent_destinations_and_keep_unknown_outcomes() {
        let mut row = json!({"id":"http:request", "kind":"http_failure", "reason":"Request failed", "action":"details", "detail":"store_data", "subject":"GET /api/monitor/log", "at":1,"diagnostics":{"context":{"source":"http","route":"GET /api/monitor/log","requestId":null,"status":503,"class":"store_data"},"certainty":"read_only","causes":[]}});
        let server: StudioError =
            serde_json::from_value(row.clone()).expect("server incident has no fake agent");
        assert!(matches!(server.target, Target::Details));
        row["action"] = json!("resume");
        assert!(serde_json::from_value::<StudioError>(row.clone()).is_err());
        row["agentId"] = json!("agent");
        row["agentName"] = json!("Agent");
        assert!(serde_json::from_value::<StudioError>(row).is_ok());
        assert_eq!(
            Kind::MonitorOutcomeLost.certainty(),
            EffectCertainty::Unknown
        );
    }

    #[test]
    fn incident_schema_and_all_components_derive_from_the_owner() {
        let openapi =
            serde_json::from_str(include_str!("../../runtime-http/openapi.json")).expect("OpenAPI");
        check_schema_parity(&openapi).expect("incident contract parity");
        let monitor: Value =
            serde_json::from_str(include_str!("schemas/persisted-monitor-record.schema.json"))
                .expect("monitor record schema");
        assert_eq!(
            monitor["properties"]["status"]["anyOf"][0],
            openapi["components"]["schemas"]["MonitorState"],
            "persisted monitor status consumes the same owner"
        );
        for code in schemas().expect("schemas")["MonitorRecoveryReason"]["enum"]
            .as_array()
            .expect("closed reason set")
        {
            let reason: MonitorRecoveryReason =
                serde_json::from_value(code.clone()).expect("owned reason code");
            assert_eq!(code.as_str(), Some(reason.code()));
        }
    }

    #[test]
    fn emitted_monitor_incidents_validate_through_the_real_sync_payload_owner() {
        let incident = StudioError {
            id: "monitor:watch".into(),
            kind: Kind::MonitorOutcomeLost,
            reason: Kind::MonitorOutcomeLost.reason().into(),
            target: Target::Check {
                agent_id: "agent".into(),
                agent_name: "Agent".into(),
            },
            detail: String::new(),
            subject: String::new(),
            at: 1.0,
            monitor_id: Some("watch".into()),
            event_id: None,
            diagnostics: Diagnostics {
                context: Context::Monitor {
                    request_id: Some("original".into()),
                    turn_id: None,
                    epoch: Some(0),
                },
                certainty: EffectCertainty::Unknown,
                causes: Vec::new(),
            },
        };
        let value = serde_json::to_value(incident).expect("typed incident");
        let openapi: Value =
            serde_json::from_str(include_str!("../../runtime-http/openapi.json")).expect("OpenAPI");
        let registry = crate::registry_for_document(&openapi).expect("registry");
        let validator = jsonschema::options()
            .with_registry(&registry)
            .build(
                &json!({"$ref":format!("{}#/components/schemas/StudioError",crate::OPENAPI_URI)}),
            )
            .expect("incident schema");
        let errors: Vec<_> = validator
            .iter_errors(&value)
            .map(|error| format!("{}: {}", error.instance_path(), error.kind().keyword()))
            .collect();
        assert!(errors.is_empty(), "{errors:?}: {value}");
        let payload = json!({"collection":"studioError","id":"monitor:watch","value":value});
        assert_eq!(
            crate::first_error("SyncEntityPayload", &payload).expect("entity schema"),
            None,
            "{payload}"
        );
    }
}
