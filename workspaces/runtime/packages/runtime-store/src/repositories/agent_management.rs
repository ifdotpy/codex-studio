//! Bounded snapshot reads for lifecycle decisions; no independent connection.
use crate::schema::{SchemaRequirements, TableRequirement};
use crate::{
    AgentRecord, AgentRepository, RequestRecord, StoreError, StoreResult, Table, WorkItemRecord,
};
use rusqlite::{Connection, Transaction};
use serde_json::{Value, json};
use std::collections::BTreeMap;

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
        let mut statement =
            connection.prepare_cached(&format!("SELECT id,record FROM {table} ORDER BY rowid"))?;
        let mut records = Vec::new();
        for row in
            statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        {
            let (id, raw) = row?;
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
    let mut statement = connection
        .prepare_cached("SELECT id,agent,status,epoch FROM runtime_events ORDER BY rowid")?;
    let events = statement.query_map([], |r| Ok(json!({"id":r.get::<_,String>(0)?,"agent":r.get::<_,String>(1)?,"status":r.get::<_,String>(2)?,"epoch":r.get::<_,i64>(3)?})))?.collect::<Result<Vec<_>,_>>()?;
    tables.insert("runtime_events".to_owned(), events);
    // These lazy tables use columns rather than JSON records. Session liveness
    // distinguishes stop from deletion, which also removes saved voice data.
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
        let columns = if table == "voice_sessions" {
            "id,agent,state,ended"
        } else {
            "id,agent"
        };
        let mut statement = connection.prepare_cached(&format!("SELECT {columns} FROM {table}"))?;
        let rows = statement
            .query_map([], |r| {
                let mut row = json!({"id":r.get::<_,String>(0)?,"agent":r.get::<_,String>(1)?});
                if table == "voice_sessions" {
                    row["state"] = json!(r.get::<_, Option<String>>(2)?);
                    row["ended"] = json!(r.get::<_, Option<f64>>(3)?);
                }
                Ok(row)
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

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[TableRequirement {
        name: "runtime_events",
        columns: &["id", "agent", "status", "epoch"],
    }],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

inventory::submit! {
    super::SchemaRegistration {
        repository: "agent_management",
        requirements: &REQUIREMENTS,
    }
}

#[cfg(test)]
mod tests {
    use super::read;
    use rusqlite::Connection;

    #[test]
    fn snapshot_reads_all_rows_above_the_former_cap() {
        let connection = Connection::open_in_memory().expect("in-memory database");
        connection
            .execute_batch(
                "CREATE TABLE runtime_events(id TEXT,agent TEXT,status TEXT,epoch INTEGER);",
            )
            .expect("runtime events table");
        let transaction = connection.unchecked_transaction().expect("transaction");
        {
            let mut insert = transaction
                .prepare("INSERT INTO runtime_events VALUES(?1,'agent','pending',0)")
                .expect("insert statement");
            for index in 0..10_001 {
                insert
                    .execute([format!("event-{index}")])
                    .expect("insert event");
            }
        }
        transaction.commit().expect("commit events");

        let snapshot = read(&connection).expect("complete snapshot");

        assert_eq!(snapshot["runtime_events"].len(), 10_001);
    }
}
