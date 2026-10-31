//! Repair legacy object-shaped native status errors on agent records.
use crate::StoreResult;
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    tx.execute_batch(
        "UPDATE runtime_agents SET record=json_set(record, '$.nativeStatus.error', \
         CASE WHEN json_type(record, '$.nativeStatus.error.message')='text' \
              THEN json_extract(record, '$.nativeStatus.error.message') \
              ELSE CAST(json_extract(record, '$.nativeStatus.error') AS TEXT) || '' END) \
         WHERE CASE WHEN json_valid(record) THEN json_type(record, '$.nativeStatus.error')='object' ELSE 0 END",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    #[test]
    fn native_status_error_objects_become_strings_once() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(r#"CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            CREATE TABLE request_results(id TEXT PRIMARY KEY,result TEXT NOT NULL);
            INSERT INTO runtime_agents VALUES('message','{"id":"message","nativeStatus":{"error":{"message":"provider stopped","detail":1}}}');
            INSERT INTO runtime_agents VALUES('fallback','{"id":"fallback","nativeStatus":{"error":{"detail":1}}}');
            INSERT INTO runtime_agents VALUES('unchanged','{"id":"unchanged","nativeStatus":{"error":"already text"}}');
            INSERT INTO request_results VALUES('receipt','{ "nativeStatus": { "error": { "message": "stored" } } }');"#).unwrap();
        let before: String = db
            .query_row(
                "SELECT result FROM request_results WHERE id='receipt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let tx = db.transaction().unwrap();
        super::apply(&tx).unwrap();
        tx.commit().unwrap();
        let rows: Vec<(String, String)> = db.prepare("SELECT id,json_extract(record,'$.nativeStatus.error') FROM runtime_agents ORDER BY id").unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect();
        assert_eq!(
            rows,
            [
                ("fallback".into(), "{\"detail\":1}".into()),
                ("message".into(), "provider stopped".into()),
                ("unchanged".into(), "already text".into())
            ]
        );
        let text_rows: i64 = db.query_row(
            "SELECT count(*) FROM runtime_agents WHERE json_type(record,'$.nativeStatus.error')='text'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(text_rows, 3);
        let changed: i64 = db
            .query_row("SELECT total_changes()", [], |r| r.get(0))
            .unwrap();
        let tx = db.transaction().unwrap();
        super::apply(&tx).unwrap();
        tx.commit().unwrap();
        let after: i64 = db
            .query_row("SELECT total_changes()", [], |r| r.get(0))
            .unwrap();
        assert_eq!(after, changed);
        assert_eq!(
            db.query_row(
                "SELECT result FROM request_results WHERE id='receipt'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            before
        );
    }
}
