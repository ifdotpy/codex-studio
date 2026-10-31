use crate::StoreResult;
use rusqlite::Transaction;
use studio_runtime_schema::project_record::ProjectRecord;

pub fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if !crate::schema::table_exists(tx, "runtime_projects")? {
        return Ok(());
    }
    let mut statement = tx.prepare("SELECT id,json_set(record,'$.recordKind',CASE WHEN json_type(record,'$.removal')='object' THEN 'removal' ELSE 'live' END) FROM runtime_projects WHERE typeof(id)='text' AND typeof(record)='text' AND CASE WHEN json_valid(record) THEN json_type(record)='object' AND json_type(record,'$.recordKind') IS NULL AND (json_type(record,'$.removal')='object' OR (json_type(record,'$.removal') IS NULL AND json_type(record,'$.path')='text')) ELSE 0 END")?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for (id, encoded) in rows {
        let matches = match serde_json::from_str::<ProjectRecord>(&encoded) {
            Ok(ProjectRecord::Live(project)) => project.id == id,
            Ok(ProjectRecord::Removal(project)) => project.id == id,
            Err(_) => false,
        };
        if matches {
            tx.execute(
                "UPDATE runtime_projects SET record=?2 WHERE id=?1",
                (&id, &encoded),
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn tags_only_recognized_rows_and_is_idempotent() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_projects(id TEXT PRIMARY KEY, record TEXT NOT NULL); INSERT INTO runtime_projects VALUES('a','{\"id\":\"a\",\"path\":\"/work/a\"}'),('b','{\"id\":\"b\",\"removal\":{\"reason\":\"delete\",\"sequence\":1,\"requestId\":\"r\",\"deletedAt\":1}}'),('c','{'),('d','{\"id\":\"d\"}'),('e','{\"id\":\"e\",\"path\":\"/work/e\",\"unknown\":1}');").unwrap();
        let tx = db.transaction().unwrap();
        apply(&tx).unwrap();
        apply(&tx).unwrap();
        let kinds: Vec<(String, Option<String>)> = tx.prepare("SELECT id,CASE WHEN json_valid(record) THEN json_extract(record,'$.recordKind') END FROM runtime_projects ORDER BY id").unwrap().query_map([], |r| Ok((r.get(0)?,r.get(1)?))).unwrap().map(Result::unwrap).collect();
        assert_eq!(
            kinds,
            vec![
                ("a".into(), Some("live".into())),
                ("b".into(), Some("removal".into())),
                ("c".into(), None),
                ("d".into(), None),
                ("e".into(), None)
            ]
        );
        let unchanged: String = tx
            .query_row(
                "SELECT record FROM runtime_projects WHERE id='e'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unchanged, r#"{"id":"e","path":"/work/e","unknown":1}"#);
    }
}
