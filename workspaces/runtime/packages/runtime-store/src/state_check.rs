use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde::Serialize;
use serde_json::Value;

use crate::ProfileRecord;
use crate::StoreResult;
use crate::records::{
    AgentRecord, AnnotationRecord, CheckpointRecord, ComplaintRecord, EventMetadataRecord,
    GraphAgentRecord, JsonRecord, MonitorRecord, PlanRecord, RequestRecord, RuleRecord, TaskRecord,
    TerminalRecord, TranscriptAssetRecord, TranscriptItemRecord, UsageResumeRecord, WorkItemRecord,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StateCheckFailure {
    pub pointer: String,
    pub keyword: String,
    pub count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StateCheckTable {
    pub table: &'static str,
    pub row_count: u64,
    pub rejected_count: u64,
    pub failures: Vec<StateCheckFailure>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StateCheckReport {
    pub tables: Vec<StateCheckTable>,
}

/// Validate every typed JSON record in an existing canvas database without taking
/// the runtime lock, creating files, or opening the database for writes.
pub fn check_state_readonly(state_dir: &Path) -> StoreResult<StateCheckReport> {
    let path = state_dir.join("canvas.sqlite3");
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.pragma_update(None, "query_only", "ON")?;

    Ok(StateCheckReport {
        tables: vec![
            check_table::<AgentRecord>(&connection)?,
            check_table::<TaskRecord>(&connection)?,
            check_table::<PlanRecord>(&connection)?,
            check_table::<AnnotationRecord>(&connection)?,
            check_table::<crate::budget::BudgetRecord>(&connection)?,
            crate::budget::check_usage_readonly(&connection)?,
            check_table::<RequestRecord>(&connection)?,
            check_table::<ComplaintRecord>(&connection)?,
            check_table::<CheckpointRecord>(&connection)?,
            check_table::<MonitorRecord>(&connection)?,
            check_table::<TerminalRecord>(&connection)?,
            check_table::<UsageResumeRecord>(&connection)?,
            check_table::<WorkItemRecord>(&connection)?,
            check_table::<RuleRecord>(&connection)?,
            check_table::<TranscriptItemRecord>(&connection)?,
            check_table::<TranscriptAssetRecord>(&connection)?,
            check_table::<EventMetadataRecord>(&connection)?,
            check_table::<GraphAgentRecord>(&connection)?,
            check_table::<ProfileRecord>(&connection)?,
        ],
    })
}

fn check_table<R: JsonRecord>(connection: &Connection) -> StoreResult<StateCheckTable> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [R::TABLE],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(StateCheckTable {
            table: R::TABLE,
            row_count: 0,
            rejected_count: 0,
            failures: Vec::new(),
        });
    }

    let sql = format!(
        "SELECT id,record FROM \"{}\"",
        R::TABLE.replace('"', "\"\"")
    );
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query([])?;
    let mut row_count = 0;
    let mut rejected_count = 0;
    let mut failures = BTreeMap::<(String, String), u64>::new();
    while let Some(row) = rows.next()? {
        row_count += 1;
        let (row_id, record) = match (row.get_ref(0)?, row.get_ref(1)?) {
            (ValueRef::Text(id), ValueRef::Text(encoded)) => {
                let row_id = std::str::from_utf8(id).ok();
                let encoded = std::str::from_utf8(encoded).ok();
                match (row_id, encoded) {
                    (Some(id), Some(encoded)) => (id.to_owned(), encoded.to_owned()),
                    _ => {
                        record_failure(&mut failures, &mut rejected_count, "", "storageType");
                        continue;
                    }
                }
            }
            _ => {
                record_failure(&mut failures, &mut rejected_count, "", "storageType");
                continue;
            }
        };
        let value: Value = match serde_json::from_str(&record) {
            Ok(value) => value,
            Err(_) => {
                record_failure(&mut failures, &mut rejected_count, "", "json");
                continue;
            }
        };
        if let Some((pointer, keyword)) = studio_runtime_schema::first_error(R::SCHEMA, &value)
            .map_err(crate::StoreError::SchemaValidation)?
        {
            record_failure(&mut failures, &mut rejected_count, &pointer, &keyword);
            continue;
        }
        let decoded = match serde_json::from_value::<R>(value) {
            Ok(record) => record,
            Err(_) => {
                record_failure(&mut failures, &mut rejected_count, "", "recordType");
                continue;
            }
        };
        if R::KEY_MODE == crate::records::RecordKeyMode::PayloadId
            && decoded.record_key().map(AsRef::as_ref) != Some(row_id.as_str())
        {
            record_failure(&mut failures, &mut rejected_count, "/id", "rowKey");
        }
    }

    Ok(StateCheckTable {
        table: R::TABLE,
        row_count,
        rejected_count,
        failures: failures
            .into_iter()
            .map(|((pointer, keyword), count)| StateCheckFailure {
                pointer,
                keyword,
                count,
            })
            .collect(),
    })
}

fn record_failure(
    failures: &mut BTreeMap<(String, String), u64>,
    rejected_count: &mut u64,
    pointer: &str,
    keyword: &str,
) {
    *rejected_count += 1;
    *failures
        .entry((pointer.to_owned(), keyword.to_owned()))
        .or_default() += 1;
}
