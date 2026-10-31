use crate::StoreResult;
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if !crate::schema::table_exists(tx, "runtime_agents")? {
        return Ok(());
    }
    tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.compactions',0) WHERE json_valid(record) AND json_type(record,'$.compactions') IS NULL", [])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_missing_but_preserves_null_and_existing_counts() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL); INSERT INTO runtime_agents VALUES('missing','{}'),('unknown','{\"compactions\":null}'),('known','{\"compactions\":4}');").unwrap();
        let tx = db.transaction().unwrap();
        apply(&tx).unwrap();
        assert_eq!(tx.changes(), 1, "only the missing value is migrated");
        apply(&tx).unwrap();
        assert_eq!(
            tx.changes(),
            0,
            "canonical rows are not rewritten on reopen"
        );
        let records = tx
            .prepare("SELECT record FROM runtime_agents ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            records,
            [
                r#"{"compactions":4}"#,
                r#"{"compactions":0}"#,
                r#"{"compactions":null}"#
            ]
        );
    }
}
