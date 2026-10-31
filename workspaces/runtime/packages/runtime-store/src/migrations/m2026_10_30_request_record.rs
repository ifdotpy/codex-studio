use crate::StoreResult;
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if !crate::schema::table_exists(tx, "runtime_requests")? {
        return Ok(());
    }
    let fields = serde_json::to_string(studio_runtime_schema::request_record::FIELDS)?;
    let required = serde_json::to_string(studio_runtime_schema::request_record::REQUIRED_FIELDS)?;
    let mut query = tx.prepare("SELECT id,record,CASE WHEN json_valid(record) AND json_type(record)='object' THEN EXISTS(SELECT 1 FROM json_each(record) AS entry WHERE entry.key NOT IN (SELECT value FROM json_each(?1))) ELSE 0 END,CASE WHEN json_valid(record) AND json_type(record)='object' THEN NOT EXISTS(SELECT 1 FROM json_each(?2) AS field WHERE json_type(record,'$.' || field.value) IS NULL OR json_type(record,'$.' || field.value)='null') ELSE 0 END FROM runtime_requests")?;
    let mut rows = query.query(rusqlite::params![fields, required])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let raw: String = row.get(1)?;
        let unknown: bool = row.get(2)?;
        let complete: bool = row.get(3)?;
        if unknown {
            return Err(crate::StoreError::InvalidStoredRecord {
                object: "requests",
                id,
            });
        }
        // Sparse legacy rows remain unchanged and are rejected individually by readers.
        if complete {
            let request: studio_runtime_schema::request_record::RequestRecord =
                serde_json::from_str(&raw)?;
            if request.id != id {
                return Err(crate::StoreError::InvalidStoredRecord {
                    object: "requests",
                    id,
                });
            }
            tx.execute(
                "UPDATE runtime_requests SET record=?1 WHERE id=?2",
                rusqlite::params![serde_json::to_string(&request)?, id],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    #[test]
    fn sparse_rows_stay_unchanged_and_unknown_fields_fail() {
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
        assert!(super::apply(&tx).is_err());
    }
}
