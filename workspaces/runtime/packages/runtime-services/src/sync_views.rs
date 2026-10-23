//! Canonical renderer entity views built from records and explicit context.

use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};

use crate::event_write::truthy;

/// Maximum last-answer text retained in an agent sync view. Unit: Unicode scalar values.
pub(crate) const AGENT_SYNC_LAST_ANSWER_CHARS: usize = 4_000;

const AGENT_SHORT_STRING_OBJECTS: &[&str] = &[
    "activity",
    "nativeStatus",
    "nativeSafetyBuffering",
    "nativeSafetyRetry",
    "nativeTurnError",
    "nativeThreadBlock",
    "connectionCheck",
    "readState",
    "startAttempt",
    "capacityRetry",
    "usageResume",
    "contextRepairWait",
];

/// The committed API schema is parsed once and shared with HTTP validation.
pub fn openapi_document() -> &'static Value {
    static DOCUMENT: OnceLock<Value> = OnceLock::new();
    DOCUMENT.get_or_init(|| {
        serde_json::from_str(include_str!("../../runtime-http/openapi.json"))
            .expect("the committed OpenAPI document is valid JSON")
    })
}

fn schema_definition<'a>(schema: &'a Value, document: &'a Value) -> &'a Value {
    schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|reference| reference.strip_prefix('#'))
        .and_then(|pointer| document.pointer(pointer))
        .unwrap_or(schema)
}

fn collect_schema_properties<'a>(
    schema: &'a Value,
    document: &'a Value,
    properties: &mut BTreeMap<&'a str, &'a Value>,
    depth: usize,
) {
    if depth > 32 {
        return;
    }
    let schema = schema_definition(schema, document);
    if let Some(fields) = schema.get("properties").and_then(Value::as_object) {
        properties.extend(fields.iter().map(|(name, value)| (name.as_str(), value)));
    }
    for keyword in ["allOf", "anyOf", "oneOf"] {
        if let Some(variants) = schema.get(keyword).and_then(Value::as_array) {
            for variant in variants {
                collect_schema_properties(variant, document, properties, depth + 1);
            }
        }
    }
}

type SchemaProjectionProperties = HashMap<usize, BTreeMap<&'static str, &'static Value>>;

fn schema_projection_properties() -> &'static SchemaProjectionProperties {
    static PROPERTIES: OnceLock<SchemaProjectionProperties> = OnceLock::new();
    PROPERTIES.get_or_init(|| {
        fn visit(
            schema: &'static Value,
            document: &'static Value,
            properties: &mut SchemaProjectionProperties,
        ) {
            if schema.is_object() {
                let is_schema = ["$ref", "properties", "allOf", "anyOf", "oneOf", "items"]
                    .iter()
                    .any(|key| schema.get(*key).is_some());
                if is_schema {
                    let mut fields = BTreeMap::new();
                    collect_schema_properties(schema, document, &mut fields, 0);
                    properties.insert(schema as *const Value as usize, fields);
                }
                if let Some(object) = schema.as_object() {
                    for child in object.values() {
                        visit(child, document, properties);
                    }
                }
            } else if let Some(items) = schema.as_array() {
                for child in items {
                    visit(child, document, properties);
                }
            }
        }

        let document = openapi_document();
        let mut properties = HashMap::new();
        if let Some(schemas) = document.pointer("/components/schemas") {
            visit(schemas, document, &mut properties);
        }
        properties
    })
}

fn bound_sync_value(value: &Value, field: &str, agent_short_strings: bool) -> Value {
    match value {
        Value::String(text) => {
            let limit = if agent_short_strings {
                2_000
            } else {
                match field {
                    "overview" => 9_000,
                    "error" | "tail" | "description" | "command" | "query" => 2_000,
                    "lastAnswer" => AGENT_SYNC_LAST_ANSWER_CHARS,
                    "text" => 4_000,
                    _ => 12_000,
                }
            };
            Value::String(text.chars().take(limit).collect())
        }
        _ => value.clone(),
    }
}

fn project_schema_value(
    value: &Value,
    schema: &Value,
    field: &str,
    document: &Value,
    depth: usize,
    agent_record: bool,
    agent_short_strings: bool,
) -> Value {
    if depth > 32 {
        return Value::Null;
    }
    let schema = schema_definition(schema, document);
    match value {
        Value::Object(source) => {
            let properties = schema_projection_properties().get(&(schema as *const Value as usize));
            if properties.is_none_or(BTreeMap::is_empty) {
                if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                    return Value::Object(Map::new());
                }
                return Value::Object(
                    source
                        .iter()
                        .map(|(name, child)| {
                            (
                                name.clone(),
                                project_schema_value(
                                    child,
                                    &Value::Null,
                                    name,
                                    document,
                                    depth + 1,
                                    agent_record,
                                    agent_short_strings,
                                ),
                            )
                        })
                        .collect(),
                );
            }
            Value::Object(
                source
                    .iter()
                    .filter_map(|(name, child)| {
                        properties
                            .and_then(|properties| properties.get(name.as_str()))
                            .map(|field_schema| {
                                (name.clone(), {
                                    let short_strings = agent_short_strings
                                        || (agent_record
                                            && depth == 0
                                            && AGENT_SHORT_STRING_OBJECTS.contains(&name.as_str()));
                                    project_schema_value(
                                        child,
                                        field_schema,
                                        name,
                                        document,
                                        depth + 1,
                                        agent_record,
                                        short_strings,
                                    )
                                })
                            })
                    })
                    .collect(),
            )
        }
        Value::Array(items) => {
            let item_schema = schema.get("items").unwrap_or(&Value::Null);
            let limit = if field == "sidebarOrder" { 10_000 } else { 200 };
            Value::Array(
                items
                    .iter()
                    .take(limit)
                    .map(|item| {
                        project_schema_value(
                            item,
                            item_schema,
                            "",
                            document,
                            depth + 1,
                            agent_record,
                            agent_short_strings,
                        )
                    })
                    .collect(),
            )
        }
        _ => bound_sync_value(value, field, agent_short_strings),
    }
}

/// Filter a stored record through the committed renderer DTO schema.
pub fn project_persisted_record(collection: &str, record: &Value) -> Option<Value> {
    let schema_name = match collection {
        "agent" => "AgentEntityDto",
        "room" => "RoomEntityDto",
        "task" => "TaskEntityDto",
        "taskDetail" => "TaskDetailResponse",
        "monitor" => "MonitorEntityDto",
        "complaint" => "ComplaintEntityDto",
        "request" => "RequestEntityDto",
        "rule" => "RuleEntityDto",
        "project" => "ProjectEntityDto",
        "peerTeam" => "PeerTeamEntityDto",
        "event" => "EventEntityDto",
        "work" => "WorkEntityDto",
        "workspace" => "WorkspaceEntityDto",
        "chat" => "ChatEntityDto",
        "edge" => "EdgeEntityDto",
        _ => return None,
    };
    record
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?;
    let document = openapi_document();
    let schema = document.pointer(&format!("/components/schemas/{schema_name}"))?;
    // Cleanup receipts contain private operation identities. Publish only the
    // retained folder and the reason the cleanup owner recorded.
    let mut public_record = std::borrow::Cow::Borrowed(record);
    if collection == "agent"
        && let Some(intent) = record
            .get("workspaceCleanupIntent")
            .filter(|v| v.is_object())
    {
        public_record.to_mut()["workspaceCleanupIntent"] = json!({
            "phase": intent.get("phase").and_then(Value::as_str).unwrap_or("unknown"),
            "reason": intent.pointer("/receipt/reason").or_else(|| intent.get("reason")).filter(|value| value.is_string()).map(|value| bound_sync_value(value, "error", false)),
            "path": intent.pointer("/effect/identity/cwd").or_else(|| intent.get("path")).or_else(|| record.get("cwd")).and_then(Value::as_str),
        });
    }
    Some(project_schema_value(
        &public_record,
        schema,
        "",
        document,
        0,
        collection == "agent",
        false,
    ))
}

#[derive(Clone, Debug, Default)]
pub struct AgentViewContext {
    pub project_server_id: Option<String>,
    pub launcher_alive: bool,
    pub empty_lead: bool,
    pub latest_work_result_file: Option<String>,
    pub command_progress: Vec<studio_runtime_schema::agent_progress::AgentCommandProgress>,
    pub stall: Option<studio_runtime_schema::agent_progress::AgentStall>,
}

/// Minimal displayable record for an agent whose stored schema failed. Keep
/// valid hierarchy and lead identity so the renderer does not detach a held
/// child from its root or render a lead as a worker.
pub(crate) fn invalid_agent_view(id: &str, record: &Value, diagnostic: &str) -> Value {
    let mut view = json!({
        "id": id,
        "rootId": record.get("rootId").and_then(Value::as_str).unwrap_or(id),
        "epoch": record.get("epoch").and_then(Value::as_i64).unwrap_or_default(),
        "status": "failed",
        "error": diagnostic,
        "nativeFailureHold": true,
        "autoWake": false
    });
    if let Some(is_lead) = record.get("isLead").filter(|value| value.is_boolean()) {
        view["isLead"] = is_lead.clone();
    }
    view
}

/// Canonical wait projection used by both sync/HTTP and lead status.
pub(crate) fn agent_start_wait(
    record: &Value,
) -> Result<Option<studio_runtime_schema::agent_progress::AgentStartWait>, serde_json::Error> {
    use studio_runtime_schema::agent_progress::{AgentStartWait, StartWaitReason};
    // Older private native holds can carry a diagnostic object. Decode the
    // envelope without exposing or inspecting its diagnostic fields.
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum NativeHold {
        Flag(bool),
        Diagnostic(BTreeMap<String, serde::de::IgnoredAny>),
    }
    impl NativeHold {
        fn active(self) -> bool {
            match self {
                Self::Flag(active) => active,
                Self::Diagnostic(fields) => !fields.is_empty(),
            }
        }
    }
    // Agent records are open documents; this typed projection reads only wait ownership.
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Source {
        status: Option<studio_runtime_store::AgentStatus>,
        #[serde(default)]
        epoch: i64,
        #[serde(default)]
        auto_wake: bool,
        #[serde(default)]
        native_failure_hold: Option<NativeHold>,
        account_transfer_id: Option<String>,
        start_wait: Option<AgentStartWait>,
    }
    let source: Source = serde_json::from_value(record.clone())?;
    if !matches!(
        source.status,
        Some(
            studio_runtime_store::AgentStatus::Queued | studio_runtime_store::AgentStatus::Starting
        )
    ) {
        return Ok(None);
    }
    let persisted = source.start_wait.filter(|wait| wait.epoch == source.epoch);
    if source.status == Some(studio_runtime_store::AgentStatus::Starting) {
        return Ok(persisted);
    }
    let hold = if !source.auto_wake {
        Some(StartWaitReason::Paused)
    } else if source.native_failure_hold.is_some_and(NativeHold::active) {
        Some(StartWaitReason::NativeFailure)
    } else if source.account_transfer_id.is_some() {
        Some(StartWaitReason::AccountTransfer)
    } else {
        None
    };
    Ok(Some(match hold {
        Some(reason) => AgentStartWait::new(reason, source.epoch),
        None => persisted
            .unwrap_or_else(|| AgentStartWait::new(StartWaitReason::DispatchPending, source.epoch)),
    }))
}

/// Build the managed renderer view from a record and explicit runtime context.
pub fn agent_entity_view(record: &Value, context: &AgentViewContext) -> Option<Value> {
    let id = record.get("id").and_then(Value::as_str)?;
    #[cfg(test)]
    crate::scheduler::publication_work::projection(id);
    let mut view = record.as_object()?.clone();
    view.insert(
        studio_runtime_schema::agent_progress::START_WAIT_FIELD.into(),
        serde_json::to_value(agent_start_wait(record).ok()?).ok()?,
    );

    view.insert(
        "commandProgress".to_owned(),
        serde_json::to_value(&context.command_progress).ok()?,
    );
    view.insert(
        "stall".to_owned(),
        serde_json::to_value(&context.stall).ok()?,
    );
    let remote_server = record
        .pointer("/remoteWorker/server")
        .and_then(Value::as_str)
        .filter(|server| !server.is_empty());
    view.insert(
        "serverId".to_owned(),
        remote_server
            .or(context.project_server_id.as_deref())
            .map_or(Value::Null, |value| Value::String(value.to_owned())),
    );
    view.insert("id".to_owned(), Value::String(id.to_owned()));
    view.insert("kind".to_owned(), Value::String("agent".to_owned()));
    view.insert("source".to_owned(), Value::String("managed".to_owned()));
    let block = native_thread_block(record);
    let can_send = block.is_none()
        && !truthy(record.get("movedTo"))
        && !truthy(record.get("moveImportPending"));
    view.insert("canSend".to_owned(), Value::Bool(can_send));
    view.insert(
        "launcherAlive".to_owned(),
        Value::Bool(context.launcher_alive),
    );
    view.insert("nextTurnSettingsSupported".to_owned(), Value::Bool(true));
    view.insert("readStateSupported".to_owned(), Value::Bool(true));
    if let Some(block) = block {
        view.insert("nativeThreadBlock".to_owned(), block);
    } else {
        view.remove("nativeThreadBlock");
    }
    if truthy(record.get("isLead")) {
        view.remove("overview");
        view.insert("empty".to_owned(), Value::Bool(context.empty_lead));
    } else {
        let task = record
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let result = if truthy(record.get("lastCompletedTurn"))
            && !truthy(record.get("turnId"))
            && !truthy(record.get("inFlight"))
            && record.get("status").and_then(Value::as_str) == Some("completed")
        {
            record
                .get("lastAnswer")
                .and_then(Value::as_str)
                .unwrap_or_default()
        } else {
            ""
        };
        view.insert(
            "overview".to_owned(),
            json!({
                "task":task.chars().take(4000).collect::<String>(),
                "taskTruncated":task.chars().count()>4000,
                "result":result.chars().take(4000).collect::<String>(),
                "resultTruncated":result.chars().count()>4000,
                "resultTurnId":if result.is_empty(){Value::Null}else{record.get("lastCompletedTurn").cloned().unwrap_or(Value::Null)},
                "resultFile":context.latest_work_result_file
            }),
        );
    }
    project_persisted_record("agent", &Value::Object(view))
}

pub fn native_thread_block(record: &Value) -> Option<Value> {
    let thread_id = record.get("threadId").and_then(Value::as_str)?;
    if thread_id.is_empty() {
        return None;
    }
    let block = record.get("nativeThreadBlock").and_then(Value::as_object);
    if block
        .and_then(|block| block.get("threadId"))
        .and_then(Value::as_str)
        == Some(thread_id)
    {
        return Some(Value::Object(block?.clone()));
    }
    let blocked = |error: &Value| {
        (error.get("codexErrorInfo").and_then(Value::as_str) == Some("misalignmentPolicyViolation"))
            .then(|| json!({"threadId":thread_id,"error":error}))
    };
    if let Some(block) = record.get("error").and_then(blocked) {
        return Some(block);
    }
    let previous = record.get("nativeTurnError")?;
    (truthy(previous.get("turnId")) && previous.get("turnId") == record.get("turnId"))
        .then(|| previous.get("error").and_then(blocked))
        .flatten()
}

#[derive(Clone, Debug, Default)]
pub struct RoomViewContext {
    pub agents: Arc<Vec<Value>>,
    pub peer_teams: Arc<Vec<Value>>,
    pub active_agent_indices: Arc<HashMap<String, usize>>,
    pub last_message: Option<Value>,
    pub federation: Option<Value>,
}

impl RoomViewContext {
    pub(crate) fn new(agents: Vec<Value>, peer_teams: Vec<Value>) -> Self {
        let active_agent_indices = agents
            .iter()
            .enumerate()
            .filter(|(_, agent)| !truthy(agent.get("deletedAt")))
            .filter_map(|(index, agent)| {
                agent
                    .get("id")
                    .and_then(Value::as_str)
                    .map(|id| (id.to_owned(), index))
            })
            .collect();
        Self {
            agents: Arc::new(agents),
            peer_teams: Arc::new(peer_teams),
            active_agent_indices: Arc::new(active_agent_indices),
            last_message: None,
            federation: None,
        }
    }
}

fn active_agent<'a>(context: &'a RoomViewContext, id: &str) -> Option<&'a Value> {
    context
        .active_agent_indices
        .get(id)
        .and_then(|index| context.agents.get(*index))
}

/// Build the viewer-independent renderer view for one persisted room.
pub fn room_entity_view(record: &Value, context: &RoomViewContext) -> Option<Value> {
    let kind = record.get("kind").and_then(Value::as_str)?;
    let mut view = record.as_object()?.clone();
    view.insert(
        studio_runtime_schema::agent_progress::START_WAIT_FIELD.into(),
        serde_json::to_value(agent_start_wait(record).ok()?).ok()?,
    );

    let members: Vec<String>;
    if kind == "federated" {
        let federation = context.federation.as_ref()?;
        if federation.get("status").and_then(Value::as_str) != Some("approved")
            || federation.get("localApproved").and_then(Value::as_bool) != Some(true)
            || federation.get("remoteApproved").and_then(Value::as_bool) != Some(true)
        {
            return None;
        }
        let local = federation.get("localMembers").and_then(Value::as_array)?;
        let mut selected = local.iter().filter_map(Value::as_str).collect::<Vec<_>>();
        selected.sort_unstable();
        selected.dedup();
        if selected.is_empty()
            || selected
                .iter()
                .any(|id| active_agent(context, id).is_none())
        {
            return None;
        }
        members = selected.into_iter().map(str::to_owned).collect();
        view.insert("localMembers".to_owned(), json!(members));
        view.insert(
            "name".to_owned(),
            record
                .get("customName")
                .filter(|name| truthy(Some(name)))
                .or_else(|| federation.get("name"))
                .cloned()
                .unwrap_or(Value::Null),
        );
        view.insert(
            "peerLabel".to_owned(),
            federation.get("peerLabel").cloned().unwrap_or(Value::Null),
        );
    } else if kind == "broadcast" {
        let root = record.get("rootId").and_then(Value::as_str)?;
        members = context
            .agents
            .iter()
            .filter(|agent| !truthy(agent.get("deletedAt")))
            .filter(|agent| {
                root == "all" || agent.get("rootId").and_then(Value::as_str) == Some(root)
            })
            .filter_map(|agent| agent.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect();
        if root != "all" && active_agent(context, root).is_none() {
            return None;
        }
        let name = if root == "all" {
            "All agents".to_owned()
        } else {
            format!(
                "{} · Broadcast",
                active_agent(context, root)?.get("name")?.as_str()?
            )
        };
        view.insert(
            "name".to_owned(),
            record
                .get("customName")
                .filter(|name| truthy(Some(name)))
                .cloned()
                .unwrap_or(Value::String(name)),
        );
    } else {
        let stored = record.get("members").and_then(Value::as_array)?;
        members = stored
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .map(str::to_owned)
            .collect();
        if members.is_empty() || members.iter().any(|id| active_agent(context, id).is_none()) {
            return None;
        }
        let name = members
            .iter()
            .filter_map(|id| active_agent(context, id))
            .filter_map(|agent| agent.get("name").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" ↔ ");
        view.insert(
            "name".to_owned(),
            record
                .get("customName")
                .filter(|name| truthy(Some(name)))
                .cloned()
                .unwrap_or(Value::String(name)),
        );
        if kind == "private" && members.len() == 2 {
            if let Some(team) = context.peer_teams.iter().find(|team| {
                team.get("members")
                    .and_then(Value::as_array)
                    .is_some_and(|team_members| {
                        members
                            .iter()
                            .all(|member| team_members.contains(&Value::String(member.clone())))
                    })
            }) {
                view.insert(
                    "peerTeamId".to_owned(),
                    team.get("id").cloned().unwrap_or(Value::Null),
                );
                view.insert(
                    "peerTeamName".to_owned(),
                    team.get("name").cloned().unwrap_or(Value::Null),
                );
            } else {
                view.remove("peerTeamId");
                view.remove("peerTeamName");
            }
        }
    }
    view.insert("members".to_owned(), json!(members));
    view.insert(
        "lastMessage".to_owned(),
        context.last_message.clone().unwrap_or(Value::Null),
    );
    project_persisted_record("room", &Value::Object(view))
}

#[derive(Clone, Debug, Default)]
pub struct ComplaintViewContext {
    pub agents: Vec<Value>,
}

/// Build the viewer-independent renderer view for one stored complaint.
pub fn complaint_entity_view(record: &Value, context: &ComplaintViewContext) -> Option<Value> {
    let id = record.get("id").and_then(Value::as_str)?;
    let author = record.get("author").and_then(Value::as_str);
    let lead_id = record.get("leadId").and_then(Value::as_str);
    let recipient = if record.get("recipient").and_then(Value::as_str) == Some("user") {
        "user"
    } else {
        "lead"
    };
    let responses = record
        .get("responses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|response| response.get("author").and_then(Value::as_str).is_some())
        .collect::<Vec<_>>();
    let status = record.get("status").and_then(Value::as_str);
    let responsible = if recipient == "user" {
        Some("user")
    } else {
        lead_id
    };
    let needs_response = recipient == "user"
        && !matches!(status, Some("resolved" | "declined"))
        && ((record.get("sourceType").and_then(Value::as_str) == Some("user_task")
            && status == Some("open"))
            || !responses
                .iter()
                .any(|response| response.get("author").and_then(Value::as_str) == responsible));
    let agent_name = |key: Option<&str>, fallback: Option<&str>| {
        let agent = key.and_then(|key| {
            context
                .agents
                .iter()
                .find(|agent| agent.get("id").and_then(Value::as_str) == Some(key))
        });
        agent
            .and_then(|agent| agent.get("name"))
            .cloned()
            .or_else(|| fallback.map(|value| Value::String(value.to_owned())))
            .unwrap_or(Value::Null)
    };
    let mut view = Map::new();
    view.insert("id".to_owned(), Value::String(id.to_owned()));
    for field in ["leadId", "author", "status", "created", "updated", "readAt"] {
        view.insert(
            field.to_owned(),
            record.get(field).cloned().unwrap_or(Value::Null),
        );
    }
    view.insert(
        "title".to_owned(),
        Value::String(
            record
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .chars()
                .take(140)
                .collect(),
        ),
    );
    view.insert("recipient".to_owned(), Value::String(recipient.to_owned()));
    view.insert(
        "version".to_owned(),
        record.get("version").cloned().unwrap_or(Value::Null),
    );
    view.insert("needsUserResponse".to_owned(), Value::Bool(needs_response));
    view.insert(
        "authorName".to_owned(),
        agent_name(
            author,
            author.map(|value| if value == "user" { "You" } else { value }),
        ),
    );
    view.insert("leadName".to_owned(), agent_name(lead_id, lead_id));
    project_persisted_record("complaint", &Value::Object(view))
}

/// Build project renderer fields from persisted records and optional live context.
pub fn project_entity_view(
    record: &Value,
    readiness: Option<&Value>,
    sync: Option<&Value>,
    vault: Option<&Value>,
) -> Option<Value> {
    let mut view = record.as_object()?.clone();
    view.insert(
        studio_runtime_schema::agent_progress::START_WAIT_FIELD.into(),
        serde_json::to_value(agent_start_wait(record).ok()?).ok()?,
    );

    if let Some(readiness) = readiness.filter(|value| truthy(Some(value))) {
        view.insert("layrReadiness".to_owned(), readiness.clone());
        if readiness.get("state").and_then(Value::as_str) == Some("ready") {
            if let Some(sync) = sync {
                view.insert("layrSync".to_owned(), sync.clone());
            }
            if let Some(vault) = vault {
                view.insert("vault".to_owned(), vault.clone());
            }
        }
    }
    project_persisted_record("project", &Value::Object(view))
}

#[cfg(test)]
mod projection_plan_tests {
    use super::{invalid_agent_view, openapi_document, schema_projection_properties};
    use serde_json::json;

    #[test]
    fn invalid_agent_view_preserves_valid_root_and_lead_identity() {
        let record = json!({"id":"child","rootId":"lead","epoch":4,"isLead":true});
        let view = invalid_agent_view("child", &record, "invalid field /prompt");
        assert_eq!(view["rootId"], "lead");
        assert_eq!(view["isLead"], true);
        assert_eq!(view["status"], "failed");

        let malformed_root = json!({"id":"child","rootId":17,"epoch":4,"isLead":false});
        let fallback = invalid_agent_view("child", &malformed_root, "invalid field /rootId");
        assert_eq!(fallback["rootId"], "child");
        assert_eq!(fallback["isLead"], false);
    }

    #[test]
    fn schema_projection_plan_reuses_openapi_property_nodes() {
        let document = openapi_document();
        let schema = document
            .pointer("/components/schemas/AgentEntityDto")
            .expect("agent entity schema");
        let property = schema
            .pointer("/properties/id")
            .expect("agent id property schema");
        let plan = schema_projection_properties();
        let properties = plan
            .get(&(schema as *const _ as usize))
            .expect("prepared agent property projection");

        assert!(std::ptr::eq(
            *properties.get("id").expect("id field"),
            property
        ));
        assert!(std::ptr::eq(plan, schema_projection_properties()));
    }
}
