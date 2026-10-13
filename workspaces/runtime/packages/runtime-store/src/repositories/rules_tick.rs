//! Bounded rule tick reads; rule writes reuse the canonical rules repository.

use rusqlite::{Connection, params};
use serde_json::Value;

use crate::{RuleRecord, StoreError, StoreResult};

pub const RULE_BATCH: usize = 128;
const MAX_RECORD_BYTES: usize = 1_048_576;
const MAX_TEAM_RECORDS: usize = 4096;

pub struct RuleRow {
    pub rowid: i64,
    pub rule: RuleRecord,
}

pub struct RuleActivity {
    pub agents: Vec<Value>,
    pub monitors: Vec<Value>,
}

pub struct RulesTickRepository;

impl RulesTickRepository {
    pub fn page(connection: &Connection, after_rowid: i64) -> StoreResult<Vec<RuleRow>> {
        let mut query = connection.prepare(
            "SELECT rowid,id,record FROM runtime_rules WHERE rowid>?1 ORDER BY rowid LIMIT ?2",
        )?;
        let mut rows = query.query(params![after_rowid, RULE_BATCH as i64])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(1)?;
            let value = decode(row.get(2)?, "rules", &id)?;
            let rule = RuleRecord::from_value(value)?;
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
            "SELECT id,record FROM runtime_agents WHERE json_extract(record,'$.rootId')=?1 ORDER BY rowid LIMIT ?2",
            root_id,
            "agents",
        )?;
        let monitors = read_activity(
            connection,
            "SELECT m.id,m.record FROM runtime_monitors m JOIN runtime_agents a ON a.id=json_extract(m.record,'$.agent') \
             WHERE json_extract(a.record,'$.rootId')=?1 AND json_extract(m.record,'$.status')='running' ORDER BY m.rowid LIMIT ?2",
            root_id,
            "monitors",
        )?;
        Ok(RuleActivity { agents, monitors })
    }
}

fn read_activity(
    connection: &Connection,
    sql: &str,
    root_id: &str,
    object: &'static str,
) -> StoreResult<Vec<Value>> {
    let mut query = connection.prepare(sql)?;
    let mut rows = query.query(params![root_id, MAX_TEAM_RECORDS as i64 + 1])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        if result.len() == MAX_TEAM_RECORDS {
            return Err(StoreError::SchemaMismatch(
                "rule activity snapshot exceeds its bound".to_owned(),
            ));
        }
        let id: String = row.get(0)?;
        result.push(decode(row.get(1)?, object, &id)?);
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
