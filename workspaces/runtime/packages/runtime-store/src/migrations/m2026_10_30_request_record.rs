use crate::StoreResult;
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if !crate::schema::table_exists(tx, "runtime_requests")? {
        return Ok(());
    }
    let mut query = tx.prepare("SELECT id,record FROM runtime_requests")?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let raw: String = row.get(1)?;
        // Rows that cannot be reconstructed remain untouched; typed reads report them per row.
        let Ok(request) =
            serde_json::from_str::<studio_runtime_schema::request_record::RequestRecord>(&raw)
        else {
            continue;
        };
        if request.id != id {
            continue;
        }
        let encoded = serde_json::to_string(&request)?;
        if encoded != raw {
            tx.execute(
                "UPDATE runtime_requests SET record=?1 WHERE id=?2",
                rusqlite::params![encoded, id],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    #[test]
    fn malformed_rows_stay_unchanged_without_blocking_migration() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_requests(id TEXT PRIMARY KEY, record TEXT NOT NULL); INSERT INTO runtime_requests VALUES('sparse','{\"id\":\"sparse\"}')").unwrap();
        let tx = db.transaction().unwrap();
        super::apply(&tx).unwrap();
        let raw: String = tx
            .query_row(
                "SELECT record FROM runtime_requests WHERE id='sparse'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(raw, r#"{"id":"sparse"}"#);
        assert!(
            crate::Table::<crate::RequestRecord>::new(&tx)
                .get(&"sparse".into())
                .is_err()
        );
        tx.execute(
            "INSERT INTO runtime_requests VALUES('unknown','{\"id\":\"unknown\",\"extra\":1}')",
            [],
        )
        .unwrap();
        tx.execute("INSERT INTO runtime_requests VALUES(?1,?2)", ["invalid", r#"{"id":"invalid","agent":"a","method":"unknown","status":"pending","createdAt":1,"params":{}}"#]).unwrap();
        super::apply(&tx).unwrap();
        let raw: String = tx
            .query_row(
                "SELECT record FROM runtime_requests WHERE id='unknown'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(raw, r#"{"id":"unknown","extra":1}"#);
        assert!(
            crate::Table::<crate::RequestRecord>::new(&tx)
                .get(&"invalid".into())
                .is_err()
        );
        let canonical: crate::RequestRecord = serde_json::from_str(r#"{"id":"valid","agent":"a","method":"agent/asyncQuestion","status":"pending","createdAt":1.0,"params":{}}"#).unwrap();
        tx.execute(
            "INSERT INTO runtime_requests VALUES(?1,?2)",
            rusqlite::params!["valid", serde_json::to_string(&canonical).unwrap()],
        )
        .unwrap();
        tx.execute_batch("CREATE TRIGGER no_rewrite BEFORE UPDATE ON runtime_requests BEGIN SELECT RAISE(ABORT, 'unexpected rewrite'); END").unwrap();
        super::apply(&tx).unwrap();
    }
}
