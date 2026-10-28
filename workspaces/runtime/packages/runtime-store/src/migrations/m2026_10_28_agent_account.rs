#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use serde_json::{Value, json};

    #[test]
    fn old_missing_and_null_accounts_become_default() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL);")
            .unwrap();
        for (id, record) in [
            ("missing", json!({})),
            ("null", json!({"accountKey":null})),
            ("other", json!({"accountKey":"other"})),
        ] {
            db.execute(
                "INSERT INTO runtime_agents VALUES(?1,?2)",
                (id, record.to_string()),
            )
            .unwrap();
        }
        let tx = db.transaction().unwrap();
        super::apply(&tx).unwrap();
        let read = |id| -> Value {
            serde_json::from_str(
                &tx.query_row("SELECT record FROM runtime_agents WHERE id=?1", [id], |r| {
                    r.get::<_, String>(0)
                })
                .unwrap(),
            )
            .unwrap()
        };
        assert_eq!(read("missing"), json!({"accountKey":"default"}));
        assert_eq!(read("null"), json!({"accountKey":"default"}));
        assert_eq!(read("other"), json!({"accountKey":"other"}));
    }
}
use crate::StoreResult;
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if crate::schema::table_exists(tx, "runtime_agents")? {
        tx.execute(
            "UPDATE runtime_agents SET record=json_set(record,'$.accountKey',?1) WHERE json_valid(record) AND (json_type(record,'$.accountKey') IS NULL OR json_type(record,'$.accountKey')='null')",
            [crate::cost_aggregate::DEFAULT_ACCOUNT],
        )?;
    }
    Ok(())
}
