//! Retire the generated start-action field after its last index was removed.
use crate::{StoreError, StoreResult};
use rusqlite::{OptionalExtension, Transaction};

pub(super) const COLUMN: &str = "start_attempt_action";

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    let kind: Option<i64> = tx
        .query_row(
            "SELECT hidden FROM pragma_table_xinfo('runtime_agents') WHERE name=?1",
            [COLUMN],
            |row| row.get(0),
        )
        .optional()?;
    match kind {
        None => Ok(()),
        Some(3) => {
            Ok(tx.execute_batch("ALTER TABLE runtime_agents DROP COLUMN start_attempt_action")?)
        }
        Some(_) => Err(StoreError::RecordContract(
            "retired start action is not stored generated",
        )),
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    #[test]
    fn removes_only_generated_column_and_reopens() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT, start_attempt_action TEXT AS (json_extract(record,'$.startAttempt.action')) STORED, hold_status TEXT); CREATE TABLE changes(id TEXT); CREATE TRIGGER changed AFTER UPDATE ON runtime_agents BEGIN INSERT INTO changes VALUES(new.id); END; INSERT INTO runtime_agents VALUES('a','{}','blocked');").unwrap();
        for _ in 0..2 {
            let tx = db.transaction().unwrap();
            super::apply(&tx).unwrap();
            tx.commit().unwrap();
        }
        let row: (String, String) = db
            .query_row(
                "SELECT record,hold_status FROM runtime_agents WHERE id='a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, ("{}".into(), "blocked".into()));
        assert_eq!(db.query_row("SELECT count(*) FROM pragma_table_xinfo('runtime_agents') WHERE name='start_attempt_action'", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        db.execute("UPDATE runtime_agents SET record='{}' WHERE id='a'", [])
            .unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM changes", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn refuses_plain_column() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT, start_attempt_action TEXT)")
            .unwrap();
        let tx = db.transaction().unwrap();
        assert!(super::apply(&tx).is_err());
        tx.rollback().unwrap();
    }
}
