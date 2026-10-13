//! Compiled persisted-state validators and generated shared runtime values.

pub mod shared_values;

use std::collections::BTreeMap;
use std::sync::OnceLock;

pub use jsonschema::{Draft, Registry as JsonSchemaRegistry, Validator};
use serde_json::{Value, json};

pub mod agent_limits_generated;

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

const PERSISTED_SCHEMAS: &[(&str, &[u8])] = &[
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
        "graph_agents" => "PersistedGraphAgentRecord",
        "runtime_access_clients" | "runtime_access_discovered" => "PersistedAccessClient",
        "runtime_access_invites" => "PersistedAccessInvite",
        "runtime_access_receipts" => "PersistedAccessReceipt",
        "runtime_account_transfers" => "PersistedAccountTransferRecord",
        "runtime_agent_moves" => "relocation/MoveRecord",
        "runtime_agents" => "PersistedAgentRecord",
        "runtime_annotations" => "PersistedAnnotationRecord",
        "runtime_assets" => "PersistedTranscriptAsset",
        "runtime_capacity_retries" => "continuation/CapacityContinuation",
        "runtime_chat_messages" => "PersistedChatMessage",
        "runtime_checkpoints" => "PersistedCheckpointRecord",
        "runtime_complaints" => "PersistedComplaintRecord",
        "runtime_event_meta" => "RuntimeEventMetadataRecord",
        "runtime_events" => "PersistedMessageEvent",
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
        "DevicePairResponse",
        "ServerIdentityResponse",
        "DiscoveryIdentity",
        "ServerOperationRequest",
        "AutoPairRequest",
        "DevicePairRequest",
        "AgentRecordResponse",
        "AgentRequest",
        "AnnotationReceipt",
        "CheckpointSummary",
        "CheckpointsResponse",
        "ComplaintRecordResponse",
        "FederationParticipant",
        "JsonObjectResponse",
        "MonitorEntityDto",
        "MonitorInput",
        "MessageRequest",
        "MonitorRecordResponse",
        "PlanView",
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
        let validator = compile(&registry, json!({"$ref": reference}), name)?;
        validators.insert(name.to_owned(), validator);
    }
    for (name, uri) in [
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
        ("PersistedChatMessage", CHAT_MESSAGE_URI),
        ("PersistedCheckpointRecord", CHECKPOINT_RECORD_URI),
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
    let relocation: Value = serde_json::from_slice(RELOCATION).map_err(|e| e.to_string())?;
    compile_definitions(
        &registry,
        RELOCATION_URI,
        "relocation",
        &relocation,
        &mut validators,
    )?;
    let continuation: Value =
        serde_json::from_slice(CONTINUATION).map_err(|error| error.to_string())?;
    compile_definitions(
        &registry,
        CONTINUATION_URI,
        "continuation",
        &continuation,
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
    fn generated_agent_contracts_supply_runtime_capacity_values() {
        use super::agent_limits_generated as limits;
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
    fn execution_run_schema_requires_status_and_types_latest_attempt_id() {
        initialize().expect("persisted schemas compile");
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
    }
}
