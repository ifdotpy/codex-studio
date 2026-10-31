//! Materialize Studio-error candidate keys for existing monitor records.
use crate::StoreResult;
use rusqlite::Transaction;

pub(crate) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if !crate::schema::table_exists(tx, "runtime_monitors")? {
        return Ok(());
    }
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_xinfo('runtime_monitors') WHERE name='monitor_error_candidate')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        tx.execute_batch("ALTER TABLE runtime_monitors ADD COLUMN monitor_error_candidate INTEGER NOT NULL DEFAULT 1 CHECK(monitor_error_candidate IN (0,1));
            ALTER TABLE runtime_monitors ADD COLUMN monitor_created REAL NOT NULL DEFAULT 0 CHECK(monitor_created >= 0 AND monitor_created <= 1.7976931348623157e308);")?;
        tx.execute_batch(&format!(
            "UPDATE runtime_monitors SET monitor_error_candidate=({}),monitor_created=({}); DROP INDEX IF EXISTS runtime_monitor_studio_error_v2",
            crate::repositories::monitors::legacy_studio_error_predicate(),
            crate::repositories::monitors::LEGACY_CREATED_EXPRESSION
        ))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn migration_preserves_candidates_and_enforces_columns() {
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            INSERT INTO runtime_monitors VALUES ('lost','{\"status\":\"lost\",\"created\":12}'),('done','{\"status\":\"completed\",\"created\":4}'),('bad','{');").expect("legacy rows");
        let tx = db.transaction().expect("transaction");
        super::apply(&tx).expect("migration");
        tx.commit().expect("commit");
        let rows: Vec<(String, i64, f64)> = db.prepare("SELECT id,monitor_error_candidate,monitor_created FROM runtime_monitors ORDER BY id").expect("query").query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).expect("rows").map(Result::unwrap).collect();
        assert_eq!(
            rows,
            vec![
                ("bad".into(), 1, 0.0),
                ("done".into(), 0, 4.0),
                ("lost".into(), 1, 12.0)
            ]
        );
        for sql in [
            "UPDATE runtime_monitors SET monitor_error_candidate=2",
            "UPDATE runtime_monitors SET monitor_error_candidate=NULL",
            "UPDATE runtime_monitors SET monitor_created=-1",
            "UPDATE runtime_monitors SET monitor_created=NULL",
        ] {
            assert!(db.execute(sql, []).is_err(), "constraint must reject {sql}");
        }
    }
}
