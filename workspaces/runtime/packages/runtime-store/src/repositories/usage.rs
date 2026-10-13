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
    #[error("invalid stored usage continuation")]
    InvalidRecord,
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
    let mut agent =
        AgentRepository::get(transaction, &id)?.ok_or(UsageRepositoryError::UnknownAgent)?;
    let raw = transaction
        .query_row(
            "SELECT record FROM runtime_capacity_retries WHERE id=?1 AND agent=?2",
            params![retry_id, agent_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or(UsageRepositoryError::UnknownContinuation)?;
    let mut retry: Value =
        serde_json::from_str(&raw).map_err(|_| UsageRepositoryError::InvalidRecord)?;
    if retry.get("id").and_then(Value::as_str) != Some(retry_id)
        || retry.get("status").and_then(Value::as_str).is_none()
    {
        return Err(UsageRepositoryError::InvalidRecord);
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
    let is_current = agent
        .extra
        .get("capacityRetry")
        .and_then(|value| value.get("id"))
        .and_then(Value::as_str)
        == Some(retry_id);
    if is_current
        && retry.get("status").and_then(Value::as_str) == Some("scheduled")
        && retry.get("claimedAt").is_none_or(Value::is_null)
    {
        retry["status"] = Value::String("cancelled".to_owned());
        retry["dueAt"] = Value::Null;
        retry["reason"] = Value::Null;
        retry["updatedAt"] = json!(now);
        transaction.execute(
            "UPDATE runtime_capacity_retries SET record=?1 WHERE id=?2 AND agent=?3",
            params![
                serde_json::to_string(&retry).map_err(|_| UsageRepositoryError::InvalidRecord)?,
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
    let mut agent =
        AgentRepository::get(transaction, &id)?.ok_or(UsageRepositoryError::UnknownAgent)?;
    let raw = transaction
        .query_row(
            "SELECT record FROM runtime_usage_resumes WHERE id=?1 AND agent=?2",
            params![resume_id, agent_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or(UsageRepositoryError::UnknownContinuation)?;
    let mut resume: Value =
        serde_json::from_str(&raw).map_err(|_| UsageRepositoryError::InvalidRecord)?;
    if resume.get("id").and_then(Value::as_str) != Some(resume_id)
        || resume.get("status").and_then(Value::as_str).is_none()
    {
        return Err(UsageRepositoryError::InvalidRecord);
    }
    let is_current = agent
        .extra
        .get("usageResume")
        .and_then(|value| value.get("id"))
        .and_then(Value::as_str)
        == Some(resume_id);
    if is_current {
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
                        .map_err(|_| UsageRepositoryError::InvalidRecord)?,
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
