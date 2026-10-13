//! Pure sync document, cursor and preference decisions.

use rusqlite::{OptionalExtension, Transaction};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};
use studio_runtime_store::{
    CoordinationRepository, RuntimeStore, StoreError, StoreResult, SyncDocumentsRepository,
};
use thiserror::Error;

pub const ENTITY_PAGE_LIMIT: i64 = 500;
pub const OTHER_PAGE_LIMIT: i64 = 100;
const PREFERENCE_FIELD_CAP: usize = 3000;
const MAX_CLOCK_SKEW_MS: i64 = 5 * 60 * 1000;
const MONITOR_TERMINAL_WINDOW: usize = 100;
const MONITOR_FINAL_WINDOW: usize = 100;
const TASK_RETIRE_BATCH: usize = 100;

/// Rebuild renderer entity projections from persisted records in the caller's
/// transaction. Re-running this operation preserves unchanged entity
/// sequences because the repository compares canonical payloads before write.
pub fn maintain_entity_projections(
    transaction: &Transaction<'_>,
    state_dir: &str,
) -> StoreResult<()> {
    for (table, collection) in [
        ("runtime_agents", "agent"),
        ("runtime_rooms", "room"),
        ("runtime_complaints", "complaint"),
        ("runtime_projects", "project"),
        ("runtime_requests", "request"),
        ("runtime_rules", "rule"),
        ("runtime_work", "work"),
    ] {
        for (id, raw) in stored_records(transaction, table)? {
            let Ok(record) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            let Some(object) = record.as_object() else {
                continue;
            };
            if collection == "agent" && object.get("deletedAt").is_some_and(|v| !v.is_null()) {
                retire_if_live(transaction, collection, &id)?;
                continue;
            }
            if collection == "request"
                && object.get("status").and_then(Value::as_str) != Some("pending")
            {
                retire_if_live(transaction, collection, &id)?;
                continue;
            }
            if let Some(projected) = project_persisted_record(collection, &record) {
                SyncDocumentsRepository::put_entity(
                    transaction,
                    collection,
                    &id,
                    projected,
                    false,
                )?;
            }
        }
    }

    maintain_task_window(transaction)?;
    maintain_monitor_window(transaction)?;
    maintain_event_window(transaction)?;
    maintain_peer_team_entities(transaction)?;
    maintain_canvas_chat_entities(transaction)?;

    // Python creates a workspace projection on first seed even though its
    // value is mostly defaults. Preserve an existing workspace view and add
    // only the stable persisted-state defaults available to this service.
    let prior = transaction
        .query_row(
            "SELECT payload FROM sync_entities WHERE collection='workspace' AND id='current' AND deleted=0",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?;
    let mut workspace = prior
        .and_then(|payload| payload)
        .and_then(|payload| serde_json::from_str::<Value>(&payload).ok())
        .and_then(|payload| payload.get("value").and_then(Value::as_object).cloned())
        .unwrap_or_default();
    workspace.insert("stateDir".to_owned(), Value::String(state_dir.to_owned()));
    workspace.entry("connected").or_insert(Value::Bool(false));
    workspace
        .entry("projectOrganizationVersion")
        .or_insert(json!(1));
    workspace.entry("peerTeamsVersion").or_insert(json!(1));
    workspace.entry("tasksHistoryLimit").or_insert(json!(100));
    SyncDocumentsRepository::put_entity(
        transaction,
        "workspace",
        "current",
        Value::Object(workspace),
        false,
    )?;
    Ok(())
}

/// Filter and bound a persisted record using the committed renderer DTO
/// property list. This is deliberately a pure records-in, projection-out step.
pub fn project_persisted_record(collection: &str, record: &Value) -> Option<Value> {
    let id = record.get("id")?.as_str()?;
    if id.is_empty() {
        return None;
    }
    let schema_name = match collection {
        "agent" => "AgentEntityDto",
        "room" => "RoomEntityDto",
        "task" => "TaskEntityDto",
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
    let fields = dto_fields(schema_name)?;
    let source = record.as_object()?;
    let mut projected = Map::new();
    for (name, value) in source {
        if fields.contains(name.as_str())
            && !(collection == "agent"
                && AGENT_REPROJECT_FIELDS.contains(&name.as_str())
                && value.is_object())
        {
            projected.insert(name.clone(), bound_sync_value(value, name));
        }
    }
    if collection == "agent" {
        for (field, allowed) in AGENT_FIELD_ALLOWLISTS {
            if let Some(object) = source.get(*field).and_then(Value::as_object) {
                let nested = object
                    .iter()
                    .filter(|(name, _)| allowed.contains(&name.as_str()))
                    .map(|(name, value)| (name.clone(), bound_sync_value(value, "error")))
                    .collect::<Map<_, _>>();
                projected.insert((*field).to_owned(), Value::Object(nested));
            }
        }
        if let Some(release) = source.get("nativeRelease").and_then(Value::as_object) {
            projected.insert(
                "nativeRelease".to_owned(),
                Value::Object(
                    release
                        .iter()
                        .filter(|(name, _)| matches!(name.as_str(), "phase" | "resetPending"))
                        .map(|(name, value)| (name.clone(), value.clone()))
                        .collect(),
                ),
            );
        }
        if let Some(overview) = source.get("overview").and_then(Value::as_object) {
            projected.insert(
                "overview".to_owned(),
                Value::Object(
                    overview
                        .iter()
                        .filter(|(name, _)| {
                            matches!(
                                name.as_str(),
                                "task"
                                    | "taskTruncated"
                                    | "result"
                                    | "resultTruncated"
                                    | "resultTurnId"
                                    | "resultFile"
                            )
                        })
                        .map(|(name, value)| (name.clone(), bound_sync_value(value, name)))
                        .collect(),
                ),
            );
        }
    }
    Some(Value::Object(projected))
}

const AGENT_REPROJECT_FIELDS: &[&str] = &[
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
    "nativeRelease",
    "overview",
];
const AGENT_FIELD_ALLOWLISTS: &[(&str, &[&str])] = &[
    ("activity", &["phase", "at", "tools"]),
    (
        "nativeStatus",
        &["phase", "error", "message", "turnId", "at"],
    ),
    (
        "nativeSafetyBuffering",
        &[
            "turnId",
            "threadId",
            "accountKey",
            "connectionId",
            "at",
            "dismissed",
            "responseStarted",
            "showBufferingUi",
            "fasterModel",
        ],
    ),
    (
        "nativeSafetyRetry",
        &[
            "id",
            "stage",
            "model",
            "turnId",
            "created",
            "updated",
            "epoch",
            "accountKey",
            "error",
            "newThreadId",
            "acceptedTurnId",
            "requestId",
            "rpcMethod",
        ],
    ),
    ("nativeTurnError", &["turnId", "error"]),
    (
        "connectionCheck",
        &[
            "epoch",
            "accountKey",
            "threadId",
            "turnId",
            "at",
            "previousError",
            "nativeState",
            "restartTurnStatus",
            "readError",
        ],
    ),
    ("readState", &["threadId", "turnId", "read", "revision"]),
    (
        "startAttempt",
        &["prepareError", "responseError", "retiredEvents"],
    ),
    (
        "capacityRetry",
        &[
            "id",
            "threadId",
            "epoch",
            "accountKey",
            "status",
            "acceptedTurnId",
            "updatedAt",
            "dueAt",
            "claimedAt",
            "reason",
        ],
    ),
    (
        "usageResume",
        &[
            "id",
            "status",
            "cause",
            "reason",
            "updatedAt",
            "plannedAt",
            "dueAt",
        ],
    ),
    ("contextRepairWait", &["scope", "error"]),
];

fn dto_fields(name: &str) -> Option<&'static BTreeSet<String>> {
    static SCHEMAS: OnceLock<Value> = OnceLock::new();
    static FIELDS: OnceLock<BTreeMap<String, BTreeSet<String>>> = OnceLock::new();
    let schemas = SCHEMAS.get_or_init(|| {
        serde_json::from_str(include_str!("../../runtime-http/openapi.json"))
            .expect("the checked-in OpenAPI document is valid JSON")
    });
    let fields = FIELDS.get_or_init(|| {
        [
            "AgentEntityDto",
            "RoomEntityDto",
            "TaskEntityDto",
            "MonitorEntityDto",
            "ComplaintEntityDto",
            "RequestEntityDto",
            "RuleEntityDto",
            "ProjectEntityDto",
            "EventEntityDto",
            "WorkEntityDto",
            "WorkspaceEntityDto",
            "ChatEntityDto",
            "EdgeEntityDto",
            "PeerTeamEntityDto",
        ]
        .into_iter()
        .filter_map(|name| {
            let properties = schemas
                .pointer(&format!("/components/schemas/{name}/properties"))?
                .as_object()?;
            Some((name.to_owned(), properties.keys().cloned().collect()))
        })
        .collect()
    });
    fields.get(name)
}

fn bound_sync_value(value: &Value, field: &str) -> Value {
    match value {
        Value::String(text) => {
            let limit = match field {
                "error" | "tail" | "description" | "command" | "query" => 2000,
                "overview" => 9000,
                "text" | "lastAnswer" => 4000,
                _ => 12000,
            };
            Value::String(text.chars().take(limit).collect())
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(if field == "sidebarOrder" { 10_000 } else { 200 })
                .map(|item| bound_sync_value(item, ""))
                .collect(),
        ),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(name, value)| (name.clone(), bound_sync_value(value, name)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn table_exists(transaction: &Transaction<'_>, name: &str) -> StoreResult<bool> {
    Ok(transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |row| row.get(0),
    )?)
}

fn stored_records(
    transaction: &Transaction<'_>,
    table: &str,
) -> StoreResult<Vec<(String, String)>> {
    if !table_exists(transaction, table)? {
        return Ok(Vec::new());
    }
    let mut statement = transaction.prepare(&format!("SELECT id,record FROM {table}"))?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)
}

fn retire_if_live(transaction: &Transaction<'_>, collection: &str, id: &str) -> StoreResult<()> {
    let live: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sync_entities WHERE collection=?1 AND id=?2 AND deleted=0)",
        [collection, id],
        |row| row.get(0),
    )?;
    if live {
        SyncDocumentsRepository::put_entity(transaction, collection, id, json!({}), true)?;
    }
    Ok(())
}

fn maintain_task_window(transaction: &Transaction<'_>) -> StoreResult<()> {
    if !table_exists(transaction, "runtime_tasks")? || !table_exists(transaction, "runtime_agents")?
    {
        return Ok(());
    }
    let mut rows = transaction.prepare(
        "SELECT t.id,t.record FROM runtime_tasks t JOIN runtime_agents a \
         ON json_extract(t.record,'$.agent')=a.id \
         WHERE json_extract(a.record,'$.deletedAt') IS NULL \
         AND json_extract(t.record,'$.status')='running' \
         UNION ALL SELECT id,record FROM (SELECT t.id,t.record FROM runtime_tasks t \
         JOIN runtime_agents a ON json_extract(t.record,'$.agent')=a.id \
         WHERE json_extract(a.record,'$.deletedAt') IS NULL \
         AND json_extract(t.record,'$.status')!='running' \
         ORDER BY json_extract(t.record,'$.created') DESC LIMIT 100)",
    )?;
    let eligible = rows
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(rows);
    let mut kept = BTreeSet::new();
    for (id, raw) in eligible {
        let Ok(record) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if let Some(projected) = project_persisted_record("task", &record) {
            kept.insert(id.clone());
            SyncDocumentsRepository::put_entity(transaction, "task", &id, projected, false)?;
        }
    }
    let mut statement = transaction
        .prepare("SELECT id FROM sync_entities WHERE collection='task' AND deleted=0")?;
    let candidates = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let stale = candidates
        .into_iter()
        .filter(|id| !kept.contains(id))
        .collect::<Vec<_>>();
    for id in stale.iter().take(TASK_RETIRE_BATCH) {
        SyncDocumentsRepository::put_entity(transaction, "task", id, json!({}), true)?;
    }
    if stale.is_empty() {
        transaction.execute(
            "INSERT INTO sync_entity_meta(key,value) VALUES('task_window_migrated','1') \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [],
        )?;
    }
    Ok(())
}

fn maintain_monitor_window(transaction: &Transaction<'_>) -> StoreResult<()> {
    if !table_exists(transaction, "runtime_monitors")? {
        return Ok(());
    }
    let agents = stored_records(transaction, "runtime_agents")?
        .into_iter()
        .filter_map(|(id, raw)| {
            let record = serde_json::from_str::<Value>(&raw).ok()?;
            (record.get("deletedAt").is_none_or(Value::is_null)).then_some(id)
        })
        .collect::<BTreeSet<_>>();
    let records = stored_records(transaction, "runtime_monitors")?
        .into_iter()
        .filter_map(|(_, raw)| serde_json::from_str::<Value>(&raw).ok())
        .filter(|record| {
            record
                .get("agent")
                .and_then(Value::as_str)
                .is_some_and(|id| agents.contains(id))
        })
        .collect::<Vec<_>>();
    let mut kept = BTreeMap::<String, Value>::new();
    for status in ["running", "starting", "approval"] {
        for record in records
            .iter()
            .filter(|record| record.get("status").and_then(Value::as_str) == Some(status))
        {
            if let Some(id) = record.get("id").and_then(Value::as_str) {
                kept.insert(id.to_owned(), record.clone());
            }
        }
    }
    let mut terminals = Vec::new();
    for status in ["completed", "failed", "cancelled", "lost"] {
        let mut status_rows = records
            .iter()
            .filter(|record| record.get("status").and_then(Value::as_str) == Some(status))
            .cloned()
            .collect::<Vec<_>>();
        status_rows.sort_by(compare_created_desc);
        terminals.extend(status_rows.into_iter().take(MONITOR_TERMINAL_WINDOW));
    }
    terminals.sort_by(compare_created_desc);
    for record in terminals.into_iter().take(MONITOR_FINAL_WINDOW) {
        if let Some(id) = record.get("id").and_then(Value::as_str) {
            kept.insert(id.to_owned(), record);
        }
    }
    let mut kept_ids = BTreeSet::new();
    for (id, record) in kept {
        if let Some(projected) = project_persisted_record("monitor", &record) {
            kept_ids.insert(id.clone());
            SyncDocumentsRepository::put_entity(transaction, "monitor", &id, projected, false)?;
        }
    }
    retire_unkept(transaction, "monitor", &kept_ids, None)
}

fn maintain_event_window(transaction: &Transaction<'_>) -> StoreResult<()> {
    if !table_exists(transaction, "runtime_events")? {
        return Ok(());
    }
    let mut statement = transaction.prepare(
        "SELECT id,agent,kind,status,created,error FROM runtime_events ORDER BY created DESC,id LIMIT 200",
    )?;
    let events = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                json!({"id":row.get::<_, String>(0)?,"agent":row.get::<_, Option<String>>(1)?,
                    "kind":row.get::<_, Option<String>>(2)?,"status":row.get::<_, Option<String>>(3)?,
                    "created":row.get::<_, Option<f64>>(4)?,"error":row.get::<_, Option<String>>(5)?}),
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut kept = BTreeSet::new();
    for (id, record) in events {
        if let Some(projected) = project_persisted_record("event", &record) {
            kept.insert(id.clone());
            SyncDocumentsRepository::put_entity(transaction, "event", &id, projected, false)?;
        }
    }
    retire_unkept(transaction, "event", &kept, Some("event_window_seq"))
}

fn compare_created_desc(left: &Value, right: &Value) -> std::cmp::Ordering {
    let left = left
        .get("created")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let right = right
        .get("created")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    right.total_cmp(&left)
}

fn maintain_peer_team_entities(transaction: &Transaction<'_>) -> StoreResult<()> {
    if !table_exists(transaction, "runtime_projects")?
        || !table_exists(transaction, "runtime_agents")?
    {
        return Ok(());
    }
    let agents = stored_records(transaction, "runtime_agents")?
        .into_iter()
        .filter_map(|(_, raw)| serde_json::from_str::<Value>(&raw).ok())
        .filter_map(|agent| {
            let id = agent.get("id")?.as_str()?.to_owned();
            let live = agent.get("deletedAt").is_none_or(Value::is_null);
            let lead = agent.get("isLead").and_then(Value::as_bool) == Some(true)
                && agent.get("parentId").is_none_or(Value::is_null)
                && agent.get("rootId").and_then(Value::as_str) == Some(id.as_str());
            (live && lead).then_some((id, agent))
        })
        .collect::<BTreeMap<_, _>>();
    let mut statement = transaction.prepare("SELECT record FROM runtime_projects")?;
    let projects = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut current = BTreeMap::<String, Value>::new();
    for raw in projects {
        let Ok(project) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let Some(project_path) = project
            .get("path")
            .and_then(Value::as_str)
            .filter(|path| !path.trim().is_empty())
        else {
            continue;
        };
        let Some(teams) = project.get("peerTeams").and_then(Value::as_array) else {
            continue;
        };
        let revision = project
            .get("peerTeamsRevision")
            .cloned()
            .unwrap_or(json!(0));
        for team in teams {
            let (Some(id), Some(name), Some(members)) = (
                team.get("id").and_then(Value::as_str),
                team.get("name").and_then(Value::as_str),
                team.get("members").and_then(Value::as_array),
            ) else {
                continue;
            };
            let mut selected = Vec::new();
            for member in members.iter().filter_map(Value::as_str) {
                if agents.get(member).is_some_and(|agent| {
                    agent.get("cwd").and_then(Value::as_str) == Some(project_path)
                }) && !selected.iter().any(|prior| prior == member)
                {
                    selected.push(member.to_owned());
                }
            }
            if selected.len() >= 2 {
                current.insert(
                    id.to_owned(),
                    json!({"id":id,"name":name,"projectPath":project_path,"members":selected,"revision":revision}),
                );
            }
        }
    }
    let mut kept = BTreeSet::new();
    for (id, record) in current {
        if let Some(projected) = project_persisted_record("peerTeam", &record) {
            kept.insert(id.clone());
            SyncDocumentsRepository::put_entity(transaction, "peerTeam", &id, projected, false)?;
        }
    }
    retire_unkept(transaction, "peerTeam", &kept, None)
}

fn retire_unkept(
    transaction: &Transaction<'_>,
    collection: &str,
    kept: &BTreeSet<String>,
    marker: Option<&str>,
) -> StoreResult<()> {
    let mut statement =
        transaction.prepare("SELECT id FROM sync_entities WHERE collection=?1 AND deleted=0")?;
    let stale = statement
        .query_map([collection], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for id in stale {
        if !kept.contains(&id) {
            SyncDocumentsRepository::put_entity(transaction, collection, &id, json!({}), true)?;
        }
    }
    if let Some(marker) = marker {
        let sequence = transaction.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM sync_entities WHERE collection=?1",
            [collection],
            |row| row.get::<_, i64>(0),
        )?;
        transaction.execute(
            "INSERT INTO sync_entity_meta(key,value) VALUES(?1,?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            rusqlite::params![marker, sequence.to_string()],
        )?;
    }
    Ok(())
}

fn maintain_canvas_chat_entities(transaction: &Transaction<'_>) -> StoreResult<()> {
    if !table_exists(transaction, "groups")? {
        return Ok(());
    }
    let mut statement = transaction.prepare("SELECT id FROM groups ORDER BY id")?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut kept = BTreeSet::new();
    for id in ids {
        CoordinationRepository::chat_projection(transaction, &id, false)?;
        kept.insert(id);
    }
    retire_unkept(transaction, "chat", &kept, None)?;

    if !table_exists(transaction, "graph_edges")? {
        return Ok(());
    }
    let mut statement = transaction
        .prepare("SELECT id,source,target FROM graph_edges WHERE kind='chat' ORDER BY id")?;
    let edges = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut kept_edges = BTreeSet::new();
    for (id, source, target) in edges {
        CoordinationRepository::edge_projection(transaction, &id, &source, &target, false)?;
        kept_edges.insert(id);
    }
    retire_unkept(transaction, "edge", &kept_edges, None)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreferenceScope {
    User,
    Server,
}

#[derive(Debug, Error)]
pub enum SyncPolicyError {
    #[error("invalid draft")]
    InvalidDraft,
    #[error("draft identity does not match its key")]
    DraftIdentity,
    #[error("invalid preference fields")]
    InvalidPreferences,
    #[error("invalid stored sync object {scope}/{id}")]
    InvalidStoredObject { scope: String, id: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub fn effective_limit(scope: &str, limit: i64) -> i64 {
    limit.clamp(
        1,
        if scope == "state:entities:v1" {
            ENTITY_PAGE_LIMIT
        } else {
            OTHER_PAGE_LIMIT
        },
    )
}

pub fn validate_draft(id: &str, payload: &Value) -> Result<(), SyncPolicyError> {
    let object = payload.as_object().ok_or(SyncPolicyError::InvalidDraft)?;
    let device = object
        .get("device")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(SyncPolicyError::DraftIdentity)?;
    let session = object
        .get("session")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(SyncPolicyError::DraftIdentity)?;
    validate_draft_identity(id, object, device, session)
}

pub fn validate_stored_draft(id: &str, payload: &Value) -> Result<(), SyncPolicyError> {
    let object = payload.as_object().ok_or(SyncPolicyError::InvalidDraft)?;
    let device = object
        .get("device")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(SyncPolicyError::DraftIdentity)?;
    let session = object
        .get("session")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(SyncPolicyError::DraftIdentity)?;
    validate_draft_identity(id, object, device, session)
}

fn validate_draft_identity(
    id: &str,
    object: &Map<String, Value>,
    device: &str,
    session: &str,
) -> Result<(), SyncPolicyError> {
    let prefix = format!("{device}:");
    let suffix = format!(":{session}");
    let writer = id
        .strip_prefix(&prefix)
        .and_then(|tail| tail.strip_suffix(&suffix))
        .unwrap_or_default();
    let identity_matches =
        id == format!("{device}:{session}") || (!writer.is_empty() && !writer.contains(':'));
    if !identity_matches
        || object
            .get("id")
            .is_some_and(|value| !value.is_null() && value.as_str() != Some(id))
    {
        return Err(SyncPolicyError::DraftIdentity);
    }
    Ok(())
}

pub fn parse_preferences(
    value: &Value,
    scope: PreferenceScope,
) -> Result<Map<String, Value>, SyncPolicyError> {
    let object = value
        .as_object()
        .ok_or(SyncPolicyError::InvalidPreferences)?;
    let mut fields = Map::new();
    for (name, field) in object {
        validate_preference(name, field, scope)?;
        fields.insert(name.clone(), field.clone());
    }
    Ok(fields)
}

pub fn merge_preferences(
    current: &Map<String, Value>,
    incoming: &Map<String, Value>,
    now_ms: i64,
) -> Result<Map<String, Value>, SyncPolicyError> {
    let mut fields = current.clone();
    for (key, value) in incoming {
        let mut candidate = value.clone();
        let timestamp = candidate
            .get("timestamp")
            .and_then(Value::as_i64)
            .ok_or(SyncPolicyError::InvalidPreferences)?;
        if timestamp > now_ms.saturating_add(MAX_CLOCK_SKEW_MS) {
            candidate["timestamp"] = json!(now_ms);
        }
        let replace = fields
            .get(key)
            .is_none_or(|prior| preference_rank(&candidate) > preference_rank(prior));
        if replace {
            fields.insert(key.clone(), candidate);
        }
    }
    prune_preferences(&mut fields);
    Ok(fields)
}

fn validate_preference(
    name: &str,
    field: &Value,
    scope: PreferenceScope,
) -> Result<(), SyncPolicyError> {
    if name.chars().count() > 2048 {
        return Err(SyncPolicyError::InvalidPreferences);
    }
    let parts: Vec<String> =
        serde_json::from_str(name).map_err(|_| SyncPolicyError::InvalidPreferences)?;
    if !matches!(parts.len(), 1 | 2) || canonical_json(&json!(parts)) != name {
        return Err(SyncPolicyError::InvalidPreferences);
    }
    let key = &parts[0];
    let value = field.get("value").unwrap_or(&Value::Null);

    let appearance = key == "codex-studio-preferences-v1";
    let visual = key.starts_with("codex-project-tree:")
        || key.starts_with("codex-project-compact:")
        || key.starts_with("codex-progress-hidden:")
        || key.starts_with("studio-prompt-bookmarks:")
        || matches!(
            key.as_str(),
            "codex-worker-disclosures"
                | "studio-logical-project-collapsed"
                | "studio-logical-project-compact"
                | "studio-server-project-compact-v1"
        );
    let shell = key == "server-order"
        || key.starts_with("server-alias:")
        || key.starts_with("server-label:");
    if !appearance && !visual && !shell {
        return Err(SyncPolicyError::InvalidPreferences);
    }
    let expected_user_scope = appearance
        || key.starts_with("server-")
        || key.starts_with("studio-logical-")
        || key == "studio-server-project-compact-v1";
    if expected_user_scope != matches!(scope, PreferenceScope::User) {
        return Err(SyncPolicyError::InvalidPreferences);
    }

    if appearance {
        if parts.len() != 2 || !valid_appearance(&parts[1], value) {
            return Err(SyncPolicyError::InvalidPreferences);
        }
    } else if visual {
        if !value.is_null() && !value.is_boolean() {
            return Err(SyncPolicyError::InvalidPreferences);
        }
    } else if key == "server-order" {
        if !value.as_array().is_some_and(|items| {
            items.len() <= 256
                && items
                    .iter()
                    .all(|item| item.as_str().is_some_and(|s| s.chars().count() <= 256))
        }) {
            return Err(SyncPolicyError::InvalidPreferences);
        }
    } else if key.starts_with("server-alias:") {
        let Some(label) = value.as_str() else {
            return Err(SyncPolicyError::InvalidPreferences);
        };
        if label.is_empty()
            || label.len() > 3
            || !label.is_ascii()
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphabetic() && byte.is_ascii_uppercase())
        {
            return Err(SyncPolicyError::InvalidPreferences);
        }
    } else if !value.as_str().is_some_and(|label| {
        !crate::event_write::python_strip(label).is_empty() && label.chars().count() <= 128
    }) {
        return Err(SyncPolicyError::InvalidPreferences);
    }
    Ok(())
}

fn valid_appearance(name: &str, value: &Value) -> bool {
    match name {
        "theme" => matches!(value.as_str(), Some("auto" | "light" | "dark")),
        "typography" | "contentLayout" => matches!(value.as_str(), Some("original" | "custom")),
        "fontFamily" => matches!(
            value.as_str(),
            Some("system" | "arial" | "inter" | "georgia" | "verdana")
        ),
        "sidebarFontSize" | "mainFontSize" => {
            value.as_i64().is_some_and(|n| (12..=24).contains(&n))
        }
        "contentWidth" => value.as_i64().is_some_and(|n| (60..=100).contains(&n)),
        "sidebarShortcut" => value.as_str().is_some_and(|s| s.chars().count() <= 32),
        "showMessageAvatars" | "checkProviderUpdates" => value.is_boolean(),
        "hideOldChatsThreshold" => value.as_i64().is_some_and(|n| (1..=50).contains(&n)),
        _ => false,
    }
}

fn preference_rank(field: &Value) -> (i64, String, String) {
    (
        field
            .get("timestamp")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        field
            .get("writer")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        canonical_json(field.get("value").unwrap_or(&Value::Null)),
    )
}

fn prune_preferences(fields: &mut Map<String, Value>) {
    let mut removable = fields
        .iter()
        .filter(|(name, _)| {
            !preference_key(name).is_some_and(|key| {
                key == "codex-studio-preferences-v1" || key.starts_with("server-")
            })
        })
        .map(|(name, field)| {
            (
                field
                    .get("timestamp")
                    .and_then(Value::as_i64)
                    .unwrap_or_default(),
                name.clone(),
            )
        })
        .collect::<Vec<_>>();
    removable.sort_by(|left, right| right.cmp(left));
    let retained = removable
        .into_iter()
        .take(PREFERENCE_FIELD_CAP)
        .map(|(_, name)| name)
        .collect::<std::collections::BTreeSet<_>>();
    fields.retain(|name, _| {
        preference_key(name)
            .is_some_and(|key| key == "codex-studio-preferences-v1" || key.starts_with("server-"))
            || retained.contains(name)
    });
}

fn preference_key(name: &str) -> Option<String> {
    serde_json::from_str::<Vec<String>>(name)
        .ok()
        .and_then(|parts| parts.into_iter().next())
}

fn canonical_json(value: &Value) -> String {
    serde_json::to_string(value).expect("serde_json::Value is serializable")
}

fn stored_preference_fields(
    payload: Option<String>,
    scope: &str,
    validate_field: &(impl Fn(&Value) -> bool + Send + Sync),
) -> Result<Map<String, Value>, SyncPolicyError> {
    let Some(payload) = payload else {
        return Ok(Map::new());
    };
    let invalid_object = || SyncPolicyError::InvalidStoredObject {
        scope: "uiPreferences".to_owned(),
        id: scope.to_owned(),
    };
    let envelope: Value = serde_json::from_str(&payload).map_err(|_| invalid_object())?;
    if envelope.as_object().is_none_or(|object| {
        object.len() != 3
            || envelope.get("collection").and_then(Value::as_str) != Some("uiPreferences")
            || envelope.get("id").and_then(Value::as_str) != Some(scope)
    }) {
        return Err(invalid_object());
    }
    let value = envelope
        .get("value")
        .and_then(Value::as_object)
        .ok_or_else(invalid_object)?;
    if value.len() != 1 {
        return Err(invalid_object());
    }
    let mut fields = value
        .get("fields")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(invalid_object)?;
    for (name, field) in &fields {
        if !validate_field(field)
            || validate_preference(
                name,
                field,
                if scope == "user" {
                    PreferenceScope::User
                } else {
                    PreferenceScope::Server
                },
            )
            .is_err()
        {
            return Err(invalid_object());
        }
    }
    fields.retain(|name, _| {
        !preference_key(name).is_some_and(|key| key.starts_with("studio-turns:"))
    });
    Ok(fields)
}

pub fn push_preferences(
    store: &RuntimeStore,
    scope: PreferenceScope,
    incoming: &Map<String, Value>,
    validate_field: impl Fn(&Value) -> bool + Send + Sync,
) -> Result<Map<String, Value>, SyncPolicyError> {
    let scope_name = match scope {
        PreferenceScope::User => "user",
        PreferenceScope::Server => "server",
    };
    store
        .transaction(|transaction| {
            let current_payload =
                SyncDocumentsRepository::preference_payload(transaction, scope_name)?;
            let current = stored_preference_fields(current_payload, scope_name, &validate_field)
                .map_err(|error| match error {
                    SyncPolicyError::InvalidStoredObject { scope, id } => {
                        StoreError::InvalidSyncObject { scope, id }
                    }
                    other => StoreError::SchemaMismatch(other.to_string()),
                })?;
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| {
                    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
                });
            let merged = merge_preferences(&current, incoming, now_ms)
                .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
            let payload = canonical_json(&json!({
                "collection": "uiPreferences",
                "id": scope_name,
                "value": {"fields": merged},
            }));
            SyncDocumentsRepository::put_preferences(transaction, scope_name, &payload)?;
            Ok(merged)
        })
        .map_err(|error| match error {
            StoreError::InvalidSyncObject { scope, id } => {
                SyncPolicyError::InvalidStoredObject { scope, id }
            }
            other => SyncPolicyError::Store(other),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_identity_matches_row_key_and_allows_extension_members() {
        let payload = json!({"device":"phone","session":"chat","id":"phone:tab:chat","text":"draft","extra":true});
        assert!(validate_draft("phone:tab:chat", &payload).is_ok());
        assert!(validate_stored_draft("phone:tab:chat", &payload).is_ok());
        assert!(validate_stored_draft("phone:chat", &json!({})).is_err());
        assert!(validate_stored_draft("phone:chat", &Value::Null).is_err());
        assert!(
            validate_stored_draft("phone:chat", &json!({"device":"other","session":"chat"}))
                .is_err()
        );
    }

    #[test]
    fn preference_rank_clamps_future_clock_and_keeps_other_fields() {
        let scope = PreferenceScope::User;
        let current = Map::new();
        let incoming = parse_preferences(&json!({
            "[\"codex-studio-preferences-v1\",\"theme\"]": {"value":"dark","timestamp":900000,"writer":"a"},
            "[\"codex-studio-preferences-v1\",\"mainFontSize\"]": {"value":18,"timestamp":100,"writer":"a"}
        }), scope).expect("valid preference fields");
        let merged = merge_preferences(&current, &incoming, 1_000).expect("merge succeeds");
        assert_eq!(
            merged["[\"codex-studio-preferences-v1\",\"theme\"]"]["timestamp"],
            1_000
        );
        assert_eq!(
            merged["[\"codex-studio-preferences-v1\",\"mainFontSize\"]"]["value"],
            18
        );
    }

    #[test]
    fn preference_validation_rejects_unknown_fields_and_scope_leaks() {
        assert!(parse_preferences(&json!({
            "[\"codex-studio-preferences-v1\",\"theme\"]": {"value":"blue","timestamp":1,"writer":"a"}
        }), PreferenceScope::User).is_err());
        assert!(parse_preferences(&json!({
            "[\"codex-project-tree:/state\",\"/repo\"]": {"value":true,"timestamp":1,"writer":"a"}
        }), PreferenceScope::User).is_err());
    }

    #[test]
    fn cursor_limits_match_scope_contract() {
        assert_eq!(effective_limit("state:entities:v1", 900), 500);
        assert_eq!(effective_limit("drafts", 900), 100);
        assert_eq!(effective_limit("drafts", 0), 1);
    }

    #[test]
    fn overflowing_numbers_are_rejected_by_json_parser() {
        assert!(serde_json::from_str::<Value>(r#"{"nested":[1e309]}"#).is_err());
    }

    #[test]
    fn stored_preference_envelope_identity_must_match_key() {
        let payload =
            Some(r#"{"collection":"agent","id":"wrong","value":{"fields":{}}}"#.to_owned());
        assert!(matches!(
            stored_preference_fields(payload, "user", &|_| true),
            Err(SyncPolicyError::InvalidStoredObject { scope, id })
                if scope == "uiPreferences" && id == "user"
        ));
    }
}
