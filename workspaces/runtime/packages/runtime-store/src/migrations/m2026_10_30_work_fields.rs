use crate::StoreResult;
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if !crate::schema::table_exists(tx, "runtime_work")? {
        return Ok(());
    }
    for field in [
        "id",
        "status",
        "dependencies",
        "owner",
        "updated",
        "rootId",
        "title",
        "version",
        "results",
        "decisions",
        "releases",
    ] {
        while tx.execute("UPDATE runtime_work SET record=json_remove(record,?1) WHERE (SELECT count(*) FROM json_each(record) WHERE key=?2)>1", rusqlite::params![format!("$.{field}"), field])? != 0 {}
    }
    tx.execute_batch("UPDATE runtime_work SET record=json_set(record,'$.dependencies',json('[]')) WHERE json_type(record,'$.dependencies') IS NULL; UPDATE runtime_work SET record=json_set(record,'$.updated',0) WHERE json_type(record,'$.updated') IS NULL; UPDATE runtime_work SET record=json_set(record,'$.version',0) WHERE json_type(record,'$.version')='integer' AND json_extract(record,'$.version')<0;")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn fills_only_absent_fields() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_work(id TEXT,record TEXT); INSERT INTO runtime_work VALUES('legacy','{\"id\":\"legacy\",\"status\":\"ready\"}'),('current','{\"id\":\"current\",\"status\":\"ready\",\"dependencies\":[],\"updated\":1}');").unwrap();
        let tx = db.transaction().unwrap();
        super::apply(&tx).unwrap();
        let rows: Vec<String> = tx
            .prepare("SELECT record FROM runtime_work ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            rows[0],
            r#"{"id":"current","status":"ready","dependencies":[],"updated":1}"#
        );
        assert_eq!(
            rows[1],
            r#"{"id":"legacy","status":"ready","dependencies":[],"updated":0}"#
        );
    }

    #[test]
    fn normalizes_duplicate_keys_and_negative_versions() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_work(id TEXT,record TEXT); INSERT INTO runtime_work VALUES('task','{\"id\":\"wrong\",\"id\":\"task\",\"status\":\"running\",\"version\":-1}');").unwrap();
        let tx = db.transaction().unwrap();
        super::apply(&tx).unwrap();
        let raw: String = tx
            .query_row("SELECT record FROM runtime_work", [], |row| row.get(0))
            .unwrap();
        let record = crate::WorkItemRecord::decode("task", &raw).unwrap();
        assert_eq!(record.id().as_ref(), "task");
        assert_eq!(record.version().unwrap().as_u64(), Some(0));
    }
}
