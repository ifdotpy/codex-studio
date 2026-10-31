use crate::StoreResult;
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if !crate::schema::table_exists(tx, "runtime_monitors")? {
        return Ok(());
    }
    let generated: i64 = tx.query_row("SELECT count(*) FROM pragma_table_xinfo('runtime_monitors') WHERE name IN ('stream_pending','monitor_status','monitor_error_candidate','monitor_created') AND hidden=3", [], |row| row.get(0))?;
    if generated == 4 {
        return Ok(());
    }
    let definitions = tx.prepare("SELECT sql FROM sqlite_schema WHERE tbl_name='runtime_monitors' AND type IN ('index','trigger') AND sql IS NOT NULL ORDER BY type,name")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let statuses = studio_runtime_schema::studio_errors::MonitorStatus::ALL
        .iter()
        .map(|status| format!("'{}'", status.as_str()))
        .collect::<Vec<_>>()
        .join(",");
    let lost = studio_runtime_schema::studio_errors::MonitorStatus::Lost.as_str();
    let candidate = crate::repositories::monitors::legacy_studio_error_predicate();
    let created = crate::repositories::monitors::LEGACY_CREATED_EXPRESSION;
    tx.execute_batch(&format!("CREATE TABLE runtime_monitors_generated (
        id TEXT PRIMARY KEY, record TEXT NOT NULL,
        stream_pending INTEGER GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN COALESCE(json_array_length(record,'$.streamQueue')>0,0) ELSE 0 END) STORED NOT NULL CHECK(stream_pending IN (0,1)),
        monitor_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN CASE WHEN json_type(record,'$.status')='text' AND json_extract(record,'$.status') IN ({statuses}) THEN json_extract(record,'$.status') ELSE '{lost}' END ELSE '{lost}' END) STORED NOT NULL CHECK(monitor_status IN ({statuses})),
        monitor_error_candidate INTEGER GENERATED ALWAYS AS ({candidate}) STORED NOT NULL CHECK(monitor_error_candidate IN (0,1)),
        monitor_created REAL GENERATED ALWAYS AS ({created}) STORED NOT NULL CHECK(monitor_created >= 0 AND monitor_created <= 1.7976931348623157e308)
    );
    INSERT INTO runtime_monitors_generated(rowid,id,record) SELECT rowid,id,record FROM runtime_monitors;
    DROP TABLE runtime_monitors;
    ALTER TABLE runtime_monitors_generated RENAME TO runtime_monitors;"))?;
    for definition in definitions {
        tx.execute_batch(&definition)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn rebuild_preserves_record_and_indexed_values() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY, record TEXT NOT NULL); INSERT INTO runtime_monitors(rowid,id,record) VALUES(42,'m','{\"status\":\"lost\",\"created\":2,\"streamQueue\":[1]}');").unwrap();
        let tx = db.transaction().unwrap();
        super::super::m2026_10_30_monitor_queue::apply(&tx).unwrap();
        super::super::m2026_10_30_monitor_status::apply(&tx).unwrap();
        super::super::m2026_10_30_monitor_errors::apply(&tx).unwrap();
        tx.execute_batch("CREATE INDEX runtime_monitor_queued_stream ON runtime_monitors(id) WHERE stream_pending=1; CREATE INDEX runtime_monitor_studio_error_v2 ON runtime_monitors(monitor_created) WHERE monitor_error_candidate=1;").unwrap();
        super::apply(&tx).unwrap();
        let rowid: i64 = tx
            .query_row("SELECT rowid FROM runtime_monitors WHERE id='m'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(rowid, 42);
        let hidden: i64 = tx.query_row("SELECT count(*) FROM pragma_table_xinfo('runtime_monitors') WHERE name IN ('stream_pending','monitor_status','monitor_error_candidate','monitor_created') AND hidden=3", [], |r| r.get(0)).unwrap();
        assert_eq!(hidden, 4);
        super::apply(&tx).unwrap();
        let row: (String, i64, String, i64, f64) = tx.query_row("SELECT record,stream_pending,monitor_status,monitor_error_candidate,monitor_created FROM runtime_monitors WHERE id='m'",[],|r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
        assert_eq!(
            row,
            (
                "{\"status\":\"lost\",\"created\":2,\"streamQueue\":[1]}".into(),
                1,
                "lost".into(),
                1,
                2.0
            )
        );
        let plan: String = tx.query_row("EXPLAIN QUERY PLAN SELECT id FROM runtime_monitors WHERE stream_pending=1 ORDER BY id",[],|r| r.get(3)).unwrap();
        assert!(plan.contains("runtime_monitor_queued_stream"), "{plan}");
        let plan: String = tx.query_row("EXPLAIN QUERY PLAN SELECT id FROM runtime_monitors WHERE monitor_error_candidate=1 AND monitor_created>=1",[],|r| r.get(3)).unwrap();
        assert!(plan.contains("runtime_monitor_studio_error_v2"), "{plan}");
        tx.execute("UPDATE runtime_monitors SET record='{\"status\":\"completed\",\"created\":3,\"streamQueue\":[]}' WHERE id='m'", []).unwrap();
        let values: (i64, String, i64, f64) = tx.query_row("SELECT stream_pending,monitor_status,monitor_error_candidate,monitor_created FROM runtime_monitors WHERE id='m'", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
        assert_eq!(values, (0, "completed".into(), 0, 3.0));
    }
}
