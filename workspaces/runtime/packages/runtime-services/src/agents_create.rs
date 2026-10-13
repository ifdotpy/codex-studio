//! Persisted agent rename behavior.

use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, AgentStatus, EventWriteRepository, QueueEvent,
    QueueEventId, StoreError, SyncDocumentsRepository, Transaction,
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
    #[error("invalid stored object {object}")]
    InvalidStoredObject { object: &'static str },
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Debug, Error)]
pub enum CreateError {
    #[error("This creation id has different settings")]
    Conflict,
    #[error("This conversation was deleted")]
    Deleted,
    #[error("invalid stored object {object}")]
    InvalidStoredObject { object: &'static str },
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
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
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
    if let Some(existing) = AgentRepository::get(transaction, &AgentId::from(id))? {
        let existing = serde_json::to_value(existing).map_err(StoreError::from)?;
        if !validate(&existing) {
            return Err(CreateError::InvalidStoredObject {
                object: "runtime_agents",
            });
        }
        if existing
            .get("deletedAt")
            .is_some_and(|value| truthy(Some(value)))
        {
            return Err(CreateError::Deleted);
        }
        if existing.get("id").and_then(Value::as_str) != Some(id)
            || existing.get("prompt").and_then(Value::as_str) != Some(prompt)
            || existing.get("name") != record.get("name")
            || existing.get("cwd") != record.get("cwd")
            || existing.get("accountKey") != record.get("accountKey")
            || existing.get("quickCreateRequest") != record.get("quickCreateRequest")
            || [
                "role",
                "yoloMode",
                "workspaceMode",
                "concurrency",
                "maxAgents",
                "tokenBudget",
            ]
            .iter()
            .any(|field| existing.get(*field) != record.get(*field))
        {
            return Err(CreateError::Conflict);
        }
        return Ok(existing);
    }
    if !validate(&record) {
        return Err(CreateError::InvalidStoredObject {
            object: "runtime_agents",
        });
    }
    let agent: AgentRecord =
        serde_json::from_value(record.clone()).map_err(|_| CreateError::InvalidStoredObject {
            object: "runtime_agents",
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
        });
    }
    AgentRepository::put(transaction, &agent)?;
    if !draft {
        let event_id = format!("{id}:initial");
        let inserted = EventWriteRepository::insert_event(
            transaction,
            &QueueEvent {
                id: QueueEventId::from(event_id),
                agent: AgentId::from(id),
                kind: "user".to_owned(),
                text: prompt.to_owned(),
                status: "pending".to_owned(),
                created,
                epoch: agent.epoch,
                turn_id: None,
                error: None,
            },
        )?;
        if !inserted {
            return Err(CreateError::Conflict);
        }
    }
    SyncDocumentsRepository::put_entity(transaction, "agent", id, record.clone(), false)?;
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
    #[error("invalid stored object {object}")]
    InvalidStoredObject { object: &'static str },
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
    let mut agent = AgentRepository::get(transaction, &AgentId::from(id))?
        .ok_or(SettingsError::UnknownAgent)?;
    let mut record = serde_json::to_value(&agent).map_err(StoreError::from)?;
    if record.get("id").and_then(Value::as_str) != Some(id) {
        return Err(SettingsError::InvalidStoredObject {
            object: "runtime_agents",
        });
    }
    if !validate(&record) {
        return Err(SettingsError::InvalidStoredObject {
            object: "runtime_agents",
        });
    }
    if record
        .get("deletedAt")
        .is_some_and(|value| truthy(Some(value)))
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
    if record.get("isLead").and_then(Value::as_bool) != Some(true) {
        return Err(SettingsError::LeadRequired);
    }
    let Some(excluded) = excluded else {
        return Ok(record);
    };
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
        });
    }
    agent =
        serde_json::from_value(record.clone()).map_err(|_| SettingsError::InvalidStoredObject {
            object: "runtime_agents",
        })?;
    AgentRepository::put(transaction, &agent)?;
    SyncDocumentsRepository::put_entity(transaction, "agent", id, record.clone(), false)?;
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
    let root = AgentRepository::get(transaction, &AgentId::from(id))?
        .ok_or(SettingsError::UnknownAgent)?;
    let mut root_value = serde_json::to_value(&root).map_err(StoreError::from)?;
    if root_value
        .get("parentId")
        .is_some_and(|value| !value.is_null())
    {
        return Err(SettingsError::LeadRequired);
    }
    if root_value
        .get("deletedAt")
        .is_some_and(|value| truthy(Some(value)))
    {
        return Err(SettingsError::Deleted);
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
            })?;
        if record.get("id").and_then(Value::as_str) != Some(agent_id.as_str()) {
            return Err(SettingsError::InvalidStoredObject {
                object: "runtime_agents",
            });
        }
        if agent_id != id && record.get("rootId").and_then(Value::as_str) != Some(id) {
            continue;
        }
        if agent_id == id {
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
            });
        }
        let stored: AgentRecord = serde_json::from_value(record.clone()).map_err(|_| {
            SettingsError::InvalidStoredObject {
                object: "runtime_agents",
            }
        })?;
        AgentRepository::put(transaction, &stored)?;
        SyncDocumentsRepository::put_entity(transaction, "agent", &agent_id, record, false)?;
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
            }
        })?;
        return Ok(result);
    }

    let Some(mut agent) = AgentRepository::get(transaction, &AgentId::from(id))? else {
        return Err(RenameError::UnknownAgent);
    };
    if agent.id.0 != id {
        return Err(RenameError::InvalidStoredObject {
            object: "runtime_agents",
        });
    }
    if truthy(agent.extra.get("deletedAt")) {
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
    EventWriteRepository::set_agent_sync_name(transaction, id, name)?;

    let mut result = json!({"id": id, "name": name});
    if let Some(request_id) = request_id {
        result["request_id"] = Value::String(request_id.to_owned());
        result["status"] = Value::String("applied".to_owned());
        let result_text = serde_json::to_string(&result).map_err(StoreError::from)?;
        EventWriteRepository::put_receipt(transaction, request_id, signature, &result_text)?;
    }
    Ok(result)
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Number(value)) => value.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
        Some(Value::Bool(true)) => true,
    }
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
                 CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,PRIMARY KEY(collection,id));
                 CREATE TABLE sync_documents(seq INTEGER,scope TEXT,id TEXT,payload TEXT,deleted INTEGER);
                 CREATE TABLE sync_versions(seq INTEGER,scope TEXT,hash TEXT,deleted INTEGER,updated REAL);",
            )
            .expect("agent-create tables exist");
        connection
    }

    #[test]
    fn agent_creation_is_atomic_replayable_and_publishes_sync_projection() {
        let mut connection = database();
        let tx = connection.transaction().expect("transaction starts");
        let record = json!({
            "id":"agent-a", "rootId":"agent-a", "epoch":0, "status":"queued",
            "name":"Agent A", "prompt":"Start the task", "cwd":"/project",
            "accountKey":"default", "isLead":true, "needsTitle":false,
            "quickCreateRequest":null
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
        changed["name"] = json!("Changed");
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
