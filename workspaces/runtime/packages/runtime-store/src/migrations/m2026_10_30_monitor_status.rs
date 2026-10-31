//! Materialize monitor status for the active-monitor index.
use crate::StoreResult;
use rusqlite::Transaction;

pub(crate) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if !crate::schema::table_exists(tx, "runtime_monitors")? {
        return Ok(());
    }
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_xinfo('runtime_monitors') WHERE name='monitor_status')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        use studio_runtime_schema::studio_errors::MonitorStatus;
        let statuses = MonitorStatus::ALL
            .iter()
            .map(|status| format!("'{}'", status.as_str()))
            .collect::<Vec<_>>()
            .join(",");
        let lost = MonitorStatus::Lost.as_str();
        tx.execute_batch(&format!("ALTER TABLE runtime_monitors ADD COLUMN monitor_status TEXT NOT NULL DEFAULT '{lost}' CHECK(monitor_status IN ({statuses}));
            UPDATE runtime_monitors SET monitor_status=CASE WHEN json_valid(record) THEN CASE WHEN json_type(record,'$.status')='text' AND json_extract(record,'$.status') IN ({statuses}) THEN json_extract(record,'$.status') ELSE '{lost}' END ELSE '{lost}' END;
            DROP INDEX IF EXISTS runtime_monitor_startup_active;"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_unknowns_become_lost_and_status_is_closed() {
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            INSERT INTO runtime_monitors(id,record) VALUES ('running','{\"status\":\"running\"}'),('unknown','{\"status\":\"future\"}'),('malformed','{');").expect("legacy rows");
        let tx = db.transaction().expect("transaction");
        super::apply(&tx).expect("migration");
        let read = |id: &str| {
            tx.query_row(
                "SELECT monitor_status FROM runtime_monitors WHERE id=?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .expect("status")
        };
        assert_eq!(read("running"), "running");
        assert_eq!(read("unknown"), "lost");
        assert_eq!(read("malformed"), "lost");
        assert!(
            tx.execute(
                "UPDATE runtime_monitors SET monitor_status='future' WHERE id='unknown'",
                []
            )
            .is_err()
        );
        assert!(
            tx.execute(
                "UPDATE runtime_monitors SET monitor_status=NULL WHERE id='unknown'",
                []
            )
            .is_err()
        );
    }
}
