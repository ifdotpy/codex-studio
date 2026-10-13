//! Existing usage continuation tables. This repository never creates them.

use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use thiserror::Error;

use crate::StoreError;
use crate::records::{AgentId, AgentRepository};
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};

const CAPACITY_RETRY_TABLE: &[TableRequirement] = &[TableRequirement {
    name: "runtime_capacity_retries",
    columns: &["id", "agent", "record"],
}];
const USAGE_RESUME_TABLE: &[TableRequirement] = &[TableRequirement {
    name: "runtime_usage_resumes",
    columns: &["id", "agent", "record"],
}];

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[
        ConditionalTableRequirement {
            table_exists: "runtime_capacity_retries",
            tables: CAPACITY_RETRY_TABLE,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_usage_resumes",
            tables: USAGE_RESUME_TABLE,
        },
    ],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Debug, Error)]
pub enum UsageRepositoryError {
    #[error("usage continuation storage is not initialized")]
    MissingFamily,
    #[error("continuation cancellation needs the event driver")]
    EventDriverRequired,
    #[error("unknown agent")]
    UnknownAgent,
    #[error("unknown usage continuation")]
    UnknownContinuation,
    #[error("invalid stored object {table}/{id}")]
    InvalidStoredObject { table: &'static str, id: String },
    #[error("SQLite operation failed")]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub type UsageRepositoryResult<T> = Result<T, UsageRepositoryError>;

pub fn cancel_capacity_retry(
    transaction: &Transaction<'_>,
    agent_id: &str,
    retry_id: &str,
    now: f64,
) -> UsageRepositoryResult<Value> {
    require_table(transaction, "runtime_capacity_retries")?;
    let id = AgentId::from(agent_id);
    let mut agent = AgentRepository::get(transaction, &id)
        .map_err(|_| invalid("runtime_agents", agent_id))?
        .ok_or(UsageRepositoryError::UnknownAgent)?;
    if agent.id.0 != agent_id {
        return Err(invalid("runtime_agents", agent_id));
    }
    let raw = transaction
        .query_row(
            "SELECT record FROM runtime_capacity_retries WHERE id=?1 AND agent=?2",
            params![retry_id, agent_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or(UsageRepositoryError::UnknownContinuation)?;
    let mut retry: Value =
        serde_json::from_str(&raw).map_err(|_| invalid("runtime_capacity_retries", retry_id))?;
    if !validate_retry(&retry, retry_id) {
        return Err(invalid("runtime_capacity_retries", retry_id));
    }
    let is_current = agent
        .extra
        .get("capacityRetry")
        .and_then(|value| value.get("id"))
        .and_then(Value::as_str)
        == Some(retry_id);
    let changes_state = is_current
        && retry.get("status").and_then(Value::as_str) == Some("scheduled")
        && retry.get("claimedAt").is_none_or(Value::is_null);
    if changes_state {
        let embedded = agent
            .extra
            .get("capacityRetry")
            .ok_or_else(|| invalid("runtime_agents", agent_id))?;
        if !validate_retry(embedded, retry_id) {
            return Err(invalid("runtime_agents", agent_id));
        }
        if agent.extra.get("isLead") != Some(&Value::Bool(true))
            && agent
                .extra
                .get("parentId")
                .and_then(Value::as_str)
                .is_some()
        {
            return Err(UsageRepositoryError::EventDriverRequired);
        }
        retry["status"] = Value::String("cancelled".to_owned());
        retry["dueAt"] = Value::Null;
        retry["reason"] = Value::Null;
        retry["updatedAt"] = json!(now);
        transaction.execute(
            "UPDATE runtime_capacity_retries SET record=?1 WHERE id=?2 AND agent=?3",
            params![
                serde_json::to_string(&retry)
                    .map_err(|_| invalid("runtime_capacity_retries", retry_id))?,
                retry_id,
                agent_id
            ],
        )?;
        agent
            .extra
            .insert("capacityRetry".to_owned(), retry.clone());
        AgentRepository::put(transaction, &agent)?;
    }
    Ok(retry)
}

pub fn disable_usage_resume(
    transaction: &Transaction<'_>,
    agent_id: &str,
    resume_id: &str,
    now: f64,
) -> UsageRepositoryResult<Value> {
    require_table(transaction, "runtime_usage_resumes")?;
    let id = AgentId::from(agent_id);
    let mut agent = AgentRepository::get(transaction, &id)
        .map_err(|_| invalid("runtime_agents", agent_id))?
        .ok_or(UsageRepositoryError::UnknownAgent)?;
    if agent.id.0 != agent_id {
        return Err(invalid("runtime_agents", agent_id));
    }
    let raw = transaction
        .query_row(
            "SELECT record FROM runtime_usage_resumes WHERE id=?1 AND agent=?2",
            params![resume_id, agent_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or(UsageRepositoryError::UnknownContinuation)?;
    let mut resume: Value =
        serde_json::from_str(&raw).map_err(|_| invalid("runtime_usage_resumes", resume_id))?;
    if !validate_resume(&resume, resume_id) {
        return Err(invalid("runtime_usage_resumes", resume_id));
    }
    let is_current = agent
        .extra
        .get("usageResume")
        .and_then(|value| value.get("id"))
        .and_then(Value::as_str)
        == Some(resume_id);
    if is_current {
        let embedded = agent
            .extra
            .get("usageResume")
            .ok_or_else(|| invalid("runtime_agents", agent_id))?;
        if !validate_resume(embedded, resume_id) {
            return Err(invalid("runtime_agents", agent_id));
        }
        if agent.extra.get("usageResumeEnabled") == Some(&Value::Bool(false)) {
            return Ok(embedded.clone());
        }
        agent
            .extra
            .insert("usageResumeEnabled".to_owned(), Value::Bool(false));
        if resume.get("status").and_then(Value::as_str) == Some("scheduled") {
            resume["status"] = Value::String("cancelled".to_owned());
            resume["dueAt"] = Value::Null;
            resume["reason"] = Value::String("Automatic resume is off for this chat.".to_owned());
            resume["updatedAt"] = json!(now);
            transaction.execute(
                "UPDATE runtime_usage_resumes SET record=?1 WHERE id=?2 AND agent=?3",
                params![
                    serde_json::to_string(&resume)
                        .map_err(|_| invalid("runtime_usage_resumes", resume_id))?,
                    resume_id,
                    agent_id
                ],
            )?;
        }
        agent.extra.insert("usageResume".to_owned(), resume.clone());
        AgentRepository::put(transaction, &agent)?;
    } else {
        return Ok(agent
            .extra
            .get("usageResume")
            .cloned()
            .unwrap_or(Value::Null));
    }
    Ok(resume)
}

inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "usage",
        requirements: &REQUIREMENTS,
    }
}

fn invalid(table: &'static str, id: &str) -> UsageRepositoryError {
    UsageRepositoryError::InvalidStoredObject {
        table,
        id: id.to_owned(),
    }
}

fn validate_retry(value: &Value, expected_id: &str) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if object.get("id").and_then(Value::as_str) != Some(expected_id)
        || !matches!(
            object.get("status").and_then(Value::as_str),
            Some(
                "scheduled"
                    | "starting"
                    | "finished"
                    | "cancelled"
                    | "unknown"
                    | "exhausted"
                    | "failed"
            )
        )
    {
        return false;
    }
    for field in [
        "acceptedTurnId",
        "accountKey",
        "cause",
        "cwd",
        "threadId",
        "turnId",
    ] {
        if object.get(field).is_some_and(|value| !value.is_string()) {
            return false;
        }
    }
    for field in ["dueAt", "claimedAt", "updatedAt"] {
        if object
            .get(field)
            .is_some_and(|value| !value.is_null() && value.as_f64().is_none())
        {
            return false;
        }
    }
    for field in ["attempt", "epoch", "maxAttempts", "waits"] {
        if object
            .get(field)
            .is_some_and(|value| !value.is_null() && value.as_u64().is_none())
        {
            return false;
        }
    }
    if object
        .get("reason")
        .is_some_and(|value| !value.is_null() && !value.is_string())
        || object.get("taskClaims").is_some_and(|value| {
            value
                .as_array()
                .is_none_or(|items| items.iter().any(|item| !item.is_string()))
        })
        || object
            .get("settings")
            .is_some_and(|value| !value.is_object())
    {
        return false;
    }
    true
}

fn validate_resume(value: &Value, expected_id: &str) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let required_string = |field: &str| object.get(field).and_then(Value::as_str).is_some();
    if object.get("id").and_then(Value::as_str) != Some(expected_id)
        || !matches!(
            object.get("status").and_then(Value::as_str),
            Some("scheduled" | "started" | "cancelled" | "unknown")
        )
        || !matches!(
            object.get("cause").and_then(Value::as_str),
            Some("usage_limit" | "rate_limit" | "auth")
        )
        || !["accountKey", "threadId", "turnId"]
            .into_iter()
            .all(required_string)
        || object.get("epoch").and_then(Value::as_u64).is_none()
        || object.get("failedAt").and_then(Value::as_f64).is_none()
    {
        return false;
    }
    for field in ["authRefreshMarker", "reason"] {
        if object
            .get(field)
            .is_some_and(|value| !value.is_null() && !value.is_string())
        {
            return false;
        }
    }
    for field in [
        "dueAt",
        "lastCheckedAt",
        "plannedAt",
        "resetAt",
        "updatedAt",
    ] {
        if object
            .get(field)
            .is_some_and(|value| !value.is_null() && value.as_f64().is_none())
        {
            return false;
        }
    }
    for field in ["failedAt", "proofAt", "startedAt"] {
        if object
            .get(field)
            .is_some_and(|value| !value.is_null() && value.as_f64().is_none())
        {
            return false;
        }
    }
    if object
        .get("authAttempt")
        .is_some_and(|value| !value.is_null() && value.as_u64().is_none())
        || object
            .get("waitingForAuth")
            .is_some_and(|value| !value.is_null() && !value.is_boolean())
        || object.get("taskClaims").is_some_and(|value| {
            value
                .as_array()
                .is_none_or(|items| items.iter().any(|item| !item.is_string()))
        })
    {
        return false;
    }
    true
}

fn require_table(transaction: &Transaction<'_>, name: &str) -> UsageRepositoryResult<()> {
    let exists = transaction
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            [name],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        Ok(())
    } else {
        Err(UsageRepositoryError::MissingFamily)
    }
}

#[cfg(test)]
mod tests {
    use super::{UsageRepositoryError, cancel_capacity_retry, disable_usage_resume};
    use rusqlite::Connection;
    use serde_json::{Value, json};

    const AGENT: &str = "agent-1";

    fn retry(id: &str, status: &str, claimed_at: Value) -> Value {
        json!({"id":id,"status":status,"dueAt":123.0,"claimedAt":claimed_at,
            "threadId":"thread","turnId":"turn","accountKey":"default","epoch":1,
            "cause":"serverOverloaded","attempt":1,"maxAttempts":3,"cwd":"/tmp",
            "settings":{},"taskClaims":[]})
    }

    fn resume(id: &str, status: &str) -> Value {
        json!({"id":id,"status":status,"dueAt":123.0,"threadId":"thread","turnId":"turn",
            "accountKey":"default","epoch":1,"cause":"usage_limit","failedAt":100.0,
            "authAttempt":0,"authRefreshMarker":null,"plannedAt":null,"resetAt":null,"reason":null})
    }

    fn database(extra: Value) -> Connection {
        let connection = Connection::open_in_memory().expect("sqlite");
        connection.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            CREATE TABLE runtime_capacity_retries(id TEXT PRIMARY KEY,agent TEXT NOT NULL,record TEXT NOT NULL);
            CREATE TABLE runtime_usage_resumes(id TEXT PRIMARY KEY,agent TEXT NOT NULL,record TEXT NOT NULL);").expect("tables");
        let mut agent = json!({"id":AGENT,"rootId":AGENT,"epoch":1,"status":"idle"});
        agent
            .as_object_mut()
            .expect("agent fixture is an object")
            .extend(
                extra
                    .as_object()
                    .expect("extra fixture is an object")
                    .clone(),
            );
        connection
            .execute(
                "INSERT INTO runtime_agents VALUES(?1,?2)",
                rusqlite::params![AGENT, agent.to_string()],
            )
            .expect("agent");
        connection
    }

    fn insert(connection: &Connection, table: &str, id: &str, value: &Value) {
        connection
            .execute(
                &format!("INSERT INTO {table}(id,agent,record) VALUES(?1,?2,?3)"),
                rusqlite::params![id, AGENT, value.to_string()],
            )
            .expect("continuation");
    }

    fn read(connection: &Connection, table: &str, id: &str) -> String {
        connection
            .query_row(
                &format!("SELECT record FROM {table} WHERE id=?1"),
                [id],
                |row| row.get(0),
            )
            .expect("continuation row")
    }

    #[test]
    fn obsolete_and_claimed_child_retries_return_stored_data_without_driver_dependency() {
        for (id, status, claimed, current) in [
            ("obsolete", "scheduled", Value::Null, "other"),
            ("claimed", "scheduled", json!(120.0), "claimed"),
            ("cancelled", "cancelled", Value::Null, "cancelled"),
        ] {
            let stored = retry(id, status, claimed);
            let current_retry = if current == id {
                stored.clone()
            } else {
                retry(current, "scheduled", Value::Null)
            };
            let connection = database(json!({"parentId":"parent","capacityRetry":current_retry}));
            insert(&connection, "runtime_capacity_retries", id, &stored);
            let tx = connection.unchecked_transaction().expect("transaction");
            let result = cancel_capacity_retry(&tx, AGENT, id, 200.0).expect("stored result");
            assert_eq!(result, stored);
            assert_eq!(
                read(&tx, "runtime_capacity_retries", id),
                stored.to_string()
            );
            tx.rollback().expect("rollback");
        }
    }

    #[test]
    fn child_retry_that_would_change_state_names_event_driver_and_writes_nothing() {
        let stored = retry("retry", "scheduled", Value::Null);
        let connection = database(json!({"parentId":"parent","capacityRetry":stored}));
        insert(&connection, "runtime_capacity_retries", "retry", &stored);
        let tx = connection.unchecked_transaction().expect("transaction");
        assert!(matches!(
            cancel_capacity_retry(&tx, AGENT, "retry", 200.0),
            Err(UsageRepositoryError::EventDriverRequired)
        ));
        assert_eq!(
            read(&tx, "runtime_capacity_retries", "retry"),
            stored.to_string()
        );
        tx.rollback().expect("rollback");
    }

    #[test]
    fn malformed_capacity_retry_names_its_stored_object_and_writes_nothing() {
        let mut malformed = retry("retry", "scheduled", Value::Null);
        malformed["dueAt"] = json!("tomorrow");
        let connection = database(
            json!({"isLead":true,"capacityRetry":retry("retry", "scheduled", Value::Null)}),
        );
        insert(&connection, "runtime_capacity_retries", "retry", &malformed);
        let before_agent = read(&connection, "runtime_agents", AGENT);
        let before_retry = read(&connection, "runtime_capacity_retries", "retry");
        let tx = connection.unchecked_transaction().expect("transaction");
        assert!(matches!(cancel_capacity_retry(&tx, AGENT, "retry", 200.0),
            Err(UsageRepositoryError::InvalidStoredObject { table: "runtime_capacity_retries", ref id }) if id == "retry"));
        assert_eq!(read(&tx, "runtime_agents", AGENT), before_agent);
        assert_eq!(read(&tx, "runtime_capacity_retries", "retry"), before_retry);
        tx.rollback().expect("rollback");
    }

    #[test]
    fn replayed_applied_disable_returns_saved_resume_without_writing_agent_or_row() {
        let saved = resume("resume", "cancelled");
        let connection = database(json!({"usageResumeEnabled":false,"usageResume":saved}));
        insert(&connection, "runtime_usage_resumes", "resume", &saved);
        let before_agent = read(&connection, "runtime_agents", AGENT);
        let before_resume = read(&connection, "runtime_usage_resumes", "resume");
        let tx = connection.unchecked_transaction().expect("transaction");
        assert_eq!(
            disable_usage_resume(&tx, AGENT, "resume", 200.0).expect("saved disable result"),
            saved
        );
        assert_eq!(read(&tx, "runtime_agents", AGENT), before_agent);
        assert_eq!(read(&tx, "runtime_usage_resumes", "resume"), before_resume);
        tx.rollback().expect("rollback");
    }

    #[test]
    fn malformed_complete_continuation_is_named_and_rejected_before_any_write() {
        let mut malformed = resume("resume", "scheduled");
        malformed["dueAt"] = json!("tomorrow");
        let connection = database(
            json!({"usageResumeEnabled":true,"usageResume":resume("resume", "scheduled")}),
        );
        insert(&connection, "runtime_usage_resumes", "resume", &malformed);
        let before_agent = read(&connection, "runtime_agents", AGENT);
        let before_resume = read(&connection, "runtime_usage_resumes", "resume");
        let tx = connection.unchecked_transaction().expect("transaction");
        assert!(matches!(disable_usage_resume(&tx, AGENT, "resume", 200.0),
            Err(UsageRepositoryError::InvalidStoredObject { table: "runtime_usage_resumes", ref id }) if id == "resume"));
        assert_eq!(read(&tx, "runtime_agents", AGENT), before_agent);
        assert_eq!(read(&tx, "runtime_usage_resumes", "resume"), before_resume);
        tx.rollback().expect("rollback");
    }

    #[test]
    fn malformed_embedded_continuation_names_agent_and_is_rejected_before_any_write() {
        let mut malformed = resume("resume", "scheduled");
        malformed["failedAt"] = json!("yesterday");
        let row = resume("resume", "scheduled");
        let connection = database(json!({"usageResumeEnabled":true,"usageResume":malformed}));
        insert(&connection, "runtime_usage_resumes", "resume", &row);
        let before_agent = read(&connection, "runtime_agents", AGENT);
        let tx = connection.unchecked_transaction().expect("transaction");
        assert!(matches!(disable_usage_resume(&tx, AGENT, "resume", 200.0),
            Err(UsageRepositoryError::InvalidStoredObject { table: "runtime_agents", ref id }) if id == AGENT));
        assert_eq!(read(&tx, "runtime_agents", AGENT), before_agent);
        assert_eq!(
            read(&tx, "runtime_usage_resumes", "resume"),
            row.to_string()
        );
        tx.rollback().expect("rollback");
    }
}
