use crate::StoreResult;
use rusqlite::Transaction;

pub fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if !crate::schema::table_exists(tx, "runtime_projects")? {
        return Ok(());
    }
    tx.execute("UPDATE runtime_projects SET record=json_set(record,'$.peerTeams',json('[]')) WHERE json_valid(record) AND json_type(record)='object' AND json_type(record,'$.peerTeams') IS NULL", [])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn preserves_existing_teams_and_other_values() {
        let mut db = Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_projects(id TEXT PRIMARY KEY, record TEXT NOT NULL); INSERT INTO runtime_projects VALUES('a','{\"id\":\"a\",\"other\":1.0}'),('b','{\"id\":\"b\",\"peerTeams\":[{\"id\":\"x\"}]}');").expect("fixtures");
        let tx = db.transaction().expect("transaction");
        apply(&tx).expect("migrate");
        apply(&tx).expect("repeat migration");
        let a: String = tx
            .query_row(
                "SELECT record FROM runtime_projects WHERE id='a'",
                [],
                |r| r.get(0),
            )
            .expect("project a");
        let b: String = tx
            .query_row(
                "SELECT record FROM runtime_projects WHERE id='b'",
                [],
                |r| r.get(0),
            )
            .expect("project b");
        assert_eq!(a, r#"{"id":"a","other":1.0,"peerTeams":[]}"#);
        assert_eq!(b, r#"{"id":"b","peerTeams":[{"id":"x"}]}"#);
    }
}
