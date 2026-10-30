//! Durable capacity retry and usage resume records.

use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value, json};
use std::collections::BTreeMap;
use studio_runtime_schema::continuation_snapshot::{
    UsageContinuationCause, UsageContinuationStatus,
};
use studio_runtime_schema::queue_command::Field;
use thiserror::Error;

use crate::StoreError;
use crate::records::{AgentId, AgentRepository};
use crate::schema::{SchemaRequirements, TableRequirement};

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
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[CAPACITY_RETRY_TABLE[0], USAGE_RESUME_TABLE[0]],
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

// Cancellation historically accepts more than the bounded decision snapshot,
// but requires more than the permissive persisted record schema.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CancellableResume {
    id: String,
    account_key: String,
    thread_id: String,
    turn_id: String,
    epoch: u64,
    failed_at: Number,
    #[serde(deserialize_with = "studio_runtime_schema::string_enum::deserialize")]
    cause: UsageContinuationCause,
    #[serde(deserialize_with = "studio_runtime_schema::string_enum::deserialize")]
    status: UsageContinuationStatus,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    due_at: Field<Number>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    last_checked_at: Field<Number>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    planned_at: Field<Number>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    reset_at: Field<Number>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    updated_at: Field<Number>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    proof_at: Field<Number>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    started_at: Field<Number>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    auth_refresh_marker: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    reason: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    auth_attempt: Field<u64>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    waiting_for_auth: Field<bool>,
    #[serde(
        default,
        deserialize_with = "optional_claims",
        skip_serializing_if = "Option::is_none"
    )]
    task_claims: Option<Vec<String>>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

fn optional_claims<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Vec<String>>, D::Error> {
    Vec::<String>::deserialize(d).map(Some)
}

impl CancellableResume {
    fn cancel(&mut self, reason: &str, now: f64) {
        self.status = UsageContinuationStatus::Cancelled;
        self.due_at = Field::Null;
        self.reason = Field::Value(reason.to_owned());
        self.updated_at = Number::from_f64(now).map_or(Field::Null, Field::Value);
    }
}

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
    // Parse through Value first to retain the stored last-duplicate-key behavior.
    let raw_value: Value =
        serde_json::from_str(&raw).map_err(|_| invalid("runtime_usage_resumes", resume_id))?;
    let mut resume = decode_resume(raw_value, resume_id)
        .ok_or_else(|| invalid("runtime_usage_resumes", resume_id))?;
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
        if decode_resume(embedded.clone(), resume_id).is_none() {
            return Err(invalid("runtime_agents", agent_id));
        }
        if agent.usage_resume_enabled == Some(false) {
            return Ok(embedded.clone());
        }
        agent.usage_resume_enabled = Some(false);
        if resume.status == UsageContinuationStatus::Scheduled {
            resume.cancel("Automatic resume is off for this chat.", now);
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
        agent.extra.insert("usageResume".to_owned(), json!(resume));
        AgentRepository::put(transaction, &agent)?;
    } else {
        return Ok(agent
            .extra
            .get("usageResume")
            .cloned()
            .unwrap_or(Value::Null));
    }
    Ok(json!(resume))
}

/// Cancel a pending resume when an agent moves accounts, preserving the user's
/// automatic-resume preference. The caller owns the encompassing transaction.
pub fn cancel_usage_resume(
    transaction: &Transaction<'_>,
    agent_id: &str,
    resume_id: &str,
    reason: &str,
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
    // Parse through Value first to retain the stored last-duplicate-key behavior.
    let raw_value: Value =
        serde_json::from_str(&raw).map_err(|_| invalid("runtime_usage_resumes", resume_id))?;
    let mut resume = decode_resume(raw_value, resume_id)
        .ok_or_else(|| invalid("runtime_usage_resumes", resume_id))?;
    if agent
        .extra
        .get("usageResume")
        .and_then(|value| value.get("id"))
        .and_then(Value::as_str)
        != Some(resume_id)
    {
        return Ok(agent
            .extra
            .get("usageResume")
            .cloned()
            .unwrap_or(Value::Null));
    }
    if !matches!(
        resume.status,
        UsageContinuationStatus::Scheduled | UsageContinuationStatus::Started
    ) {
        return Ok(json!(resume));
    }
    resume.cancel(reason, now);
    transaction.execute(
        "UPDATE runtime_usage_resumes SET record=?1 WHERE id=?2 AND agent=?3",
        params![
            serde_json::to_string(&resume)
                .map_err(|_| invalid("runtime_usage_resumes", resume_id))?,
            resume_id,
            agent_id
        ],
    )?;
    agent.extra.insert("usageResume".to_owned(), json!(resume));
    AgentRepository::put(transaction, &agent)?;
    Ok(json!(resume))
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
    value.get("id").and_then(Value::as_str) == Some(expected_id)
        && studio_runtime_schema::validate("continuation/CapacityContinuation", value)
            .unwrap_or(false)
}

fn decode_resume(value: Value, expected_id: &str) -> Option<CancellableResume> {
    let studio_runtime_schema::json_object::JsonObject(resume) =
        studio_runtime_schema::json_object::JsonObject::<CancellableResume>::deserialize(value)
            .ok()?;
    (resume.id == expected_id).then_some(resume)
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
    use super::{
        UsageRepositoryError, cancel_capacity_retry, cancel_usage_resume, disable_usage_resume,
    };
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
        connection.execute_batch(&crate::agent_columns::fixture_sql("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            CREATE TABLE runtime_capacity_retries(id TEXT PRIMARY KEY,agent TEXT NOT NULL,record TEXT NOT NULL);
            CREATE TABLE runtime_usage_resumes(id TEXT PRIMARY KEY,agent TEXT NOT NULL,record TEXT NOT NULL);")).expect("tables");
        connection
            .execute_batch(crate::agent_volatile::agent_volatile_table_sql())
            .expect("volatile agent table creates");
        let mut agent =
            json!({"accountKey":"default","id":AGENT,"provider":"codex","rootId":AGENT,"epoch":1,"status":"idle"});
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
                "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
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
    fn disabling_resume_updates_the_typed_agent_field() {
        let saved = resume("resume", "scheduled");
        let connection = database(json!({"usageResumeEnabled":true,"usageResume":saved}));
        insert(&connection, "runtime_usage_resumes", "resume", &saved);
        let tx = connection.unchecked_transaction().expect("transaction");
        disable_usage_resume(&tx, AGENT, "resume", 200.0).expect("disable result");
        let agent: serde_json::Value =
            serde_json::from_str(&read(&tx, "runtime_agents", AGENT)).unwrap();
        assert_eq!(agent["usageResumeEnabled"], false);
        assert_eq!(agent["usageResume"]["status"], "cancelled");
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

    #[test]
    fn cancellation_preserves_extensions_presence_and_numeric_spelling() {
        let mut stored = resume("resume", "started");
        stored["failedAt"] = json!(1.0);
        stored["proofAt"] = json!(3.0);
        stored["taskClaims"] = json!(["", "task"]);
        stored["future"] = json!({"epoch": 7.0, "arbitrary": [null, false]});
        let db = database(json!({"usageResumeEnabled":true,"usageResume":stored}));
        insert(&db, "runtime_usage_resumes", "resume", &stored);
        let tx = db.unchecked_transaction().unwrap();
        let result = cancel_usage_resume(&tx, AGENT, "resume", "moved", 200.0).unwrap();
        let mut expected = stored;
        expected["status"] = json!("cancelled");
        expected["dueAt"] = Value::Null;
        expected["reason"] = json!("moved");
        expected["updatedAt"] = json!(200.0);
        assert_eq!(result, expected);
        assert_eq!(
            serde_json::from_str::<Value>(&read(&tx, "runtime_usage_resumes", "resume")).unwrap(),
            expected
        );
        assert_eq!(
            serde_json::from_str::<Value>(&read(&tx, "runtime_agents", AGENT)).unwrap()["usageResume"],
            expected
        );
        tx.rollback().unwrap();
    }

    #[test]
    fn cancellation_rejects_invalid_identity_and_claims_without_writes() {
        for (field, bad) in [
            ("epoch", json!(1.0)),
            ("taskClaims", Value::Null),
            ("status", json!({"scheduled":{}})),
            ("id", json!("other")),
        ] {
            let mut row = resume("resume", "scheduled");
            row[field] = bad;
            let db = database(json!({"usageResume":resume("resume", "scheduled")}));
            insert(&db, "runtime_usage_resumes", "resume", &row);
            let agent_before = read(&db, "runtime_agents", AGENT);
            let record_before = read(&db, "runtime_usage_resumes", "resume");
            let tx = db.unchecked_transaction().unwrap();
            assert!(
                matches!(
                    cancel_usage_resume(&tx, AGENT, "resume", "moved", 200.0),
                    Err(UsageRepositoryError::InvalidStoredObject {
                        table: "runtime_usage_resumes",
                        ..
                    })
                ),
                "{field}"
            );
            assert_eq!(read(&tx, "runtime_agents", AGENT), agent_before);
            assert_eq!(read(&tx, "runtime_usage_resumes", "resume"), record_before);
            tx.rollback().unwrap();
        }
    }
}
