//! Read current Python-written analytics usage rows for the cost service.

use rusqlite::Connection;
use serde_json::Value;
use thiserror::Error;

const MAX_RECORDS: i64 = 1_000_001;
const MAX_RECORD_BYTES: i64 = 1_048_576;
const MAX_TOTAL_BYTES: usize = 134_217_728;

#[derive(Debug, Error)]
pub enum CostUsageRepositoryError {
    #[error("analytics usage state is not initialized")]
    MissingFamily,
    #[error("invalid stored object analytics_usage/{0}")]
    InvalidStoredRow(String),
    #[error("cost usage read failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("cost usage record bound exceeded")]
    RecordLimit,
}

#[derive(Debug)]
pub struct CostUsageRow {
    pub sequence: i64,
    pub agent: Option<String>,
    pub thread: Option<String>,
    pub turn: Option<String>,
    pub at: f64,
    pub record: Value,
}

/// Reads one root's current source rows from a consistent SQLite snapshot.
pub fn read_cost_usage(
    db: &Connection,
    root: &str,
) -> Result<Vec<CostUsageRow>, CostUsageRepositoryError> {
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.sqlite_master WHERE type='table' AND name='analytics_usage')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(CostUsageRepositoryError::MissingFamily);
    }
    let mut statement = db.prepare(
        "SELECT seq,agent,thread,turn,at,id,length(record),record FROM analytics_usage WHERE root=?1 ORDER BY seq LIMIT ?2",
    )?;
    let mapped = statement.query_map((root, MAX_RECORDS), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, f64>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, String>(7)?,
        ))
    })?;
    let mut rows = Vec::new();
    let mut total_bytes = 0_usize;
    for item in mapped {
        if rows.len() == MAX_RECORDS as usize - 1 {
            return Err(CostUsageRepositoryError::RecordLimit);
        }
        let (sequence, agent, thread, turn, at, id, record_bytes, raw) = item?;
        if !(0..=MAX_RECORD_BYTES).contains(&record_bytes) {
            return Err(CostUsageRepositoryError::RecordLimit);
        }
        total_bytes = total_bytes
            .checked_add(record_bytes as usize)
            .ok_or(CostUsageRepositoryError::RecordLimit)?;
        if total_bytes > MAX_TOTAL_BYTES {
            return Err(CostUsageRepositoryError::RecordLimit);
        }
        if !at.is_finite() {
            return Err(CostUsageRepositoryError::InvalidStoredRow(id));
        }
        let record: Value = serde_json::from_str(&raw)
            .map_err(|_| CostUsageRepositoryError::InvalidStoredRow(id.clone()))?;
        if !record.is_object() {
            return Err(CostUsageRepositoryError::InvalidStoredRow(id));
        }
        rows.push(CostUsageRow {
            sequence,
            agent,
            thread,
            turn,
            at,
            record,
        });
    }
    Ok(rows)
}
