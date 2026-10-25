//! Compiled persisted-state validators and the Rust owners of the shared
//! runtime contracts (`contracts` renders their copies in other languages).

pub mod agent_limits;
pub mod agent_progress;
pub mod api_schema;
pub mod contracts;
pub mod delivery;
pub mod desktop;
pub mod federation_limits;
pub mod idle_compaction;
pub mod multi_server_protocol;
pub mod provider_input;
pub mod shared_values;
pub mod task_results;

pub use delivery::DeliveryMoment;

use std::collections::BTreeMap;
use std::sync::OnceLock;

pub use jsonschema::{
    Draft, Registry as JsonSchemaRegistry, ValidationError, Validator, error::ValidationErrorKind,
};
use serde_json::{Value, json};

pub mod sync_protocol;
pub use sync_protocol::{SYNC_ENTITY_SCOPE, SYNC_PROTOCOL_VERSION, SYNC_PROTOCOL_VERSION_TEXT};
mod token_rate_limits;
pub use token_rate_limits::{TOKEN_RATE_IDENTIFIER_MAX_CODEPOINTS, TOKEN_RATE_MAX_ENTRIES};

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
const RECOVERY_AGENT_URI: &str = "https://codex-studio.invalid/recovery-agent-record.schema.json";
const RECOVERY_RECEIPT_URI: &str = "https://codex-studio.invalid/recovery-tool-receipt.schema.json";
const RECOVERY_TASK_URI: &str = "https://codex-studio.invalid/recovery-task-record.schema.json";
const REQUEST_RECORD_URI: &str =
    "https://codex-studio.invalid/persisted-request-record.schema.json";
const COMPLAINT_RECORD_URI: &str =
    "https://codex-studio.invalid/persisted-complaint-record.schema.json";
const CHECKPOINT_RECORD_URI: &str =
    "https://codex-studio.invalid/persisted-checkpoint-record.schema.json";
const MONITOR_RECORD_URI: &str =
    "https://codex-studio.invalid/persisted-monitor-record.schema.json";
const MONITOR_RESULT_URI: &str =
    "https://codex-studio.invalid/persisted-monitor-result.schema.json";
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
const EXECUTION_RUN_RECORD_URI: &str =
    "https://codex-studio.invalid/persisted-execution-run-record.schema.json";

const TOOL_REQUEST_RECORD_URI: &str =
    "https://codex-studio.invalid/persisted-tool-request-record.schema.json";
const TOOL_REQUEST_RECORD: &[u8] =
    include_bytes!("schemas/persisted-tool-request-record.schema.json");

const TOOL_RESULT_URI: &str = "https://codex-studio.invalid/persisted-tool-result.schema.json";
const TOOL_RESULT: &[u8] = include_bytes!("schemas/persisted-tool-result.schema.json");

const EXECUTION_RECORD_URI: &str =
    "https://codex-studio.invalid/persisted-execution-record.schema.json";
const EXECUTION_RECORD: &[u8] = include_bytes!("schemas/persisted-execution-record.schema.json");

const ROOM_RECORD_URI: &str = "https://codex-studio.invalid/persisted-room-record.schema.json";
const ROOM_RECORD: &[u8] = include_bytes!("schemas/persisted-room-record.schema.json");

const PROJECT_RECORD_URI: &str =
    "https://codex-studio.invalid/persisted-project-record.schema.json";
const PROJECT_RECORD: &[u8] = include_bytes!("schemas/persisted-project-record.schema.json");

const FEDERATION_ROOM_RECORD_URI: &str =
    "https://codex-studio.invalid/persisted-federation-room-record.schema.json";
const FEDERATION_ROOM_RECORD: &[u8] =
    include_bytes!("schemas/persisted-federation-room-record.schema.json");

const CHAT_MESSAGE_URI: &str = "https://codex-studio.invalid/persisted-chat-message.schema.json";
const CHAT_MESSAGE: &[u8] = include_bytes!("schemas/persisted-chat-message.schema.json");

const SYNC_ENTITY_URI: &str = "https://codex-studio.invalid/persisted-sync-entity.schema.json";
const SYNC_ENTITY: &[u8] = include_bytes!("schemas/persisted-sync-entity.schema.json");

const ACCESS_RECORD_URI: &str = "https://codex-studio.invalid/persisted-access-record.schema.json";
const ACCESS_RECORD: &[u8] = include_bytes!("schemas/persisted-access-record.schema.json");

const OPERATION_RESULT_URI: &str =
    "https://codex-studio.invalid/persisted-operation-result.schema.json";
const OPERATION_RESULT: &[u8] = include_bytes!("schemas/persisted-operation-result.schema.json");

const PROJECT_LOCATION_OUTBOX_URI: &str =
    "https://codex-studio.invalid/persisted-project-location-outbox.schema.json";
const PROJECT_LOCATION_OUTBOX: &[u8] =
    include_bytes!("schemas/persisted-project-location-outbox.schema.json");
const MESSAGE_EVENT_URI: &str = "https://codex-studio.invalid/persisted-message-event.schema.json";
const MESSAGE_EVENT: &[u8] = include_bytes!("schemas/persisted-message-event.schema.json");
const TOOL_ALIAS_URI: &str = "https://codex-studio.invalid/persisted-tool-alias.schema.json";
const TOOL_ALIAS: &[u8] = include_bytes!("schemas/persisted-tool-alias.schema.json");
const CONTINUATION_URI: &str =
    "https://codex-studio.invalid/persisted-continuation-snapshot.schema.json";
const CONTINUATION: &[u8] = include_bytes!("schemas/persisted-continuation-snapshot.schema.json");
const ACCESS_RECEIPT_URI: &str =
    "https://codex-studio.invalid/persisted-access-receipt.schema.json";
const ACCESS_RECEIPT: &[u8] = include_bytes!("schemas/persisted-access-receipt.schema.json");

const ACCESS_CLIENT_URI: &str = "https://codex-studio.invalid/persisted-access-client.schema.json";
const ACCESS_CLIENT: &[u8] = include_bytes!("schemas/persisted-access-client.schema.json");

const ACCESS_INVITE_URI: &str = "https://codex-studio.invalid/persisted-access-invite.schema.json";
const ACCESS_INVITE: &[u8] = include_bytes!("schemas/persisted-access-invite.schema.json");

const SERVER_DELIVERY_URI: &str =
    "https://codex-studio.invalid/persisted-server-delivery.schema.json";
const SERVER_DELIVERY: &[u8] = include_bytes!("schemas/persisted-server-delivery.schema.json");

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
const RECOVERY_AGENT: &[u8] = include_bytes!("schemas/recovery-agent-record.schema.json");
const RECOVERY_RECEIPT: &[u8] = include_bytes!("schemas/recovery-tool-receipt.schema.json");
const RECOVERY_TASK: &[u8] = include_bytes!("schemas/recovery-task-record.schema.json");
const REQUEST_RECORD: &[u8] = include_bytes!("schemas/persisted-request-record.schema.json");
const COMPLAINT_RECORD: &[u8] = include_bytes!("schemas/persisted-complaint-record.schema.json");
const CHECKPOINT_RECORD: &[u8] = include_bytes!("schemas/persisted-checkpoint-record.schema.json");
const MONITOR_RECORD: &[u8] = include_bytes!("schemas/persisted-monitor-record.schema.json");
const MONITOR_RESULT: &[u8] = include_bytes!("schemas/persisted-monitor-result.schema.json");
const TERMINAL_RECORD: &[u8] = include_bytes!("schemas/persisted-terminal-record.schema.json");
const USAGE_RESUME_RECORD: &[u8] =
    include_bytes!("schemas/persisted-usage-resume-record.schema.json");
const WORK_ITEM_RECORD: &[u8] = include_bytes!("schemas/persisted-work-item-record.schema.json");
const RULE_RECORD: &[u8] = include_bytes!("schemas/persisted-rule-record.schema.json");
const GRAPH_AGENT_RECORD: &[u8] =
    include_bytes!("schemas/persisted-graph-agent-record.schema.json");
const PROFILE_RECORD: &[u8] = include_bytes!("schemas/persisted-profile-record.schema.json");
const EXECUTION_RUN_RECORD: &[u8] =
    include_bytes!("schemas/persisted-execution-run-record.schema.json");

const WORKSPACE_OPERATION_URI: &str =
    "https://codex-studio.invalid/persisted-workspace-operation-record.schema.json";
const WORKSPACE_OPERATION: &[u8] =
    include_bytes!("schemas/persisted-workspace-operation-record.schema.json");

const WORKSPACE_CHECKPOINT_AGENT_URI: &str =
    "https://codex-studio.invalid/workspace-checkpoint-agent-record.schema.json";
const WORKSPACE_CHECKPOINT_AGENT: &[u8] =
    include_bytes!("schemas/workspace-checkpoint-agent-record.schema.json");

const WORKSPACE_CHECKPOINT_HISTORY_URI: &str =
    "https://codex-studio.invalid/workspace-checkpoint-history-record.schema.json";
const WORKSPACE_CHECKPOINT_HISTORY: &[u8] =
    include_bytes!("schemas/workspace-checkpoint-history-record.schema.json");
const RELOCATION_URI: &str = "https://codex-studio.invalid/persisted-relocation.schema.json";
const RELOCATION: &[u8] = include_bytes!("schemas/persisted-relocation.schema.json");
const FEDERATION_SETTINGS_URI: &str =
    "https://codex-studio.invalid/persisted-federation-settings.schema.json";
const FEDERATION_SETTINGS: &[u8] =
    include_bytes!("schemas/persisted-federation-settings.schema.json");
const FEDERATION_PEER_URI: &str =
    "https://codex-studio.invalid/persisted-federation-peer.schema.json";
const FEDERATION_PEER: &[u8] = include_bytes!("schemas/persisted-federation-peer.schema.json");
const FEDERATION_ROOM_URI: &str =
    "https://codex-studio.invalid/persisted-federation-room.schema.json";
const FEDERATION_ROOM: &[u8] = include_bytes!("schemas/persisted-federation-room.schema.json");
const FEDERATION_OUTBOX_URI: &str =
    "https://codex-studio.invalid/persisted-federation-outbox.schema.json";
const FEDERATION_OUTBOX: &[u8] = include_bytes!("schemas/persisted-federation-outbox.schema.json");
const FEDERATION_INBOX_URI: &str =
    "https://codex-studio.invalid/persisted-federation-inbox.schema.json";
const FEDERATION_INBOX: &[u8] = include_bytes!("schemas/persisted-federation-inbox.schema.json");

const FEDERATION_CHAT_PAYLOAD_URI: &str =
    "https://codex-studio.invalid/federation-chat-payload.schema.json";
const FEDERATION_CHAT_PAYLOAD: &[u8] =
    include_bytes!("schemas/federation-chat-payload.schema.json");
const FEDERATION_ROOM_PAYLOAD_URI: &str =
    "https://codex-studio.invalid/federation-room-payload.schema.json";
const FEDERATION_ROOM_PAYLOAD: &[u8] =
    include_bytes!("schemas/federation-room-payload.schema.json");

const CLAUDE_ATTEMPT_URI: &str =
    "https://codex-studio.invalid/persisted-claude-attempt.schema.json";
const CLAUDE_CONTROL_URI: &str =
    "https://codex-studio.invalid/persisted-claude-control.schema.json";
const CLAUDE_LOGIN_URI: &str = "https://codex-studio.invalid/persisted-claude-login.schema.json";

const PERSISTED_SCHEMAS: &[(&str, &[u8])] = &[
    (
        "https://codex-studio.invalid/budget-capture.schema.json",
        include_bytes!("schemas/budget-capture.schema.json"),
    ),
    (
        "https://codex-studio.invalid/budget-history.schema.json",
        include_bytes!("schemas/budget-history.schema.json"),
    ),
    (
        "https://codex-studio.invalid/persisted-budget-agent.schema.json",
        include_bytes!("schemas/persisted-budget-agent.schema.json"),
    ),
    (
        "https://codex-studio.invalid/persisted-budget-state.schema.json",
        include_bytes!("schemas/persisted-budget-state.schema.json"),
    ),
    (
        "https://codex-studio.invalid/persisted-budget-usage.schema.json",
        include_bytes!("schemas/persisted-budget-usage.schema.json"),
    ),
    (ACCESS_CLIENT_URI, ACCESS_CLIENT),
    (ACCESS_INVITE_URI, ACCESS_INVITE),
    (ACCESS_RECEIPT_URI, ACCESS_RECEIPT),
    (ACCESS_RECORD_URI, ACCESS_RECORD),
    (ACCOUNT_REGISTRY_URI, ACCOUNT_REGISTRY),
    (
        ACCOUNT_TRANSFER_URI,
        include_bytes!("schemas/persisted-account-transfer-record.schema.json"),
    ),
    (AGENT_RECORD_URI, AGENT_RECORD),
    (ANNOTATION_RECORD_URI, ANNOTATION_RECORD),
    (CHAT_MESSAGE_URI, CHAT_MESSAGE),
    (CHECKPOINT_RECORD_URI, CHECKPOINT_RECORD),
    (
        CLAUDE_ATTEMPT_URI,
        include_bytes!("schemas/persisted-claude-attempt.schema.json"),
    ),
    (
        CLAUDE_CONTROL_URI,
        include_bytes!("schemas/persisted-claude-control.schema.json"),
    ),
    (
        CLAUDE_LOGIN_URI,
        include_bytes!("schemas/persisted-claude-login.schema.json"),
    ),
    (COMPLAINT_RECORD_URI, COMPLAINT_RECORD),
    (CONTINUATION_URI, CONTINUATION),
    (EVENT_METADATA_URI, EVENT_METADATA),
    (EXECUTION_RECORD_URI, EXECUTION_RECORD),
    (EXECUTION_RUN_RECORD_URI, EXECUTION_RUN_RECORD),
    (FEDERATION_CHAT_PAYLOAD_URI, FEDERATION_CHAT_PAYLOAD),
    (FEDERATION_INBOX_URI, FEDERATION_INBOX),
    (FEDERATION_OUTBOX_URI, FEDERATION_OUTBOX),
    (FEDERATION_PEER_URI, FEDERATION_PEER),
    (FEDERATION_ROOM_PAYLOAD_URI, FEDERATION_ROOM_PAYLOAD),
    (FEDERATION_ROOM_RECORD_URI, FEDERATION_ROOM_RECORD),
    (FEDERATION_ROOM_URI, FEDERATION_ROOM),
    (FEDERATION_SETTINGS_URI, FEDERATION_SETTINGS),
    (GRAPH_AGENT_RECORD_URI, GRAPH_AGENT_RECORD),
    (MESSAGE_EVENT_URI, MESSAGE_EVENT),
    (MONITOR_RECORD_URI, MONITOR_RECORD),
    (MONITOR_RESULT_URI, MONITOR_RESULT),
    (OPERATION_RESULT_URI, OPERATION_RESULT),
    (PLAN_RECORD_URI, PLAN_RECORD),
    (PROFILE_RECORD_URI, PROFILE_RECORD),
    (PROJECT_LOCATION_OUTBOX_URI, PROJECT_LOCATION_OUTBOX),
    (PROJECT_RECORD_URI, PROJECT_RECORD),
    (RECOVERY_AGENT_URI, RECOVERY_AGENT),
    (RECOVERY_RECEIPT_URI, RECOVERY_RECEIPT),
    (RECOVERY_TASK_URI, RECOVERY_TASK),
    (RELOCATION_URI, RELOCATION),
    (REQUEST_RECORD_URI, REQUEST_RECORD),
    (ROOM_RECORD_URI, ROOM_RECORD),
    (RULE_RECORD_URI, RULE_RECORD),
    (SERVER_DELIVERY_URI, SERVER_DELIVERY),
    (SYNC_ENTITY_URI, SYNC_ENTITY),
    (TASK_RECORD_URI, TASK_RECORD),
    (TERMINAL_RECORD_URI, TERMINAL_RECORD),
    (TOOL_ALIAS_URI, TOOL_ALIAS),
    (TOOL_REQUEST_RECORD_URI, TOOL_REQUEST_RECORD),
    (TOOL_RESULT_URI, TOOL_RESULT),
    (TRANSCRIPT_ASSET_URI, TRANSCRIPT_ASSET),
    (TRANSCRIPT_RECORD_URI, TRANSCRIPT_RECORD),
    (
        TRANSFER_AGENT_URI,
        include_bytes!("schemas/persisted-transfer-agent-record.schema.json"),
    ),
    (USAGE_LIMITS_URI, USAGE_LIMITS),
    (USAGE_RESUME_RECORD_URI, USAGE_RESUME_RECORD),
    (WORKSPACE_CHECKPOINT_AGENT_URI, WORKSPACE_CHECKPOINT_AGENT),
    (
        WORKSPACE_CHECKPOINT_HISTORY_URI,
        WORKSPACE_CHECKPOINT_HISTORY,
    ),
    (WORKSPACE_OPERATION_URI, WORKSPACE_OPERATION),
    (WORK_ITEM_RECORD_URI, WORK_ITEM_RECORD),
    (
        "https://codex-studio.invalid/analytics-agent.schema.json",
        include_bytes!("schemas/analytics-agent.schema.json"),
    ),
    (
        "https://codex-studio.invalid/analytics-context.schema.json",
        include_bytes!("schemas/analytics-context.schema.json"),
    ),
    (
        "https://codex-studio.invalid/analytics-history.schema.json",
        include_bytes!("schemas/analytics-history.schema.json"),
    ),
    (
        "https://codex-studio.invalid/analytics-history-importer.schema.json",
        include_bytes!("schemas/analytics-history-importer.schema.json"),
    ),
    (
        "https://codex-studio.invalid/analytics-item.schema.json",
        include_bytes!("schemas/analytics-item.schema.json"),
    ),
    (
        "https://codex-studio.invalid/analytics-notice.schema.json",
        include_bytes!("schemas/analytics-notice.schema.json"),
    ),
    (
        "https://codex-studio.invalid/analytics-turn.schema.json",
        include_bytes!("schemas/analytics-turn.schema.json"),
    ),
    (
        "https://codex-studio.invalid/analytics-usage.schema.json",
        include_bytes!("schemas/analytics-usage.schema.json"),
    ),
];

type SchemaResult<T> = Result<T, String>;

struct CompiledSchemas {
    registry: JsonSchemaRegistry<'static>,
    validators: BTreeMap<String, NamedValidator>,
    sync_entity_branches: BTreeMap<String, String>,
}

impl CompiledSchemas {
    fn initialize(&self) -> SchemaResult<()> {
        for validator in self.validators.values() {
            validator.get(&self.registry)?;
        }
        Ok(())
    }
}

struct NamedValidator {
    reference: String,
    name: String,
    compiled: OnceLock<SchemaResult<Validator>>,
}

impl NamedValidator {
    fn new(reference: String, name: &str) -> Self {
        Self {
            reference,
            name: name.to_owned(),
            compiled: OnceLock::new(),
        }
    }

    fn get(&self, registry: &JsonSchemaRegistry<'_>) -> SchemaResult<&Validator> {
        self.compiled
            .get_or_init(|| compile(registry, json!({"$ref": self.reference}), &self.name))
            .as_ref()
            .map_err(Clone::clone)
    }
}

static COMPILED_SCHEMAS: OnceLock<SchemaResult<CompiledSchemas>> = OnceLock::new();

/// Build the reference registry from the committed OpenAPI and persisted-state schemas.
pub fn registry_for_document(document: &Value) -> SchemaResult<JsonSchemaRegistry<'static>> {
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

/// Ensure every named runtime schema has compiled successfully.
pub fn initialize() -> SchemaResult<()> {
    compiled_schemas()?.initialize()
}

/// Compile only the requested named validator on first use; clones share its root graph.
/// HTTP keeps its own schema allowlist while reusing this owner for compilation.
pub fn compiled_validator(schema: &str) -> SchemaResult<&'static Validator> {
    let schemas = compiled_schemas()?;
    schemas
        .validators
        .get(schema)
        .ok_or_else(|| format!("unknown persisted schema {schema}"))?
        .get(&schemas.registry)
}

/// Validate one persisted value against its named schema.
pub fn validate(schema: &str, value: &Value) -> SchemaResult<bool> {
    let validator = compiled_validator(schema)?;
    Ok(validator.is_valid(value))
}

/// Return the authoritative persisted schema for a typed runtime table.
pub fn table_schema(table: &str) -> Option<&'static str> {
    Some(match table {
        "graph_agents" => "PersistedGraphAgentRecord",
        "runtime_access_clients" | "runtime_access_discovered" => "PersistedAccessClient",
        "runtime_access_invites" => "PersistedAccessInvite",
        "runtime_access_receipts" => "PersistedAccessReceipt",
        "runtime_account_transfers" => "PersistedAccountTransferRecord",
        "runtime_agent_moves" => "relocation/MoveRecord",
        "runtime_agents" => "PersistedAgentRecord",
        "runtime_annotations" => "PersistedAnnotationRecord",
        "runtime_assets" => "PersistedTranscriptAsset",
        "runtime_budget" => "PersistedBudgetState",
        "runtime_budget_usage" => "PersistedBudgetUsage",
        "runtime_capacity_retries" => "continuation/CapacityContinuation",
        "runtime_chat_messages" => "PersistedChatMessage",
        "runtime_checkpoints" => "PersistedCheckpointRecord",
        "runtime_complaints" => "PersistedComplaintRecord",
        "runtime_event_meta" => "RuntimeEventMetadataRecord",
        "runtime_events" => "PersistedMessageEvent",
        "runtime_execution_attempts" => "PersistedClaudeAttemptRecord",
        "runtime_execution_effects" => "PersistedExecutionRecord",
        "runtime_execution_runs" => "PersistedExecutionRunRecord",
        "runtime_federation_inbox" => "PersistedFederationInbox",
        "runtime_federation_outbox" => "PersistedFederationOutbox",
        "runtime_federation_peers" => "PersistedFederationPeer",
        "runtime_federation_rooms" => "PersistedFederationRoom",
        "runtime_federation_settings" => "PersistedFederationSettings",
        "runtime_items" => "PersistedTranscriptRecord",
        "runtime_monitors" => "PersistedMonitorRecord",
        "runtime_operation_receipts" => "PersistedOperationResult",
        "runtime_plans" => "PersistedPlanRecord",
        "runtime_projects" => "PersistedProjectRecord",
        "runtime_requests" => "PersistedRequestRecord",
        "runtime_rooms" => "PersistedRoomRecord",
        "runtime_rules" => "PersistedRuleRecord",
        "runtime_server_inbox" | "runtime_server_outbox" => "PersistedServerDelivery",
        "runtime_tasks" => "PersistedTaskRecord",
        "runtime_tool_request_aliases" => "PersistedToolAlias",
        "runtime_tool_requests" => "PersistedToolRequestRecord",
        "runtime_tool_results" => "PersistedToolResult",
        "runtime_usage_resumes" => "PersistedUsageResumeRecord",
        "runtime_work" => "PersistedWorkItemRecord",
        "runtime_workspace_operations" => "PersistedWorkspaceOperationRecord",
        "sync_entities" => "PersistedSyncEntity",
        "user_terminals" => "PersistedTerminalRecord",
        _ => return None,
    })
}

/// Return validation errors as JSON pointers and keywords.
pub fn validation_errors(schema: &str, value: &Value) -> SchemaResult<Vec<(String, String)>> {
    let validator = compiled_validator(schema)?;
    Ok(validator
        .iter_errors(value)
        .map(|error| {
            (
                error.instance_path().to_string(),
                error.kind().keyword().to_owned(),
            )
        })
        .collect())
}

/// Return the first validation error's JSON pointer and keyword, if any.
pub fn first_error(schema: &str, value: &Value) -> SchemaResult<Option<(String, String)>> {
    Ok(validation_errors(schema, value)?.into_iter().next())
}

/// Remove only extra fields rejected by the selected authoritative schema branch.
///
/// This preserves historical forward-compatible stored fields while refusing to
/// strip a required/type/union error into a valid value.
pub fn strip_stored_extra_fields(schema: &str, value: &mut Value) -> SchemaResult<()> {
    let validator = compiled_validator(schema)?;
    strip_extra_fields_with_validator(validator, value);
    Ok(())
}

/// Apply the stored-value extra-field policy to a validator owned by a consumer.
pub fn strip_extra_fields_with_validator(validator: &Validator, value: &mut Value) {
    let paths = validator
        .iter_errors(value)
        .flat_map(|error| extra_field_errors(&error))
        .collect::<Vec<_>>();
    for (pointer, keys) in paths {
        let Some(target) = value.pointer_mut(&pointer).and_then(Value::as_object_mut) else {
            continue;
        };
        for key in keys {
            drop(target.remove(&key));
        }
    }
}

/// Normalize and validate one stored sync entity using the generated union contract.
///
/// Live records have their old extra fields stripped only after the collection branch
/// is selected from the generated discriminator mapping. Tombstones are envelope-only
/// and intentionally skip the live entity DTO.
pub fn normalize_sync_entity_payload(
    collection: &str,
    id: &str,
    deleted: bool,
    payload: &mut Value,
) -> SchemaResult<Option<(String, String)>> {
    if !sync_entity_envelope_matches(collection, id, payload) {
        return Ok(Some((String::new(), "identity".to_owned())));
    }

    let Some(schema) = sync_entity_branch(collection)? else {
        return Ok(Some((
            "/collection".to_owned(),
            "unknownCollection".to_owned(),
        )));
    };
    let Some(object) = payload.as_object_mut() else {
        return Ok(Some((String::new(), "identity".to_owned())));
    };
    object.retain(|key, _| matches!(key.as_str(), "collection" | "id" | "value"));

    if deleted {
        object.insert("value".to_owned(), json!({}));
        return Ok(None);
    }

    strip_stored_extra_fields(&schema, payload)?;
    first_error("SyncEntityPayload", payload)
}

/// Validate a sync entity that is about to be written without normalizing it.
///
/// Projection candidates must already match the exact envelope and strict live
/// entity schema; only persisted historical rows may use extra-field stripping.
pub fn validate_sync_entity_payload_strict(
    collection: &str,
    id: &str,
    deleted: bool,
    payload: &Value,
) -> SchemaResult<bool> {
    if !sync_entity_envelope_matches(collection, id, payload) {
        return Ok(false);
    }
    let Some(object) = payload.as_object() else {
        return Ok(false);
    };
    if object.len() != 3 || sync_entity_branch(collection)?.is_none() {
        return Ok(false);
    }
    if deleted {
        return Ok(object
            .get("value")
            .and_then(Value::as_object)
            .is_some_and(serde_json::Map::is_empty));
    }
    Ok(first_error("SyncEntityPayload", payload)?.is_none())
}

fn sync_entity_envelope_matches(collection: &str, id: &str, payload: &Value) -> bool {
    let Some(object) = payload.as_object() else {
        return false;
    };
    object.get("collection").and_then(Value::as_str) == Some(collection)
        && object.get("id").and_then(Value::as_str) == Some(id)
        && object.get("value").is_some_and(Value::is_object)
}

fn sync_entity_branch(collection: &str) -> SchemaResult<Option<String>> {
    Ok(compiled_schemas()?
        .sync_entity_branches
        .get(collection)
        .cloned())
}

fn extra_field_errors(error: &ValidationError<'_>) -> Vec<(String, Vec<String>)> {
    match error.kind() {
        ValidationErrorKind::AdditionalProperties { unexpected } => vec![(
            error.instance_path().as_str().to_owned(),
            unexpected.clone(),
        )],
        ValidationErrorKind::AnyOf { context } => context
            .iter()
            .map(|branch| extra_only_branch(branch))
            .filter(|(other_errors, _)| *other_errors == 0)
            .min_by_key(|(_, fields)| fields.len())
            .map_or_else(Vec::new, |(_, fields)| fields),
        ValidationErrorKind::OneOfNotValid { context } => {
            let mut branches = context
                .iter()
                .map(|branch| extra_only_branch(branch))
                .filter(|(other_errors, _)| *other_errors == 0);
            let selected = branches.next();
            if branches.next().is_none() {
                selected.map_or_else(Vec::new, |(_, fields)| fields)
            } else {
                Vec::new()
            }
        }
        ValidationErrorKind::OneOfMultipleValid { .. } => Vec::new(),
        _ => Vec::new(),
    }
}

fn extra_only_branch(branch: &[ValidationError<'_>]) -> (usize, Vec<(String, Vec<String>)>) {
    (
        branch.iter().map(non_extra_error_count).sum(),
        branch.iter().flat_map(extra_field_errors).collect(),
    )
}

fn non_extra_error_count(error: &ValidationError<'_>) -> usize {
    match error.kind() {
        ValidationErrorKind::AdditionalProperties { .. } => 0,
        ValidationErrorKind::AnyOf { context }
        | ValidationErrorKind::OneOfNotValid { context }
        | ValidationErrorKind::OneOfMultipleValid { context } => context
            .iter()
            .map(|branch| branch.iter().map(non_extra_error_count).sum::<usize>())
            .min()
            .unwrap_or(1),
        _ => 1,
    }
}

fn compiled_schemas() -> SchemaResult<&'static CompiledSchemas> {
    match COMPILED_SCHEMAS.get_or_init(prepare_schemas) {
        Ok(schemas) => Ok(schemas),
        Err(message) => Err(message.clone()),
    }
}

fn prepare_schemas() -> SchemaResult<CompiledSchemas> {
    let openapi: Value = serde_json::from_slice(OPENAPI)
        .map_err(|error| format!("cannot parse OpenAPI schema document: {error}"))?;
    let account_registry: Value = serde_json::from_slice(ACCOUNT_REGISTRY)
        .map_err(|error| format!("cannot parse account-registry schema: {error}"))?;
    let usage_limits: Value = serde_json::from_slice(USAGE_LIMITS)
        .map_err(|error| format!("cannot parse workspace-usage-limits schema: {error}"))?;
    let registry = registry_for_document(&openapi)?;
    let mut validators = BTreeMap::new();

    for name in [
        "AgentEntityDto",
        "AgentSummaryEntityDto",
        "AgentDetailEntityDto",
        "ChatEntityDto",
        "ComplaintEntityDto",
        "ConfigureResponse",
        "DiagnosticsResponse",
        "EdgeEntityDto",
        "ErrorResponse",
        "EventEntityDto",
        "ExecutionLedger",
        "PeerTeamEntityDto",
        "PreferenceField",
        "ProjectEntityDto",
        "ProjectMutationRecord",
        "RequestEntityDto",
        "ResourceRef",
        "SubscriptionAdd",
        "SubscriptionCursor",
        "SubscriptionReplyKind",
        "ResourceProgressEvent",
        "ResourceEntityTransferEvent",
        "EntityChangeBatch",
        "RoomEntityDto",
        "RuleEntityDto",
        "RuntimeLockOperation",
        "RuntimeLockSample",
        "SqliteContention",
        "SyncPullResetResponse",
        "SyncPullResponse",
        "TranscriptStreamDelta",
        "TaskEntityDto",
        "RetainedWorktreesResponse",
        "WorkEntityDto",
        "WorkspaceEntityDto",
        "DevicePairResponse",
        "ServerIdentityResponse",
        "DiscoveryIdentity",
        "ServerOperationRequest",
        "ServerOperationResponse",
        "SyncEntityPayload",
        "SyncEntity",
        "AutoPairRequest",
        "DevicePairRequest",
        "AgentRecordResponse",
        "AgentRequest",
        "AnnotationReceipt",
        "CheckpointSummary",
        "CheckpointsResponse",
        "CreateInvite",
        "ComplaintRecordResponse",
        "FederationParticipant",
        "JsonObjectResponse",
        "MonitorEntityDto",
        "MonitorInput",
        "MessageRequest",
        "MonitorRecordResponse",
        "PlanView",
        "PairRequest",
        "ProfileRecord",
        "PullRequest",
        "PullResult",
        "RenameServer",
        "QuestionEntry",
        "QuestionHistory",
        "QueueItem",
        "RequestRecordResponse",
        "RuleRecordResponse",
        "SignedEnvelope",
        "StatusRequest",
        "TaskDetailResponse",
        "TerminalRecord",
        "TranscriptAsset",
        "UsageResumeRecordResponse",
        "WorkItem",
        "studio_api__federation__models__MessageReceipt",
    ] {
        let reference = format!("{OPENAPI_URI}#/components/schemas/{name}");
        let validator = NamedValidator::new(reference, name);
        validators.insert(name.to_owned(), validator);
    }
    let sync_mapping = openapi
        .pointer("/components/schemas/SyncEntityPayload/discriminator/mapping")
        .and_then(Value::as_object)
        .ok_or_else(|| "SyncEntityPayload has no discriminator mapping".to_owned())?;
    let mut sync_entity_branches = BTreeMap::new();
    for (collection, reference) in sync_mapping {
        let reference = reference.as_str().ok_or_else(|| {
            "SyncEntityPayload discriminator reference is not a string".to_owned()
        })?;
        let name = reference
            .strip_prefix("#/components/schemas/")
            .ok_or_else(|| "SyncEntityPayload discriminator reference is not local".to_owned())?;
        sync_entity_branches.insert(collection.clone(), name.to_owned());
        if !validators.contains_key(name) {
            let validator = NamedValidator::new(format!("{OPENAPI_URI}{reference}"), name);
            validators.insert(name.to_owned(), validator);
        }
    }
    for (name, uri) in [
        (
            "BudgetCapture",
            "https://codex-studio.invalid/budget-capture.schema.json",
        ),
        (
            "BudgetHistory",
            "https://codex-studio.invalid/budget-history.schema.json",
        ),
        (
            "AnalyticsAgent",
            "https://codex-studio.invalid/analytics-agent.schema.json",
        ),
        (
            "AnalyticsContext",
            "https://codex-studio.invalid/analytics-context.schema.json",
        ),
        (
            "AnalyticsHistory",
            "https://codex-studio.invalid/analytics-history.schema.json",
        ),
        (
            "AnalyticsHistoryImporterDiagnostic",
            "https://codex-studio.invalid/analytics-history-importer.schema.json",
        ),
        (
            "AnalyticsItem",
            "https://codex-studio.invalid/analytics-item.schema.json",
        ),
        (
            "AnalyticsNotice",
            "https://codex-studio.invalid/analytics-notice.schema.json",
        ),
        (
            "AnalyticsTurn",
            "https://codex-studio.invalid/analytics-turn.schema.json",
        ),
        (
            "AnalyticsUsage",
            "https://codex-studio.invalid/analytics-usage.schema.json",
        ),
        ("FederationChatPayload", FEDERATION_CHAT_PAYLOAD_URI),
        ("FederationRoomPayload", FEDERATION_ROOM_PAYLOAD_URI),
        ("PersistedAccessClient", ACCESS_CLIENT_URI),
        ("PersistedAccessInvite", ACCESS_INVITE_URI),
        ("PersistedAccessReceipt", ACCESS_RECEIPT_URI),
        ("PersistedAccessRecord", ACCESS_RECORD_URI),
        ("PersistedAccountTransferRecord", ACCOUNT_TRANSFER_URI),
        ("PersistedAgentRecord", AGENT_RECORD_URI),
        ("PersistedAnnotationRecord", ANNOTATION_RECORD_URI),
        (
            "PersistedBudgetAgent",
            "https://codex-studio.invalid/persisted-budget-agent.schema.json",
        ),
        (
            "PersistedBudgetState",
            "https://codex-studio.invalid/persisted-budget-state.schema.json",
        ),
        (
            "PersistedBudgetUsage",
            "https://codex-studio.invalid/persisted-budget-usage.schema.json",
        ),
        ("PersistedChatMessage", CHAT_MESSAGE_URI),
        ("PersistedCheckpointRecord", CHECKPOINT_RECORD_URI),
        ("PersistedClaudeAttemptRecord", CLAUDE_ATTEMPT_URI),
        ("PersistedClaudeControlRecord", CLAUDE_CONTROL_URI),
        ("PersistedClaudeLoginRecord", CLAUDE_LOGIN_URI),
        ("PersistedComplaintRecord", COMPLAINT_RECORD_URI),
        ("PersistedExecutionRecord", EXECUTION_RECORD_URI),
        ("PersistedExecutionRunRecord", EXECUTION_RUN_RECORD_URI),
        ("PersistedFederationInbox", FEDERATION_INBOX_URI),
        ("PersistedFederationOutbox", FEDERATION_OUTBOX_URI),
        ("PersistedFederationPeer", FEDERATION_PEER_URI),
        ("PersistedFederationRoom", FEDERATION_ROOM_URI),
        ("PersistedFederationRoomRecord", FEDERATION_ROOM_RECORD_URI),
        ("PersistedFederationSettings", FEDERATION_SETTINGS_URI),
        ("PersistedGraphAgentRecord", GRAPH_AGENT_RECORD_URI),
        ("PersistedMessageEvent", MESSAGE_EVENT_URI),
        ("PersistedMonitorRecord", MONITOR_RECORD_URI),
        ("PersistedMonitorResult", MONITOR_RESULT_URI),
        ("PersistedOperationResult", OPERATION_RESULT_URI),
        ("PersistedPlanRecord", PLAN_RECORD_URI),
        ("PersistedProfileRecord", PROFILE_RECORD_URI),
        (
            "PersistedProjectLocationOutbox",
            PROJECT_LOCATION_OUTBOX_URI,
        ),
        ("PersistedProjectRecord", PROJECT_RECORD_URI),
        ("PersistedRequestRecord", REQUEST_RECORD_URI),
        ("PersistedRoomRecord", ROOM_RECORD_URI),
        ("PersistedRuleRecord", RULE_RECORD_URI),
        ("PersistedServerDelivery", SERVER_DELIVERY_URI),
        ("PersistedSyncEntity", SYNC_ENTITY_URI),
        ("PersistedTaskRecord", TASK_RECORD_URI),
        ("PersistedTerminalRecord", TERMINAL_RECORD_URI),
        ("PersistedToolAlias", TOOL_ALIAS_URI),
        ("PersistedToolRequestRecord", TOOL_REQUEST_RECORD_URI),
        ("PersistedToolResult", TOOL_RESULT_URI),
        ("PersistedTranscriptAsset", TRANSCRIPT_ASSET_URI),
        ("PersistedTranscriptRecord", TRANSCRIPT_RECORD_URI),
        ("PersistedTransferAgentRecord", TRANSFER_AGENT_URI),
        ("PersistedUsageResumeRecord", USAGE_RESUME_RECORD_URI),
        ("PersistedWorkItemRecord", WORK_ITEM_RECORD_URI),
        ("PersistedWorkspaceOperationRecord", WORKSPACE_OPERATION_URI),
        ("RecoveryAgentRecord", RECOVERY_AGENT_URI),
        ("RecoveryTaskRecord", RECOVERY_TASK_URI),
        ("RecoveryToolReceipt", RECOVERY_RECEIPT_URI),
        ("RuntimeEventMetadataRecord", EVENT_METADATA_URI),
        (
            "WorkspaceCheckpointAgentRecord",
            WORKSPACE_CHECKPOINT_AGENT_URI,
        ),
        (
            "WorkspaceCheckpointHistoryRecord",
            WORKSPACE_CHECKPOINT_HISTORY_URI,
        ),
        (
            "account-registry/AccountRegistryRecord",
            ACCOUNT_REGISTRY_URI,
        ),
        (
            "workspace-usage-limits/UsageLimitsWorkspaceEnvelope",
            USAGE_LIMITS_URI,
        ),
    ] {
        let validator = NamedValidator::new(uri.to_owned(), name);
        validators.insert(name.to_owned(), validator);
    }

    register_definitions(
        ACCOUNT_REGISTRY_URI,
        "account-registry",
        &account_registry,
        &mut validators,
    )?;
    register_definitions(
        USAGE_LIMITS_URI,
        "workspace-usage-limits",
        &usage_limits,
        &mut validators,
    )?;
    let relocation: Value = serde_json::from_slice(RELOCATION).map_err(|e| e.to_string())?;
    register_definitions(RELOCATION_URI, "relocation", &relocation, &mut validators)?;
    let continuation: Value =
        serde_json::from_slice(CONTINUATION).map_err(|error| error.to_string())?;
    register_definitions(
        CONTINUATION_URI,
        "continuation",
        &continuation,
        &mut validators,
    )?;
    Ok(CompiledSchemas {
        registry,
        validators,
        sync_entity_branches,
    })
}

fn register_definitions(
    document_uri: &str,
    namespace: &str,
    document: &Value,
    validators: &mut BTreeMap<String, NamedValidator>,
) -> SchemaResult<()> {
    let definitions = document
        .get("$defs")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{namespace} schema has no $defs object"))?;
    for name in definitions.keys() {
        let reference = format!("{document_uri}#/$defs/{}", escape_pointer(name));
        let validator = NamedValidator::new(reference, name);
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

// Rust owns draft payloads shared by HTTP and the native client.
pub use studio_client_domain::drafts;

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        first_error, initialize, normalize_sync_entity_payload, registry_for_document,
        table_schema, validate,
    };

    #[test]
    fn entity_v4_collection_wire_contracts_match_payload_owner() {
        use std::collections::BTreeMap;
        let openapi: serde_json::Value = serde_json::from_slice(super::OPENAPI).expect("OpenAPI");
        let schemas = &openapi["components"]["schemas"];
        // SyncEntityPayload's discriminator owns the collection set and value DTOs.
        let owner: BTreeMap<_, _> = schemas["SyncEntityPayload"]["discriminator"]["mapping"]
            .as_object()
            .expect("payload owner")
            .iter()
            .map(|(collection, reference)| {
                let name = reference
                    .as_str()
                    .expect("payload ref")
                    .rsplit('/')
                    .next()
                    .expect("name");
                (
                    collection.as_str(),
                    schemas[name]["properties"]["value"]["$ref"]
                        .as_str()
                        .expect("value DTO"),
                )
            })
            .collect();
        let variants = schemas["SyncEntity"]["oneOf"]
            .as_array()
            .expect("wire variants");
        let upserts: BTreeMap<_, _> = variants
            .iter()
            .filter(|variant| variant["properties"]["op"]["const"] == "upsert")
            .map(|variant| {
                (
                    variant["properties"]["collection"]["const"]
                        .as_str()
                        .expect("wire collection"),
                    variant["properties"]["value"]["$ref"]
                        .as_str()
                        .expect("wire value DTO"),
                )
            })
            .collect();
        assert_eq!(
            upserts.len(),
            variants
                .iter()
                .filter(|variant| variant["properties"]["op"]["const"] == "upsert")
                .count(),
            "SyncEntity must have exactly one upsert variant for each collection"
        );
        assert_eq!(
            upserts, owner,
            "SyncEntity upserts must consume the SyncEntityPayload owner"
        );
        let deletes = variants
            .iter()
            .find(|variant| variant["properties"]["op"]["const"] == "delete")
            .expect("delete variant");
        let deleted: std::collections::BTreeSet<_> = deletes["properties"]["collection"]["enum"]
            .as_array()
            .expect("delete collections")
            .iter()
            .map(|value| value.as_str().expect("collection"))
            .collect();
        assert_eq!(
            deleted,
            owner.keys().copied().collect(),
            "SyncEntity deletes must consume the SyncEntityPayload owner"
        );
    }

    #[test]
    fn view_scopes_cover_every_collection_and_keep_detail_out_of_global() {
        let openapi: serde_json::Value = serde_json::from_slice(super::OPENAPI).expect("OpenAPI");
        let scopes = super::sync_protocol::subscription_entity_scopes();
        let global = &scopes["state"].collections;
        assert!(global.iter().any(|value| value == "agentSummary"));
        for collection in [
            "agent",
            "agentDetail",
            "task",
            "monitor",
            "sessionCost",
            "accountCost",
            "desktop",
            "terminal",
        ] {
            assert!(
                !global.iter().any(|value| value == collection),
                "{collection} must be scoped"
            );
        }
        for collection in
            openapi["components"]["schemas"]["SyncEntityPayload"]["discriminator"]["mapping"]
                .as_object()
                .expect("owner")
                .keys()
        {
            assert!(
                collection == "agent"
                    || scopes
                        .values()
                        .any(|scope| scope.collections.contains(collection)),
                "{collection} has no subscription owner"
            );
        }
        let summary = &openapi["components"]["schemas"]["AgentSummaryEntityDto"]["properties"];
        for field in [
            "lastAnswer",
            "tail",
            "overview",
            "commandProgress",
            "contextUsage",
            "tokensUsed",
        ] {
            assert!(summary.get(field).is_none(), "{field} is detail");
        }
    }

    #[test]
    fn preparing_named_schemas_does_not_compile_validators() {
        let mut schemas = super::prepare_schemas().expect("registry prepares");
        assert!(
            schemas
                .validators
                .values()
                .all(|entry| entry.compiled.get().is_none())
        );
        let requested = &schemas.validators["account-registry/AccountRecord"];
        requested
            .get(&schemas.registry)
            .expect("requested schema compiles");
        assert_eq!(
            schemas
                .validators
                .values()
                .filter(|entry| entry.compiled.get().is_some())
                .count(),
            1
        );

        schemas.validators.insert(
            "!unused-invalid".to_owned(),
            super::NamedValidator::new(
                format!("{}#/components/schemas/MissingSchema", super::OPENAPI_URI),
                "unused-invalid",
            ),
        );
        let error = schemas
            .initialize()
            .expect_err("unused invalid schema fails startup");
        assert!(error.starts_with("cannot compile persisted schema unused-invalid:"));
        assert_eq!(schemas.initialize(), Err(error));
    }

    #[test]
    fn generated_agent_contracts_supply_runtime_capacity_values() {
        use super::agent_limits as limits;
        assert_eq!(
            limits::MAX_TEAM_AGENTS as i64,
            limits::MAX_SUBAGENT_CONCURRENCY + limits::TEAM_AGENT_HEADROOM
        );
        assert_eq!(limits::MAX_TRANSFER_AGENT_RECORDS, limits::MAX_TEAM_AGENTS);
        assert!(limits::DEFAULT_MAX_TEAM_AGENTS <= limits::MAX_TEAM_AGENTS as i64);
        assert_eq!(
            limits::DEFAULT_GLOBAL_CONCURRENCY,
            limits::MAX_SUBAGENT_CONCURRENCY + limits::LEAD_TEAM_RECORDS
        );
    }

    #[test]
    fn typed_table_selects_one_authoritative_storage_schema() {
        for (table, schema) in [
            ("runtime_access_clients", "PersistedAccessClient"),
            ("runtime_access_discovered", "PersistedAccessClient"),
            ("runtime_server_outbox", "PersistedServerDelivery"),
            ("runtime_federation_rooms", "PersistedFederationRoom"),
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
            ("runtime_execution_runs", "PersistedExecutionRunRecord"),
        ] {
            assert_eq!(table_schema(table), Some(schema), "{table}");
        }
        assert_eq!(table_schema("untyped_table"), None);
    }

    #[test]
    fn compiled_record_schemas_accept_known_records_and_reject_invalid_values() {
        initialize().expect("persisted schemas compile");
        assert!(
            super::compiled_schemas()
                .expect("registry prepares")
                .validators
                .values()
                .all(|entry| matches!(entry.compiled.get(), Some(Ok(_))))
        );
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
            validate(
                "PersistedAgentRecord",
                &json!({
                    "id":"agent-1",
                    "rootId":"agent-1",
                    "epoch":0,
                    "status":"running",
                    "startAttempt":{
                        "id":"attempt-1",
                        "submitted":true,
                        "connectionId":"fcd7c814-1048-4599-9b2d-4151fbc40b68"
                    }
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
    fn execution_run_schema_requires_status_and_types_latest_attempt_id() {
        let valid = json!({
            "id":"run-1","agent":"agent-1","accountKey":"default",
            "epoch":2,"status":"running","created":1.0,"latestAttemptId":null
        });
        assert!(validate("PersistedExecutionRunRecord", &valid).expect("schema validates"));
        let mut missing_status = valid.clone();
        missing_status
            .as_object_mut()
            .expect("record is an object")
            .remove("status");
        assert!(
            !validate("PersistedExecutionRunRecord", &missing_status).expect("schema validates")
        );
        let mut invalid_attempt = valid;
        invalid_attempt["latestAttemptId"] = json!(7);
        assert!(
            !validate("PersistedExecutionRunRecord", &invalid_attempt).expect("schema validates")
        );
        for epoch in [json!(-1), json!(9_007_199_254_740_992_i64)] {
            let mut invalid_epoch = json!({
                "id":"run-1","agent":"agent-1","accountKey":"default",
                "epoch":epoch,"status":"running","created":1.0
            });
            assert!(
                !validate("PersistedExecutionRunRecord", &invalid_epoch).expect("schema validates")
            );
            invalid_epoch["epoch"] = json!(0);
            assert!(
                validate("PersistedExecutionRunRecord", &invalid_epoch).expect("schema validates")
            );
        }
    }

    #[test]
    fn request_registry_includes_shared_state_documents() {
        let document = json!({"components":{"schemas":{}}});
        registry_for_document(&document).expect("shared registry prepares");
    }

    #[test]
    fn unknown_schema_names_are_configuration_errors() {
        assert!(validate("MissingRecordSchema", &json!({})).is_err());
        assert!(super::compiled_validator("MissingRecordSchema").is_err());
        assert!(std::ptr::eq(
            super::compiled_validator("SyncTaskEntityPayload").expect("sync branch compiles"),
            super::compiled_validator("SyncTaskEntityPayload").expect("same cached branch"),
        ));
    }

    #[test]
    fn server_operation_response_schema_is_available_at_http_boundary() {
        assert_eq!(
            first_error(
                "ServerOperationResponse",
                &json!({"requestId":"request-1","outcome":"applied","value":{"ok":true}})
            )
            .expect("response schema is compiled"),
            None
        );
        assert!(
            first_error("ServerOperationResponse", &json!({"outcome":"applied"}))
                .expect("response schema is compiled")
                .is_some()
        );
    }

    #[test]
    fn stored_sync_payload_selects_generated_branch_before_stripping() {
        let mut valid_task = json!({
            "collection":"task",
            "id":"task-1",
            "legacyEnvelopeField":true,
            "value":{"id":"task-1","legacyField":true}
        });
        assert_eq!(
            normalize_sync_entity_payload("task", "task-1", false, &mut valid_task)
                .expect("generated sync schemas compile"),
            None
        );
        assert_eq!(
            valid_task,
            json!({
                "collection":"task",
                "id":"task-1",
                "value":{"id":"task-1"}
            })
        );

        let mut malformed_task = json!({
            "collection":"task",
            "id":"task-invalid",
            "value":{"id":17,"legacyField":true}
        });
        assert!(
            normalize_sync_entity_payload("task", "task-invalid", false, &mut malformed_task)
                .expect("stored payload validation runs")
                .is_some()
        );

        let mut tombstone = json!({
            "collection":"room",
            "id":"room-1",
            "legacyEnvelopeField":true,
            "value":{"not-a-live-room":17}
        });
        assert_eq!(
            normalize_sync_entity_payload("room", "room-1", true, &mut tombstone)
                .expect("tombstone envelope validates"),
            None
        );
        assert_eq!(
            tombstone,
            json!({
                "collection":"room",
                "id":"room-1",
                "value":{}
            })
        );

        let mut unknown = json!({
            "collection":"not-supported",
            "id":"unknown-1",
            "value":{"id":"unknown-1"}
        });
        assert_eq!(
            normalize_sync_entity_payload("not-supported", "unknown-1", false, &mut unknown)
                .expect("unknown collection is a row failure"),
            Some(("/collection".to_owned(), "unknownCollection".to_owned()))
        );
    }
}

pub mod task_gate;

pub mod claude_permissions;

pub mod workspace_modes;
