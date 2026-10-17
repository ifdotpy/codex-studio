//! Persisted agent creation and management.
//!
//! Project creation uses the local server identity supplied by its caller and
//! falls back to the persisted agent `serverId` when that identity is absent.
//! If neither exists, creation fails before writing the project. This path does
//! not create identity files: it depends on the runtime's existing identity
//! source. The store-held identity from `origin/fix/rust-reads-no-writer` will
//! provide the authoritative value once that change is merged.
mod operations;
pub(crate) use operations::DEFAULT_LEAD_MODEL;
pub use operations::*;
pub(crate) use operations::{image_provision_is_current, persist_image_provision_failure};
pub(crate) use operations::{set_image_workspace_fields, set_vm_workspace_fields};

use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, AgentStatus, EventWriteRepository, RuntimeStore,
    StoreError, SyncDocumentsRepository, Transaction,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MutationError {
    #[error("This request id has different content")]
    Conflict,
    #[error("No saved creation intent; inspect the existing agent before creating another")]
    MissingCreationIntent,
    #[error("This conversation was deleted")]
    Deleted,
    #[error("Unknown managed agent")]
    UnknownAgent,
    #[error("Configure the lead agent")]
    LeadRequired,
    #[error("A rename request id is required")]
    RequestIdRequired,
    #[error("Invalid host agent creation field: {0}")]
    InvalidCreation(&'static str),
    #[error("Invalid host agent creation field: base commit could not be resolved")]
    BaseUnresolved,
    #[error("The selected model is not available for the selected account")]
    ModelUnavailable,
    #[error("Not implemented: requires {0}")]
    CapabilityUnavailable(&'static str),
    #[error("The account or project changed; reload before creating the agent")]
    IdentityChanged,
    #[error("Internal server error")]
    Internal,
    #[error("The team changed. Reload before saving")]
    Stale,
    #[error("Switch this chat to another account before disabling its current account")]
    CurrentAccountExcluded,
    #[error("Wait for this turn to end before changing execution settings")]
    InFlight,
    #[error("invalid stored object {table}/{id}")]
    InvalidStoredObject { table: &'static str, id: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}
pub type CreateError = MutationError;
pub type RenameError = MutationError;
pub type SettingsError = MutationError;

pub(crate) fn canonical_project_directory(path: &std::path::Path) -> Result<String, &'static str> {
    let path = path
        .canonicalize()
        .map_err(|_| "Select an existing project directory")?;
    if !path.is_dir() {
        return Err("Select an existing project directory");
    }
    path.to_str()
        .map(str::to_owned)
        .ok_or("Select an existing project directory")
}

fn new_id() -> Result<String, CreateError> {
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

/// Normalize the persisted mode fields using the same rules as Python mode_fields.
pub fn mode_fields(record: &mut Value) {
    if record.get("isLead").and_then(Value::as_bool) != Some(true) {
        if let Some(fields) = record.as_object_mut() {
            fields.remove("concurrency");
        }
        return;
    }
    let single = record.get("agentMode").and_then(Value::as_str) == Some("single");
    let limit = if single
        && record
            .get("subagentConcurrencyVersion")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            < 2
    {
        0
    } else {
        record
            .get("concurrency")
            .and_then(Value::as_i64)
            .unwrap_or(if single { 0 } else { 32 })
    };
    record["concurrency"] = json!(limit);
    record["subagentConcurrencyVersion"] = json!(2);
    record["agentMode"] = json!(if limit == 0 { "single" } else { "multi" });
    record["agentModeSupported"] = json!(true);
    if record.get("agentModeRevision").is_none() {
        record["agentModeRevision"] = json!(0);
    }
    if record.get("maxAgentsExplicit").is_none() {
        record["maxAgentsExplicit"] = json!(
            record
                .get("maxAgents")
                .and_then(Value::as_i64)
                .unwrap_or(64)
                != 64
        );
    }
}

/// Use the shared canonical renderer builder for every agent publication.
pub(crate) fn sync_projection(
    transaction: &Transaction<'_>,
    record: &Value,
) -> Result<Value, StoreError> {
    let mut record = record.clone();
    mode_fields(&mut record);
    let server_id = RuntimeStore::transaction_local_server_id(transaction)?;
    let context =
        crate::sync_entity_context::agent_view_context(transaction, &record, server_id.as_deref())?;
    crate::sync_views::agent_entity_view(&record, &context)
        .ok_or_else(|| StoreError::SchemaMismatch("agent sync projection has no id".to_owned()))
}

pub fn publish_agent(transaction: &Transaction<'_>, record: &Value) -> Result<Value, StoreError> {
    let id = record
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::SchemaMismatch("agent sync projection has no id".to_owned()))?;
    let deleted = crate::event_write::truthy(record.get("deletedAt"));
    let projection = sync_projection(transaction, record)?;
    SyncDocumentsRepository::put_entity(transaction, "agent", id, projection.clone(), deleted)?;
    Ok(projection)
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

fn replay_lead(
    transaction: &Connection,
    request_id: &str,
    signature: &str,
    validate: impl Fn(&str, &Value) -> bool,
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
                table: "runtime_lead_requests",
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

fn replay_agent(
    transaction: &Connection,
    id: &str,
    request: &Value,
    validate: impl Fn(&str, &Value) -> bool,
) -> Result<Option<Value>, CreateError> {
    let Some(record) = load_agent_value(transaction, id, &validate)? else {
        return Ok(None);
    };
    if crate::event_write::truthy(record.get("deletedAt")) {
        return Err(CreateError::Deleted);
    }
    let saved = record
        .get("creationIntent")
        .ok_or(CreateError::MissingCreationIntent)?;
    if saved != request {
        return Err(CreateError::Conflict);
    }
    Ok(Some(record))
}

fn load_agent_value(
    connection: &Connection,
    id: &str,
    validate: &impl Fn(&str, &Value) -> bool,
) -> Result<Option<Value>, MutationError> {
    load_record(
        connection,
        "runtime_agents",
        "PersistedAgentRecord",
        id,
        validate,
    )
}

fn load_record(
    connection: &Connection,
    table: &'static str,
    schema: &str,
    id: &str,
    validate: &impl Fn(&str, &Value) -> bool,
) -> Result<Option<Value>, MutationError> {
    let raw = connection
        .query_row(
            &format!("SELECT record FROM {table} WHERE id=?1"),
            [id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(StoreError::from)?;
    raw.map(|raw| decode_record(table, schema, id, &raw, validate))
        .transpose()
}

fn decode_record(
    table: &'static str,
    schema: &str,
    id: &str,
    raw: &str,
    validate: &impl Fn(&str, &Value) -> bool,
) -> Result<Value, MutationError> {
    let invalid = || MutationError::InvalidStoredObject {
        table,
        id: id.to_owned(),
    };
    let value: Value = serde_json::from_str(raw).map_err(|_| invalid())?;
    if value.get("id").and_then(Value::as_str) != Some(id) || !validate(schema, &value) {
        return Err(invalid());
    }
    Ok(value)
}

fn save_agent(
    transaction: &Transaction<'_>,
    record: &Value,
    validate: &impl Fn(&str, &Value) -> bool,
) -> Result<Value, MutationError> {
    if !validate("PersistedAgentRecord", record) {
        return Err(MutationError::InvalidStoredObject {
            table: "runtime_agents",
            id: record["id"].as_str().unwrap_or_default().to_owned(),
        });
    }
    let agent: AgentRecord = serde_json::from_value(record.clone()).map_err(StoreError::from)?;
    AgentRepository::put(transaction, &agent)?;
    Ok(publish_agent(transaction, record)?)
}

pub(crate) fn ensure_project(
    transaction: &Transaction<'_>,
    agent: &Value,
) -> Result<(), MutationError> {
    let path = agent
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::SchemaMismatch("new lead has no project path".to_owned()))?;
    if load_open_record(transaction, "runtime_projects", path)?.is_some() {
        return Ok(());
    }
    let account = agent
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let local_server_id = RuntimeStore::transaction_local_server_id(transaction)?;
    let server = local_server_id
        .as_deref()
        .or_else(|| agent.get("serverId").and_then(Value::as_str))
        .filter(|server| !server.is_empty())
        .ok_or_else(|| StoreError::SchemaMismatch("new lead has no server identity".to_owned()))?;
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(path);
    let project = json!({
        "id":path,
        "path":path,
        "name":name,
        "created":crate::event_write::unix_time().map_err(|error| StoreError::SchemaMismatch(error.to_string()))?,
        "accountKey":account,
        "accountRevision":1,
        "homeServerId":server,
        "locations":[{"serverId":server,"path":path,"projectId":path}],
        "locationsRevision":0
    });
    transaction
        .execute(
            "INSERT INTO runtime_projects(id,record) VALUES(?1,?2)",
            rusqlite::params![
                path,
                serde_json::to_string(&project).map_err(StoreError::from)?
            ],
        )
        .map_err(StoreError::from)?;
    SyncDocumentsRepository::put_entity(transaction, "project", path, project, false)?;
    Ok(())
}

fn load_open_record(
    connection: &Connection,
    table: &'static str,
    id: &str,
) -> Result<Option<Value>, MutationError> {
    let raw = connection
        .query_row(
            &format!("SELECT record FROM {table} WHERE id=?1"),
            [id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(StoreError::from)?;
    raw.map(|raw| {
        let invalid = || MutationError::InvalidStoredObject {
            table,
            id: id.to_owned(),
        };
        let record: Value = serde_json::from_str(&raw).map_err(|_| invalid())?;
        if !record.is_object() || record.get("id").and_then(Value::as_str) != Some(id) {
            return Err(invalid());
        }
        Ok(record)
    })
    .transpose()
}

/// Persist a lead and its first pending message atomically. The caller supplies
/// the fully validated, product-shaped record and controls account admission.
#[cfg(test)]
fn create_agent(
    transaction: &Transaction<'_>,
    id: &str,
    prompt: &str,
    record: Value,
    created: f64,
    validate: impl Fn(&str, &Value) -> bool,
) -> Result<Value, CreateError> {
    create_agent_with_identity(transaction, id, prompt, record, created, validate)
}

fn create_agent_with_identity(
    transaction: &Transaction<'_>,
    id: &str,
    prompt: &str,
    record: Value,
    created: f64,
    validate: impl Fn(&str, &Value) -> bool,
) -> Result<Value, CreateError> {
    if !validate("PersistedAgentRecord", &record) {
        return Err(CreateError::InvalidStoredObject {
            table: "runtime_agents",
            id: id.to_owned(),
        });
    }
    let agent: AgentRecord =
        serde_json::from_value(record.clone()).map_err(|_| CreateError::InvalidStoredObject {
            table: "runtime_agents",
            id: id.to_owned(),
        })?;
    let draft = record.get("needsTitle").and_then(Value::as_bool) == Some(true);
    let expected_status = if draft {
        AgentStatus::Idle
    } else {
        AgentStatus::Queued
    };
    let valid_root = if record.get("isLead").and_then(Value::as_bool) == Some(true)
        || record.get("parentId").is_none_or(Value::is_null)
    {
        agent.root_id.0 == id
    } else {
        !agent.root_id.0.is_empty()
    };
    if agent.id.0 != id || !valid_root || agent.status != expected_status {
        return Err(CreateError::InvalidStoredObject {
            table: "runtime_agents",
            id: id.to_owned(),
        });
    }
    if record.get("isLead").and_then(Value::as_bool) == Some(true) {
        ensure_project(transaction, &record)?;
    }
    AgentRepository::put(transaction, &agent)?;
    let id = record
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::SchemaMismatch("agent sync projection has no id".to_owned()))?;
    let deleted = crate::event_write::truthy(record.get("deletedAt"));
    let projection = sync_projection(transaction, &record)?;
    SyncDocumentsRepository::put_entity(transaction, "agent", id, projection.clone(), deleted)?;
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
    Ok(projection)
}

fn update_exclusions(
    transaction: &Transaction<'_>,
    id: &str,
    expected_account: Option<&str>,
    excluded: Option<&[String]>,
    non_defaults: bool,
    validate: impl Fn(&str, &Value) -> bool,
) -> Result<Value, SettingsError> {
    let mut record =
        load_agent_value(transaction, id, &validate)?.ok_or(SettingsError::UnknownAgent)?;
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
    if (excluded.is_none() || non_defaults)
        && (crate::event_write::truthy(record.get("inFlight"))
            || matches!(
                record.get("status").and_then(Value::as_str),
                Some("running" | "starting" | "approval")
            ))
    {
        return Err(SettingsError::InFlight);
    }
    let Some(excluded) = excluded else {
        return Ok(sync_projection(transaction, &record)?);
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
    save_agent(transaction, &record, &validate)
}

/// Apply the persisted team storage and token limits without provider calls.
fn configure_limits(
    transaction: &Transaction<'_>,
    id: &str,
    max_agents: Option<i64>,
    token_budget: Option<Option<i64>>,
    validate: impl Fn(&str, &Value) -> bool,
) -> Result<Value, SettingsError> {
    let mut root_value =
        load_agent_value(transaction, id, &validate)?.ok_or(SettingsError::UnknownAgent)?;
    if root_value
        .get("parentId")
        .is_some_and(|value| !value.is_null())
    {
        return Err(SettingsError::LeadRequired);
    }
    mode_fields(&mut root_value);
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
        let mut record = decode_record(
            "runtime_agents",
            "PersistedAgentRecord",
            &agent_id,
            &raw,
            &validate,
        )?;
        if agent_id != id && record.get("rootId").and_then(Value::as_str) != Some(id) {
            continue;
        }
        if agent_id == id {
            record = root_value.clone();
            if let Some(max_agents) = max_agents {
                record["maxAgents"] = json!(max_agents);
                if record.get("isLead").and_then(Value::as_bool) == Some(true) {
                    record["maxAgentsExplicit"] = json!(true);
                }
            }
            if let Some(token_budget) = token_budget {
                record["tokenBudget"] = token_budget.map_or(Value::Null, |value| json!(value));
            }
        } else {
            record
                .as_object_mut()
                .ok_or(SettingsError::InvalidStoredObject {
                    table: "runtime_agents",
                    id: agent_id.clone(),
                })?
                .remove("concurrency");
            if let Some(max_agents) = team_max_agents {
                record["maxAgents"] = json!(max_agents);
            }
            record["tokenBudget"] = team_token_budget.map_or(Value::Null, |value| json!(value));
        }
        save_agent(transaction, &record, &validate)?;
        if agent_id == id {
            root_value = record;
        }
    }
    Ok(json!({
        "id": id,
        "concurrency": root_value.get("concurrency").cloned().unwrap_or(json!(0)),
        "agentMode": if root_value.get("concurrency").and_then(Value::as_i64).unwrap_or(0) == 0 { "single" } else { "multi" },
        "agentModeRevision": root_value.get("agentModeRevision").cloned().unwrap_or(json!(0)),
        "maxAgents": root_value.get("maxAgents").cloned().unwrap_or(Value::Null),
        "tokenBudget": root_value.get("tokenBudget").cloned().unwrap_or(Value::Null),
    }))
}

fn rename(
    transaction: &Transaction<'_>,
    id: &str,
    name: Option<&str>,
    request_id: Option<&str>,
    signature: &str,
    validate: impl Fn(&str, &Value) -> bool,
) -> Result<Value, RenameError> {
    let mut record =
        load_agent_value(transaction, id, &validate)?.ok_or(RenameError::UnknownAgent)?;
    if let Some(request_id) = request_id
        && let Some(receipt) = EventWriteRepository::receipt(transaction, request_id)?
    {
        if receipt.signature != signature {
            return Err(RenameError::Conflict);
        }
        let result: Value = serde_json::from_str(&receipt.result).map_err(|_| {
            RenameError::InvalidStoredObject {
                table: "runtime_operation_receipts",
                id: request_id.to_owned(),
            }
        })?;
        return Ok(result);
    }

    let Some(agent) = AgentRepository::get(transaction, &AgentId::from(id))? else {
        return Err(RenameError::UnknownAgent);
    };
    if agent.id.0 != id {
        return Err(RenameError::InvalidStoredObject {
            table: "runtime_agents",
            id: id.to_owned(),
        });
    }
    if crate::event_write::truthy(agent.extra.get("deletedAt")) {
        return Err(RenameError::Deleted);
    }
    let Some(name) = name else {
        return Err(if request_id.is_some() {
            RenameError::InvalidCreation("name")
        } else {
            RenameError::RequestIdRequired
        });
    };

    record["name"] = json!(name);
    record["manualName"] = json!(true);
    record["needsTitle"] = json!(false);
    save_agent(transaction, &record, &validate)?;

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
#[allow(clippy::duplicate_mod)]
#[path = "../tests/support/prepared_python.rs"]
mod prepared_python_test_support;
#[cfg(test)]
pub(crate) use prepared_python_test_support::prepared_python;

#[cfg(test)]
mod tests {
    use super::{create_agent, ensure_project, replay_agent};
    use rusqlite::Connection;
    use serde_json::{Value, json};

    fn prepared_python(source: &std::path::Path) -> std::path::PathBuf {
        super::prepared_python_test_support::prepared_python(source)
    }
    fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("memory database opens");
        create_schema(&connection);
        connection
    }

    fn create_schema(connection: &Connection) {
        connection
            .execute_batch(
                "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,created REAL,epoch INTEGER,turn_id TEXT,error TEXT);
                 CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_lead_requests(id TEXT PRIMARY KEY,agent TEXT NOT NULL,signature TEXT NOT NULL);
                 CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT);
                 CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,PRIMARY KEY(collection,id));
                 CREATE TABLE sync_documents(seq INTEGER,scope TEXT,id TEXT,payload TEXT,deleted INTEGER);
                 CREATE TABLE sync_versions(seq INTEGER,scope TEXT,hash TEXT,deleted INTEGER,updated REAL);",
            )
            .expect("agent-create tables exist");
    }

    #[test]
    fn mode_fields_matches_python_on_schema_boundary_corpus() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let corpus =
            include_str!("../../../apps/server/tests/fixtures/rust-agents-create/mode-corpus.json");
        let mut actual: Vec<serde_json::Value> = serde_json::from_str(corpus).expect("mode corpus");
        for record in &mut actual {
            super::mode_fields(record);
        }
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let python = prepared_python(&source);
        let mut child = Command::new(python).args(["-c", "import json,sys; from codex_agent_modes import mode_fields; print(json.dumps([mode_fields(row) for row in json.load(sys.stdin)]))"])
            .env("PYTHONPATH", source).env("PYTHONDONTWRITEBYTECODE", "1")
            .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().expect("Python oracle starts");
        child
            .stdin
            .take()
            .expect("oracle stdin")
            .write_all(corpus.as_bytes())
            .expect("send corpus");
        let output = child.wait_with_output().expect("oracle exits");
        assert!(output.status.success());
        let expected: Vec<serde_json::Value> =
            serde_json::from_slice(&output.stdout).expect("oracle output");
        for (index, (actual, expected)) in actual.iter().zip(&expected).enumerate() {
            assert_eq!(actual, expected, "mode corpus input {index}");
        }
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
            super::replay_agent(&connection, "agent", &intent, |_, _| true)
                .expect("replay")
                .is_some()
        );
        for field in intent.as_object().expect("intent object").keys() {
            let mut changed = intent.clone();
            changed[field] = json!("changed");
            assert!(
                super::replay_agent(&connection, "agent", &changed, |_, _| true).is_err(),
                "{field}"
            );
        }
        let mut omitted = intent.clone();
        omitted.as_object_mut().expect("object").remove("maxAgents");
        assert!(super::replay_agent(&connection, "agent", &omitted, |_, _| true).is_err());
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
            super::replay_lead(&connection, "alias", "{}", |_, _| true),
            Err(super::CreateError::InvalidStoredObject {
                table: "runtime_lead_requests",
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
            "accountKey":"default", "serverId":"server-a", "isLead":true, "needsTitle":false, "autoWake":true,
            "creationIntent":{"name":"Agent A","prompt":"Start the task"}
        });
        let created = create_agent(
            &tx,
            "agent-a",
            "Start the task",
            record.clone(),
            10.0,
            |_, _| true,
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
        let project: String = connection
            .query_row(
                "SELECT payload FROM sync_entities WHERE collection='project' AND id='/project'",
                [],
                |row| row.get(0),
            )
            .expect("project projection exists");
        let project = serde_json::from_str::<serde_json::Value>(&project)
            .expect("project projection JSON")["value"]
            .clone();
        assert_eq!(project["homeServerId"], "server-a");
        assert_eq!(
            project["locations"],
            json!([{"serverId":"server-a","path":"/project","projectId":"/project"}])
        );
        assert_eq!(project["locationsRevision"], 0);

        let tx = connection.transaction().expect("replay transaction starts");
        super::replay_agent(&tx, "agent-a", &record["creationIntent"], |_, _| true)
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
        assert!(
            super::replay_agent(&tx, "agent-a", &changed["creationIntent"], |_, _| true).is_err()
        );
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

    #[test]
    fn project_creation_uses_persisted_agent_server_id_without_local_identity() {
        let mut connection = database();
        let transaction = connection.transaction().expect("transaction starts");
        let agent = json!({
            "id":"agent-a",
            "cwd":"/project",
            "accountKey":"default",
            "serverId":"persisted-server"
        });

        ensure_project(&transaction, &agent)
            .expect("persisted agent identity supplies project location");

        let payload: String = transaction
            .query_row(
                "SELECT payload FROM sync_entities WHERE collection='project' AND id='/project'",
                [],
                |row| row.get(0),
            )
            .expect("project sync projection exists");
        let project =
            serde_json::from_str::<Value>(&payload).expect("project projection JSON")["value"]
                .clone();
        assert_eq!(project["homeServerId"], "persisted-server");
        assert_eq!(
            project["locations"],
            json!([{
                "serverId":"persisted-server",
                "path":"/project",
                "projectId":"/project"
            }])
        );
        assert_eq!(project["locationsRevision"], 0);
    }

    #[test]
    fn creation_failpoints_rollback_every_row_and_retry_once_after_restart() {
        let failure_points = [
            ("agent row", "runtime_agents", "INSERT"),
            ("sync entity and sequence", "sync_entities", "INSERT"),
            ("initial event", "runtime_events", "INSERT"),
            ("event queue metadata", "runtime_event_meta", "INSERT"),
            ("agent queue", "runtime_agents", "UPDATE"),
        ];
        for (index, (boundary, table, operation)) in failure_points.into_iter().enumerate() {
            let state = tempfile::tempdir().expect("isolated state directory");
            let path = state
                .path()
                .join(format!("creation-failpoint-{index}.sqlite3"));
            let mut connection = Connection::open(&path).expect("failure database opens");
            create_schema(&connection);
            connection
                .execute_batch(&format!(
                    "CREATE TRIGGER injected_creation_failure AFTER {operation} ON {table}
                     BEGIN SELECT RAISE(ABORT,'injected creation failure'); END;"
                ))
                .expect("install targeted failpoint");
            let record = json!({
                "id":"agent-failpoint", "rootId":"parent", "parentId":"parent", "epoch":0,
                "status":"queued", "name":"Agent", "prompt":"Task", "cwd":"/project",
                "accountKey":"default", "isLead":false, "role":"implementer",
                "needsTitle":false, "autoWake":true,
                "creationIntent":{"name":"Agent","prompt":"Task"}
            });
            let transaction = connection
                .transaction()
                .expect("start failpoint transaction");
            assert!(
                create_agent(
                    &transaction,
                    "agent-failpoint",
                    "Task",
                    record.clone(),
                    10.0,
                    |_, _| true
                )
                .is_err(),
                "failpoint {index} ({boundary}): {table} {operation}"
            );
            drop(transaction);
            drop(connection);
            let connection = Connection::open(&path).expect("restart after failed transaction");
            for table in [
                "runtime_agents",
                "sync_entities",
                "runtime_events",
                "runtime_event_meta",
            ] {
                let count: i64 = connection
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })
                    .expect("count rows after rollback");
                assert_eq!(count, 0, "failpoint {index} left rows in {table}");
            }
            connection
                .execute_batch("DROP TRIGGER injected_creation_failure")
                .expect("remove failpoint");
            drop(connection);
            let mut connection = Connection::open(&path).expect("restart before retry");
            let transaction = connection.transaction().expect("start retry transaction");
            create_agent(
                &transaction,
                "agent-failpoint",
                "Task",
                record.clone(),
                10.0,
                |_, _| true,
            )
            .expect("retry succeeds");
            transaction.commit().expect("retry commits");
            drop(connection);
            let mut connection = Connection::open(&path).expect("restart before replay");
            let transaction = connection.transaction().expect("start replay transaction");
            replay_agent(
                &transaction,
                "agent-failpoint",
                &record["creationIntent"],
                |_, _| true,
            )
            .expect("same creation intent replays");
            transaction.commit().expect("replay commits");
            let (agents, events, projections): (i64, i64, i64) = connection
                .query_row(
                    "SELECT (SELECT COUNT(*) FROM runtime_agents),
                            (SELECT COUNT(*) FROM runtime_events),
                            (SELECT COUNT(*) FROM sync_entities)",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("count successful retry rows");
            assert_eq!(
                (agents, events, projections),
                (1, 1, 1),
                "failpoint {index}"
            );
            let (event_status, queue_metadata): (String, i64) = connection
                .query_row(
                    "SELECT e.status,
                            (SELECT COUNT(*) FROM runtime_event_meta WHERE id='agent-failpoint:initial')
                     FROM runtime_events e WHERE e.id='agent-failpoint:initial'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("queued event and metadata survive retry");
            assert_eq!(event_status, "pending");
            assert_eq!(queue_metadata, 1);
            drop(connection);
            std::fs::remove_file(path).expect("remove failpoint database");
        }
    }
}
