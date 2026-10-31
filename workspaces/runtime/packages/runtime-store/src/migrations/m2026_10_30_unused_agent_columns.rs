//! Retire generated fields that no agent query reads.
use crate::{StoreError, StoreResult};
use rusqlite::{OptionalExtension, Transaction};

pub(super) const RETIRED: [&str; 10] = [
    "native_reset_pending",
    "capacity_retry_id",
    "capacity_retry_status",
    "capacity_retry_due_at",
    // The start attempt lives on a turn row now (`domain`); older states still
    // hold these columns until the agent table is rebuilt.
    "start_attempt_submitted",
    "start_attempt_connection_generation",
    "start_attempt_active_at_reservation",
    // Auto-wake and execution mode live on the chat row (`domain`).
    "auto_wake",
    "execution_mode",
    // The thread id lives on the session row (`domain`).
    "thread_id",
];

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    tx.execute_batch("DROP INDEX IF EXISTS runtime_agent_account_wake_candidates_v4")?;
    for column in RETIRED {
        let kind: Option<i64> = tx
            .query_row(
                "SELECT hidden FROM pragma_table_xinfo('runtime_agents') WHERE name=?1",
                [column],
                |row| row.get(0),
            )
            .optional()?;
        match kind {
            None => {}
            Some(3) => {
                tx.execute_batch(&format!("ALTER TABLE runtime_agents DROP COLUMN {column}"))?
            }
            Some(_) => {
                return Err(StoreError::RecordContract(
                    "retired agent column is not stored generated",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    #[test]
    fn retired_columns_preserve_rows_triggers_and_plain_columns_on_retry() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL,\
             native_reset_pending GENERATED ALWAYS AS (json_extract(record,'$.nativeRelease.resetPending')) STORED,\
             capacity_retry_id GENERATED ALWAYS AS (json_extract(record,'$.capacityRetry.id')) STORED,\
             capacity_retry_status GENERATED ALWAYS AS (json_extract(record,'$.capacityRetry.status')) STORED,\
             capacity_retry_due_at GENERATED ALWAYS AS (json_extract(record,'$.capacityRetry.dueAt')) STORED,\
             hold_status TEXT NOT NULL DEFAULT 'clear');\
             CREATE TABLE changes(id TEXT);\
             CREATE TRIGGER agent_change AFTER UPDATE ON runtime_agents BEGIN INSERT INTO changes VALUES(new.id); END;\
             INSERT INTO runtime_agents(id,record,hold_status) VALUES('a','{}','blocked');",
        ).unwrap();
        for _ in 0..2 {
            let tx = db.transaction().unwrap();
            super::apply(&tx).unwrap();
            tx.commit().unwrap();
        }
        let remaining: i64 = db.query_row(
            "SELECT count(*) FROM pragma_table_xinfo('runtime_agents') WHERE name IN ('native_reset_pending','capacity_retry_id','capacity_retry_status','capacity_retry_due_at')",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(remaining, 0);
        let row: (String, String) = db
            .query_row(
                "SELECT record,hold_status FROM runtime_agents WHERE id='a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, ("{}".into(), "blocked".into()));
        assert_eq!(
            db.query_row("SELECT count(*) FROM changes", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        db.execute("UPDATE runtime_agents SET record='{}' WHERE id='a'", [])
            .unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM changes", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn unexpected_plain_column_fails_and_rolls_back_prior_drops() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT, record TEXT, native_reset_pending AS (json_extract(record,'$.x')) STORED, capacity_retry_id TEXT);").unwrap();
        let tx = db.transaction().unwrap();
        assert!(super::apply(&tx).is_err());
        tx.rollback().unwrap();
        let count: i64 = db.query_row("SELECT count(*) FROM pragma_table_xinfo('runtime_agents') WHERE name IN ('native_reset_pending','capacity_retry_id')", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 2);
    }
}
