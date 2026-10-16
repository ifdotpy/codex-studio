//! Pure sync document, cursor and preference decisions.

use crate::event_write::truthy;
pub use crate::sync_views::{
    AgentViewContext, ComplaintViewContext, RoomViewContext, agent_entity_view,
    complaint_entity_view, openapi_document, project_entity_view, project_persisted_record,
    room_entity_view,
};
use rusqlite::{Connection, OptionalExtension, Transaction};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use studio_runtime_schema::SYNC_ENTITY_SCOPE;
pub use studio_runtime_schema::shared_values::{SYNC_BATCH_LIMIT, SYNC_ENTITY_PAGE_LIMIT};
use studio_runtime_store::{
    RuntimeStore, StoreError, StoreResult, SyncDocumentsRepository, sha256_hex,
};
use thiserror::Error;

pub const ENTITY_PAGE_LIMIT: i64 = SYNC_ENTITY_PAGE_LIMIT;
pub const OTHER_PAGE_LIMIT: i64 = SYNC_BATCH_LIMIT;
const PREFERENCE_FIELD_CAP: usize = 3000;
const MAX_CLOCK_SKEW_MS: i64 = 5 * 60 * 1000;
// Python owner: codex_entity_contracts.MONITOR_TERMINAL_WINDOW and
// MONITOR_FINAL_WINDOW. A parity test below keeps both independently named
// caps aligned; the caps have separate meanings despite sharing a value.
const MONITOR_TERMINAL_WINDOW: usize = 100;
const MONITOR_FINAL_WINDOW: usize = 100;
const MONITOR_ACTIVE_STATUSES: [&str; 3] = ["running", "starting", "approval"];
const MONITOR_TERMINAL_STATUSES: [&str; 4] = ["completed", "failed", "cancelled", "lost"];
const TASK_RETIRE_BATCH: usize = 100;
const SYNC_SEEDED_MARKER: &str = "seeded";
const AGENT_ORGANIZATION_FIELDS_MARKER: &str = "agent_organization_fields";
const AGENT_ORGANIZATION_FIELDS_VERSION: i64 = 3;

/// Whether Python's durable seed and entity-view upgrade markers prove the
/// persisted sync projection is complete. Python owner: codex_sync_entities.seed.
pub fn has_complete_entity_seed(connection: &Connection) -> StoreResult<bool> {
    let seeded = connection
        .query_row(
            "SELECT value FROM sync_entity_meta WHERE key=?1",
            [SYNC_SEEDED_MARKER],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let version = connection
        .query_row(
            "SELECT value FROM sync_entity_meta WHERE key=?1",
            [AGENT_ORGANIZATION_FIELDS_MARKER],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let mut complete = seeded.as_deref() == Some("1")
        && version
            .and_then(|value| value.parse::<i64>().ok())
            .is_some_and(|value| value >= AGENT_ORGANIZATION_FIELDS_VERSION);
    if !complete {
        return Ok(false);
    }
    let table_exists = |name: &str| {
        connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [name],
            |row| row.get::<_, bool>(0),
        )
    };
    if table_exists("runtime_tasks")? && table_exists("runtime_agents")? {
        let task_window = connection
            .query_row(
                "SELECT value FROM sync_entity_meta WHERE key='task_window_migrated'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        complete &= task_window.as_deref() == Some("1");
    }
    if table_exists("runtime_events")? {
        let event_window = connection
            .query_row(
                "SELECT value FROM sync_entity_meta WHERE key='event_window_seq'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let projected_sequence = connection.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM sync_entities WHERE collection='event'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        complete &= event_window
            .and_then(|value| value.parse::<i64>().ok())
            .is_some_and(|sequence| sequence == projected_sequence);
    }
    Ok(complete)
}

/// Filesystem vault views captured before opening a projection write transaction.
#[derive(Clone, Debug, Default)]
pub struct ProjectVaultSnapshot {
    projects: BTreeMap<String, Value>,
    views: BTreeMap<String, Result<Option<Value>, (&'static str, String)>>,
}

impl ProjectVaultSnapshot {
    /// Capture filesystem views for valid stored projects while ignoring
    /// malformed rows, matching Python's sync projection reader.
    pub fn capture_for_sync(state_dir: &Path, connection: &Connection) -> StoreResult<Self> {
        let mut statement =
            connection.prepare_cached("SELECT id,record FROM runtime_projects ORDER BY rowid")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut projects = Vec::new();
        for row in rows {
            let (id, raw) = row?;
            if valid_stored_record("runtime_projects", &id, &raw) {
                projects.push((id, serde_json::from_str(&raw)?));
            }
        }
        Ok(Self::capture(state_dir, &projects))
    }

    /// Read each project's cached vault record. Lifecycle conflict recovery may
    /// call this under writer admission so the read-only projection and commit
    /// share one stable database snapshot.
    pub fn capture(state_dir: &Path, projects: &[(String, Value)]) -> Self {
        let vm_state_dir = crate::vault::linux_vm_state_dir(state_dir);
        let mut snapshot = Self::default();
        for (id, project) in projects {
            snapshot.projects.insert(id.clone(), project.clone());
            let Some(source) = project_vault_source(project) else {
                snapshot.views.insert(id.clone(), Ok(None));
                continue;
            };
            let project_id = sha256_hex(source.as_bytes());
            let view = crate::vault::view(&vm_state_dir, &project_id).map_err(|error| {
                let object = match error {
                    crate::vault::VaultError::InvalidStoredRecord(object) => object,
                    _ => "vault project id",
                };
                (object, project_id)
            });
            snapshot.views.insert(id.clone(), view);
        }
        snapshot
    }

    /// Hash the validated vault views used by project projections for a process-local skip key.
    pub fn signature(&self) -> String {
        let serialized = serde_json::to_vec(&self.views)
            .expect("project vault views serialize as JSON values and error tuples");
        studio_runtime_store::sha256_hex(&serialized)
    }

    /// Check the small persisted project set before deciding whether a full
    /// cold projection rebuild is needed after process restart.
    pub fn matches_entity_projections(&self, connection: &Connection) -> StoreResult<bool> {
        for (id, project) in &self.projects {
            let payload = connection
                .query_row(
                    "SELECT payload FROM sync_entities WHERE collection='project' AND id=?1 AND deleted=0",
                    [id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let Some(payload) = payload else {
                return Ok(false);
            };
            let payload: Value = serde_json::from_str(&payload)?;
            let ready = payload
                .pointer("/value/layrReadiness/state")
                .and_then(Value::as_str)
                == Some("ready");
            let expected_vault = if ready { self.view(id, project)? } else { None };
            if payload.pointer("/value/vault") != expected_vault.as_ref() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn recheck(&self, transaction: &Transaction<'_>) -> StoreResult<()> {
        let current = projection_records(transaction, "runtime_projects")?;
        if current.len() != self.projects.len()
            || current.into_iter().any(|(id, raw)| {
                serde_json::from_str::<Value>(&raw).ok().as_ref() != self.projects.get(&id)
            })
        {
            return Err(StoreError::RecordContract(
                "project records changed after cached vault views were read",
            ));
        }
        Ok(())
    }

    fn view(&self, id: &str, project: &Value) -> StoreResult<Option<Value>> {
        if self.projects.get(id) != Some(project) {
            return Err(StoreError::RecordContract(
                "project record changed after its cached vault view was read",
            ));
        }
        match self.views.get(id) {
            Some(Ok(view)) => Ok(view.clone()),
            Some(Err((object, project_id))) => Err(StoreError::InvalidStoredRecord {
                object,
                id: project_id.clone(),
            }),
            None => Err(StoreError::RecordContract(
                "cached vault view is missing for a persisted project",
            )),
        }
    }
}

/// Rebuild renderer entity projections from persisted records in the caller's
/// transaction. Re-running this operation preserves unchanged entity
/// sequences because the repository compares canonical payloads before write.
pub fn maintain_entity_projections(
    transaction: &Transaction<'_>,
    _state_dir: &str,
    vault_snapshot: &ProjectVaultSnapshot,
    rooms: &crate::sync_entity_context::RoomDirectory,
    validate_projection: impl Fn(&str, &str, &Value, bool) -> bool,
) -> StoreResult<()> {
    let project_server_id =
        studio_runtime_store::RuntimeStore::transaction_local_server_id(transaction)?;
    maintain_entity_projections_with_server_id(
        transaction,
        _state_dir,
        project_server_id.as_deref(),
        vault_snapshot,
        rooms,
        validate_projection,
    )
}

pub fn maintain_entity_projections_with_server_id(
    transaction: &Transaction<'_>,
    state_dir: &str,
    project_server_id: Option<&str>,
    vault_snapshot: &ProjectVaultSnapshot,
    rooms: &crate::sync_entity_context::RoomDirectory,
    validate_projection: impl Fn(&str, &str, &Value, bool) -> bool,
) -> StoreResult<()> {
    let source_records =
        crate::diagnostics::measure_runtime_stage("projection.source_snapshot", || {
            [
                "runtime_agents",
                "runtime_rooms",
                "runtime_complaints",
                "runtime_projects",
                "runtime_requests",
                "runtime_rules",
                "runtime_work",
                "runtime_federation_rooms",
            ]
            .into_iter()
            .map(|table| projection_records(transaction, table).map(|records| (table, records)))
            .collect::<StoreResult<HashMap<_, _>>>()
        })?;
    crate::diagnostics::measure_runtime_stage("projection.vault_recheck", || {
        vault_snapshot.recheck(transaction)
    })?;
    let peer_agents = source_records
        .get("runtime_agents")
        .ok_or(StoreError::RecordContract(
            "entity projection source snapshot is incomplete",
        ))?;
    let peer_projects =
        source_records
            .get("runtime_projects")
            .ok_or(StoreError::RecordContract(
                "entity projection source snapshot is incomplete",
            ))?;
    let agent_contexts =
        crate::diagnostics::measure_runtime_stage("projection.agent_contexts", || {
            crate::sync_entity_context::agent_view_contexts(
                transaction,
                peer_agents,
                source_records
                    .get("runtime_work")
                    .ok_or(StoreError::RecordContract(
                        "entity projection source snapshot is incomplete",
                    ))?,
                project_server_id,
            )
        })?
        .0;
    crate::diagnostics::measure_runtime_stage("projection.peer_team_entities", || {
        maintain_peer_team_entities(
            transaction,
            peer_agents,
            peer_projects,
            &validate_projection,
        )
    })?;
    let mut current_rooms = BTreeSet::new();
    crate::diagnostics::measure_runtime_stage("projection.direct_entities", || {
        for (table, collection) in [
            ("runtime_agents", "agent"),
            ("runtime_rooms", "room"),
            ("runtime_complaints", "complaint"),
            ("runtime_projects", "project"),
            ("runtime_requests", "request"),
            ("runtime_rules", "rule"),
            ("runtime_work", "work"),
        ] {
            let records = source_records.get(table).ok_or(StoreError::RecordContract(
                "entity projection source snapshot is incomplete",
            ))?;
            for (id, raw) in records {
                let record = serde_json::from_str::<Value>(raw)?;
                if collection == "agent" && truthy(record.get("deletedAt")) {
                    retire_if_live(transaction, collection, id, &validate_projection)?;
                    continue;
                }
                if collection == "request"
                    && record.get("status").and_then(Value::as_str) != Some("pending")
                {
                    retire_if_live(transaction, collection, id, &validate_projection)?;
                    continue;
                }
                let projected = match collection {
                    "agent" => agent_entity_view(
                        &record,
                        agent_contexts.get(id).ok_or(StoreError::RecordContract(
                            "agent projection context snapshot is incomplete",
                        ))?,
                    ),
                    "room" => {
                        let context = rooms.for_room(transaction, &record, true)?;
                        room_entity_view(&record, &context)
                    }
                    "complaint" => complaint_entity_view(
                        &record,
                        &ComplaintViewContext {
                            agents: rooms.agents().to_vec(),
                        },
                    ),
                    "project" => {
                        let prior = transaction.query_row(
                        "SELECT payload FROM sync_entities WHERE collection='project' AND id=?1 AND deleted=0",
                        [&id],
                        |row| row.get::<_, Option<String>>(0),
                    ).optional()?.flatten();
                        let prior =
                            prior.and_then(|payload| serde_json::from_str::<Value>(&payload).ok());
                        let value = prior.as_ref().and_then(|payload| payload.get("value"));
                        let readiness = value.and_then(|value| value.get("layrReadiness"));
                        let readiness_is_ready = readiness.is_some_and(|value| {
                            value.get("state").and_then(Value::as_str) == Some("ready")
                        });
                        let cached_vault = if readiness_is_ready {
                            vault_snapshot.view(id, &record)?
                        } else {
                            None
                        };
                        let vault = if readiness_is_ready && !state_dir.is_empty() {
                            cached_vault.as_ref()
                        } else {
                            value.and_then(|value| value.get("vault"))
                        };
                        project_entity_view(
                            &record,
                            readiness,
                            value.and_then(|value| value.get("layrSync")),
                            vault,
                        )
                    }
                    _ => project_persisted_record(collection, &record),
                };
                if let Some(projected) = projected {
                    if collection == "room" {
                        current_rooms.insert(id.clone());
                    }
                    put_projection(
                        transaction,
                        collection,
                        id,
                        projected,
                        false,
                        &validate_projection,
                    )?;
                }
            }
        }
        Ok::<(), StoreError>(())
    })?;
    if table_exists(transaction, "runtime_rooms")? {
        retire_unkept(
            transaction,
            "room",
            &current_rooms,
            None,
            &validate_projection,
        )?;
    }

    crate::diagnostics::measure_runtime_stage("projection.task_window", || {
        maintain_task_window(transaction, &validate_projection)
    })?;
    crate::diagnostics::measure_runtime_stage("projection.monitor_window", || {
        maintain_monitor_window(transaction, &validate_projection)
    })?;
    crate::diagnostics::measure_runtime_stage("projection.event_window", || {
        maintain_event_window(transaction, &validate_projection)
    })?;
    crate::diagnostics::measure_runtime_stage("projection.canvas_chat_entities", || {
        maintain_canvas_chat_entities(transaction, &validate_projection)
    })?;

    // Python refreshes workspace/current after entity mutations only when a
    // workspace projection already exists. Initial creation belongs to seed
    // and explicit workspace publishers, not unrelated lifecycle mutations.
    let prior = transaction
        .query_row(
            "SELECT payload FROM sync_entities WHERE collection='workspace' AND id='current' AND deleted=0",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?;
    if let Some(Ok(payload)) = prior
        .flatten()
        .map(|payload| serde_json::from_str::<Value>(&payload))
        && let Some(mut workspace) = payload.get("value").and_then(Value::as_object).cloned()
    {
        workspace.insert("stateDir".to_owned(), Value::String(state_dir.to_owned()));
        workspace.entry("connected").or_insert(Value::Bool(false));
        workspace
            .entry("projectOrganizationVersion")
            .or_insert(json!(1));
        workspace.entry("peerTeamsVersion").or_insert(json!(1));
        workspace.entry("tasksHistoryLimit").or_insert(json!(100));
        put_projection(
            transaction,
            "workspace",
            "current",
            Value::Object(workspace),
            false,
            &validate_projection,
        )?;
    }
    Ok(())
}

/// Seed the initial workspace entity when a sync pull is the first projection
/// operation. Python's `codex_sync_entities.seed` creates this row on first
/// seed; ordinary projection maintenance only refreshes an existing row.
pub fn seed_workspace_projection(
    transaction: &Transaction<'_>,
    state_dir: &str,
    validate_projection: impl Fn(&str, &str, &Value, bool) -> bool,
) -> StoreResult<()> {
    let exists = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sync_entities WHERE collection='workspace' AND id='current' AND deleted=0)",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if exists {
        return Ok(());
    }
    put_projection(
        transaction,
        "workspace",
        "current",
        json!({
            "stateDir": state_dir,
            "connected": false,
            "projectOrganizationVersion": 1,
            "peerTeamsVersion": 1,
            "tasksHistoryLimit": 100,
        }),
        false,
        &validate_projection,
    )
}

/// Identity selected by Python's `codex_vm_agents.project_vault` projection.
fn project_vault_source(project: &Value) -> Option<&str> {
    let aliases = project.get("projectAliases").and_then(Value::as_array);
    let logical_id = aliases
        .and_then(|aliases| aliases.first())
        .and_then(|alias| alias.get("projectId"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    logical_id
        .or_else(|| {
            project
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
        })
        .or_else(|| {
            project
                .get("path")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
        })
}

fn put_projection(
    transaction: &Transaction<'_>,
    collection: &str,
    id: &str,
    value: Value,
    deleted: bool,
    validate: &impl Fn(&str, &str, &Value, bool) -> bool,
) -> StoreResult<()> {
    if !validate(collection, id, &value, deleted) {
        return Err(StoreError::InvalidSyncObject {
            scope: collection.to_owned(),
            id: id.to_owned(),
        });
    }
    SyncDocumentsRepository::put_entity(transaction, collection, id, value, deleted)?;
    Ok(())
}

pub(crate) fn table_exists(connection: &Connection, name: &str) -> StoreResult<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |row| row.get(0),
    )?)
}

pub(crate) fn stored_records(
    transaction: &Transaction<'_>,
    table: &str,
) -> StoreResult<Vec<(String, String)>> {
    let rows = stored_record_rows(transaction, table)?;
    for (id, raw) in &rows {
        if !valid_stored_record(table, id, raw) {
            return Err(StoreError::InvalidSyncObject {
                scope: table.to_owned(),
                id: id.clone(),
            });
        }
    }
    Ok(rows)
}

fn projection_records(
    transaction: &Transaction<'_>,
    table: &str,
) -> StoreResult<Vec<(String, String)>> {
    // Python owner: codex_sync_entities._stored_record skips malformed rows
    // while refreshing sync projections instead of failing the whole pull.
    Ok(stored_record_rows(transaction, table)?
        .into_iter()
        .filter(|(id, raw)| valid_stored_record(table, id, raw))
        .collect())
}

pub(crate) fn projection_values(connection: &Connection, table: &str) -> StoreResult<Vec<Value>> {
    Ok(stored_record_rows(connection, table)?
        .into_iter()
        .filter(|(id, raw)| valid_stored_record(table, id, raw))
        .map(|(_, raw)| serde_json::from_str(&raw))
        .collect::<Result<Vec<_>, _>>()?)
}

fn stored_record_rows(connection: &Connection, table: &str) -> StoreResult<Vec<(String, String)>> {
    if !table_exists(connection, table)? {
        return Ok(Vec::new());
    }
    let mut statement =
        connection.prepare(&format!("SELECT id,record FROM {table} ORDER BY rowid"))?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn valid_stored_record(table: &str, id: &str, raw: &str) -> bool {
    serde_json::from_str::<Value>(raw).is_ok_and(|record| {
        record.is_object()
            && record.get("id").and_then(Value::as_str) == Some(id)
            && studio_runtime_schema::table_schema(table).is_none_or(|schema| {
                studio_runtime_schema::validate(schema, &record).unwrap_or(false)
            })
    })
}

pub(crate) fn normalized_project_path(value: &str) -> Option<String> {
    if value.trim().is_empty() {
        return None;
    }
    let expanded = if value == "~" {
        PathBuf::from(std::env::var_os("HOME")?)
    } else if let Some(rest) = value.strip_prefix("~/") {
        PathBuf::from(std::env::var_os("HOME")?).join(rest)
    } else {
        PathBuf::from(value)
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().ok()?.join(expanded)
    };
    if let Ok(canonical) = absolute.canonicalize() {
        return Some(canonical.to_string_lossy().into_owned());
    }
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Some(normalized.to_string_lossy().into_owned())
}

fn retire_if_live(
    transaction: &Transaction<'_>,
    collection: &str,
    id: &str,
    validate: &impl Fn(&str, &str, &Value, bool) -> bool,
) -> StoreResult<()> {
    let live: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sync_entities WHERE collection=?1 AND id=?2 AND deleted=0)",
        [collection, id],
        |row| row.get(0),
    )?;
    if live {
        put_projection(transaction, collection, id, json!({}), true, validate)?;
    }
    Ok(())
}

pub(crate) fn maintain_task_window(
    transaction: &Transaction<'_>,
    validate: &impl Fn(&str, &str, &Value, bool) -> bool,
) -> StoreResult<()> {
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
            put_projection(transaction, "task", &id, projected, false, validate)?;
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
        put_projection(transaction, "task", id, json!({}), true, validate)?;
    }
    if stale.is_empty() {
        let migrated = transaction
            .query_row(
                "SELECT value FROM sync_entity_meta WHERE key='task_window_migrated'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if migrated.as_deref() != Some("1") {
            transaction.execute(
                "INSERT INTO sync_entity_meta(key,value) VALUES('task_window_migrated','1') \
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [],
            )?;
        }
    }
    Ok(())
}

/// Update one running task without rescanning the history window when its
/// membership and ordering inputs are unchanged. All other writes rebuild the
/// canonical window because they may change eligibility or rank.
pub(crate) fn maintain_task_window_after_write(
    transaction: &Transaction<'_>,
    previous: Option<&Value>,
    task: &Value,
    validate: &impl Fn(&str, &str, &Value, bool) -> bool,
) -> StoreResult<()> {
    let migration_complete: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sync_entity_meta \
         WHERE key='task_window_migrated' AND value='1')",
        [],
        |row| row.get(0),
    )?;
    if !migration_complete {
        return maintain_task_window(transaction, validate);
    }

    let stable_running_task = previous.is_some_and(|previous| {
        previous.get("id") == task.get("id")
            && previous.get("agent") == task.get("agent")
            && previous.get("created") == task.get("created")
            && previous.get("status").and_then(Value::as_str) == Some("running")
            && task.get("status").and_then(Value::as_str) == Some("running")
    });
    if stable_running_task
        && table_exists(transaction, "runtime_agents")?
        && let Some(task_id) = task.get("id").and_then(Value::as_str)
        && let Some(agent_id) = task.get("agent").and_then(Value::as_str)
    {
        let agent_is_live: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_agents WHERE id=?1 \
             AND json_extract(record,'$.deletedAt') IS NULL)",
            [agent_id],
            |row| row.get(0),
        )?;
        let task_is_projected: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sync_entities \
             WHERE collection='task' AND id=?1 AND deleted=0)",
            [task_id],
            |row| row.get(0),
        )?;
        if agent_is_live
            && task_is_projected
            && let Some(projected) = project_persisted_record("task", task)
        {
            put_projection(transaction, "task", task_id, projected, false, validate)?;
            return Ok(());
        }
    }
    maintain_task_window(transaction, validate)
}

pub fn maintain_monitor_window(
    transaction: &Transaction<'_>,
    validate: &impl Fn(&str, &str, &Value, bool) -> bool,
) -> StoreResult<()> {
    if !table_exists(transaction, "runtime_monitors")? {
        return Ok(());
    }
    let mut preserved_invalid_monitor_ids = BTreeSet::new();
    let mut kept = BTreeMap::<String, Value>::new();
    let has_status_index = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='runtime_monitor_status')",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if has_status_index {
        // This expression index can only contain rows accepted by json_extract,
        // so indexed predicates are safe and cannot encounter malformed JSON.
        let active_placeholders = (1..=MONITOR_ACTIVE_STATUSES.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut statement = transaction.prepare(&format!(
            "SELECT record FROM runtime_monitors \
             WHERE json_extract(record,'$.status') IN ({active_placeholders})"
        ))?;
        let active_records = statement
            .query_map(rusqlite::params_from_iter(MONITOR_ACTIVE_STATUSES), |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for raw in active_records {
            if let Ok(record) = serde_json::from_str::<Value>(&raw)
                && let Some(id) = record.get("id").and_then(Value::as_str)
            {
                kept.insert(id.to_owned(), record);
            }
        }

        let mut terminals = Vec::new();
        for status in MONITOR_TERMINAL_STATUSES {
            // The runtime_monitor_status expression index supports this
            // status predicate and newest-first bounded scan.
            let mut statement = transaction.prepare(
                "SELECT record FROM runtime_monitors \
                 WHERE json_extract(record,'$.status')=?1 \
                 ORDER BY json_extract(record,'$.created') DESC LIMIT ?2",
            )?;
            let status_rows = statement
                .query_map(
                    rusqlite::params![status, MONITOR_TERMINAL_WINDOW as i64],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            drop(statement);
            for raw in status_rows {
                if let Ok(record) = serde_json::from_str::<Value>(&raw) {
                    terminals.push(record);
                }
            }
        }
        terminals.sort_by(compare_created_desc);
        for record in terminals.into_iter().take(MONITOR_FINAL_WINDOW) {
            if let Some(id) = record.get("id").and_then(Value::as_str) {
                kept.insert(id.to_owned(), record);
            }
        }

        // Malformed source rows cannot have entered the expression index, but
        // a legacy or test schema may still contain one. Hold its current
        // canonical entity without scanning all historical rows.
        let mut statement = transaction.prepare(
            "SELECT source.id FROM sync_entities AS entity \
             CROSS JOIN runtime_monitors AS source \
             WHERE entity.collection='monitor' AND entity.deleted=0 \
               AND source.id=entity.id \
               AND CASE WHEN json_valid(source.record) \
                        THEN json_type(source.record)!='object' ELSE 1 END",
        )?;
        preserved_invalid_monitor_ids.extend(
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?,
        );
        drop(statement);
    } else {
        // Compatibility path for legacy/test databases without the owner
        // expression index. Decode each row safely and preserve malformed ids.
        let mut statement =
            transaction.prepare("SELECT id,record FROM runtime_monitors ORDER BY rowid")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut records = Vec::new();
        for (id, raw) in rows {
            match serde_json::from_str::<Value>(&raw) {
                Ok(record) if record.is_object() => records.push(record),
                _ => {
                    eprintln!("monitor projection recovery skipped row id={id}: invalid record");
                    preserved_invalid_monitor_ids.insert(id);
                }
            }
        }
        for status in MONITOR_ACTIVE_STATUSES {
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
        for status in MONITOR_TERMINAL_STATUSES {
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
    }

    let owner_ids = kept
        .values()
        .filter_map(|record| record.get("agent").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let mut agents = BTreeMap::new();
    let mut invalid_agent_ids = BTreeSet::new();
    for chunk in owner_ids.iter().collect::<Vec<_>>().chunks(500) {
        if chunk.is_empty() {
            continue;
        }
        let placeholders = (1..=chunk.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut statement = transaction.prepare(&format!(
            "SELECT id,record FROM runtime_agents WHERE id IN ({placeholders})"
        ))?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(chunk.iter().copied()), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for (id, raw) in rows {
            match serde_json::from_str::<Value>(&raw) {
                Ok(record) if record.is_object() => {
                    agents.insert(id, record.get("deletedAt").is_none_or(Value::is_null));
                }
                _ => {
                    eprintln!(
                        "monitor projection recovery skipped owner row id={id}: invalid record"
                    );
                    invalid_agent_ids.insert(id);
                }
            }
        }
    }
    let mut kept_ids = preserved_invalid_monitor_ids;
    for (id, record) in kept {
        let owner = record.get("agent").and_then(Value::as_str);
        if owner.is_some_and(|agent| invalid_agent_ids.contains(agent)) {
            eprintln!("monitor projection recovery skipped row id={id}: invalid owner record");
            kept_ids.insert(id);
            continue;
        }
        let active_owner = owner
            .and_then(|agent| agents.get(agent))
            .copied()
            .unwrap_or(false);
        if !active_owner {
            continue;
        }
        if let Some(projected) = project_persisted_record("monitor", &record) {
            kept_ids.insert(id.clone());
            put_projection(transaction, "monitor", &id, projected, false, validate)?;
        }
    }
    retire_unkept(transaction, "monitor", &kept_ids, None, validate)
}

fn maintain_event_window(
    transaction: &Transaction<'_>,
    validate: &impl Fn(&str, &str, &Value, bool) -> bool,
) -> StoreResult<()> {
    if !table_exists(transaction, "runtime_events")? {
        return Ok(());
    }
    // The generated runtime_events triggers use the same marker fast path as
    // Python's sync_event_window(full=False). Avoid reprojecting the entire
    // window after a trigger has already recorded its current high sequence.
    let changed = transaction.query_row(
        "SELECT COALESCE(MAX(seq),0) FROM sync_entities WHERE collection='event'",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    let marker = transaction
        .query_row(
            "SELECT value FROM sync_entity_meta WHERE key='event_window_seq'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if marker
        .as_deref()
        .and_then(|value| value.parse::<i64>().ok())
        == Some(changed)
    {
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
            put_projection(transaction, "event", &id, projected, false, validate)?;
        }
    }
    retire_unkept(
        transaction,
        "event",
        &kept,
        Some("event_window_seq"),
        validate,
    )
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

fn maintain_peer_team_entities(
    transaction: &Transaction<'_>,
    agent_records: &[(String, String)],
    project_records: &[(String, String)],
    validate: &impl Fn(&str, &str, &Value, bool) -> bool,
) -> StoreResult<()> {
    if !table_exists(transaction, "runtime_projects")?
        || !table_exists(transaction, "runtime_agents")?
    {
        return Ok(());
    }
    let agents = agent_records
        .iter()
        .filter_map(|(_, raw)| serde_json::from_str::<Value>(raw).ok())
        .filter_map(|agent| {
            let id = agent.get("id")?.as_str()?.to_owned();
            let live = agent.get("deletedAt").is_none_or(Value::is_null);
            let lead = agent.get("isLead").and_then(Value::as_bool) == Some(true)
                && agent.get("parentId").is_none_or(Value::is_null)
                && agent.get("rootId").and_then(Value::as_str) == Some(id.as_str());
            (live && lead).then_some((id, agent))
        })
        .collect::<BTreeMap<_, _>>();
    let mut current = BTreeMap::<String, Value>::new();
    for (_, raw) in project_records {
        let Ok(project) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        let Some(project_path) = project
            .get("path")
            .and_then(Value::as_str)
            .filter(|path| !path.trim().is_empty())
            .and_then(normalized_project_path)
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
                    agent
                        .get("cwd")
                        .and_then(Value::as_str)
                        .and_then(normalized_project_path)
                        .as_deref()
                        == Some(project_path.as_str())
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
            put_projection(transaction, "peerTeam", &id, projected, false, validate)?;
        }
    }
    retire_unkept(transaction, "peerTeam", &kept, None, validate)
}

fn retire_unkept(
    transaction: &Transaction<'_>,
    collection: &str,
    kept: &BTreeSet<String>,
    marker: Option<&str>,
    validate: &impl Fn(&str, &str, &Value, bool) -> bool,
) -> StoreResult<()> {
    let mut statement =
        transaction.prepare("SELECT id FROM sync_entities WHERE collection=?1 AND deleted=0")?;
    let stale = statement
        .query_map([collection], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for id in stale {
        if !kept.contains(&id) {
            put_projection(transaction, collection, &id, json!({}), true, validate)?;
        }
    }
    if let Some(marker) = marker {
        let sequence = transaction.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM sync_entities WHERE collection=?1",
            [collection],
            |row| row.get::<_, i64>(0),
        )?;
        let sequence = sequence.to_string();
        let current = transaction
            .query_row(
                "SELECT value FROM sync_entity_meta WHERE key=?1",
                [marker],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if current.as_deref() != Some(sequence.as_str()) {
            transaction.execute(
                "INSERT INTO sync_entity_meta(key,value) VALUES(?1,?2) \
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                rusqlite::params![marker, sequence],
            )?;
        }
    }
    Ok(())
}

fn maintain_canvas_chat_entities(
    transaction: &Transaction<'_>,
    validate: &impl Fn(&str, &str, &Value, bool) -> bool,
) -> StoreResult<()> {
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
        let name = transaction.query_row("SELECT name FROM groups WHERE id=?1", [&id], |row| {
            row.get::<_, String>(0)
        })?;
        let members = transaction
            .prepare(
                "SELECT source FROM graph_edges WHERE target=?1 AND kind='chat' ORDER BY source",
            )?
            .query_map([&id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let last = transaction
            .query_row(
                "SELECT text,at FROM messages WHERE room=?1 ORDER BY at DESC LIMIT 1",
                [&id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)),
            )
            .optional()?;
        let count: i64 = transaction.query_row(
            "SELECT count(*) FROM messages WHERE room=?1",
            [&id],
            |row| row.get(0),
        )?;
        let tail = last
            .as_ref()
            .map(|message| message.0.chars().take(2_000).collect::<String>())
            .unwrap_or_default();
        let value = json!({
            "id":id,
            "name":name,
            "members":members,
            "kind":"chat",
            "messageCount":count,
            "tail":tail,
            "lastMessageAt":last.map(|message| message.1),
        });
        put_projection(transaction, "chat", &id, value, false, validate)?;
        kept.insert(id);
    }
    retire_unkept(transaction, "chat", &kept, None, validate)?;

    if !table_exists(transaction, "graph_edges")? {
        return Ok(());
    }
    let mut statement =
        transaction.prepare("SELECT id,source,target,kind FROM graph_edges ORDER BY id")?;
    let edges = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for (id, source, target, kind) in edges {
        put_projection(
            transaction,
            "edge",
            &id,
            json!({"id":id,"source":source,"target":target,"kind":kind}),
            false,
            validate,
        )?;
    }
    Ok(())
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
        if scope == SYNC_ENTITY_SCOPE {
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

/// Whether a JSON integer is outside serde_json's exact signed/unsigned range.
pub fn contains_unrepresentable_integer(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            index += 1;
            continue;
        }
        if byte == b'-' || byte.is_ascii_digit() {
            let start = index;
            index += 1;
            while index < bytes.len()
                && !matches!(
                    bytes[index],
                    b',' | b']' | b'}' | b' ' | b'\n' | b'\r' | b'\t'
                )
            {
                index += 1;
            }
            let token = &raw[start..index];
            if !token.contains(['.', 'e', 'E'])
                && token.parse::<i64>().is_err()
                && token.parse::<u64>().is_err()
            {
                return true;
            }
            continue;
        }
        index += 1;
    }
    false
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
        || (object.contains_key("id") && object.get("id").and_then(Value::as_str) != Some(id))
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

pub async fn push_preferences(
    store: &RuntimeStore,
    scope: PreferenceScope,
    incoming: Map<String, Value>,
    validate_field: impl Fn(&Value) -> bool + Send + Sync + 'static,
    on_provider_update_change: impl FnOnce() + Send + 'static,
) -> Result<Map<String, Value>, SyncPolicyError> {
    let scope_name = match scope {
        PreferenceScope::User => "user",
        PreferenceScope::Server => "server",
    };
    let ((merged, _provider_updates_changed), _) = store
        .transaction_with_post_commit(
            move |transaction| {
                let current_payload =
                    SyncDocumentsRepository::preference_payload(transaction, scope_name)?;
                let current =
                    stored_preference_fields(current_payload, scope_name, &validate_field)
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
                let merged = merge_preferences(&current, &incoming, now_ms)
                    .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
                let provider_updates_changed = scope == PreferenceScope::User
                    && crate::provider_versions::provider_updates_enabled_from_fields(&current)
                        != crate::provider_versions::provider_updates_enabled_from_fields(&merged);
                let payload = canonical_json(&json!({
                    "collection": "uiPreferences",
                    "id": scope_name,
                    "value": {"fields": merged},
                }));
                SyncDocumentsRepository::put_preferences(transaction, scope_name, &payload)?;
                Ok((merged, provider_updates_changed))
            },
            move |(_, provider_updates_changed)| {
                if *provider_updates_changed {
                    on_provider_update_change();
                }
            },
        )
        .await
        .map_err(|error| match error {
            StoreError::InvalidSyncObject { scope, id } => {
                SyncPolicyError::InvalidStoredObject { scope, id }
            }
            other => SyncPolicyError::Store(other),
        })?;
    Ok(merged)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use rusqlite::trace::{TraceEvent, TraceEventCodes};
    use std::cell::RefCell;
    use std::fs;
    use studio_runtime_store::ProjectsRepository;

    thread_local! {
        static TRACED_SQL: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    fn trace_sql(event: TraceEvent<'_>) {
        if let TraceEvent::Stmt(statement, _) = event {
            TRACED_SQL.with_borrow_mut(|statements| {
                statements.push(statement.sql().into_owned());
            });
        }
    }

    #[test]
    fn monitor_window_contract_matches_python_owner() {
        let python = include_str!("../../../apps/server/src/codex_entity_contracts.py");
        for expected in [
            format!("MONITOR_TERMINAL_WINDOW = {MONITOR_TERMINAL_WINDOW}"),
            format!("MONITOR_FINAL_WINDOW = {MONITOR_FINAL_WINDOW}"),
            format!(
                "MONITOR_TERMINAL_STATUSES = ({})",
                MONITOR_TERMINAL_STATUSES
                    .iter()
                    .map(|status| format!("\"{status}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            format!(
                "ACTIVE_MONITOR_STATUSES = ({})",
                MONITOR_ACTIVE_STATUSES
                    .iter()
                    .map(|status| format!("\"{status}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ] {
            assert!(
                python.lines().any(|line| line.trim() == expected),
                "Python owner codex_entity_contracts.py must declare {expected}"
            );
        }
    }

    #[test]
    fn monitor_window_uses_indexed_status_ranges() {
        let connection = Connection::open_in_memory().expect("database");
        connection
            .execute_batch(
                "CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                 CREATE INDEX runtime_monitor_status ON runtime_monitors(
                     json_extract(record,'$.status'),json_extract(record,'$.created'));",
            )
            .expect("monitor fixture and owner index");
        let plan = connection
            .prepare(
                "EXPLAIN QUERY PLAN SELECT record FROM runtime_monitors \
                 WHERE json_extract(record,'$.status')=?1 \
                 ORDER BY json_extract(record,'$.created') DESC LIMIT ?2",
            )
            .expect("selector explain plan")
            .query_map(rusqlite::params!["completed", 100], |row| {
                row.get::<_, String>(3)
            })
            .expect("selector plan rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("selector plan read");
        assert!(
            plan.iter()
                .any(|line| line.contains("runtime_monitor_status")),
            "terminal status window must use the owner index: {plan:?}"
        );
    }

    #[test]
    fn monitor_window_keeps_active_rows_and_holds_invalid_rows_without_projecting_inactive_owners()
    {
        let mut connection = Connection::open_in_memory().expect("database");
        connection
            .execute_batch(
                "CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                 CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                 CREATE TABLE sync_entities(
                     collection TEXT NOT NULL,id TEXT NOT NULL,seq INTEGER NOT NULL,
                     hash TEXT NOT NULL,payload TEXT,deleted INTEGER NOT NULL DEFAULT 0,
                     PRIMARY KEY(collection,id));
                 CREATE TABLE sync_documents(seq INTEGER PRIMARY KEY);
                 CREATE TABLE sync_versions(seq INTEGER PRIMARY KEY);
                 CREATE TABLE sync_entity_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
                 INSERT INTO runtime_agents VALUES
                     ('active','{\"id\":\"active\",\"deletedAt\":null}'),
                     ('deleted','{\"id\":\"deleted\",\"deletedAt\":1}'),
                     ('broken','{');",
            )
            .expect("projection fixture");
        let insert_monitor =
            |connection: &Connection, id: &str, agent: &str, status: &str, created: i64| {
                connection
                    .execute(
                        "INSERT INTO runtime_monitors(id,record) VALUES(?1,?2)",
                        rusqlite::params![
                            id,
                            json!({"id":id,"agent":agent,"status":status,"created":created})
                                .to_string()
                        ],
                    )
                    .expect("monitor row");
            };
        for index in 0..105 {
            let id = format!("active-{index}");
            insert_monitor(&connection, &id, "active", "running", index);
        }
        for index in 0..104 {
            let id = format!("terminal-{index}");
            let owner = if index == 50 { "deleted" } else { "active" };
            insert_monitor(&connection, &id, owner, "completed", 10);
        }
        insert_monitor(&connection, "deleted-owner", "deleted", "starting", 200);
        insert_monitor(&connection, "invalid-owner", "broken", "approval", 201);
        insert_monitor(&connection, "missing-owner", "missing", "running", 202);
        // Python owner selector: codex_entity_contracts.monitor_records.
        // Equal timestamps intentionally exercise the SQL tie behavior at the
        // terminal window boundary, including owner filtering after selection.
        let expected_terminal_ids = connection
            .prepare(
                "SELECT id FROM runtime_monitors \
                 WHERE json_extract(record,'$.status')='completed' \
                 ORDER BY json_extract(record,'$.created') DESC LIMIT ?1",
            )
            .expect("canonical terminal selector")
            .query_map([MONITOR_TERMINAL_WINDOW as i64], |row| {
                row.get::<_, String>(0)
            })
            .expect("canonical terminal ids")
            .collect::<Result<BTreeSet<_>, _>>()
            .expect("canonical terminal selection");
        assert_eq!(expected_terminal_ids.len(), MONITOR_TERMINAL_WINDOW);
        assert!(expected_terminal_ids.contains("terminal-50"));
        connection
            .execute(
                "INSERT INTO runtime_monitors(id,record) VALUES('invalid-monitor','{')",
                [],
            )
            .expect("malformed monitor row");
        let malformed_hold_plan = connection
            .prepare(
                "EXPLAIN QUERY PLAN SELECT source.id FROM sync_entities AS entity \
                 CROSS JOIN runtime_monitors AS source \
                 WHERE entity.collection='monitor' AND entity.deleted=0 \
                   AND source.id=entity.id \
                   AND CASE WHEN json_valid(source.record) \
                            THEN json_type(source.record)!='object' ELSE 1 END",
            )
            .expect("malformed monitor hold plan")
            .query_map([], |row| row.get::<_, String>(3))
            .expect("malformed monitor hold plan rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("malformed monitor hold plan read");
        assert!(
            malformed_hold_plan
                .iter()
                .any(|line| line.contains("SEARCH source")),
            "malformed-row protection must probe by source id: {malformed_hold_plan:?}"
        );

        let transaction = connection.transaction().expect("projection transaction");
        for (id, value) in [
            ("stale", json!({"agent":"active"})),
            ("invalid-monitor", json!({"preserve":"prior"})),
            ("invalid-owner", json!({"preserve":"owner-prior"})),
        ] {
            SyncDocumentsRepository::put_entity(&transaction, "monitor", id, value, false)
                .expect("prior canonical monitor entity");
        }
        maintain_monitor_window(&transaction, &|_, _, _, _| true)
            .expect("bounded monitor projection");

        let count: i64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM sync_entities WHERE collection='monitor' AND deleted=0",
                [],
                |row| row.get(0),
            )
            .expect("live projected monitors");
        assert_eq!(count, 206);
        for id in ["deleted-owner", "missing-owner"] {
            let exists: bool = transaction
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sync_entities WHERE collection='monitor' AND id=?1 AND deleted=0)",
                    [id],
                    |row| row.get(0),
                )
                .expect("owner-filtered projection query");
            assert!(!exists, "inactive or missing owner must not project {id}");
        }
        for id in ["active-0", "active-104", "invalid-owner", "invalid-monitor"] {
            let exists: bool = transaction
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sync_entities WHERE collection='monitor' AND id=?1 AND deleted=0)",
                    [id],
                    |row| row.get(0),
                )
                .expect("retained monitor query");
            assert!(exists, "active or malformed source must retain {id}");
        }
        let actual_terminal_ids = transaction
            .prepare(
                "SELECT id FROM sync_entities WHERE collection='monitor' AND id GLOB 'terminal-*' AND deleted=0",
            )
            .expect("projected terminal ids")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("projected terminal rows")
            .collect::<Result<BTreeSet<_>, _>>()
            .expect("projected terminal selection");
        let expected_active_terminal_ids = expected_terminal_ids
            .into_iter()
            .filter(|id| id != "terminal-50")
            .collect::<BTreeSet<_>>();
        assert_eq!(actual_terminal_ids, expected_active_terminal_ids);
        let stale_deleted: bool = transaction
            .query_row(
                "SELECT deleted=1 FROM sync_entities WHERE collection='monitor' AND id='stale'",
                [],
                |row| row.get(0),
            )
            .expect("stale canonical tombstone");
        assert!(stale_deleted, "unselected canonical rows still tombstone");
        let invalid_payload: String = transaction
            .query_row(
                "SELECT payload FROM sync_entities WHERE collection='monitor' AND id='invalid-monitor' AND deleted=0",
                [],
                |row| row.get(0),
            )
            .expect("invalid source keeps existing canonical entity");
        assert_eq!(
            serde_json::from_str::<Value>(&invalid_payload).expect("prior payload")["value"],
            json!({"preserve":"prior"})
        );
        let invalid_owner_payload: String = transaction
            .query_row(
                "SELECT payload FROM sync_entities WHERE collection='monitor' AND id='invalid-owner' AND deleted=0",
                [],
                |row| row.get(0),
            )
            .expect("invalid owner keeps existing canonical entity");
        assert_eq!(
            serde_json::from_str::<Value>(&invalid_owner_payload).expect("prior payload")["value"],
            json!({"preserve":"owner-prior"})
        );
    }

    #[test]
    fn indexed_monitor_window_matches_python_tie_boundary_and_filters_owner_after_selection() {
        let mut connection = Connection::open_in_memory().expect("database");
        connection
            .execute_batch(
                "CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE INDEX runtime_monitor_status ON runtime_monitors(
                     json_extract(record,'$.status'),json_extract(record,'$.created'));
                 CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE sync_entities(
                     collection TEXT NOT NULL,id TEXT NOT NULL,seq INTEGER NOT NULL,
                     hash TEXT NOT NULL,payload TEXT,deleted INTEGER NOT NULL DEFAULT 0,
                     PRIMARY KEY(collection,id));
                 CREATE TABLE sync_documents(seq INTEGER PRIMARY KEY);
                 CREATE TABLE sync_versions(seq INTEGER PRIMARY KEY);
                 CREATE TABLE sync_entity_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
                 INSERT INTO runtime_agents VALUES
                     ('active','{\"id\":\"active\",\"deletedAt\":null}'),
                     ('deleted','{\"id\":\"deleted\",\"deletedAt\":1}');",
            )
            .expect("indexed projection fixture");
        for index in 0..104 {
            let id = format!("terminal-{index}");
            let agent = if index == 50 { "deleted" } else { "active" };
            connection
                .execute(
                    "INSERT INTO runtime_monitors(id,record) VALUES(?1,?2)",
                    rusqlite::params![
                        id,
                        json!({"id":id,"agent":agent,"status":"completed","created":10})
                            .to_string()
                    ],
                )
                .expect("indexed monitor row");
        }
        let expected_terminal_ids = connection
            .prepare(
                "SELECT id FROM runtime_monitors \
                 WHERE json_extract(record,'$.status')='completed' \
                 ORDER BY json_extract(record,'$.created') DESC LIMIT ?1",
            )
            .expect("canonical terminal selector")
            .query_map([MONITOR_TERMINAL_WINDOW as i64], |row| {
                row.get::<_, String>(0)
            })
            .expect("canonical terminal ids")
            .collect::<Result<BTreeSet<_>, _>>()
            .expect("canonical terminal selection");
        assert_eq!(expected_terminal_ids.len(), MONITOR_TERMINAL_WINDOW);
        assert!(expected_terminal_ids.contains("terminal-50"));

        let transaction = connection.transaction().expect("projection transaction");
        maintain_monitor_window(&transaction, &|_, _, _, _| true)
            .expect("indexed monitor projection");
        let actual_ids = transaction
            .prepare("SELECT id FROM sync_entities WHERE collection='monitor' AND deleted=0")
            .expect("projected terminal ids")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("projected terminal rows")
            .collect::<Result<BTreeSet<_>, _>>()
            .expect("projected terminal set");
        let expected_active_ids = expected_terminal_ids
            .into_iter()
            .filter(|id| id != "terminal-50")
            .collect::<BTreeSet<_>>();
        assert_eq!(actual_ids, expected_active_ids);
    }

    #[test]
    fn sync_projection_skips_malformed_persisted_rows() {
        let mut connection = Connection::open_in_memory().expect("open test database");
        connection
            .execute_batch("CREATE TABLE runtime_agents (id TEXT, record TEXT);")
            .expect("create source table");
        connection
            .execute(
                "INSERT INTO runtime_agents (id, record) VALUES ('broken-agent', '{')",
                [],
            )
            .expect("insert malformed source row");
        let transaction = connection.transaction().expect("begin test transaction");

        assert!(
            projection_records(&transaction, "runtime_agents")
                .expect("sync projection ignores one malformed stored row")
                .is_empty()
        );
        assert!(matches!(
            stored_records(&transaction, "runtime_agents"),
            Err(StoreError::InvalidSyncObject { scope, id })
                if scope == "runtime_agents" && id == "broken-agent"
        ));
    }

    #[tokio::test]
    async fn entity_projection_skips_unused_task_snapshot_and_keeps_task_window_behavior() {
        let root = tempfile::tempdir().expect("isolated state directory");
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("open isolated runtime store");
        let agent = json!({"id":"agent","rootId":"agent","epoch":0,"status":"idle","isLead":true});
        let snapshot = ProjectVaultSnapshot::default();

        store
            .transaction(move |transaction| {
                let agent = studio_runtime_store::AgentRecord::from_value(agent.clone())?;
                studio_runtime_store::AgentRepository::put(transaction, &agent)?;
                transaction.execute(
                    "INSERT INTO runtime_tasks(id,record) VALUES(?1,?2)",
                    rusqlite::params![
                        "task",
                        json!({"id":"task","agent":"agent","status":"running","created":1})
                            .to_string()
                    ],
                )?;
                Ok(())
            })
            .await
            .expect("seed task-window fixture");

        let state_dir = root.path().to_string_lossy().into_owned();
        let snapshot_for_first_maintenance = snapshot.clone();
        let statements = store
            .transaction(move |transaction| {
                TRACED_SQL.with_borrow_mut(Vec::clear);
                transaction.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(trace_sql));
                let rooms = crate::sync_entity_context::RoomDirectory::load(transaction)?;
                maintain_entity_projections_with_server_id(
                    transaction,
                    &state_dir,
                    None,
                    &snapshot_for_first_maintenance,
                    &rooms,
                    |_, _, _, _| true,
                )?;
                let payload: String = transaction.query_row(
                    "SELECT payload FROM sync_entities WHERE collection='task' AND id='task' AND deleted=0",
                    [],
                    |row| row.get(0),
                )?;
                let payload: Value = serde_json::from_str(&payload)?;
                assert_eq!(payload["value"]["status"], "running");
                transaction.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, None);
                Ok(TRACED_SQL.with_borrow_mut(std::mem::take))
            })
            .await
            .expect("maintain canonical task projection");

        assert!(statements.iter().any(|sql| {
            sql.starts_with("SELECT t.id,t.record FROM runtime_tasks t JOIN runtime_agents")
        }));
        assert!(
            !statements
                .iter()
                .any(|sql| { sql == "SELECT id,record FROM runtime_tasks ORDER BY rowid" })
        );

        let state_dir = root.path().to_string_lossy().into_owned();
        let snapshot_for_second_maintenance = snapshot.clone();
        store
            .transaction(move |transaction| {
                transaction.execute(
                    "UPDATE runtime_tasks SET record=?1 WHERE id='task'",
                    [json!({"id":"task","agent":"agent","status":"completed","created":2}).to_string()],
                )?;
                let rooms = crate::sync_entity_context::RoomDirectory::load(transaction)?;
                maintain_entity_projections_with_server_id(
                    transaction,
                    &state_dir,
                    None,
                    &snapshot_for_second_maintenance,
                    &rooms,
                    |_, _, _, _| true,
                )?;
                let payload: String = transaction.query_row(
                    "SELECT payload FROM sync_entities WHERE collection='task' AND id='task' AND deleted=0",
                    [],
                    |row| row.get(0),
                )?;
                let payload: Value = serde_json::from_str(&payload)?;
                assert_eq!(payload["value"]["status"], "completed");

                let mut deleted_agent = studio_runtime_store::AgentRepository::get(
                    transaction,
                    &studio_runtime_store::AgentId::from("agent"),
                )?
                .expect("fixture agent exists");
                deleted_agent.extra.insert("deletedAt".to_owned(), json!(3));
                studio_runtime_store::AgentRepository::put(transaction, &deleted_agent)?;
                let rooms = crate::sync_entity_context::RoomDirectory::load(transaction)?;
                maintain_entity_projections_with_server_id(
                    transaction,
                    &state_dir,
                    None,
                    &snapshot_for_second_maintenance,
                    &rooms,
                    |_, _, _, _| true,
                )?;
                let deleted: bool = transaction.query_row(
                    "SELECT deleted FROM sync_entities WHERE collection='task' AND id='task'",
                    [],
                    |row| row.get(0),
                )?;
                assert!(deleted);
                Ok(())
            })
            .await
            .expect("task-window eligibility transition remains canonical");
    }

    #[test]
    fn task_window_fast_path_preserves_full_rebuild_results_for_stable_running_updates() {
        let root = tempfile::tempdir().expect("isolated state directory");
        let store = studio_runtime_store::RuntimeStore::open(Some(root.path()))
            .expect("open isolated runtime store");
        store
            .transaction(|transaction| {
                for id in ["live", "deleting"] {
                    let agent = studio_runtime_store::AgentRecord::from_value(json!({
                        "id": id,
                        "rootId": id,
                        "epoch": 0,
                        "status": "idle",
                        "isLead": true
                    }))?;
                    studio_runtime_store::AgentRepository::put(transaction, &agent)?;
                }
                for index in 0..105 {
                    let id = format!("history-{index}");
                    transaction.execute(
                        "INSERT INTO runtime_tasks(id,record) VALUES(?1,?2)",
                        rusqlite::params![
                            id,
                            json!({
                                "id": id,
                                "agent": "live",
                                "status": "completed",
                                "created": index
                            })
                            .to_string()
                        ],
                    )?;
                }
                for (id, agent) in [("running-stable", "live"), ("running-deleting", "deleting")] {
                    transaction.execute(
                        "INSERT INTO runtime_tasks(id,record) VALUES(?1,?2)",
                        rusqlite::params![
                            id,
                            json!({
                                "id": id,
                                "agent": agent,
                                "status": "running",
                                "created": 200
                            })
                            .to_string()
                        ],
                    )?;
                }
                maintain_task_window(transaction, &|_, _, _, _| true)
            })
            .expect("seed complete task-window projection");

        let task_projection = |connection: &Connection| {
            connection
                .prepare(
                    "SELECT id,payload,deleted FROM sync_entities WHERE collection='task' ORDER BY id",
                )
                .expect("task projection rows")
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        (row.get::<_, Option<String>>(1)?, row.get::<_, bool>(2)?),
                    ))
                })
                .expect("task projection query")
                .collect::<Result<BTreeMap<_, _>, _>>()
                .expect("task projection snapshot")
        };
        let full_window_was_traced = || {
            TRACED_SQL.with_borrow(|statements| {
                statements.iter().any(|sql| {
                    sql.starts_with("SELECT t.id,t.record FROM runtime_tasks t JOIN runtime_agents")
                })
            })
        };

        TRACED_SQL.with_borrow_mut(Vec::clear);
        let direct_running_projection = store
            .transaction(|transaction| {
                transaction.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(trace_sql));
                let previous_raw: String = transaction.query_row(
                    "SELECT record FROM runtime_tasks WHERE id='running-stable'",
                    [],
                    |row| row.get(0),
                )?;
                let previous: Value = serde_json::from_str(&previous_raw)?;
                let mut updated = previous.clone();
                updated["tail"] = json!("streaming progress");
                transaction.execute(
                    "UPDATE runtime_tasks SET record=?1 WHERE id='running-stable'",
                    [updated.to_string()],
                )?;
                maintain_task_window_after_write(
                    transaction,
                    Some(&previous),
                    &updated,
                    &|_, _, _, _| true,
                )?;
                assert!(
                    !full_window_was_traced(),
                    "stable running write must not rescan history"
                );
                let direct = task_projection(transaction);
                maintain_task_window(transaction, &|_, _, _, _| true)?;
                assert_eq!(task_projection(transaction), direct);
                Ok(direct)
            })
            .expect("stable running update stays equivalent to a full rebuild");
        assert_eq!(direct_running_projection.len(), 102);

        TRACED_SQL.with_borrow_mut(Vec::clear);
        store
            .transaction(|transaction| {
                transaction.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(trace_sql));
                let raw: String = transaction.query_row(
                    "SELECT record FROM runtime_tasks WHERE id='running-stable'",
                    [],
                    |row| row.get(0),
                )?;
                let task: Value = serde_json::from_str(&raw)?;
                put_projection(
                    transaction,
                    "task",
                    "running-stable",
                    json!({}),
                    true,
                    &|_, _, _, _| true,
                )?;
                maintain_task_window_after_write(
                    transaction,
                    Some(&task),
                    &task,
                    &|_, _, _, _| true,
                )?;
                assert!(
                    full_window_was_traced(),
                    "missing projection must force rebuild"
                );
                let repaired = task_projection(transaction);
                maintain_task_window(transaction, &|_, _, _, _| true)?;
                assert_eq!(task_projection(transaction), repaired);
                assert!(
                    repaired
                        .get("running-stable")
                        .is_some_and(|(_, deleted)| !deleted)
                );
                Ok(())
            })
            .expect("missing stable-running projection is repaired by full rebuild");

        let compare_after_full_write = |id: &str, status: &str, created: i64| {
            TRACED_SQL.with_borrow_mut(Vec::clear);
            store
                .transaction(|transaction| {
                    transaction.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(trace_sql));
                    let previous_raw: Option<String> = transaction
                        .query_row(
                            "SELECT record FROM runtime_tasks WHERE id=?1",
                            [id],
                            |row| row.get(0),
                        )
                        .optional()?;
                    let previous = previous_raw
                        .as_deref()
                        .map(serde_json::from_str::<Value>)
                        .transpose()?;
                    let mut task = previous
                        .clone()
                        .unwrap_or_else(|| json!({"id":id,"agent":"live"}));
                    task["status"] = json!(status);
                    task["created"] = json!(created);
                    transaction.execute(
                        "INSERT INTO runtime_tasks(id,record) VALUES(?1,?2) \
                         ON CONFLICT(id) DO UPDATE SET record=excluded.record",
                        rusqlite::params![id, task.to_string()],
                    )?;
                    maintain_task_window_after_write(
                        transaction,
                        previous.as_ref(),
                        &task,
                        &|_, _, _, _| true,
                    )?;
                    assert!(
                        full_window_was_traced(),
                        "eligibility/rank change must rebuild"
                    );
                    let updated = task_projection(transaction);
                    maintain_task_window(transaction, &|_, _, _, _| true)?;
                    assert_eq!(task_projection(transaction), updated);
                    Ok(())
                })
                .expect("changed task must match a full window rebuild");
        };

        compare_after_full_write("running-stable", "completed", 300);
        compare_after_full_write("new-latest", "completed", 301);

        TRACED_SQL.with_borrow_mut(Vec::clear);
        store
            .transaction(|transaction| {
                transaction.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(trace_sql));
                let previous_raw: String = transaction.query_row(
                    "SELECT record FROM runtime_tasks WHERE id='running-deleting'",
                    [],
                    |row| row.get(0),
                )?;
                let previous: Value = serde_json::from_str(&previous_raw)?;
                let mut deleting_agent = studio_runtime_store::AgentRepository::get(
                    transaction,
                    &studio_runtime_store::AgentId::from("deleting"),
                )?
                .expect("agent exists");
                deleting_agent
                    .extra
                    .insert("deletedAt".to_owned(), json!(1));
                studio_runtime_store::AgentRepository::put(transaction, &deleting_agent)?;
                maintain_task_window_after_write(
                    transaction,
                    Some(&previous),
                    &previous,
                    &|_, _, _, _| true,
                )?;
                assert!(
                    full_window_was_traced(),
                    "agent eligibility change must rebuild"
                );
                let projection = task_projection(transaction);
                assert!(
                    projection
                        .get("running-deleting")
                        .is_some_and(|(_, deleted)| *deleted)
                );
                maintain_task_window(transaction, &|_, _, _, _| true)?;
                assert_eq!(task_projection(transaction), projection);
                Ok(())
            })
            .expect("deleted-agent eligibility change must match a full rebuild");
    }

    #[test]
    fn sync_vault_snapshot_skips_malformed_persisted_projects() {
        let connection = Connection::open_in_memory().expect("open test database");
        connection
            .execute_batch("CREATE TABLE runtime_projects (id TEXT, record TEXT);")
            .expect("create source table");
        connection
            .execute(
                "INSERT INTO runtime_projects (id, record) VALUES ('broken-project', '{')",
                [],
            )
            .expect("insert malformed project row");

        let snapshot = ProjectVaultSnapshot::capture_for_sync(Path::new("/unused"), &connection)
            .expect("sync snapshot ignores malformed stored project");
        let mut connection = connection;
        let transaction = connection.transaction().expect("begin sync transaction");
        snapshot
            .recheck(&transaction)
            .expect("malformed row stays outside the valid project snapshot");
    }

    #[test]
    fn draft_identity_matches_row_key_and_allows_extension_members() {
        let payload = json!({"device":"phone","session":"chat","id":"phone:tab:chat","text":"draft","extra":true});
        assert!(validate_draft("phone:tab:chat", &payload).is_ok());
        assert!(validate_draft("phone:chat", &json!({})).is_err());
        assert!(validate_draft("phone:chat", &Value::Null).is_err());
        assert!(validate_draft("phone:chat", &json!({"device":"other","session":"chat"})).is_err());
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

    fn preference_field(name: &str, value: Value, timestamp: i64) -> Map<String, Value> {
        Map::from_iter([(
            name.to_owned(),
            json!({"value":value,"timestamp":timestamp,"writer":"sync-policy-test"}),
        )])
    }

    #[tokio::test]
    async fn provider_update_preference_notifies_only_after_effective_user_value_change() {
        let root = tempfile::tempdir().expect("isolated state directory");
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("open isolated runtime store");
        let notifications = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let notification = |notifications: &std::sync::Arc<std::sync::atomic::AtomicUsize>| {
            let notifications = std::sync::Arc::clone(notifications);
            move || {
                notifications.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        };
        let key = crate::provider_versions::PROVIDER_UPDATE_PREFERENCE_KEY;

        let merged = push_preferences(
            &store,
            PreferenceScope::User,
            preference_field(key, json!(false), 1),
            |_| true,
            notification(&notifications),
        )
        .await
        .expect("provider preference writes");
        assert_eq!(
            merged[key]["value"], false,
            "the existing preference field map is returned"
        );
        assert_eq!(
            notifications.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "absent/default-enabled to disabled commits and wakes once"
        );

        let _ = push_preferences(
            &store,
            PreferenceScope::User,
            preference_field(key, json!(false), 2),
            |_| true,
            notification(&notifications),
        )
        .await
        .expect("same effective value with newer metadata writes");
        let _ = push_preferences(
            &store,
            PreferenceScope::User,
            preference_field(key, json!(false), 2),
            |_| true,
            notification(&notifications),
        )
        .await
        .expect("identical preference update is a no-op");
        let _ = push_preferences(
            &store,
            PreferenceScope::User,
            preference_field(
                r#"["codex-studio-preferences-v1","theme"]"#,
                json!("dark"),
                3,
            ),
            |_| true,
            notification(&notifications),
        )
        .await
        .expect("unrelated appearance preference writes");
        assert_eq!(
            notifications.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "metadata-only and unrelated changes do not wake the provider monitor"
        );
        let _ = push_preferences(
            &store,
            PreferenceScope::User,
            preference_field(key, json!(true), 4),
            |_| true,
            notification(&notifications),
        )
        .await
        .expect("provider preference can be re-enabled");
        assert_eq!(
            notifications.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "enabled transition also wakes after commit"
        );

        let malformed_fields = Map::from_iter([(
            key.to_owned(),
            json!({"value":"invalid","timestamp":4,"writer":"legacy"}),
        )]);
        let malformed = canonical_json(&json!({
            "collection":"uiPreferences",
            "id":"user",
            "value":{"fields":malformed_fields}
        }));
        let malformed_for_write = malformed.clone();
        store
            .transaction(move |transaction| {
                SyncDocumentsRepository::put_preferences(
                    transaction,
                    "user",
                    &malformed_for_write,
                )?;
                Ok(())
            })
            .await
            .expect("malformed persisted fixture seeds");
        let result = push_preferences(
            &store,
            PreferenceScope::User,
            preference_field(key, json!(true), 5),
            |_| true,
            notification(&notifications),
        )
        .await;
        assert!(
            matches!(result, Err(SyncPolicyError::InvalidStoredObject { .. })),
            "malformed existing preference remains refused"
        );
        assert_eq!(
            notifications.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "a rolled-back malformed stored preference does not wake"
        );
        let payload = store.read().expect("preference reader");
        assert_eq!(
            SyncDocumentsRepository::preference_payload(&payload, "user")
                .expect("stored payload query"),
            Some(malformed),
            "the refused update leaves malformed stored data untouched"
        );
    }

    #[test]
    fn cursor_limits_match_scope_contract() {
        assert_eq!(effective_limit(SYNC_ENTITY_SCOPE, 900), 500);
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

    #[tokio::test]
    async fn maintained_project_projection_matches_python_vault_corpus_and_surfaces_bad_records() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../tests/vault/project-view-cases.json"))
                .expect("project vault corpus");
        for case in cases {
            let root = tempfile::tempdir().expect("isolated state directory");
            let store = RuntimeStore::open(Some(root.path()))
                .await
                .expect("open isolated runtime store");
            let project_id = case["projectId"]
                .as_str()
                .expect("fixture project id")
                .to_owned();
            let folder = store
                .root()
                .join("linux-vm/layr-projects")
                .join(&project_id);
            fs::create_dir_all(&folder).unwrap();
            for (name, value) in case["records"].as_object().unwrap() {
                fs::write(folder.join(name), serde_json::to_vec(value).unwrap()).unwrap();
            }
            if let Some(records) = case.get("rawRecords").and_then(Value::as_object) {
                for (name, contents) in records {
                    fs::write(folder.join(name), contents.as_str().unwrap()).unwrap();
                }
            }
            let workspace = root.path().join("workspace");
            fs::create_dir(&workspace).unwrap();
            let mut project = case["project"].clone();
            project["path"] = json!(workspace.to_string_lossy());
            let prior = project.clone();
            let snapshot = ProjectVaultSnapshot::capture(
                store.root(),
                &[(project["id"].as_str().unwrap().to_owned(), project.clone())],
            );

            let state_dir = store.root().to_string_lossy().into_owned();
            let expected = case["expected"].clone();
            let expected_error = case.get("expectedError").is_some();
            let transaction_project_id = project["id"]
                .as_str()
                .expect("fixture project record ID")
                .to_owned();
            let result = store.transaction(move |transaction| {
                ProjectsRepository::put_project(transaction, &project)?;
                let mut prior = prior;
                prior["layrReadiness"] = json!({"state":"ready"});
                prior["layrSync"] = json!({"state":"ok"});
                prior["vault"] = json!({"state":"stale"});
                SyncDocumentsRepository::put_entity(
                    transaction,
                    "project",
                    project["id"].as_str().unwrap(),
                    prior,
                    false,
                )?;
                let rooms = crate::sync_entity_context::RoomDirectory::load(transaction)?;
                maintain_entity_projections_with_server_id(
                    transaction,
                    &state_dir,
                    None,
                    &snapshot,
                    &rooms,
                    |_, _, _, _| true,
                )?;
                let payload: String = transaction.query_row(
                    "SELECT payload FROM sync_entities WHERE collection='project' AND id=?1 AND deleted=0",
                    [transaction_project_id.as_str()],
                    |row| row.get(0),
                )?;
                let payload: Value = serde_json::from_str(&payload)?;
                assert_eq!(payload["value"]["vault"], expected);
                Ok(())
            }).await;
            if expected_error {
                assert!(matches!(
                    result,
                    Err(StoreError::InvalidStoredRecord {
                        object: "vault.json",
                        id,
                    }) if id == project_id
                ));
            } else {
                result.expect("maintain project projection");
            }
        }
    }
}
