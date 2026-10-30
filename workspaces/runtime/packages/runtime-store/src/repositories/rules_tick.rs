//! Bounded rule tick reads; rule writes reuse the canonical rules repository.

use rusqlite::{Connection, params};
use serde_json::Value;

use crate::{CachedSql, MonitorRecord, RuleRecord, StoreError, StoreResult};

pub const RULE_BATCH: usize = 128;
const MAX_RECORD_BYTES: usize = 1_048_576;
const MAX_TEAM_RECORDS: usize = 4096;

pub struct RuleRow {
    pub rowid: i64,
    pub rule: RuleRecord,
}

pub struct RuleActivity {
    pub agents: Vec<Value>,
    pub monitors: Vec<MonitorRecord>,
}

pub struct RulesTickRepository;

impl RulesTickRepository {
    pub fn page(connection: &Connection, after_rowid: i64) -> StoreResult<Vec<RuleRow>> {
        let mut query = connection.prepare_cached(
            "SELECT rowid,id,record FROM runtime_rules WHERE rowid>?1 ORDER BY rowid LIMIT ?2",
        )?;
        let mut rows = query.query(params![after_rowid, RULE_BATCH as i64])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(1)?;
            let raw: String = row.get(2)?;
            let invalid = || StoreError::InvalidStoredRecord {
                object: "rules",
                id: id.clone(),
            };
            if raw.len() > MAX_RECORD_BYTES {
                return Err(invalid());
            }
            let rule: RuleRecord = serde_json::from_str(&raw).map_err(|_| invalid())?;
            if rule.id() != &id {
                return Err(StoreError::InvalidStoredRecord {
                    object: "rules",
                    id,
                });
            }
            result.push(RuleRow {
                rowid: row.get(0)?,
                rule,
            });
        }
        Ok(result)
    }

    /// Read current activity inside the writer that latches the alert. Panels are not
    /// command monitors and live in a separate table; cancelled commands do not count.
    pub fn activity(connection: &Connection, root_id: &str) -> StoreResult<RuleActivity> {
        let agents = read_activity(
            connection,
            "SELECT id,record FROM runtime_agents WHERE root_id=?1 ORDER BY rowid LIMIT ?2",
            root_id,
            "agents",
        )?;
        let mut query = connection.prepare_cached(
            "SELECT m.id,m.record FROM runtime_monitors m JOIN runtime_agents a ON a.id=json_extract(m.record,'$.agent') \
             WHERE a.root_id=?1 AND json_extract(m.record,'$.status')='running' ORDER BY m.rowid LIMIT ?2",
        )?;
        let mut rows = query.query(params![root_id, MAX_TEAM_RECORDS as i64 + 1])?;
        let mut monitors = Vec::new();
        while let Some(row) = rows.next()? {
            if monitors.len() == MAX_TEAM_RECORDS {
                return Err(StoreError::SchemaMismatch(
                    "rule activity snapshot exceeds its bound".into(),
                ));
            }
            let id: String = row.get(0)?;
            let raw: String = row.get(1)?;
            let invalid = || StoreError::InvalidStoredRecord {
                object: "monitors",
                id: id.clone(),
            };
            if raw.len() > MAX_RECORD_BYTES {
                return Err(invalid());
            }
            let monitor: MonitorRecord = serde_json::from_str(&raw).map_err(|_| invalid())?;
            if monitor.id != id {
                return Err(invalid());
            }
            monitors.push(monitor);
        }
        Ok(RuleActivity { agents, monitors })
    }

    /// Whether agent `agent` has input that a failed start kept pending at
    /// `epoch`: a pending event of that epoch with a stored start error.
    pub fn held_input(connection: &Connection, agent: &str, epoch: u64) -> StoreResult<bool> {
        let epoch = i64::try_from(epoch)
            .map_err(|_| StoreError::SchemaMismatch("invalid agent epoch".to_owned()))?;
        Ok(connection.query_row_cached(
            "SELECT EXISTS(SELECT 1 FROM runtime_events WHERE status='pending' AND agent=?1 AND epoch=?2 AND COALESCE(error,'')<>'')",
            params![agent, epoch],
            |row| row.get::<_, bool>(0),
        )?)
    }
}

fn read_activity(
    connection: &Connection,
    sql: &str,
    root_id: &str,
    object: &'static str,
) -> StoreResult<Vec<Value>> {
    let mut query = connection.prepare_cached(sql)?;
    let mut rows = query.query(params![root_id, MAX_TEAM_RECORDS as i64 + 1])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        if result.len() == MAX_TEAM_RECORDS {
            return Err(StoreError::SchemaMismatch(
                "rule activity snapshot exceeds its bound".to_owned(),
            ));
        }
        let id: String = row.get(0)?;
        let mut record = decode(row.get(1)?, object, &id)?;
        crate::agent_volatile::merge(connection, &id, &mut record)?;
        result.push(record);
    }
    Ok(result)
}

fn decode(raw: String, object: &'static str, id: &str) -> StoreResult<Value> {
    let invalid = || StoreError::InvalidStoredRecord {
        object,
        id: id.to_owned(),
    };
    if raw.len() > MAX_RECORD_BYTES {
        return Err(invalid());
    }
    serde_json::from_str(&raw).map_err(|_| invalid())
}

#[cfg(test)]
mod typed_tests {
    use super::*;

    #[test]
    fn page_checks_typed_rule_and_physical_identity() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_rules(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            .unwrap();
        let raw = r#"{"id":"r","agent":"a","epoch":1,"name":"name","kind":"file","path":"fixture-file","status":"active","intervalSeconds":60,"nextAt":1,"created":1,"command":"","text":"","checks":0,"wakes":0,"inFlight":false,"fileGeneration":0,"stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"livenessCommand":"","event":""}"#;
        db.execute("INSERT INTO runtime_rules VALUES ('r',?1)", [raw])
            .unwrap();
        assert_eq!(RulesTickRepository::page(&db, 0).unwrap()[0].rule.id, "r");
        db.execute(
            "UPDATE runtime_rules SET record=?1",
            [raw.replace("\"id\":\"r\"", "\"id\":\"other\"")],
        )
        .unwrap();
        assert!(RulesTickRepository::page(&db, 0).is_err());
        db.execute(
            "UPDATE runtime_rules SET record=?1",
            [raw.replace("\"path\":\"fixture-file\",", "")],
        )
        .unwrap();
        assert!(RulesTickRepository::page(&db, 0).is_err());
    }
}
