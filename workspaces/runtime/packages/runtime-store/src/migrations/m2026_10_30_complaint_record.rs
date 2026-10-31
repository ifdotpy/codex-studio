use crate::StoreResult;
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if !crate::schema::table_exists(tx, "runtime_complaints")? {
        return Ok(());
    }
    let mut query = tx.prepare("SELECT id,json_insert(json_remove(record,'$.legacyStatus','$.legacyLeadReadAt'),'$.author',null,'$.leadId',null,'$.text',null,'$.recipient','lead','$.version',null,'$.created',null,'$.updated',null,'$.readAt',null,'$.responses',json('[]')) FROM runtime_complaints")?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let mut value: serde_json::Value = serde_json::from_str(&row.get::<_, String>(1)?)?;
        // The retired Python backend saved "open" replies; they mean work is in progress.
        if let Some(responses) = value["responses"].as_array_mut() {
            for response in responses {
                if response["status"] == "open" {
                    response["status"] = serde_json::json!("in_progress");
                }
            }
        }
        let raw = value.to_string();
        let record: studio_runtime_schema::complaint_record::ComplaintRecord =
            serde_json::from_str(&raw)?;
        if record.id != id {
            return Err(crate::StoreError::InvalidStoredRecord {
                object: "complaints",
                id,
            });
        }
        tx.execute(
            "UPDATE runtime_complaints SET record=?1 WHERE id=?2",
            rusqlite::params![raw, id],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    #[test]
    fn migrates_sparse_rows_preserving_complaint_text_and_refuses_unknown_fields() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE runtime_complaints(id TEXT PRIMARY KEY, record TEXT NOT NULL)",
        )
        .unwrap();
        let original = "  fingerprint \u{e9}\\line  ";
        let raw = format!(
            r#"{{"id":"c","text":{},"status":"open"}}"#,
            serde_json::to_string(original).unwrap()
        );
        db.execute("INSERT INTO runtime_complaints VALUES('c',?1)", [&raw])
            .unwrap();
        let tx = db.transaction().unwrap();
        super::apply(&tx).unwrap();
        let raw: String = tx
            .query_row(
                "SELECT record FROM runtime_complaints WHERE id='c'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let record: studio_runtime_schema::complaint_record::ComplaintRecord =
            serde_json::from_str(&raw).unwrap();
        assert_eq!(record.text.as_deref(), Some(original));
        tx.execute("INSERT INTO runtime_complaints VALUES('unknown', '{\"id\":\"unknown\",\"status\":\"open\",\"future\":true}')", []).unwrap();
        assert!(super::apply(&tx).is_err());
    }
}
