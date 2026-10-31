//! Normalize legacy work rows before reading them as checked work items.
use crate::{StoreError, StoreResult};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::Deserialize;
use serde_json::{Value, json};
use studio_runtime_schema::{persistence::MigrationStatus, work_item_record::WorkItem};

const CHECKPOINT: &str = "2026-10-31-work-records";

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if !crate::schema::table_exists(tx, "runtime_work")? {
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
            "invalid work migration checkpoint",
        ));
    }
    let mut query = tx.prepare("SELECT id,record FROM runtime_work")?;
    let rows = query
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(query);
    for (id, raw) in rows {
        if serde_json::from_str::<WorkItem>(&raw).is_ok() {
            let value: Value = serde_json::from_str(&raw)?;
            let encoded = serde_json::to_string(&value)?;
            if raw != encoded {
                tx.execute(
                    "UPDATE runtime_work SET record=?2 WHERE id=?1",
                    params![id, encoded],
                )?;
            }
            continue;
        }
        let mut value: Value =
            serde_json::from_str(&raw).map_err(|_| StoreError::InvalidStoredObject {
                table: "runtime_work",
                id: id.clone(),
            })?;
        let fields = value
            .as_object_mut()
            .ok_or_else(|| StoreError::InvalidStoredObject {
                table: "runtime_work",
                id: id.clone(),
            })?;
        if !fields.get("id").is_some_and(Value::is_string) {
            fields.insert("id".into(), json!(id));
        }
        if !fields
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|s| {
                s.parse::<studio_runtime_schema::persistence::WorkStatus>()
                    .is_ok()
            })
        {
            fields.insert(
                "status".into(),
                json!(studio_runtime_schema::persistence::WorkStatus::Cancelled),
            );
        }
        for key in ["rootId", "owner", "title"] {
            if fields
                .get(key)
                .is_some_and(|v| !v.is_null() && !v.is_string())
            {
                fields.remove(key);
            }
        }
        for key in ["updated", "version", "dependencies", "releases"] {
            let valid = fields.get(key).is_none_or(|v| match key {
                "updated" => v.is_number(),
                "version" => {
                    studio_runtime_schema::work_item_record::WorkItemVersion::deserialize(v).is_ok()
                }
                "dependencies" => v
                    .as_array()
                    .is_some_and(|items| items.iter().all(Value::is_string)),
                "releases" => v.is_array(),
                _ => false,
            });
            if !valid {
                fields.remove(key);
            }
        }
        for key in ["results", "decisions"] {
            match fields.get_mut(key) {
                Some(Value::Array(items)) => {
                    for item in items {
                        if !item.is_object() {
                            *item = json!({});
                        }
                    }
                }
                Some(_) => {
                    fields.remove(key);
                }
                None => {}
            }
        }
        let encoded = serde_json::to_string(&value)?;
        serde_json::from_str::<WorkItem>(&encoded).map_err(|_| {
            StoreError::InvalidStoredObject {
                table: "runtime_work",
                id: id.clone(),
            }
        })?;
        tx.execute(
            "UPDATE runtime_work SET record=?2 WHERE id=?1",
            params![id, encoded],
        )?;
    }
    tx.execute(
        "INSERT INTO runtime_payload_migrations(name,cursor,complete,status) VALUES(?1,0,1,?2)",
        params![CHECKPOINT, MigrationStatus::Complete.as_str()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalizes_legacy_work_once() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_work(id TEXT PRIMARY KEY, record TEXT NOT NULL); CREATE TABLE runtime_payload_migrations(name TEXT PRIMARY KEY,cursor INTEGER,complete INTEGER,status TEXT); INSERT INTO runtime_work VALUES('row','{\"id\":5,\"id\":{},\"status\":9,\"status\":\"review\",\"updated\":\"bad\",\"results\":false,\"decisions\":[1,null,{\"resultId\":null}]}'),('extra','{\"id\":\"extra\",\"status\":\"ready\",\"note\":1,\"note\":2}');").unwrap();
        let tx = db.transaction().unwrap();
        apply(&tx).unwrap();
        apply(&tx).unwrap();
        let raw: String = tx
            .query_row(
                "SELECT record FROM runtime_work WHERE id='row'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let work: WorkItem = serde_json::from_str(&raw).unwrap();
        assert_eq!(work.id().0, "row");
        assert_eq!(work.status().as_str(), "review");
        assert_eq!(work.updated(), 0.0);
        assert!(work.results().is_none());
        assert_eq!(work.decisions().unwrap().len(), 3);
        assert!(work.decisions().unwrap()[0].is_empty());
        assert!(work.decisions().unwrap()[1].is_empty());
        let extra: String = tx
            .query_row(
                "SELECT record FROM runtime_work WHERE id='extra'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(extra.matches("\"note\"").count(), 1);
        assert_eq!(serde_json::from_str::<Value>(&extra).unwrap()["note"], 2);
    }

    #[test]
    #[ignore = "manual verification on a disposable reflink copy"]
    fn real_work_records_migrate_without_refusal() {
        let path = std::env::var_os("WORK_MIGRATION_TEST_DB").unwrap();
        let mut db = rusqlite::Connection::open(path).unwrap();
        let tx = db.transaction().unwrap();
        apply(&tx).unwrap();
        if crate::schema::table_exists(&tx, "runtime_work").unwrap() {
            let mut query = tx.prepare("SELECT record FROM runtime_work").unwrap();
            let records = query.query_map([], |row| row.get::<_, String>(0)).unwrap();
            for record in records {
                serde_json::from_str::<WorkItem>(&record.unwrap()).unwrap();
            }
        }
        tx.commit().unwrap();
    }
}
