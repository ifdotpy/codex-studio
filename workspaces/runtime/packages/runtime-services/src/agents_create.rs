//! Persisted agent creation and management.
mod operations;
pub use operations::*;

use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, AgentStatus, EventWriteRepository, StoreError,
    SyncDocumentsRepository, Transaction,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RenameError {
    #[error("Unknown managed agent")]
    UnknownAgent,
    #[error("This conversation was deleted")]
    Deleted,
    #[error("This request id has different content")]
    Conflict,
    #[error("A rename request id is required")]
    RequestIdRequired,
    #[error("Not implemented: requires provider-session/title-generation")]
    TitleGenerationUnavailable,
    #[error("Invalid stored object {object}/{id}")]
    InvalidStoredObject { object: &'static str, id: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Debug, Error)]
pub enum CreateError {
    #[error("This creation id has different settings")]
    Conflict,
    #[error("This conversation was deleted")]
    Deleted,
    #[error("Invalid stored object {object}/{id}")]
    InvalidStoredObject { object: &'static str, id: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub fn new_id() -> Result<String, CreateError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| {
        CreateError::Store(StoreError::StateDirectory(std::io::Error::other(
            error.to_string(),
        )))
    })?;
    Ok(uuid::Builder::from_random_bytes(bytes)
        .into_uuid()
        .to_string())
}

const AGENT_ENTITY_FIELDS: &[&str] = &[
    "accountKey",
    "accountTransfer",
    "accountTransferId",
    "activity",
    "agentMode",
    "agentModeRevision",
    "agentModeSupported",
    "archived",
    "autoWake",
    "canSend",
    "capacityRetry",
    "compactions",
    "concurrency",
    "connectionCheck",
    "contextRepairWait",
    "contextUsage",
    "convertedFromLead",
    "created",
    "cwd",
    "cyberAccessProgram",
    "daybreakEnabled",
    "deletedAt",
    "effort",
    "empty",
    "environment",
    "epoch",
    "error",
    "excludedAccountKeys",
    "executionMode",
    "fastMode",
    "hasApproval",
    "hasQuestion",
    "hasUnread",
    "hostProjectPath",
    "id",
    "imageWorkspace",
    "imageWorkspaceBaseRef",
    "imageWorkspaceBaseRepo",
    "imageWorkspaceCreatedAt",
    "imageWorkspaceError",
    "imageWorkspaceHasGit",
    "imageWorkspacePhase",
    "imageWorkspaceReady",
    "imageWorkspaceRepo",
    "imageWorkspaceSubpath",
    "inFlight",
    "isLead",
    "kind",
    "lastAnswer",
    "lastCompletedTurn",
    "lastCompletedTurnStatus",
    "lastEvent",
    "lastReadAt",
    "launcherAlive",
    "layrError",
    "layrLine",
    "layrProjectId",
    "layrReady",
    "layrStateId",
    "manualName",
    "model",
    "moveImportPending",
    "movedFrom",
    "movedTo",
    "name",
    "nativeError",
    "nativeLimitErrorAt",
    "nativeRelease",
    "nativeSafetyBuffering",
    "nativeSafetyRetry",
    "nativeStatus",
    "nativeThreadBlock",
    "nativeTurnError",
    "nextTurnSettingsSupported",
    "orchestratorId",
    "orchestratorName",
    "overview",
    "panelDataVersion",
    "panelVersion",
    "parentId",
    "parkedEvent",
    "pendingSettings",
    "pendingSettingsAccountKey",
    "pinned",
    "project",
    "projectFolder",
    "projectFolderRevision",
    "projectId",
    "projectServerId",
    "provider",
    "queuedSettings",
    "quickCreate",
    "readState",
    "readStateSupported",
    "remoteAnchor",
    "remoteOrigin",
    "remoteWorker",
    "retryAt",
    "reviewDefaults",
    "role",
    "rootId",
    "serverId",
    "sharedRoomId",
    "source",
    "startAttempt",
    "status",
    "statusDetail",
    "subagentConcurrencyVersion",
    "tail",
    "threadId",
    "tokensUsed",
    "turnId",
    "turnStatus",
    "unreadCount",
    "updated",
    "usageResume",
    "voiceState",
    "workerDefaults",
    "workspaceBackend",
    "workspaceMode",
    "workspaceOperation",
    "worktree",
    "worktreePreparation",
    "yoloMode",
];

/// Canonical renderer projection for all agent entity writes in this slice.
pub fn sync_projection(transaction: &Transaction<'_>, record: &Value) -> Result<Value, StoreError> {
    let id = record
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::SchemaMismatch("agent sync projection has no id".to_owned()))?;
    let mut view = record.clone();
    let object = view.as_object_mut().ok_or_else(|| {
        StoreError::SchemaMismatch("agent sync projection is not an object".to_owned())
    })?;
    object.retain(|field, _| AGENT_ENTITY_FIELDS.contains(&field.as_str()));
    let remote_server = record
        .get("remoteWorker")
        .and_then(|value| value.get("server"))
        .cloned()
        .unwrap_or(Value::Null);
    object.insert("serverId".into(), remote_server);
    object.insert("kind".into(), json!("agent"));
    object.insert("source".into(), json!("managed"));
    object.insert(
        "canSend".into(),
        json!(
            !crate::event_write::truthy(record.get("movedTo"))
                && !crate::event_write::truthy(record.get("moveImportPending"))
        ),
    );
    object.insert("launcherAlive".into(), json!(true));
    object.insert("nextTurnSettingsSupported".into(), json!(true));
    object.insert("readStateSupported".into(), json!(true));
    if record.get("isLead").and_then(Value::as_bool) == Some(true) {
        let concurrency = if record
            .get("subagentConcurrencyVersion")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            < 2
        {
            if record.get("agentMode").and_then(Value::as_str) == Some("single") {
                0
            } else {
                record
                    .get("concurrency")
                    .and_then(Value::as_i64)
                    .unwrap_or(32)
            }
        } else {
            record
                .get("concurrency")
                .and_then(Value::as_i64)
                .unwrap_or(32)
        };
        object.insert("concurrency".into(), json!(concurrency));
        object.insert("subagentConcurrencyVersion".into(), json!(2));
        object.insert(
            "agentMode".into(),
            json!(if concurrency == 0 { "single" } else { "multi" }),
        );
        object.insert(
            "agentModeRevision".into(),
            record.get("agentModeRevision").cloned().unwrap_or(json!(0)),
        );
        object.insert("agentModeSupported".into(), json!(true));
        let has_event: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1)",
                [id],
                |row| row.get(0),
            )
            .map_err(StoreError::from)?;
        let has_item: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_items WHERE agent=?1)",
                [id],
                |row| row.get(0),
            )
            .map_err(StoreError::from)?;
        let has_child: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_agents WHERE json_extract(record,'$.parentId')=?1)", [id], |row| row.get(0),
        ).map_err(StoreError::from)?;
        let has_monitor: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_monitors WHERE json_extract(record,'$.agent')=?1)", [id], |row| row.get(0),
        ).map_err(StoreError::from)?;
        object.insert(
            "empty".into(),
            json!(
                !crate::event_write::truthy(record.get("deletedAt"))
                    && !crate::event_write::truthy(record.get("accountTransferId"))
                    && record.get("status").and_then(Value::as_str) == Some("idle")
                    && !crate::event_write::truthy(record.get("threadId"))
                    && record
                        .get("prompt")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .is_empty()
                    && !has_event
                    && !has_item
                    && !has_child
                    && !has_monitor
            ),
        );
    } else {
        object.remove("concurrency");
        let task = record
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let result = if crate::event_write::truthy(record.get("lastCompletedTurn"))
            && !crate::event_write::truthy(record.get("turnId"))
            && !crate::event_write::truthy(record.get("inFlight"))
            && record.get("status").and_then(Value::as_str) == Some("completed")
        {
            record
                .get("lastAnswer")
                .and_then(Value::as_str)
                .unwrap_or_default()
        } else {
            ""
        };
        object.insert("overview".into(), json!({
            "task": task.chars().take(4000).collect::<String>(),
            "taskTruncated": task.chars().count() > 4000,
            "result": result.chars().take(4000).collect::<String>(),
            "resultTruncated": result.chars().count() > 4000,
            "resultTurnId": if result.is_empty() { Value::Null } else { record.get("lastCompletedTurn").cloned().unwrap_or(Value::Null) },
            "resultFile": null
        }));
    }
    Ok(view)
}

pub fn publish_agent(transaction: &Transaction<'_>, record: &Value) -> Result<(), StoreError> {
    let id = record
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::SchemaMismatch("agent sync projection has no id".to_owned()))?;
    let deleted = crate::event_write::truthy(record.get("deletedAt"));
    let projection = sync_projection(transaction, record)?;
    SyncDocumentsRepository::put_entity(transaction, "agent", id, projection, deleted)?;
    Ok(())
}

fn same_intent(saved: &str, requested: &str) -> bool {
    match (
        serde_json::from_str::<Value>(saved),
        serde_json::from_str::<Value>(requested),
    ) {
        (Ok(saved), Ok(requested)) => saved == requested,
        _ => false,
    }
}

pub fn replay_lead(
    transaction: &Connection,
    request_id: &str,
    signature: &str,
    validate: impl Fn(&Value) -> bool,
) -> Result<Option<Value>, CreateError> {
    let alias = transaction
        .query_row(
            "SELECT agent,signature FROM runtime_lead_requests WHERE id=?1",
            [request_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(StoreError::from)?;
    let (agent_id, saved_signature, aliased) = alias.map_or_else(
        || Ok::<_, CreateError>((request_id.to_owned(), String::new(), false)),
        |(agent, signature)| Ok((agent, signature, true)),
    )?;
    let stored = load_agent_value(transaction, &agent_id, &validate)?;
    let Some(record) = stored else {
        return if aliased {
            Err(CreateError::InvalidStoredObject {
                object: "runtime_lead_requests",
                id: request_id.to_owned(),
            })
        } else {
            Ok(None)
        };
    };
    if aliased {
        if !same_intent(&saved_signature, signature) {
            return Err(CreateError::Conflict);
        }
    } else if record.get("quickCreate").and_then(Value::as_bool) != Some(true)
        || !record
            .get("quickCreateRequest")
            .and_then(Value::as_str)
            .is_some_and(|saved| same_intent(saved, signature))
    {
        return Err(CreateError::Conflict);
    }
    if crate::event_write::truthy(record.get("deletedAt")) {
        return Err(CreateError::Deleted);
    }
    Ok(Some(record))
}

pub fn replay_agent(
    transaction: &Connection,
    id: &str,
    request: &Value,
    validate: impl Fn(&Value) -> bool,
) -> Result<Option<Value>, CreateError> {
    let Some(record) = load_agent_value(transaction, id, &validate)? else {
        return Ok(None);
    };
    if crate::event_write::truthy(record.get("deletedAt")) {
        return Err(CreateError::Deleted);
    }
    if record.get("creationIntent") != Some(request) {
        return Err(CreateError::Conflict);
    }
    Ok(Some(record))
}

fn load_agent_value(
    transaction: &Connection,
    id: &str,
    validate: &impl Fn(&Value) -> bool,
) -> Result<Option<Value>, CreateError> {
    let raw = transaction
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(StoreError::from)?;
    let Some(raw) = raw else { return Ok(None) };
    let value: Value =
        serde_json::from_str(&raw).map_err(|_| CreateError::InvalidStoredObject {
            object: "runtime_agents",
            id: id.to_owned(),
        })?;
    if value.get("id").and_then(Value::as_str) != Some(id) || !validate(&value) {
        return Err(CreateError::InvalidStoredObject {
            object: "runtime_agents",
            id: id.to_owned(),
        });
    }
    Ok(Some(value))
}

fn ensure_project(transaction: &Transaction<'_>, agent: &Value) -> Result<(), StoreError> {
    let path = agent
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::SchemaMismatch("new lead has no project path".to_owned()))?;
    let existing = transaction
        .query_row(
            "SELECT record FROM runtime_projects WHERE id=?1",
            [path],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if existing.is_some() {
        return Ok(());
    }
    let account = agent
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(path);
    let project = json!({"id":path,"path":path,"name":name,"created":crate::event_write::unix_time().map_err(|error| StoreError::SchemaMismatch(error.to_string()))?,"accountKey":account,"accountRevision":1});
    transaction.execute(
        "INSERT INTO runtime_projects(id,record) VALUES(?1,?2)",
        rusqlite::params![path, serde_json::to_string(&project)?],
    )?;
    SyncDocumentsRepository::put_entity(transaction, "project", path, project, false)?;
    Ok(())
}

/// Persist a lead and its first pending message atomically. The caller supplies
/// the fully validated, product-shaped record and controls account admission.
pub fn create_agent(
    transaction: &Transaction<'_>,
    id: &str,
    prompt: &str,
    record: Value,
    created: f64,
    validate: impl Fn(&Value) -> bool,
) -> Result<Value, CreateError> {
    let existing = if record.get("quickCreate").and_then(Value::as_bool) == Some(true) {
        replay_lead(
            transaction,
            id,
            record["quickCreateRequest"]
                .as_str()
                .ok_or(CreateError::Conflict)?,
            &validate,
        )?
    } else {
        replay_agent(transaction, id, &record["creationIntent"], &validate)?
    };
    if let Some(existing) = existing {
        return Ok(existing);
    }
    if !validate(&record) {
        return Err(CreateError::InvalidStoredObject {
            object: "runtime_agents",
            id: id.to_owned(),
        });
    }
    let agent: AgentRecord =
        serde_json::from_value(record.clone()).map_err(|_| CreateError::InvalidStoredObject {
            object: "runtime_agents",
            id: id.to_owned(),
        })?;
    let draft = record.get("needsTitle").and_then(Value::as_bool) == Some(true);
    let expected_status = if draft {
        AgentStatus::Idle
    } else {
        AgentStatus::Queued
    };
    if agent.id.0 != id || agent.root_id.0 != id || agent.status != expected_status {
        return Err(CreateError::InvalidStoredObject {
            object: "runtime_agents",
            id: id.to_owned(),
        });
    }
    if record.get("isLead").and_then(Value::as_bool) == Some(true) {
        ensure_project(transaction, &record)?;
    }
    AgentRepository::put(transaction, &agent)?;
    publish_agent(transaction, &record)?;
    if !draft {
        let event_id = format!("{id}:initial");
        let inserted = crate::event_write::enqueue_event(
            transaction,
            &agent,
            event_id,
            "user",
            prompt.to_owned(),
            created,
        )
        .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
        if !inserted {
            return Err(CreateError::Conflict);
        }
    }
    Ok(record)
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("Unknown managed agent")]
    UnknownAgent,
    #[error("Configure the lead agent")]
    LeadRequired,
    #[error("This conversation was deleted")]
    Deleted,
    #[error("This request id has different content")]
    Conflict,
    #[error("The team changed. Reload before saving")]
    Stale,
    #[error("Switch this chat to another account before disabling its current account")]
    CurrentAccountExcluded,
    #[error("Wait for this turn to end before changing execution settings")]
    InFlight,
    #[error("Invalid stored object {object}/{id}")]
    InvalidStoredObject { object: &'static str, id: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub fn update_exclusions(
    transaction: &Transaction<'_>,
    id: &str,
    expected_account: Option<&str>,
    excluded: Option<&[String]>,
    validate: impl Fn(&Value) -> bool,
) -> Result<Value, SettingsError> {
    let mut agent = AgentRepository::get(transaction, &AgentId::from(id))
        .map_err(|_| SettingsError::InvalidStoredObject {
            object: "runtime_agents",
            id: id.to_owned(),
        })?
        .ok_or(SettingsError::UnknownAgent)?;
    let mut record = serde_json::to_value(&agent).map_err(StoreError::from)?;
    if record.get("id").and_then(Value::as_str) != Some(id) {
        return Err(SettingsError::InvalidStoredObject {
            object: "runtime_agents",
            id: id.to_owned(),
        });
    }
    if !validate(&record) {
        return Err(SettingsError::InvalidStoredObject {
            object: "runtime_agents",
            id: id.to_owned(),
        });
    }
    if record
        .get("deletedAt")
        .is_some_and(|value| crate::event_write::truthy(Some(value)))
    {
        return Err(SettingsError::Deleted);
    }
    if expected_account.is_some_and(|expected| {
        record
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            != expected
    }) {
        return Err(SettingsError::Stale);
    }
    if crate::event_write::truthy(record.get("inFlight"))
        || matches!(
            record.get("status").and_then(Value::as_str),
            Some("running" | "starting" | "approval")
        )
    {
        return Err(SettingsError::InFlight);
    }
    let Some(excluded) = excluded else {
        return Ok(record);
    };
    if record.get("isLead").and_then(Value::as_bool) != Some(true) {
        return Err(SettingsError::LeadRequired);
    }
    let account = record
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    if excluded.iter().any(|value| value == account) {
        return Err(SettingsError::CurrentAccountExcluded);
    }
    record["excludedAccountKeys"] = json!(excluded);
    if !validate(&record) {
        return Err(SettingsError::InvalidStoredObject {
            object: "runtime_agents",
            id: id.to_owned(),
        });
    }
    agent =
        serde_json::from_value(record.clone()).map_err(|_| SettingsError::InvalidStoredObject {
            object: "runtime_agents",
            id: id.to_owned(),
        })?;
    AgentRepository::put(transaction, &agent)?;
    publish_agent(transaction, &record)?;
    Ok(record)
}

/// Apply the persisted team storage and token limits without provider calls.
pub fn configure_limits(
    transaction: &Transaction<'_>,
    id: &str,
    max_agents: Option<i64>,
    token_budget: Option<Option<i64>>,
    validate: impl Fn(&Value) -> bool,
) -> Result<Value, SettingsError> {
    let root = AgentRepository::get(transaction, &AgentId::from(id))
        .map_err(|_| SettingsError::InvalidStoredObject {
            object: "runtime_agents",
            id: id.to_owned(),
        })?
        .ok_or(SettingsError::UnknownAgent)?;
    let mut root_value = serde_json::to_value(&root).map_err(StoreError::from)?;
    if root_value
        .get("parentId")
        .is_some_and(|value| !value.is_null())
    {
        return Err(SettingsError::LeadRequired);
    }
    let concurrency = if root_value
        .get("subagentConcurrencyVersion")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        < 2
    {
        if root_value.get("agentMode").and_then(Value::as_str) == Some("single") {
            0
        } else {
            root_value
                .get("concurrency")
                .and_then(Value::as_i64)
                .unwrap_or(32)
        }
    } else {
        root_value
            .get("concurrency")
            .and_then(Value::as_i64)
            .unwrap_or(32)
    };
    root_value["concurrency"] = json!(concurrency);
    root_value["subagentConcurrencyVersion"] = json!(2);
    root_value["agentMode"] = json!(if concurrency == 0 { "single" } else { "multi" });
    root_value["agentModeSupported"] = json!(true);
    if root_value.get("agentModeRevision").is_none() {
        root_value["agentModeRevision"] = json!(0);
    }
    if root_value.get("maxAgentsExplicit").is_none() {
        root_value["maxAgentsExplicit"] = json!(
            root_value
                .get("maxAgents")
                .and_then(Value::as_i64)
                .unwrap_or(64)
                != 64
        );
    }
    let team_max_agents =
        max_agents.or_else(|| root_value.get("maxAgents").and_then(Value::as_i64));
    let team_token_budget =
        token_budget.unwrap_or_else(|| root_value.get("tokenBudget").and_then(Value::as_i64));
    let mut statement = transaction
        .prepare("SELECT id,record FROM runtime_agents")
        .map_err(StoreError::from)?;
    let records = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(StoreError::from)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)?;
    drop(statement);
    for (agent_id, raw) in records {
        let mut record: Value =
            serde_json::from_str(&raw).map_err(|_| SettingsError::InvalidStoredObject {
                object: "runtime_agents",
                id: agent_id.clone(),
            })?;
        if record.get("id").and_then(Value::as_str) != Some(agent_id.as_str()) {
            return Err(SettingsError::InvalidStoredObject {
                object: "runtime_agents",
                id: agent_id.clone(),
            });
        }
        if agent_id != id && record.get("rootId").and_then(Value::as_str) != Some(id) {
            continue;
        }
        if agent_id == id {
            record["concurrency"] = json!(concurrency);
            record["subagentConcurrencyVersion"] = json!(2);
            record["agentMode"] = json!(if concurrency == 0 { "single" } else { "multi" });
            record["agentModeSupported"] = json!(true);
            if record.get("agentModeRevision").is_none() {
                record["agentModeRevision"] = json!(0);
            }
            if record.get("maxAgentsExplicit").is_none() {
                record["maxAgentsExplicit"] = root_value["maxAgentsExplicit"].clone();
            }
            if let Some(max_agents) = max_agents {
                record["maxAgents"] = json!(max_agents);
                record["maxAgentsExplicit"] = json!(true);
            }
            if let Some(token_budget) = token_budget {
                record["tokenBudget"] = token_budget.map_or(Value::Null, |value| json!(value));
            }
        } else {
            record
                .as_object_mut()
                .ok_or(SettingsError::InvalidStoredObject {
                    object: "runtime_agents",
                    id: agent_id.clone(),
                })?
                .remove("concurrency");
            if let Some(max_agents) = team_max_agents {
                record["maxAgents"] = json!(max_agents);
            }
            record["tokenBudget"] = team_token_budget.map_or(Value::Null, |value| json!(value));
        }
        if !validate(&record) {
            return Err(SettingsError::InvalidStoredObject {
                object: "runtime_agents",
                id: agent_id.clone(),
            });
        }
        let stored: AgentRecord = serde_json::from_value(record.clone()).map_err(|_| {
            SettingsError::InvalidStoredObject {
                object: "runtime_agents",
                id: agent_id.clone(),
            }
        })?;
        AgentRepository::put(transaction, &stored)?;
        publish_agent(transaction, &record)?;
        if agent_id == id {
            root_value = serde_json::to_value(stored).map_err(StoreError::from)?;
        }
    }
    Ok(json!({
        "id": id,
        "concurrency": root_value.get("concurrency").cloned().unwrap_or(Value::Null),
        "agentMode": if root_value.get("concurrency").and_then(Value::as_i64).unwrap_or(0) == 0 { "single" } else { "multi" },
        "agentModeRevision": root_value.get("agentModeRevision").cloned().unwrap_or(json!(0)),
        "maxAgents": root_value.get("maxAgents").cloned().unwrap_or(Value::Null),
        "tokenBudget": root_value.get("tokenBudget").cloned().unwrap_or(Value::Null),
    }))
}

pub fn rename(
    transaction: &Transaction<'_>,
    id: &str,
    name: Option<&str>,
    request_id: Option<&str>,
    signature: &str,
) -> Result<Value, RenameError> {
    if let Some(request_id) = request_id
        && let Some(receipt) = EventWriteRepository::receipt(transaction, request_id)?
    {
        if receipt.signature != signature {
            return Err(RenameError::Conflict);
        }
        let result: Value = serde_json::from_str(&receipt.result).map_err(|_| {
            RenameError::InvalidStoredObject {
                object: "runtime_operation_receipts",
                id: request_id.to_owned(),
            }
        })?;
        return Ok(result);
    }

    let Some(mut agent) = AgentRepository::get(transaction, &AgentId::from(id)).map_err(|_| {
        RenameError::InvalidStoredObject {
            object: "runtime_agents",
            id: id.to_owned(),
        }
    })?
    else {
        return Err(RenameError::UnknownAgent);
    };
    if agent.id.0 != id {
        return Err(RenameError::InvalidStoredObject {
            object: "runtime_agents",
            id: id.to_owned(),
        });
    }
    if crate::event_write::truthy(agent.extra.get("deletedAt")) {
        return Err(RenameError::Deleted);
    }
    let Some(name) = name else {
        return Err(if request_id.is_some() {
            RenameError::TitleGenerationUnavailable
        } else {
            RenameError::RequestIdRequired
        });
    };

    agent
        .extra
        .insert("name".to_owned(), Value::String(name.to_owned()));
    agent
        .extra
        .insert("manualName".to_owned(), Value::Bool(true));
    agent
        .extra
        .insert("needsTitle".to_owned(), Value::Bool(false));
    AgentRepository::put(transaction, &agent)?;
    let record = serde_json::to_value(&agent).map_err(StoreError::from)?;
    publish_agent(transaction, &record)?;

    let mut result = json!({"id": id, "name": name});
    if let Some(request_id) = request_id {
        result["request_id"] = Value::String(request_id.to_owned());
        result["status"] = Value::String("applied".to_owned());
        let result_text = serde_json::to_string(&result).map_err(StoreError::from)?;
        EventWriteRepository::put_receipt(transaction, request_id, signature, &result_text)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::create_agent;
    use rusqlite::Connection;
    use serde_json::json;

    fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,created REAL,epoch INTEGER,turn_id TEXT,error TEXT);
                 CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_lead_requests(id TEXT PRIMARY KEY,agent TEXT NOT NULL,signature TEXT NOT NULL);
                 CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT);
                 CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,PRIMARY KEY(collection,id));
                 CREATE TABLE sync_documents(seq INTEGER,scope TEXT,id TEXT,payload TEXT,deleted INTEGER);
                 CREATE TABLE sync_versions(seq INTEGER,scope TEXT,hash TEXT,deleted INTEGER,updated REAL);",
            )
            .expect("agent-create tables exist");
        connection
    }

    #[test]
    fn canonical_agent_replay_checks_every_saved_value_after_rename() {
        let connection = database();
        let intent = json!({"prompt":"Task", "name":"Original", "cwd":"/project", "role":"orchestrator", "concurrency":32, "maxAgents":64, "tokenBudget":null, "parent":null, "profile_id":null});
        let record = json!({"id":"agent", "name":"Renamed", "creationIntent":intent});
        connection
            .execute(
                "INSERT INTO runtime_agents VALUES(?1,?2)",
                rusqlite::params!["agent", record.to_string()],
            )
            .expect("insert");
        assert!(
            super::replay_agent(&connection, "agent", &intent, |_| true)
                .expect("replay")
                .is_some()
        );
        for field in intent.as_object().expect("intent object").keys() {
            let mut changed = intent.clone();
            changed[field] = json!("changed");
            assert!(
                super::replay_agent(&connection, "agent", &changed, |_| true).is_err(),
                "{field}"
            );
        }
        let mut omitted = intent.clone();
        omitted.as_object_mut().expect("object").remove("maxAgents");
        assert!(super::replay_agent(&connection, "agent", &omitted, |_| true).is_err());
    }

    #[test]
    fn missing_alias_target_is_refused_by_preliminary_replay() {
        let connection = database();
        connection
            .execute(
                "INSERT INTO runtime_lead_requests VALUES('alias','missing','{}')",
                [],
            )
            .expect("alias");
        assert!(matches!(
            super::replay_lead(&connection, "alias", "{}", |_| true),
            Err(super::CreateError::InvalidStoredObject {
                object: "runtime_lead_requests",
                ..
            })
        ));
    }

    #[test]
    fn agent_creation_is_atomic_replayable_and_publishes_sync_projection() {
        let mut connection = database();
        let tx = connection.transaction().expect("transaction starts");
        let record = json!({
            "id":"agent-a", "rootId":"agent-a", "epoch":0, "status":"queued",
            "name":"Agent A", "prompt":"Start the task", "cwd":"/project",
            "accountKey":"default", "isLead":true, "needsTitle":false, "autoWake":true,
            "creationIntent":{"name":"Agent A","prompt":"Start the task"}
        });
        let created = create_agent(
            &tx,
            "agent-a",
            "Start the task",
            record.clone(),
            10.0,
            |_| true,
        )
        .expect("record and initial event persist");
        assert_eq!(created["id"], "agent-a");
        tx.commit().expect("transaction commits");

        let event_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM runtime_events", [], |row| row.get(0))
            .expect("event count reads");
        assert_eq!(event_count, 1);
        let (kind, status, text): (String, String, String) = connection
            .query_row(
                "SELECT kind,status,text FROM runtime_events WHERE id='agent-a:initial'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("initial event exists");
        assert_eq!(
            (kind.as_str(), status.as_str(), text.as_str()),
            ("user", "pending", "Start the task")
        );
        let projection: String = connection
            .query_row(
                "SELECT payload FROM sync_entities WHERE collection='agent' AND id='agent-a'",
                [],
                |row| row.get(0),
            )
            .expect("agent projection exists");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&projection).expect("projection JSON")["value"]
                ["id"],
            "agent-a"
        );

        let tx = connection.transaction().expect("replay transaction starts");
        create_agent(
            &tx,
            "agent-a",
            "Start the task",
            record.clone(),
            99.0,
            |_| true,
        )
        .expect("same content replays");
        tx.commit().expect("replay transaction commits");
        let event_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM runtime_events", [], |row| row.get(0))
            .expect("event count reads after replay");
        assert_eq!(event_count, 1);

        let before = connection
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='agent-a'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("created agent remains");
        let tx = connection
            .transaction()
            .expect("conflict transaction starts");
        let mut changed = record;
        changed["creationIntent"]["name"] = json!("Changed");
        assert!(create_agent(&tx, "agent-a", "Start the task", changed, 100.0, |_| true).is_err());
        drop(tx);
        let after = connection
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='agent-a'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("conflict does not change agent");
        assert_eq!(after, before);
    }
}
