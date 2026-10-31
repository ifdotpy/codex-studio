use crate::StoreResult;
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if crate::schema::table_exists(tx, "runtime_rules")? {
        let mut query = tx.prepare("SELECT id,json_insert(record,'$.checks',0,'$.wakes',0,'$.inFlight',json('false'),'$.fileGeneration',0,'$.stallTimeoutSeconds',1800,'$.stallWakeGeneration',-1,'$.livenessCommand','', '$.event','') FROM runtime_rules")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let raw: String = row.get(1)?;
            let rule: studio_runtime_schema::rule_record::RuleRecord = serde_json::from_str(&raw)?;
            if rule.id != id {
                return Err(crate::StoreError::InvalidStoredRecord {
                    object: "rules",
                    id,
                });
            }
            tx.execute(
                "UPDATE runtime_rules SET record=?1 WHERE id=?2",
                rusqlite::params![serde_json::to_string(&rule)?, id],
            )?;
        }
    }
    if crate::schema::table_exists(tx, "runtime_monitors")? {
        let mut query = tx.prepare("SELECT id,CASE WHEN json_type(record,'$.stallProbeGeneration') IS NULL OR json_type(record,'$.stallProbeGeneration')='null' THEN json_remove(record,'$.stallProbeGeneration','$.stallProbeStarted') ELSE record END,COALESCE(json_array_length(record,'$.streamQueue'),0) FROM runtime_monitors")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let mut raw: String = row.get(1)?;
            let count: i64 = row.get(2)?;
            for index in 0..count {
                raw = tx.query_row(
                    "SELECT json_insert(?1,?2,'output',?3,'stdout')",
                    rusqlite::params![
                        raw,
                        format!("$.streamQueue[{index}].kind"),
                        format!("$.streamQueue[{index}].stream")
                    ],
                    |r| r.get(0),
                )?;
            }
            let monitor: studio_runtime_schema::monitor_record::MonitorRecord =
                serde_json::from_str(&raw)?;
            if monitor.id != id {
                return Err(crate::StoreError::InvalidStoredRecord {
                    object: "monitors",
                    id,
                });
            }
            tx.execute(
                "UPDATE runtime_monitors SET record=?1 WHERE id=?2",
                rusqlite::params![serde_json::to_string(&monitor)?, id],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    #[test]
    fn migration_normalizes_rule_and_drops_unread_extensions() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_rules(id TEXT PRIMARY KEY,record TEXT NOT NULL);")
            .unwrap();
        db.execute("INSERT INTO runtime_rules VALUES('r',?1)", [r#"{"id":"r","agent":"a","epoch":1,"name":"name","kind":"interval","status":"active","intervalSeconds":60,"nextAt":1,"created":1,"command":"","text":"","path":null,"unused":true}"#]).unwrap();
        let tx = db.transaction().unwrap();
        super::apply(&tx).unwrap();
        let raw: String = tx
            .query_row("SELECT record FROM runtime_rules WHERE id='r'", [], |row| {
                row.get(0)
            })
            .unwrap();
        let rule: studio_runtime_schema::rule_record::RuleRecord =
            serde_json::from_str(&raw).unwrap();
        assert_eq!(rule.id, "r");
        assert!(!raw.contains("unused"));
        assert!(!raw.contains("\"path\""));
        assert_eq!(rule.checks.get(), 0);
    }

    #[test]
    fn migration_refuses_duplicate_record_keys() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_rules(id TEXT PRIMARY KEY,record TEXT NOT NULL);")
            .unwrap();
        db.execute("INSERT INTO runtime_rules VALUES('r',?1)", [r#"{"id":"r","id":"r","agent":"a","epoch":1,"name":"name","kind":"interval","status":"active","intervalSeconds":60,"nextAt":1,"created":1,"command":"","text":""}"#]).unwrap();
        let tx = db.transaction().unwrap();
        assert!(super::apply(&tx).is_err());
    }

    #[test]
    fn migration_keeps_legacy_queued_output_and_drops_unused_monitor_field() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL);",
        )
        .unwrap();
        let raw = r#"{"id":"m","agent":"a","epoch":1,"status":"running","requestId":"request","activityAt":1,"activityGeneration":0,"bytes":0,"command":"echo x","created":1,"cwd":"/fixture","interactive":false,"livenessCommand":"","log":"/fixture/log","stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"successExitCodes":[0],"tail":"","timeout_ms":3600000,"wakeOn":"exit","resultRecoveredAt":9,"stallProbeStarted":2,"streamQueue":[{"cursor":"c","offset":0,"byteLength":1,"deltaBase64":"YQ==","stream":"stdout","operation":{"agent":"a","epoch":1,"accountKey":"default","connectionId":"conn"}}]}"#;
        db.execute("INSERT INTO runtime_monitors VALUES('m',?1)", [raw])
            .unwrap();
        let tx = db.transaction().unwrap();
        super::apply(&tx).unwrap();
        let raw: String = tx
            .query_row(
                "SELECT record FROM runtime_monitors WHERE id='m'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let record: studio_runtime_schema::monitor_record::MonitorRecord =
            serde_json::from_str(&raw).unwrap();
        assert!(matches!(
            record.stream_queue.unwrap().as_slice(),
            [studio_runtime_schema::monitor_record::MonitorQueueEvent::Output { .. }]
        ));
        assert!(!raw.contains("resultRecoveredAt"));
        assert!(!raw.contains("stallProbeStarted"));
    }
}
