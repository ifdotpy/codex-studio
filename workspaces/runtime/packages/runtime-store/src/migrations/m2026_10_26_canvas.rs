//! Source-complete Canvas transitions retained until their supported source/restore formats retire.
use crate::{
    AgentId, AgentRepository, AgentStatus, StoreError, StoreResult, SyncDocumentsRepository,
    Transaction,
};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};

pub(super) fn preflight_retired(db: &rusqlite::Connection) -> StoreResult<()> {
    // These are exactly the JSON record families the retired-wait/cache SQL
    // expressions consume. Malformed pending input must fail before any target writes.
    for (table, field, predicate) in [
        ("runtime_usage_resumes", "record", "1"),
        (
            "runtime_operation_receipts",
            "result",
            "signature='provider-callback'",
        ),
    ] {
        if !crate::schema::table_exists(db, table)? {
            continue;
        }
        let mut statement = db.prepare(&format!(
            "SELECT rowid,{field} FROM {table} WHERE {predicate}"
        ))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let raw: String = row.get(1)?;
            serde_json::from_str::<Value>(&raw).map_err(|_| StoreError::InvalidStoredRecord {
                object: table,
                id: id.to_string(),
            })?;
        }
    }
    Ok(())
}

pub(super) fn preflight_workspace(db: &rusqlite::Connection) -> StoreResult<()> {
    if !crate::schema::table_exists(db, "sync_entities")? {
        return Ok(());
    }
    let raw: Option<String> = db.query_row("SELECT payload FROM sync_entities WHERE collection='workspace' AND id='current' AND deleted=0", [], |row| row.get(0)).optional()?;
    if let Some(raw) = raw {
        let envelope: Value = serde_json::from_str(&raw)?;
        if envelope
            .pointer("/value/rateLimitsByAccount")
            .is_some_and(|value| !value.is_null() && !value.is_object())
        {
            return Err(StoreError::InvalidSyncObject {
                scope: "workspace".into(),
                id: "current".into(),
            });
        }
    }
    Ok(())
}

pub(crate) fn migrate_workspace_limits(transaction: &Transaction<'_>) -> StoreResult<()> {
    let raw: Option<String> = transaction.query_row(
        "SELECT payload FROM sync_entities WHERE collection='workspace' AND id='current' AND deleted=0",
        [], |row| row.get(0),
    ).optional()?;
    let Some(raw) = raw else {
        return Ok(());
    };
    let mut envelope: Value = serde_json::from_str(&raw)?;
    let Some(value) = envelope.get_mut("value").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    if value
        .get("rateLimitsByAccount")
        .is_some_and(|accounts| !accounts.is_null() && !accounts.is_object())
    {
        return Err(StoreError::InvalidSyncObject {
            scope: "workspace".into(),
            id: "current".into(),
        });
    }
    let accounts = value.remove("rateLimitsByAccount");
    let default = value.remove("rateLimits");
    if accounts.is_none() && default.is_none() {
        return Ok(());
    }
    if let Some(Value::Object(accounts)) = accounts {
        for (key, snapshot) in accounts {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM sync_entities WHERE collection='rateLimit' AND id=?1)",
                [&key],
                |row| row.get(0),
            )?;
            if !exists {
                SyncDocumentsRepository::put_entity(
                    transaction,
                    "rateLimit",
                    &key,
                    snapshot,
                    false,
                )?;
            }
        }
    }
    let value = envelope
        .get_mut("value")
        .map(Value::take)
        .ok_or_else(|| StoreError::SchemaMismatch("workspace value missing".into()))?;
    SyncDocumentsRepository::put_entity(transaction, "workspace", "current", value, false)?;
    Ok(())
}

pub(crate) fn feature_columns(transaction: &Transaction<'_>) -> StoreResult<()> {
    let has_terminal_generation: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('user_terminals') WHERE name='output_generation')",
        [], |row| row.get(0),
    )?;
    if !has_terminal_generation {
        transaction.execute_batch(
            "ALTER TABLE user_terminals ADD COLUMN output_generation INTEGER NOT NULL DEFAULT 0",
        )?;
    }

    Ok(())
}

/// Status of a retired Claude auth probe wait in `runtime_usage_resumes`.
const RETIRED_AUTH_PROBE_WAIT: &str = "auth_probe_wait";

pub(super) fn remove_auth_probe_receipts(transaction: &Transaction<'_>) -> Result<(), StoreError> {
    transaction.execute("DELETE FROM runtime_usage_resumes WHERE json_extract(record, '$.cause') = 'claude_auth_probe'", [])?;
    Ok(())
}

/// One-time move of the retired Claude auth probe waits into the account auth
/// gate's held state: the row is removed, and its still waiting agent becomes
/// a held turn that `account_auth_gate` releases after an account change. Its
/// input already returned to `pending` when the wait was recorded.
pub(super) fn migrate_auth_probe_waits(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StoreError> {
    let exists: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_usage_resumes')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(());
    }
    let agents = transaction
        .prepare(
            "DELETE FROM runtime_usage_resumes WHERE json_extract(record,'$.status')=?1 \
             RETURNING agent",
        )?
        .query_map([RETIRED_AUTH_PROBE_WAIT], |row| row.get::<_, String>(0))?
        .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
    for id in agents {
        let Some(mut agent) = AgentRepository::get(transaction, &AgentId::from(id.as_str()))?
        else {
            continue;
        };
        if agent.status != AgentStatus::Waiting {
            continue;
        }
        agent.status = AgentStatus::Failed;
        agent.extra.insert(
            "error".into(),
            json!(crate::account_state::CLAUDE_SIGN_IN_MESSAGE),
        );
        AgentRepository::put(transaction, &agent)?;
    }
    Ok(())
}

/// One-time release of the retired capacity start attempts. Before capacity
/// retries queued a follow-up input, a claim parked its agent `starting` with an
/// unsent `capacity` start attempt that no worker ever claimed (the 512-agent
/// run of 21 October 2026). Such a retry becomes due again at `now`, and its
/// agent returns to its failed, held state, so the continuation tick claims it
/// through the follow-up path. A submitted attempt is left to restart recovery.
pub(super) fn release_capacity_start_attempts(
    transaction: &rusqlite::Transaction<'_>,
    now: f64,
) -> Result<(), StoreError> {
    let ids = transaction
        .prepare(
            "SELECT id FROM runtime_agents WHERE json_extract(record,'$.startAttempt.action')='capacity' \
             AND json_extract(record,'$.startAttempt.submitted')=0",
        )?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for id in ids {
        let Some(row) = AgentRepository::get(transaction, &AgentId::from(id.as_str()))? else {
            continue;
        };
        let mut agent = row.value();
        let retry_id = agent["startAttempt"]["capacityRetryId"].clone();
        let mut retry = agent["capacityRetry"].clone();
        if retry["id"] == retry_id && retry["status"] == "starting" {
            if let Some(record) = retry.as_object_mut() {
                record.remove("claimedAt");
            }
            retry["status"] = json!("scheduled");
            retry["dueAt"] = json!(now);
            retry["updatedAt"] = json!(now);
            crate::ContinuationRepository::put_record(
                transaction,
                crate::ContinuationTable::Capacity,
                &id,
                &retry,
            )?;
            agent["capacityRetry"] = retry;
        }
        if let Some(record) = agent.as_object_mut() {
            record.remove("startAttempt");
        }
        agent["status"] = json!("failed");
        agent["inFlight"] = json!(false);
        crate::ContinuationRepository::put_agent(transaction, &agent)?;
    }
    Ok(())
}

pub(super) fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

#[cfg(test)]
mod auth_probe_receipt_tests {
    use super::*;

    #[test]
    fn removes_retired_auth_probe_receipts_in_all_statuses() {
        let mut db = match std::env::var_os("CONTINUATION_MIGRATION_TEST_DB") {
            Some(path) => rusqlite::Connection::open(path).unwrap(),
            None => rusqlite::Connection::open_in_memory().unwrap(),
        };
        db.execute_batch("CREATE TABLE IF NOT EXISTS runtime_usage_resumes(id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL)").unwrap();
        let tx = db.transaction().unwrap();
        for (id, cause, status) in [
            ("621-completed", "claude_auth_probe", "completed"),
            ("621-cancelled", "claude_auth_probe", "cancelled"),
            ("621-waiting", "claude_auth_probe", "auth_probe_wait"),
            ("621-other", "auth", "cancelled"),
        ] {
            tx.execute("INSERT INTO runtime_usage_resumes(id, agent, record) VALUES (?1, 'test', json_object('cause', ?2, 'status', ?3))", (id, cause, status)).unwrap();
        }
        remove_auth_probe_receipts(&tx).unwrap();
        let ids = tx
            .prepare("SELECT id FROM runtime_usage_resumes WHERE id LIKE '621-%' ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(ids, ["621-other"]);
    }
}
