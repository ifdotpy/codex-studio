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
    WorkspaceOperationRecord,
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

    let mut tables = vec![
        check_table::<AgentRecord>(&connection)?,
        check_table::<TaskRecord>(&connection)?,
        check_table::<PlanRecord>(&connection)?,
        check_table::<AnnotationRecord>(&connection)?,
        check_table::<RequestRecord>(&connection)?,
        check_table::<ComplaintRecord>(&connection)?,
        check_table::<CheckpointRecord>(&connection)?,
        check_table::<MonitorRecord>(&connection)?,
        check_table::<TerminalRecord>(&connection)?,
        check_table::<UsageResumeRecord>(&connection)?,
        check_table::<WorkItemRecord>(&connection)?,
        check_table::<WorkspaceOperationRecord>(&connection)?,
        check_table::<RuleRecord>(&connection)?,
        check_table::<TranscriptItemRecord>(&connection)?,
        check_table::<TranscriptAssetRecord>(&connection)?,
        check_table::<EventMetadataRecord>(&connection)?,
        check_table::<GraphAgentRecord>(&connection)?,
        check_table::<ProfileRecord>(&connection)?,
    ];
    tables.extend(check_system_tables(&connection)?);
    Ok(StateCheckReport { tables })
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

fn check_system_tables(connection: &Connection) -> StoreResult<Vec<StateCheckTable>> {
    use crate::system_reads;
    let mut reports = Vec::new();
    for (table, column) in [
        ("runtime_access_clients", "record"),
        ("runtime_access_discovered", "record"),
        ("runtime_chat_messages", "deliveries"),
        ("runtime_events", "status"),
        ("runtime_execution_effects", "record"),
        ("runtime_execution_runs", "record"),
        ("runtime_federation_rooms", "record"),
        ("runtime_operation_receipts", "result"),
        ("runtime_projects", "record"),
        ("runtime_rooms", "record"),
        ("runtime_tool_request_aliases", "request"),
        ("runtime_tool_requests", "record"),
        ("runtime_tool_results", "result"),
        ("sync_entities", "payload"),
    ] {
        let mut report = StateCheckTable {
            table,
            row_count: 0,
            rejected_count: 0,
            failures: Vec::new(),
        };
        if !system_reads::exists(connection, table)? {
            reports.push(report);
            continue;
        }
        let condition = if table == "sync_entities" {
            " WHERE deleted=0 AND collection IN ('agent','project','room','request','complaint')"
        } else {
            ""
        };
        let query = if table == "runtime_chat_messages" {
            "SELECT id,deliveries,seq,room,sender,text,created FROM runtime_chat_messages"
                .to_owned()
        } else if table == "runtime_events" {
            "SELECT id,json_object('id',id,'status',status,'error',error) FROM runtime_events"
                .to_owned()
        } else if table == "runtime_server_outbox" {
            "SELECT id,json_object('id',id) FROM runtime_server_outbox".to_owned()
        } else if table == "runtime_tool_request_aliases" {
            "SELECT alias,json_object('agent',agent,'alias',alias,'request',request) FROM runtime_tool_request_aliases".to_owned()
        } else {
            format!("SELECT id,{column} FROM {table}{condition}")
        };
        let mut statement = connection.prepare(&query)?;
        let mut rows = statement.query([])?;
        let mut failures = BTreeMap::new();
        while let Some(row) = rows.next()? {
            report.row_count += 1;
            let (id, raw) = match (row.get::<_, String>(0), row.get::<_, String>(1)) {
                (Ok(id), Ok(raw)) => (id, raw),
                _ => {
                    record_failure(&mut failures, &mut report.rejected_count, "", "storageType");
                    continue;
                }
            };
            let mut value = match serde_json::from_str::<Value>(&raw) {
                Ok(v) => v,
                Err(_) => {
                    record_failure(&mut failures, &mut report.rejected_count, "", "json");
                    continue;
                }
            };
            if table == "runtime_chat_messages" {
                value = serde_json::json!({"id":id,"deliveries":value,"seq":row.get::<_,i64>(2)?,"room":row.get::<_,String>(3)?,"sender":row.get::<_,String>(4)?,"text":row.get::<_,String>(5)?,"created":row.get::<_,f64>(6)?});
            }
            let schema = studio_runtime_schema::table_schema(table).ok_or_else(|| {
                crate::StoreError::SchemaValidation(format!("missing table schema {table}"))
            })?;
            if let Some((pointer, keyword)) = studio_runtime_schema::first_error(schema, &value)
                .map_err(crate::StoreError::SchemaValidation)?
            {
                record_failure(
                    &mut failures,
                    &mut report.rejected_count,
                    &pointer,
                    &keyword,
                );
            } else if system_reads::validate(table, &id, value).is_err() {
                record_failure(&mut failures, &mut report.rejected_count, "/id", "rowKey");
            }
        }
        report.failures = failures
            .into_iter()
            .map(|((pointer, keyword), count)| StateCheckFailure {
                pointer,
                keyword,
                count,
            })
            .collect();
        reports.push(report);
    }
    Ok(reports)
}
