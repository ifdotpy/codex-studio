//! Bounded snapshot reads for lifecycle decisions; no independent connection.
use crate::schema::{SchemaRequirements, TableRequirement};
use crate::{
    AgentRecord, AgentRepository, RequestRecord, StoreError, StoreResult, Table, WorkItemRecord,
};
use rusqlite::{Connection, Transaction};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const RECORD_TABLES: &[&str] = &[
    "runtime_agents",
    "runtime_monitors",
    "runtime_requests",
    "runtime_tasks",
    "runtime_tool_requests",
    "runtime_work",
];

pub fn read(connection: &Connection) -> StoreResult<BTreeMap<String, Vec<Value>>> {
    read_for_agents(connection, None)
}

/// Parent of every agent as `(id, parentId)` in rowid order, without parsing
/// whole records. The expression matches the `runtime_agent_valid_parent` index.
/// A malformed record has no parent; its own read fails later if it is in scope.
pub fn read_agent_topology(connection: &Connection) -> StoreResult<Vec<(String, Option<String>)>> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_agents')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare_cached(
        "SELECT id,CASE WHEN json_valid(record) THEN json_extract(record,'$.parentId') END \
         FROM runtime_agents ORDER BY rowid",
    )?;
    Ok(statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?)
}

/// Validated agent records for exactly `ids`, in rowid order; absent ids are skipped.
pub fn read_agents(connection: &Connection, ids: &BTreeSet<String>) -> StoreResult<Vec<Value>> {
    let mut statement =
        connection.prepare_cached("SELECT rowid,record FROM runtime_agents WHERE id=?1")?;
    let mut rows = Vec::with_capacity(ids.len());
    for id in ids {
        let found = statement
            .query_row([id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                error => Err(error),
            })?;
        if let Some((rowid, raw)) = found {
            rows.push((
                rowid,
                parse_record(connection, "runtime_agents", id.clone(), raw)?,
            ));
        }
    }
    rows.sort_by_key(|(rowid, _)| *rowid);
    Ok(rows.into_iter().map(|(_, record)| record).collect())
}

/// Read lifecycle-owned history for the supplied owners (the caller loads their
/// agent rows with `read_agents`). Lifecycle actions pass their target's descendant tree;
/// their decisions never inspect unrelated task/monitor/event/voice history.
/// `None` reads the complete snapshot for repository callers.
pub fn read_for_agents(
    connection: &Connection,
    owners: Option<&BTreeSet<String>>,
) -> StoreResult<BTreeMap<String, Vec<Value>>> {
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
        let mut records = Vec::new();
        let owner_field = match *table {
            "runtime_agents" if owners.is_some() => continue,
            "runtime_agents" => None,
            "runtime_work" => Some("owner"),
            _ => Some("agent"),
        };
        if let (Some(owners), Some(owner_field)) = (owners, owner_field) {
            let mut ordered = Vec::new();
            for owner in owners {
                let mut statement = connection.prepare_cached(&format!(
                    "SELECT rowid,id,record FROM {table} \
                     WHERE json_extract(record,'$.{owner_field}')=?1"
                ))?;
                let rows = statement.query_map([owner], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?;
                ordered.extend(rows.collect::<Result<Vec<_>, _>>()?);
            }
            ordered.sort_by_key(|(rowid, _, _)| *rowid);
            records.reserve(ordered.len());
            for (_, id, raw) in ordered {
                records.push(parse_record(connection, table, id, raw)?);
            }
        } else {
            let mut statement = connection
                .prepare_cached(&format!("SELECT id,record FROM {table} ORDER BY rowid"))?;
            for row in
                statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            {
                let (id, raw) = row?;
                records.push(parse_record(connection, table, id, raw)?);
            }
        }
        tables.insert((*table).to_owned(), records);
    }
    let events = if let Some(owners) = owners {
        let mut ordered = Vec::new();
        for owner in owners {
            let mut statement = connection.prepare_cached(
                "SELECT rowid,id,agent,status,epoch FROM runtime_events WHERE agent=?1",
            )?;
            let rows = statement.query_map([owner], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })?;
            ordered.extend(rows.collect::<Result<Vec<_>, _>>()?);
        }
        ordered.sort_by_key(|(rowid, ..)| *rowid);
        ordered
            .into_iter()
            .map(|(_, id, agent, status, epoch)| json!({"id":id,"agent":agent,"status":status,"epoch":epoch}))
            .collect()
    } else {
        let mut statement = connection
            .prepare_cached("SELECT id,agent,status,epoch FROM runtime_events ORDER BY rowid")?;
        statement
            .query_map([], |r| Ok(json!({"id":r.get::<_,String>(0)?,"agent":r.get::<_,String>(1)?,"status":r.get::<_,String>(2)?,"epoch":r.get::<_,i64>(3)?})))?
            .collect::<Result<Vec<_>,_>>()?
    };
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
        let rows = if let Some(owners) = owners {
            let mut statement = connection
                .prepare_cached(&format!("SELECT {columns} FROM {table} WHERE agent=?1"))?;
            let mut rows = Vec::new();
            for owner in owners {
                rows.extend(
                    statement
                        .query_map([owner], |r| voice_row(table, r))?
                        .collect::<Result<Vec<_>, _>>()?,
                );
            }
            rows
        } else {
            let mut statement =
                connection.prepare_cached(&format!("SELECT {columns} FROM {table}"))?;
            statement
                .query_map([], |r| voice_row(table, r))?
                .collect::<Result<Vec<_>, _>>()?
        };
        tables.insert(table.to_owned(), rows);
    }
    Ok(tables)
}

fn parse_record(
    connection: &Connection,
    table: &'static str,
    id: String,
    raw: String,
) -> StoreResult<Value> {
    let mut record: Value =
        serde_json::from_str(&raw).map_err(|_| StoreError::InvalidStoredObject {
            table,
            id: id.clone(),
        })?;
    if record.get("id").and_then(Value::as_str) != Some(&id) {
        return Err(StoreError::InvalidStoredObject { table, id });
    }
    crate::agent_volatile::merge_if_agent(connection, table, &id, &mut record)?;
    Ok(record)
}

fn voice_row(table: &str, row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let mut value = json!({"id":row.get::<_,String>(0)?,"agent":row.get::<_,String>(1)?});
    if table == "voice_sessions" {
        value["state"] = json!(row.get::<_, Option<String>>(2)?);
        value["ended"] = json!(row.get::<_, Option<f64>>(3)?);
    }
    Ok(value)
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

/// SQL predicate over `runtime_agents.record` selecting soft-deleted agents. Single owner.
pub const DELETED_AGENT_SQL: &str = "(json_extract(record,'$.deletedAt') IS NOT NULL AND json_extract(record,'$.deletedAt') NOT IN (0,''))";

const ABANDONED_INPUT_ERROR: &str = "Agent deleted; delivery outcome unknown";

/// Settle the unsettled input rows of an agent that was just stopped or deleted.
///
/// Pending rows are always cancelled. When the agent is deleted, nothing will ever
/// consume its rows, so reserved rows (provably not submitted) are cancelled too, and
/// dispatching or uncertain rows become `failed` with the uncertainty kept in `error`:
/// they are never marked delivered and never resent.
pub fn settle_inputs(transaction: &Transaction<'_>, agent: &str) -> StoreResult<()> {
    transaction.execute(
        "UPDATE runtime_events SET status='cancelled' WHERE agent=?1 AND status='pending'",
        [agent],
    )?;
    settle_deleted_inputs(transaction, Some(agent))
}

/// Startup sweep: apply the deleted-agent rule to rows left by earlier versions.
/// Also cancels pending rows stamped with an epoch older than their agent's current
/// epoch, which the scheduler can never select.
pub fn settle_orphaned_inputs(transaction: &Transaction<'_>) -> StoreResult<()> {
    settle_deleted_inputs(transaction, None)?;
    transaction.execute(
        "UPDATE runtime_events SET status='cancelled',error='Superseded by a newer agent epoch' \
         WHERE status='pending' AND epoch < (SELECT json_extract(a.record,'$.epoch') \
         FROM runtime_agents a WHERE a.id=runtime_events.agent)",
        [],
    )?;
    Ok(())
}

fn settle_deleted_inputs(transaction: &Transaction<'_>, agent: Option<&str>) -> StoreResult<()> {
    // One agent: seek its rows by agent. The sweep (`None`) tests every agent.
    let deleted = if agent.is_some() {
        format!(
            "agent=?1 AND EXISTS(SELECT 1 FROM runtime_agents WHERE id=?1 AND {DELETED_AGENT_SQL})"
        )
    } else {
        format!("agent IN (SELECT id FROM runtime_agents WHERE {DELETED_AGENT_SQL}) AND ?1 IS NULL")
    };
    transaction.execute(
        &format!(
            "UPDATE runtime_events SET status='cancelled',error='Agent deleted' \
             WHERE status IN ('pending','reserved') AND {deleted}"
        ),
        [agent],
    )?;
    transaction.execute(
        &format!(
            "UPDATE runtime_events SET status='failed',error=?2 \
             WHERE status IN ('dispatching','uncertain') AND {deleted}"
        ),
        rusqlite::params![agent, ABANDONED_INPUT_ERROR],
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
    use super::{read, read_for_agents};
    use rusqlite::Connection;
    use std::collections::BTreeSet;

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

    #[test]
    fn scoped_snapshot_skips_agent_rows_and_uses_owner_index() {
        let connection = Connection::open_in_memory().expect("in-memory database");
        connection
            .execute_batch(
                "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE INDEX runtime_task_agent ON runtime_tasks(json_extract(record,'$.agent'));
                 CREATE TABLE runtime_events(id TEXT,agent TEXT,status TEXT,epoch INTEGER);
                 CREATE INDEX runtime_event_agent ON runtime_events(agent);
                 INSERT INTO runtime_agents VALUES
                    ('root','{\"id\":\"root\"}'),('child','{\"id\":\"child\"}'),
                    ('unrelated','{\"id\":\"unrelated\"}');
                 INSERT INTO runtime_tasks VALUES
                    ('child-first','{\"id\":\"child-first\",\"agent\":\"child\"}'),
                    ('other','{\"id\":\"other\",\"agent\":\"unrelated\"}'),
                    ('root-second','{\"id\":\"root-second\",\"agent\":\"root\"}');
                 INSERT INTO runtime_events VALUES
                    ('child-event','child','pending',1),('other-event','unrelated','pending',1);",
            )
            .expect("fixture schema and records");
        connection
            .execute_batch(crate::agent_volatile::agent_volatile_table_sql())
            .expect("volatile agent table creates");

        let all = read(&connection).expect("complete snapshot");
        let owners = BTreeSet::from(["root".to_owned(), "child".to_owned()]);
        let scoped = read_for_agents(&connection, Some(&owners)).expect("scoped snapshot");

        assert!(!scoped.contains_key("runtime_agents"));
        assert_eq!(all["runtime_agents"].len(), 3);
        let agents = super::read_agents(&connection, &owners).expect("scoped agents");
        assert_eq!(agents.len(), 2);
        let topology = super::read_agent_topology(&connection).expect("topology");
        assert_eq!(topology.len(), 3);
        assert_eq!(
            scoped["runtime_tasks"]
                .iter()
                .map(|row| row["id"].as_str().expect("task id"))
                .collect::<Vec<_>>(),
            vec!["child-first", "root-second"]
        );
        assert_eq!(scoped["runtime_events"].len(), 1);

        let plan = connection
            .prepare(
                "EXPLAIN QUERY PLAN SELECT rowid,id,record FROM runtime_tasks \
                 WHERE json_extract(record,'$.agent')=?1",
            )
            .expect("explain scoped task lookup")
            .query_map(["child"], |row| row.get::<_, String>(3))
            .expect("query plan")
            .collect::<Result<Vec<_>, _>>()
            .expect("plan rows")
            .join(" ");
        assert!(plan.contains("runtime_task_agent"), "{plan}");
    }
}
