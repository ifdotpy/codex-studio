//! Retire legacy registered graph agents; managed agents and chat edges remain.
use crate::StoreResult;
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    tx.execute_batch("DROP TABLE IF EXISTS graph_agents")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn drops_legacy_agents_and_preserves_groups_and_edges() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE graph_agents(id TEXT PRIMARY KEY,record TEXT); INSERT INTO graph_agents VALUES('legacy','{}'); CREATE TABLE groups(id TEXT); INSERT INTO groups VALUES('chat'); CREATE TABLE graph_edges(id TEXT); INSERT INTO graph_edges VALUES('edge');").unwrap();
        for _ in 0..2 {
            let tx = db.transaction().unwrap();
            super::apply(&tx).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='graph_agents'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        for table in ["groups", "graph_edges"] {
            assert_eq!(
                db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                1
            );
        }
    }
}
