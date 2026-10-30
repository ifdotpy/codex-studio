//! Normalize legacy task names and reject unclassified task records.
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
    tx.execute("UPDATE runtime_tasks SET record=json_set(record,'$.name',json_extract(record,'$.type')) WHERE json_valid(record) AND json_type(record,'$.type')='text' AND (json_type(record,'$.name') IS NULL OR json_type(record,'$.name')='null')", [])?;
    let mut query = tx.prepare("SELECT id,record FROM runtime_tasks ORDER BY id")?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let raw: String = row.get(1)?;
        let record: TaskRecord =
            serde_json::from_str(&raw).map_err(|error| StoreError::InvalidSyncObjectField {
                scope: "runtime_tasks".into(),
                id: id.clone(),
                pointer: String::new(),
                reason: error.to_string(),
            })?;
        if record.id().as_ref() != id {
            return Err(StoreError::InvalidStoredObject {
                table: "runtime_tasks",
                id,
            });
        }
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
    fn migrates_legacy_name_and_rejects_unknown_keys() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY,record TEXT NOT NULL); CREATE TABLE runtime_payload_migrations(name TEXT PRIMARY KEY,cursor INTEGER NOT NULL,complete INTEGER NOT NULL,status TEXT NOT NULL)")
            .unwrap();
        let raw = r#"{"id":"a:i","agent":"a","created":1,"itemId":"i","turnId":"t","kind":"command","status":"running","type":"commandExecution","processId":42}"#;
        db.execute("INSERT INTO runtime_tasks VALUES('a:i',?1)", [raw])
            .unwrap();
        let tx = db.transaction().unwrap();
        super::apply(&tx).unwrap();
        let name: String = tx
            .query_row(
                "SELECT json_extract(record,'$.name') FROM runtime_tasks",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(name, "commandExecution");
        let process_type: String = tx
            .query_row(
                "SELECT json_type(record,'$.processId') FROM runtime_tasks",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(process_type, "integer");
        tx.commit().unwrap();
        db.execute(
            "UPDATE runtime_tasks SET record=json_set(record,'$.unclassified',1)",
            [],
        )
        .unwrap();
        let tx = db.transaction().unwrap();
        super::apply(&tx).unwrap();
        tx.commit().unwrap();
        db.execute(
            "DELETE FROM runtime_payload_migrations WHERE name=?1",
            [super::CHECKPOINT],
        )
        .unwrap();
        let tx = db.transaction().unwrap();
        let error = super::apply(&tx).unwrap_err().to_string();
        assert!(
            error.contains("a:i") && error.contains("unclassified"),
            "{error}"
        );
    }

    #[test]
    fn invalid_legacy_row_rolls_back_normalization_and_checkpoint() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY,record TEXT NOT NULL); CREATE TABLE runtime_payload_migrations(name TEXT PRIMARY KEY,cursor INTEGER NOT NULL,complete INTEGER NOT NULL,status TEXT NOT NULL); INSERT INTO runtime_tasks VALUES('a:i','{\"id\":\"a:i\",\"agent\":\"a\",\"created\":1,\"itemId\":\"i\",\"turnId\":\"t\",\"kind\":\"command\",\"status\":\"running\",\"type\":\"commandExecution\",\"unknown\":true}')").unwrap();
        let tx = db.transaction().unwrap();
        assert!(super::apply(&tx).is_err());
        tx.rollback().unwrap();
        let name: Option<String> = db
            .query_row(
                "SELECT json_extract(record,'$.name') FROM runtime_tasks",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let count: i64 = db
            .query_row(
                "SELECT count(*) FROM runtime_payload_migrations",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(name.is_none());
        assert_eq!(count, 0);
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
