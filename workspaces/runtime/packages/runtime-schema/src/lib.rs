//! Compiled validators for the schemas persisted by the Rust runtime.

use std::collections::BTreeMap;
use std::sync::OnceLock;

pub use jsonschema::{Draft, Registry as JsonSchemaRegistry, Validator};
use serde_json::{Value, json};

pub const OPENAPI_URI: &str = "https://codex-studio.invalid/openapi.json";
const ACCOUNT_REGISTRY_URI: &str = "https://codex-studio.invalid/account-registry.schema.json";
const ACCOUNT_TRANSFER_URI: &str =
    "https://codex-studio.invalid/persisted-account-transfer-record.schema.json";
const USAGE_LIMITS_URI: &str = "https://codex-studio.invalid/workspace-usage-limits.schema.json";
const TRANSCRIPT_ASSET_URI: &str =
    "https://codex-studio.invalid/runtime-transcript-asset.schema.json";
const TRANSCRIPT_RECORD_URI: &str =
    "https://codex-studio.invalid/persisted-transcript-record.schema.json";
const TRANSFER_AGENT_URI: &str =
    "https://codex-studio.invalid/persisted-transfer-agent-record.schema.json";
const EVENT_METADATA_URI: &str = "https://codex-studio.invalid/runtime-event-metadata.schema.json";
const AGENT_RECORD_URI: &str = "https://codex-studio.invalid/persisted-agent-record.schema.json";
const TASK_RECORD_URI: &str = "https://codex-studio.invalid/persisted-task-record.schema.json";
const PLAN_RECORD_URI: &str = "https://codex-studio.invalid/persisted-plan-record.schema.json";
const ANNOTATION_RECORD_URI: &str =
    "https://codex-studio.invalid/persisted-annotation-record.schema.json";
const REQUEST_RECORD_URI: &str =
    "https://codex-studio.invalid/persisted-request-record.schema.json";
const COMPLAINT_RECORD_URI: &str =
    "https://codex-studio.invalid/persisted-complaint-record.schema.json";
const CHECKPOINT_RECORD_URI: &str =
    "https://codex-studio.invalid/persisted-checkpoint-record.schema.json";
const MONITOR_RECORD_URI: &str =
    "https://codex-studio.invalid/persisted-monitor-record.schema.json";
const TERMINAL_RECORD_URI: &str =
    "https://codex-studio.invalid/persisted-terminal-record.schema.json";
const USAGE_RESUME_RECORD_URI: &str =
    "https://codex-studio.invalid/persisted-usage-resume-record.schema.json";
const WORK_ITEM_RECORD_URI: &str =
    "https://codex-studio.invalid/persisted-work-item-record.schema.json";
const RULE_RECORD_URI: &str = "https://codex-studio.invalid/persisted-rule-record.schema.json";
const GRAPH_AGENT_RECORD_URI: &str =
    "https://codex-studio.invalid/persisted-graph-agent-record.schema.json";
const PROFILE_RECORD_URI: &str =
    "https://codex-studio.invalid/persisted-profile-record.schema.json";

const OPENAPI: &[u8] = include_bytes!("../../runtime-http/openapi.json");
const ACCOUNT_REGISTRY: &[u8] = include_bytes!("schemas/account-registry.schema.json");
const USAGE_LIMITS: &[u8] = include_bytes!("schemas/workspace-usage-limits.schema.json");
const TRANSCRIPT_ASSET: &[u8] = include_bytes!("schemas/runtime-transcript-asset.schema.json");
const TRANSCRIPT_RECORD: &[u8] = include_bytes!("schemas/persisted-transcript-record.schema.json");
const EVENT_METADATA: &[u8] = include_bytes!("schemas/runtime-event-metadata.schema.json");
const AGENT_RECORD: &[u8] = include_bytes!("schemas/persisted-agent-record.schema.json");
const TASK_RECORD: &[u8] = include_bytes!("schemas/persisted-task-record.schema.json");
const PLAN_RECORD: &[u8] = include_bytes!("schemas/persisted-plan-record.schema.json");
const ANNOTATION_RECORD: &[u8] = include_bytes!("schemas/persisted-annotation-record.schema.json");
const REQUEST_RECORD: &[u8] = include_bytes!("schemas/persisted-request-record.schema.json");
const COMPLAINT_RECORD: &[u8] = include_bytes!("schemas/persisted-complaint-record.schema.json");
const CHECKPOINT_RECORD: &[u8] = include_bytes!("schemas/persisted-checkpoint-record.schema.json");
const MONITOR_RECORD: &[u8] = include_bytes!("schemas/persisted-monitor-record.schema.json");
const TERMINAL_RECORD: &[u8] = include_bytes!("schemas/persisted-terminal-record.schema.json");
const USAGE_RESUME_RECORD: &[u8] =
    include_bytes!("schemas/persisted-usage-resume-record.schema.json");
const WORK_ITEM_RECORD: &[u8] = include_bytes!("schemas/persisted-work-item-record.schema.json");
const RULE_RECORD: &[u8] = include_bytes!("schemas/persisted-rule-record.schema.json");
const GRAPH_AGENT_RECORD: &[u8] =
    include_bytes!("schemas/persisted-graph-agent-record.schema.json");
const PROFILE_RECORD: &[u8] = include_bytes!("schemas/persisted-profile-record.schema.json");

const PERSISTED_SCHEMAS: &[(&str, &[u8])] = &[
    (ACCOUNT_REGISTRY_URI, ACCOUNT_REGISTRY),
    (
        ACCOUNT_TRANSFER_URI,
        include_bytes!("schemas/persisted-account-transfer-record.schema.json"),
    ),
    (USAGE_LIMITS_URI, USAGE_LIMITS),
    (TRANSCRIPT_ASSET_URI, TRANSCRIPT_ASSET),
    (TRANSCRIPT_RECORD_URI, TRANSCRIPT_RECORD),
    (
        TRANSFER_AGENT_URI,
        include_bytes!("schemas/persisted-transfer-agent-record.schema.json"),
    ),
    (EVENT_METADATA_URI, EVENT_METADATA),
    (AGENT_RECORD_URI, AGENT_RECORD),
    (TASK_RECORD_URI, TASK_RECORD),
    (PLAN_RECORD_URI, PLAN_RECORD),
    (ANNOTATION_RECORD_URI, ANNOTATION_RECORD),
    (REQUEST_RECORD_URI, REQUEST_RECORD),
    (COMPLAINT_RECORD_URI, COMPLAINT_RECORD),
    (CHECKPOINT_RECORD_URI, CHECKPOINT_RECORD),
    (MONITOR_RECORD_URI, MONITOR_RECORD),
    (TERMINAL_RECORD_URI, TERMINAL_RECORD),
    (USAGE_RESUME_RECORD_URI, USAGE_RESUME_RECORD),
    (WORK_ITEM_RECORD_URI, WORK_ITEM_RECORD),
    (RULE_RECORD_URI, RULE_RECORD),
    (GRAPH_AGENT_RECORD_URI, GRAPH_AGENT_RECORD),
    (PROFILE_RECORD_URI, PROFILE_RECORD),
];

type SchemaResult<T> = Result<T, String>;

struct CompiledSchemas {
    validators: BTreeMap<String, Validator>,
}

static COMPILED_SCHEMAS: OnceLock<SchemaResult<CompiledSchemas>> = OnceLock::new();

/// Build the reference registry from the committed OpenAPI and persisted-state schemas.
pub fn registry_for_document(document: &Value) -> SchemaResult<JsonSchemaRegistry<'_>> {
    let mut registry = JsonSchemaRegistry::new()
        .draft(Draft::Draft202012)
        .add(
            OPENAPI_URI,
            jsonschema::Resource::from_contents(document.clone()),
        )
        .map_err(|error| error.to_string())?;
    for (uri, bytes) in PERSISTED_SCHEMAS {
        let schema: Value = serde_json::from_slice(bytes)
            .map_err(|error| format!("cannot parse persisted schema {uri}: {error}"))?;
        registry = registry
            .add(*uri, jsonschema::Resource::from_contents(schema))
            .map_err(|error| error.to_string())?;
    }
    registry.prepare().map_err(|error| error.to_string())
}

/// Ensure every schema used by persisted Rust records has compiled successfully.
pub fn initialize() -> SchemaResult<()> {
    compiled_schemas().map(|_| ())
}

/// Validate one persisted value against its named schema.
pub fn validate(schema: &str, value: &Value) -> SchemaResult<bool> {
    let validator = compiled_schemas()?
        .validators
        .get(schema)
        .ok_or_else(|| format!("unknown persisted schema {schema}"))?;
    Ok(validator.is_valid(value))
}

/// Return the authoritative persisted schema for a typed runtime table.
pub fn table_schema(table: &str) -> Option<&'static str> {
    Some(match table {
        "runtime_account_transfers" => "PersistedAccountTransferRecord",
        "runtime_agents" => "PersistedAgentRecord",
        "runtime_tasks" => "PersistedTaskRecord",
        "runtime_plans" => "PersistedPlanRecord",
        "runtime_annotations" => "PersistedAnnotationRecord",
        "runtime_requests" => "PersistedRequestRecord",
        "runtime_complaints" => "PersistedComplaintRecord",
        "runtime_checkpoints" => "PersistedCheckpointRecord",
        "runtime_monitors" => "PersistedMonitorRecord",
        "user_terminals" => "PersistedTerminalRecord",
        "runtime_usage_resumes" => "PersistedUsageResumeRecord",
        "runtime_work" => "PersistedWorkItemRecord",
        "runtime_rules" => "PersistedRuleRecord",
        "graph_agents" => "PersistedGraphAgentRecord",
        "runtime_assets" => "PersistedTranscriptAsset",
        "runtime_items" => "PersistedTranscriptRecord",
        "runtime_event_meta" => "RuntimeEventMetadataRecord",
        _ => return None,
    })
}

/// Return the first validation error's JSON pointer and keyword, if any.
pub fn first_error(schema: &str, value: &Value) -> SchemaResult<Option<(String, String)>> {
    let validator = compiled_schemas()?
        .validators
        .get(schema)
        .ok_or_else(|| format!("unknown persisted schema {schema}"))?;
    Ok(validator.iter_errors(value).next().map(|error| {
        (
            error.instance_path().to_string(),
            error.kind().keyword().to_owned(),
        )
    }))
}

fn compiled_schemas() -> SchemaResult<&'static CompiledSchemas> {
    match COMPILED_SCHEMAS.get_or_init(compile_schemas) {
        Ok(schemas) => Ok(schemas),
        Err(message) => Err(message.clone()),
    }
}

fn compile_schemas() -> SchemaResult<CompiledSchemas> {
    let openapi: Value = serde_json::from_slice(OPENAPI)
        .map_err(|error| format!("cannot parse OpenAPI schema document: {error}"))?;
    let account_registry: Value = serde_json::from_slice(ACCOUNT_REGISTRY)
        .map_err(|error| format!("cannot parse account-registry schema: {error}"))?;
    let usage_limits: Value = serde_json::from_slice(USAGE_LIMITS)
        .map_err(|error| format!("cannot parse workspace-usage-limits schema: {error}"))?;
    let registry = registry_for_document(&openapi)?;
    let mut validators = BTreeMap::new();

    for name in [
        "AgentRecordResponse",
        "AgentRequest",
        "AnnotationReceipt",
        "CheckpointSummary",
        "CheckpointsResponse",
        "ComplaintRecordResponse",
        "JsonObjectResponse",
        "MonitorRecordResponse",
        "PlanView",
        "ProfileRecord",
        "QueueItem",
        "QuestionEntry",
        "QuestionHistory",
        "RequestRecordResponse",
        "RuleRecordResponse",
        "TaskDetailResponse",
        "TerminalRecord",
        "TranscriptAsset",
        "UsageResumeRecordResponse",
        "WorkItem",
    ] {
        let reference = format!("{OPENAPI_URI}#/components/schemas/{name}");
        let validator = compile(&registry, json!({"$ref": reference}), name)?;
        validators.insert(name.to_owned(), validator);
    }
    for (name, uri) in [
        ("PersistedTranscriptAsset", TRANSCRIPT_ASSET_URI),
        ("PersistedTranscriptRecord", TRANSCRIPT_RECORD_URI),
        ("PersistedTransferAgentRecord", TRANSFER_AGENT_URI),
        ("RuntimeEventMetadataRecord", EVENT_METADATA_URI),
        ("PersistedAccountTransferRecord", ACCOUNT_TRANSFER_URI),
        ("PersistedAgentRecord", AGENT_RECORD_URI),
        ("PersistedTaskRecord", TASK_RECORD_URI),
        ("PersistedPlanRecord", PLAN_RECORD_URI),
        ("PersistedAnnotationRecord", ANNOTATION_RECORD_URI),
        ("PersistedRequestRecord", REQUEST_RECORD_URI),
        ("PersistedComplaintRecord", COMPLAINT_RECORD_URI),
        ("PersistedCheckpointRecord", CHECKPOINT_RECORD_URI),
        ("PersistedMonitorRecord", MONITOR_RECORD_URI),
        ("PersistedTerminalRecord", TERMINAL_RECORD_URI),
        ("PersistedUsageResumeRecord", USAGE_RESUME_RECORD_URI),
        ("PersistedWorkItemRecord", WORK_ITEM_RECORD_URI),
        ("PersistedRuleRecord", RULE_RECORD_URI),
        ("PersistedGraphAgentRecord", GRAPH_AGENT_RECORD_URI),
        ("PersistedProfileRecord", PROFILE_RECORD_URI),
        (
            "account-registry/AccountRegistryRecord",
            ACCOUNT_REGISTRY_URI,
        ),
        (
            "workspace-usage-limits/UsageLimitsWorkspaceEnvelope",
            USAGE_LIMITS_URI,
        ),
    ] {
        let validator = compile(&registry, json!({"$ref": uri}), name)?;
        validators.insert(name.to_owned(), validator);
    }

    compile_definitions(
        &registry,
        ACCOUNT_REGISTRY_URI,
        "account-registry",
        &account_registry,
        &mut validators,
    )?;
    compile_definitions(
        &registry,
        USAGE_LIMITS_URI,
        "workspace-usage-limits",
        &usage_limits,
        &mut validators,
    )?;
    Ok(CompiledSchemas { validators })
}

fn compile_definitions(
    registry: &JsonSchemaRegistry<'_>,
    document_uri: &str,
    namespace: &str,
    document: &Value,
    validators: &mut BTreeMap<String, Validator>,
) -> SchemaResult<()> {
    let definitions = document
        .get("$defs")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{namespace} schema has no $defs object"))?;
    for name in definitions.keys() {
        let reference = format!("{document_uri}#/$defs/{}", escape_pointer(name));
        let validator = compile(registry, json!({"$ref": reference}), name)?;
        validators.insert(format!("{namespace}/{name}"), validator);
    }
    Ok(())
}

fn compile(
    registry: &JsonSchemaRegistry<'_>,
    schema: Value,
    name: &str,
) -> SchemaResult<Validator> {
    jsonschema::options()
        .with_registry(registry)
        .with_pattern_options(jsonschema::PatternOptions::regex())
        .build(&schema)
        .map_err(|error| format!("cannot compile persisted schema {name}: {error}"))
}

fn escape_pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{initialize, registry_for_document, table_schema, validate};

    #[test]
    fn typed_table_selects_one_authoritative_storage_schema() {
        for (table, schema) in [
            ("runtime_agents", "PersistedAgentRecord"),
            ("runtime_tasks", "PersistedTaskRecord"),
            ("runtime_plans", "PersistedPlanRecord"),
            ("runtime_annotations", "PersistedAnnotationRecord"),
            ("runtime_requests", "PersistedRequestRecord"),
            ("runtime_complaints", "PersistedComplaintRecord"),
            ("runtime_checkpoints", "PersistedCheckpointRecord"),
            ("runtime_monitors", "PersistedMonitorRecord"),
            ("user_terminals", "PersistedTerminalRecord"),
            ("runtime_usage_resumes", "PersistedUsageResumeRecord"),
            ("runtime_work", "PersistedWorkItemRecord"),
            ("runtime_rules", "PersistedRuleRecord"),
            ("graph_agents", "PersistedGraphAgentRecord"),
            ("runtime_assets", "PersistedTranscriptAsset"),
            ("runtime_items", "PersistedTranscriptRecord"),
            ("runtime_event_meta", "RuntimeEventMetadataRecord"),
        ] {
            assert_eq!(table_schema(table), Some(schema), "{table}");
        }
        assert_eq!(table_schema("untyped_table"), None);
    }

    #[test]
    fn compiled_record_schemas_accept_known_records_and_reject_invalid_values() {
        initialize().expect("persisted schemas compile");
        assert!(
            validate(
                "AgentRecordResponse",
                &json!({"id":"agent-1","rootId":"agent-1","epoch":0,"status":"idle"})
            )
            .expect("agent schema exists")
        );
        assert!(
            validate(
                "PersistedAgentRecord",
                &json!({"id":"agent-1","rootId":"agent-1","epoch":0,"status":"idle"})
            )
            .expect("persisted agent schema exists")
        );
        assert!(
            validate(
                "PersistedAgentRecord",
                &json!({
                    "id":"agent-1",
                    "rootId":"agent-1",
                    "epoch":0,
                    "status":"idle",
                    "sandbox":null,
                    "approvalPolicy":null
                })
            )
            .expect("persisted agent schema exists")
        );
        assert!(
            validate(
                "PersistedAgentRecord",
                &json!({
                    "id":"agent-1",
                    "rootId":"agent-1",
                    "epoch":0,
                    "status":"idle",
                    "sandbox":{},
                    "approvalPolicy":"never"
                })
            )
            .expect("persisted agent schema exists")
        );
        assert!(
            !validate(
                "PersistedAgentRecord",
                &json!({"id":"agent-1","rootId":null,"epoch":0,"status":"idle"})
            )
            .expect("persisted agent schema exists")
        );
        assert!(
            !validate(
                "PersistedAgentRecord",
                &json!({
                    "id":"agent-1",
                    "rootId":"agent-1",
                    "epoch":0,
                    "status":"idle",
                    "sandbox":"workspace-write"
                })
            )
            .expect("persisted agent schema exists")
        );
        assert!(validate(
            "WorkItem",
            &json!({"id":"task-1","rootId":"agent-1","title":"T","status":"ready","created":1,"updated":1,"version":1})
        )
        .expect("work item schema exists"));
        assert!(
            validate(
                "TaskDetailResponse",
                &json!({"id":"t","agent":"a","kind":"command","status":"completed","created":1})
            )
            .expect("runtime task schema exists")
        );
        assert!(validate(
            "PersistedTaskRecord",
            &json!({"id":"t","agent":"a","kind":"command","status":"completed","reattachRecovery":{"attempt":2},"outputPreviewOnly":true})
        )
        .expect("persisted task schema accepts Python state extensions"));
        assert!(
            validate(
                "PersistedRequestRecord",
                &json!({"id":"q","connectionId":null,"agent":null,"status":"pending"})
            )
            .expect("persisted request schema accepts nullable connection identity")
        );
        assert!(
            !validate(
                "PersistedRequestRecord",
                &json!({"id":"q","connectionId":7})
            )
            .expect("persisted request schema exists")
        );
        assert!(
            validate(
                "PersistedTranscriptRecord",
                &json!({"id":"i","inputs":[{"id":"a"}]})
            )
            .expect("legacy transcript item can be hydrated before response validation")
        );
        assert!(
            validate(
                "PersistedTranscriptRecord",
                &json!({"id":"i","text":null,"inputs":[{"text":null}]})
            )
            .expect("nullable transcript text remains readable")
        );
        assert!(validate(
            "PersistedTranscriptAsset",
            &json!({"id":"asset","agent":"a","name":"n","path":"/tmp/n","mime":"text/plain","image":false,"size":1,"hash":"sha256:x","created":1.0,"future":true})
        )
        .expect("persisted asset schema exists"));
        assert!(
            validate(
                "RuntimeEventMetadataRecord",
                &json!({"timing":{"enqueuedAt":123},"queueMutationRevision":4})
            )
            .expect("event metadata schema exists")
        );
        assert!(
            !validate("RuntimeEventMetadataRecord", &json!({"assets":{}}))
                .expect("event metadata schema exists")
        );
        assert!(
            !validate("RuntimeEventMetadataRecord", &json!({"timing":true}))
                .expect("event metadata schema exists")
        );
        assert!(!validate("WorkItem", &json!([])).expect("work item schema exists"));
        assert!(
            validate(
                "account-registry/AccountRecord",
                &json!({
                    "id":"default",
                    "home":"/tmp/default",
                    "label":"Default",
                    "source":"test",
                    "status":"ready"
                })
            )
            .expect("account state schema exists")
        );
        assert!(
            validate(
                "workspace-usage-limits/UsageLimitsResponse",
                &json!({"accountKey":"default"})
            )
            .expect("usage limits schema exists")
        );
    }

    #[test]
    fn request_registry_includes_shared_state_documents() {
        let document = json!({"components":{"schemas":{}}});
        registry_for_document(&document).expect("shared registry prepares");
    }

    #[test]
    fn unknown_schema_names_are_configuration_errors() {
        assert!(validate("MissingRecordSchema", &json!({})).is_err());
    }
}
