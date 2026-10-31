//! Source-complete Canvas transitions retained until their supported source/restore formats retire.
use crate::{StoreError, StoreResult, Transaction};
use rusqlite::OptionalExtension;
use serde_json::Value;

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

pub(super) fn remove_auth_probe_receipts(transaction: &Transaction<'_>) -> Result<(), StoreError> {
    transaction.execute("DELETE FROM runtime_usage_resumes WHERE json_extract(record, '$.cause') = 'claude_auth_probe'", [])?;
    Ok(())
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
