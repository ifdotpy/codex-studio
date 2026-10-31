//! Repair reconstructible legacy task fields without blocking startup on other rows.
use crate::{StoreError, StoreResult};
use rusqlite::{OptionalExtension, Transaction};
use studio_runtime_schema::persistence::MigrationStatus;
use studio_runtime_schema::task_record::TaskRecord;

const CHECKPOINT: &str = "2026-10-30-task-records";

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if !crate::schema::table_exists(tx, "runtime_tasks")? {
        return Ok(());
    }
    let previous: Option<(i64, String)> = tx
        .query_row(
            "SELECT complete,status FROM runtime_payload_migrations WHERE name=?1",
            [CHECKPOINT],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((complete, status)) = previous {
        if complete == 1 && status == MigrationStatus::Complete.as_str() {
            return Ok(());
        }
        return Err(StoreError::RecordContract(
            "invalid task migration checkpoint",
        ));
    }
    let mut query = tx.prepare("SELECT id, record, CASE WHEN json_type(record,'$.name') IS NULL OR json_type(record,'$.name')='null' THEN json_set(with_id,'$.name',json_extract(record,'$.type')) ELSE with_id END FROM (SELECT id,record,CASE WHEN json_type(record,'$.id') IS NULL OR json_type(record,'$.id')='null' THEN json_set(record,'$.id',id) ELSE record END AS with_id FROM runtime_tasks WHERE CASE WHEN json_valid(record) THEN json_type(record)='object' ELSE 0 END) WHERE CASE WHEN json_valid(record) THEN json_type(record,'$.id') IS NULL OR json_type(record,'$.id')='null' OR ((json_type(record,'$.name') IS NULL OR json_type(record,'$.name')='null') AND json_type(record,'$.type')='text') ELSE 0 END")?;
    let mut rows = query.query([])?;
    let mut repairs = Vec::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let raw: String = row.get(1)?;
        let repaired: String = row.get(2)?;
        if let Ok(record) = serde_json::from_str::<TaskRecord>(&repaired)
            && record.id().as_ref() == id
            && repaired != raw
        {
            repairs.push((id, repaired));
        }
    }
    drop(rows);
    drop(query);
    for (id, record) in repairs {
        tx.execute(
            "UPDATE runtime_tasks SET record=?1 WHERE id=?2",
            (&record, &id),
        )?;
    }
    tx.execute(
        "INSERT INTO runtime_payload_migrations(name,cursor,complete,status) VALUES(?1,0,1,?2)",
        rusqlite::params![CHECKPOINT, MigrationStatus::Complete.as_str()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    #[test]
    fn repairs_reconstructible_rows_and_preserves_invalid_rows() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY,record TEXT NOT NULL); CREATE TABLE runtime_payload_migrations(name TEXT PRIMARY KEY,cursor INTEGER NOT NULL,complete INTEGER NOT NULL,status TEXT NOT NULL)").unwrap();
        let valid = r#"{"agent":"a","created":1,"itemId":"i","turnId":"t","kind":"command","status":"running","type":"commandExecution","processId":42}"#;
        let unknown = r#"{ "agent":"a", "type":"commandExecution", "unknown":true }"#;
        for (id, raw) in [
            ("a:i", valid),
            ("unknown", unknown),
            ("incomplete", "{}"),
            ("malformed", "{"),
        ] {
            db.execute("INSERT INTO runtime_tasks VALUES(?1,?2)", (id, raw))
                .unwrap();
        }
        let tx = db.transaction().unwrap();
        super::apply(&tx).unwrap();
        tx.commit().unwrap();
        let repaired: String = db
            .query_row(
                "SELECT record FROM runtime_tasks WHERE id='a:i'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let task: studio_runtime_schema::task_record::TaskRecord =
            serde_json::from_str(&repaired).unwrap();
        assert_eq!(task.id().as_ref(), "a:i");
        assert_eq!(
            db.query_row(
                "SELECT json_type(record,'$.processId') FROM runtime_tasks WHERE id='a:i'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "integer"
        );
        for (id, raw) in [
            ("unknown", unknown),
            ("incomplete", "{}"),
            ("malformed", "{"),
        ] {
            let stored: String = db
                .query_row(
                    "SELECT record FROM runtime_tasks WHERE id=?1",
                    [id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(stored, raw);
            assert!(
                serde_json::from_str::<studio_runtime_schema::task_record::TaskRecord>(&stored)
                    .is_err()
            );
        }
        let tx = db.transaction().unwrap();
        super::apply(&tx).unwrap();
        tx.commit().unwrap();
    }

    #[test]
    #[ignore = "manual measurement on a disposable reflink copy"]
    fn measure_task_migration_on_copy() {
        let path = std::env::var_os("STUDIO_TASK_MIGRATION_COPY").unwrap();
        let mut db = Connection::open(std::path::Path::new(&path).join("canvas.sqlite3")).unwrap();
        let start = std::time::Instant::now();
        let tx = db.transaction().unwrap();
        super::apply(&tx).unwrap();
        tx.commit().unwrap();
        println!("task migration elapsed_ms={}", start.elapsed().as_millis());
        let start = std::time::Instant::now();
        let tx = db.transaction().unwrap();
        super::apply(&tx).unwrap();
        tx.commit().unwrap();
        println!(
            "task migration repeat_elapsed_ms={}",
            start.elapsed().as_millis()
        );
    }
}
