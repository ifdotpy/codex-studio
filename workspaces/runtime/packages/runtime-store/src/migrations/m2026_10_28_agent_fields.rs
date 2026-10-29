use crate::StoreResult;
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if !crate::schema::table_exists(tx, "runtime_agents")? {
        return Ok(());
    }
    tx.execute_batch(
        "UPDATE runtime_agents SET record=json_remove(record,'$.autoWake')
         WHERE json_valid(record) AND json_type(record,'$.autoWake') IS NOT NULL
           AND json_type(record,'$.autoWake') NOT IN ('true','false');
         UPDATE runtime_agents SET record=json_remove(record,'$.executionMode')
         WHERE json_valid(record) AND json_type(record,'$.executionMode') IS NOT NULL
           AND (json_type(record,'$.executionMode')!='text'
                OR json_extract(record,'$.executionMode') NOT IN ('native','host','vm'));
         UPDATE runtime_agents SET record=json_remove(record,'$.turnEpoch')
         WHERE json_valid(record) AND json_type(record,'$.turnEpoch') IS NOT NULL
           AND (json_type(record,'$.turnEpoch')!='integer'
                OR typeof(json_extract(record,'$.turnEpoch'))!='integer');",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use serde_json::{Value, json};

    #[test]
    fn old_agent_fields_are_normalized_without_changing_valid_values() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL);")
            .unwrap();
        let old = json!({"autoWake":"invalid", "executionMode":"obsolete", "turnEpoch":{}});
        let valid = json!({"autoWake":false, "executionMode":"vm", "turnEpoch":3});
        for (id, row) in [("old", &old), ("valid", &valid)] {
            db.execute(
                "INSERT INTO runtime_agents VALUES(?1,?2)",
                (id, row.to_string()),
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
        assert_eq!(read("old"), json!({}));
        assert_eq!(read("valid"), valid);
    }
}
