//! Bounded snapshot reads for lifecycle decisions; no independent connection.
use crate::{
    AgentRecord, AgentRepository, RequestRecord, StoreError, StoreResult, Table, WorkItemRecord,
};
use rusqlite::{Connection, Transaction};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const ROW_LIMIT: usize = 10_000;
const RECORD_LIMIT: usize = 1_048_576;
const RECORD_TABLES: &[&str] = &[
    "runtime_agents",
    "runtime_monitors",
    "runtime_requests",
    "runtime_tasks",
    "runtime_tool_requests",
    "runtime_work",
];

pub fn read(connection: &Connection) -> StoreResult<BTreeMap<String, Vec<Value>>> {
    let mut tables = BTreeMap::new();
    for table in RECORD_TABLES {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |r| r.get(0),
        )?;
        if !exists {
            continue;
        }
        let mut statement = connection.prepare(&format!(
            "SELECT id,record FROM {table} ORDER BY rowid LIMIT {}",
            ROW_LIMIT + 1
        ))?;
        let mut records = Vec::new();
        for row in
            statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        {
            let (id, raw) = row?;
            if raw.len() > RECORD_LIMIT {
                return Err(StoreError::InvalidStoredObject { table, id });
            }
            let record: Value =
                serde_json::from_str(&raw).map_err(|_| StoreError::InvalidStoredObject {
                    table,
                    id: id.clone(),
                })?;
            if record.get("id").and_then(Value::as_str) != Some(&id) {
                return Err(StoreError::InvalidStoredObject { table, id });
            }
            records.push(record);
        }
        tables.insert((*table).to_owned(), records);
    }
    let mut statement = connection.prepare(&format!(
        "SELECT id,agent,status,epoch FROM runtime_events ORDER BY rowid LIMIT {}",
        ROW_LIMIT + 1
    ))?;
    let events = statement.query_map([], |r| Ok(json!({"id":r.get::<_,String>(0)?,"agent":r.get::<_,String>(1)?,"status":r.get::<_,String>(2)?,"epoch":r.get::<_,i64>(3)?})))?.collect::<Result<Vec<_>,_>>()?;
    tables.insert("runtime_events".to_owned(), events);
    // These tables are lazy and have columns rather than JSON records. Any
    // affected voice data requires the voice owner's termination adapter.
    for table in [
        "voice_sessions",
        "voice_records",
        "voice_deliveries",
        "voice_approvals",
    ] {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |r| r.get(0),
        )?;
        if !exists {
            continue;
        }
        let mut statement = connection.prepare(&format!(
            "SELECT id,agent FROM {table} LIMIT {}",
            ROW_LIMIT + 1
        ))?;
        let rows = statement
            .query_map([], |r| {
                Ok(json!({"id":r.get::<_,String>(0)?,"agent":r.get::<_,String>(1)?}))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        tables.insert(table.to_owned(), rows);
    }
    Ok(tables)
}

pub fn write(transaction: &Transaction<'_>, table: &str, record: &Value) -> StoreResult<()> {
    match table {
        "runtime_agents" => AgentRepository::put(
            transaction,
            &serde_json::from_value::<AgentRecord>(record.clone())?,
        ),
        "runtime_requests" => Table::<RequestRecord>::new(transaction)
            .put(&RequestRecord::from_value(record.clone())?),
        "runtime_work" => Table::<WorkItemRecord>::new(transaction)
            .put(&WorkItemRecord::from_value(record.clone())?),
        _ => Err(StoreError::RecordContract("Unsupported management write")),
    }
}

pub fn cancel_pending(transaction: &Transaction<'_>, agent: &str) -> StoreResult<()> {
    transaction.execute(
        "UPDATE runtime_events SET status='cancelled' WHERE agent=?1 AND status='pending'",
        [agent],
    )?;
    Ok(())
}

inventory::submit! {
    super::SchemaRegistration {
        repository: "agent_management",
        requirements: &crate::schema::SchemaRequirements {
            tables: &[], conditional_tables: &[], triggers: &[], conditional_triggers: &[], connection_functions: &[], bootstrap_tables: &[], bootstrap_triggers: &[], bootstrap_connection_functions: &[],
        },
    }
}
