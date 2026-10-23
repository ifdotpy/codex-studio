use rusqlite::{Connection, params_from_iter, types::Value as SqlValue};
use serde_json::Value;

use crate::StoreResult;
use crate::StoredRecord;
use crate::records::{
    AgentId, AgentRecord, AnnotationRecord, CheckpointRecord, ComplaintRecord, JsonRecord,
    MonitorRecord, PlanRecord, RequestRecord, RuleRecord, Table, TaskId, TaskRecord,
    WorkItemRecord,
};
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};

const BOOTSTRAP_TABLES: &[TableRequirement] = &[
    TableRequirement {
        name: "runtime_annotations",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_checkpoints",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_plans",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_rules",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_work",
        columns: &["id", "record"],
    },
];
const CONDITIONAL_TABLES: &[ConditionalTableRequirement] = &[
    ConditionalTableRequirement {
        table_exists: "runtime_agents",
        tables: &[TableRequirement {
            name: "runtime_agents",
            columns: &["id", "record"],
        }],
    },
    ConditionalTableRequirement {
        table_exists: "runtime_complaints",
        tables: &[TableRequirement {
            name: "runtime_complaints",
            columns: &["id", "record"],
        }],
    },
    ConditionalTableRequirement {
        table_exists: "runtime_requests",
        tables: &[TableRequirement {
            name: "runtime_requests",
            columns: &["id", "record"],
        }],
    },
    ConditionalTableRequirement {
        table_exists: "runtime_tasks",
        tables: &[TableRequirement {
            name: "runtime_tasks",
            columns: &["id", "record"],
        }],
    },
];

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: CONDITIONAL_TABLES,
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: BOOTSTRAP_TABLES,
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceTaskRow {
    pub id: String,
    record: String,
}

impl WorkspaceTaskRow {
    /// Decode a served row without another lookup; the lookahead row stays unchecked.
    pub fn decode(self, connection: &Connection) -> StoreResult<Value> {
        Ok(Table::<TaskRecord>::new(connection)
            .decode_stored(&self.id, &self.record)?
            .value()
            .clone())
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WorkspaceViewsRepository;

impl WorkspaceViewsRepository {
    pub fn records(connection: &Connection, table: &str) -> StoreResult<Vec<StoredRecord>> {
        if !workspace_table(table) || !table_exists(connection, table)? {
            return Ok(Vec::new());
        }
        read_records(connection, table)
    }

    pub fn records_for_agents(
        connection: &Connection,
        table: &str,
        field: &str,
        agent_ids: &[String],
    ) -> StoreResult<Vec<StoredRecord>> {
        if !workspace_table(table) || !matches!(field, "agent" | "id" | "leadId") {
            return Ok(Vec::new());
        }
        if agent_ids.is_empty() || !table_exists(connection, table)? {
            return Ok(Vec::new());
        }
        let field = match field {
            "id" => "id",
            "leadId" => "json_extract(record,'$.leadId')",
            _ => "json_extract(record,'$.agent')",
        };
        let placeholders = vec!["?"; agent_ids.len()].join(",");
        let sql = format!(
            "SELECT id,record FROM {table} WHERE {field} IN ({placeholders}) ORDER BY rowid"
        );
        let mut statement = connection.prepare_cached(&sql)?;
        let rows = statement.query_map(params_from_iter(agent_ids.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (id, raw) = row?;
            decode_workspace_record(connection, table, &id, &raw)
        })
        .collect()
    }

    pub fn agents(connection: &Connection) -> StoreResult<Vec<StoredRecord>> {
        let fields = crate::agent_volatile::columns("v.");
        let mut statement = connection.prepare_cached(&format!(
            "SELECT {fields},a.id,a.record FROM runtime_agents a \
             LEFT JOIN runtime_agent_volatile v ON v.id=a.id ORDER BY a.rowid"
        ))?;
        let mut rows = statement.query([])?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(crate::agent_volatile::AGENT_VOLATILE_FIELDS.len())?;
            let raw: String = row.get(crate::agent_volatile::AGENT_VOLATILE_FIELDS.len() + 1)?;
            let mut value = crate::records::parse_agent_value(&id, &raw)?;
            if let Some(fields) = value.as_object_mut() {
                crate::agent_volatile::merge_row(row, &id, fields)?;
            }
            let value = crate::records::validate_agent_value(&id, value)?;
            records.push(StoredRecord {
                id,
                record: Some(serde_json::to_string(&value)?),
            });
        }
        Ok(records)
    }

    pub fn snapshot_tasks(
        connection: &Connection,
        root: Option<&str>,
    ) -> StoreResult<Vec<StoredRecord>> {
        let root_filter = root.map(|_| " AND a.root_id=?").unwrap_or("");
        let sql = format!(
            "SELECT t.id,t.record FROM runtime_tasks t JOIN runtime_agents a \
             ON json_extract(t.record,'$.agent')=a.id \
             WHERE a.deleted_at IS NULL {root_filter} \
             AND json_extract(t.record,'$.status')='running' \
             UNION ALL SELECT id,record FROM (SELECT t.id,t.record FROM runtime_tasks t \
             JOIN runtime_agents a ON json_extract(t.record,'$.agent')=a.id \
             WHERE a.deleted_at IS NULL {root_filter} \
             AND json_extract(t.record,'$.status')!='running' \
             ORDER BY json_extract(t.record,'$.created') DESC LIMIT 100)"
        );
        let mut statement = connection.prepare_cached(&sql)?;
        let args = root
            .map(|root| {
                vec![
                    SqlValue::Text(root.to_owned()),
                    SqlValue::Text(root.to_owned()),
                ]
            })
            .unwrap_or_default();
        let rows = statement.query_map(params_from_iter(args.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (id, raw) = row?;
            stored_record::<TaskRecord>(connection, id, &raw)
        })
        .collect()
    }

    pub fn snapshot_monitors(
        connection: &Connection,
        root: Option<&str>,
    ) -> StoreResult<Vec<StoredRecord>> {
        if !table_exists(connection, "runtime_monitors")? {
            return Ok(Vec::new());
        }
        let agent_scope = root
            .map(|_| " AND json_extract(record,'$.agent') IN (SELECT id FROM runtime_agents WHERE root_id=? AND deleted_at IS NULL)")
            .unwrap_or("");
        let terminal = ["completed", "failed", "cancelled", "lost"]
            .iter()
            .map(|status| format!(
                "SELECT * FROM (SELECT id,record,json_extract(record,'$.created') AS created FROM runtime_monitors WHERE json_extract(record,'$.status')='{status}' {agent_scope} ORDER BY json_extract(record,'$.created') DESC LIMIT 100)"
            ))
            .collect::<Vec<_>>()
            .join(" UNION ALL ");
        let sql = format!(
            "SELECT id,record FROM runtime_monitors WHERE json_extract(record,'$.status') IN ('running','starting','approval') {agent_scope} \
             UNION ALL SELECT id,record FROM (SELECT id,record,created FROM ({terminal}) ORDER BY created DESC LIMIT 100)"
        );
        let mut statement = connection.prepare_cached(&sql)?;
        let count = if root.is_some() { 5 } else { 0 };
        let args = std::iter::repeat_n(root.unwrap_or_default().to_owned(), count)
            .map(SqlValue::Text)
            .collect::<Vec<_>>();
        let rows = statement.query_map(params_from_iter(args.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (id, raw) = row?;
            stored_record::<MonitorRecord>(connection, id, &raw)
        })
        .collect()
    }

    pub fn task(connection: &Connection, id: &str) -> StoreResult<Option<Value>> {
        Ok(Table::<TaskRecord>::new(connection)
            .get(&TaskId::from(id))?
            .map(|record| record.value().clone()))
    }

    pub fn agent(connection: &Connection, id: &str) -> StoreResult<Option<AgentRecord>> {
        Table::<AgentRecord>::new(connection).get(&AgentId::from(id))
    }

    /// The composite cursor remains specialized because it orders by two JSON timestamps.
    pub fn task_page(
        connection: &Connection,
        agent_ids: &[String],
        cursor: Option<(f64, &str)>,
        before: Option<(f64, &str)>,
        limit: usize,
    ) -> StoreResult<Vec<WorkspaceTaskRow>> {
        if agent_ids.is_empty() {
            return Ok(Vec::new());
        }
        let marks = vec!["?"; agent_ids.len()].join(",");
        let created = "json_extract(record,'$.created')";
        let updated = format!(
            "CASE WHEN COALESCE(json_extract(record,'$.finished'),0) > \
             COALESCE({created},0) THEN json_extract(record,'$.finished') ELSE {created} END"
        );
        let (sort_value, condition, order, cursor_args) = if let Some((time, id)) = cursor {
            (
                updated.clone(),
                format!("({updated} > ? OR ({updated} = ? AND id > ?))"),
                "ASC",
                vec![
                    SqlValue::Real(time),
                    SqlValue::Real(time),
                    SqlValue::Text(id.to_owned()),
                ],
            )
        } else if let Some((created_at, id)) = before {
            (
                created.to_owned(),
                format!("({created} < ? OR ({created} = ? AND id < ?))"),
                "DESC",
                vec![
                    SqlValue::Real(created_at),
                    SqlValue::Real(created_at),
                    SqlValue::Text(id.to_owned()),
                ],
            )
        } else {
            (created.to_owned(), "1=1".to_owned(), "DESC", Vec::new())
        };
        let agents_predicate = format!("json_extract(record,'$.agent') IN ({marks})");
        let sql = format!(
            "SELECT id,record \
             FROM runtime_tasks WHERE {agents_predicate} AND {condition} \
             ORDER BY {sort_value} {order}, id {order} LIMIT ?"
        );
        let mut args = agent_ids
            .iter()
            .cloned()
            .map(SqlValue::Text)
            .collect::<Vec<_>>();
        args.extend(cursor_args);
        args.push(SqlValue::Integer(
            i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX),
        ));
        let mut statement = connection.prepare_cached(&sql)?;
        statement
            .query_map(params_from_iter(args.iter()), |row| {
                Ok(WorkspaceTaskRow {
                    id: row.get(0)?,
                    record: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

fn workspace_table(table: &str) -> bool {
    matches!(
        table,
        "runtime_agents"
            | "runtime_annotations"
            | "runtime_checkpoints"
            | "runtime_complaints"
            | "runtime_plans"
            | "runtime_requests"
            | "runtime_rules"
            | "runtime_tasks"
            | "runtime_work"
    )
}

fn read_records(connection: &Connection, table: &str) -> StoreResult<Vec<StoredRecord>> {
    let mut statement = connection.prepare_cached(&format!(
        "SELECT id,record FROM {} ORDER BY rowid",
        table.replace('"', "\"\"")
    ))?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.map(|row| {
        let (id, raw) = row?;
        decode_workspace_record(connection, table, &id, &raw)
    })
    .collect()
}

fn decode_workspace_record(
    connection: &Connection,
    table: &str,
    id: &str,
    raw: &str,
) -> StoreResult<StoredRecord> {
    match table {
        "runtime_agents" => stored_record::<AgentRecord>(connection, id.to_owned(), raw),
        "runtime_annotations" => stored_record::<AnnotationRecord>(connection, id.to_owned(), raw),
        "runtime_checkpoints" => stored_record::<CheckpointRecord>(connection, id.to_owned(), raw),
        "runtime_complaints" => stored_record::<ComplaintRecord>(connection, id.to_owned(), raw),
        "runtime_plans" => stored_record::<PlanRecord>(connection, id.to_owned(), raw),
        "runtime_requests" => stored_record::<RequestRecord>(connection, id.to_owned(), raw),
        "runtime_rules" => stored_record::<RuleRecord>(connection, id.to_owned(), raw),
        "runtime_tasks" => stored_record::<TaskRecord>(connection, id.to_owned(), raw),
        "runtime_work" => stored_record::<WorkItemRecord>(connection, id.to_owned(), raw),
        _ => Err(crate::StoreError::RecordContract(
            "workspace record table is not declared",
        )),
    }
}

fn stored_record<R: JsonRecord>(
    connection: &Connection,
    id: String,
    raw: &str,
) -> StoreResult<StoredRecord> {
    let record = Table::<R>::new(connection).decode_stored(&id, raw)?;
    Ok(StoredRecord {
        id,
        record: Some(serde_json::to_string(&record)?),
    })
}

fn table_exists(connection: &Connection, table: &str) -> StoreResult<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
}

inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "workspace_views",
        requirements: &REQUIREMENTS,
    }
}

#[cfg(test)]
mod bulk_read_tests {
    use super::*;
    use rusqlite::trace::{TraceEvent, TraceEventCodes};
    use serde_json::json;
    use std::cell::Cell;

    thread_local! {
        static STATEMENTS: Cell<usize> = const { Cell::new(0) };
    }

    fn count_statement(event: TraceEvent<'_>) {
        if matches!(event, TraceEvent::Stmt(_, _)) {
            STATEMENTS.with(|count| count.set(count.get() + 1));
        }
    }

    fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("database");
        connection.execute_batch("CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY,record TEXT NOT NULL); CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);").expect("tables");
        connection
            .execute_batch(crate::agent_volatile::agent_volatile_table_sql())
            .expect("volatile table");
        connection
    }

    #[test]
    fn task_page_and_served_records_use_one_statement() {
        let connection = database();
        for (id, created) in [("a", 1), ("b", 2), ("c", 3)] {
            connection
                .execute(
                    "INSERT INTO runtime_tasks VALUES(?1,?2)",
                    (
                        id,
                        json!({"id":id,"agent":"agent","created":created,"status":"completed"})
                            .to_string(),
                    ),
                )
                .expect("task");
        }
        connection.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(count_statement));
        STATEMENTS.set(0);
        let page =
            WorkspaceViewsRepository::task_page(&connection, &["agent".to_owned()], None, None, 2)
                .expect("page");
        assert_eq!(page.len(), 3);
        let tasks = page
            .into_iter()
            .take(2)
            .map(|row| row.decode(&connection).expect("task"))
            .collect::<Vec<_>>();
        assert_eq!(
            tasks
                .iter()
                .map(|task| task["id"].as_str().expect("id"))
                .collect::<Vec<_>>(),
            ["c", "b"]
        );
        assert_eq!(
            STATEMENTS.get(),
            1,
            "one page SELECT, no per-record SELECTs"
        );
    }

    #[test]
    fn task_page_keeps_cursor_order_and_only_decodes_served_rows() {
        let connection = database();
        for (id, created, finished) in [("a", 1, 1), ("b", 2, 5), ("c", 2, 4), ("foreign", 9, 9)] {
            let agent = if id == "foreign" { "other" } else { "agent" };
            let payload_id = if id == "a" { "wrong" } else { id };
            connection.execute("INSERT INTO runtime_tasks VALUES(?1,?2)", (id, json!({"id":payload_id,"agent":agent,"created":created,"finished":finished}).to_string())).expect("task");
        }
        let agents = ["agent".to_owned()];
        let page =
            WorkspaceViewsRepository::task_page(&connection, &agents, None, None, 2).expect("page");
        assert_eq!(
            page.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["c", "b", "a"]
        );
        for row in page.into_iter().take(2) {
            row.decode(&connection).expect("only served rows are valid");
        }
        let older =
            WorkspaceViewsRepository::task_page(&connection, &agents, None, Some((2.0, "c")), 2)
                .expect("older");
        assert_eq!(
            older.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["b", "a"]
        );
        let current =
            WorkspaceViewsRepository::task_page(&connection, &agents, Some((4.0, "c")), None, 2)
                .expect("current");
        assert_eq!(
            current
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["b"]
        );
        assert!(
            older
                .into_iter()
                .last()
                .expect("bad row")
                .decode(&connection)
                .is_err()
        );
        assert!(
            WorkspaceViewsRepository::task_page(&connection, &[], None, None, 2)
                .expect("empty scope")
                .is_empty()
        );
    }

    #[test]
    fn agents_with_volatile_fields_use_one_statement() {
        let connection = database();
        for id in ["a", "b", "c"] {
            connection
                .execute(
                    "INSERT INTO runtime_agents VALUES(?1,?2)",
                    (
                        id,
                        json!({"id":id,"rootId":"a","epoch":0,"status":"idle","tail":"legacy"})
                            .to_string(),
                    ),
                )
                .expect("agent");
        }
        connection.execute(r#"INSERT INTO runtime_agent_volatile(id,tail) VALUES('a','null'),('c','"current"')"#, []).expect("volatile values");
        connection.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(count_statement));
        STATEMENTS.set(0);
        let agents = WorkspaceViewsRepository::agents(&connection).expect("agents");
        let records = agents
            .into_iter()
            .map(|row| serde_json::from_str::<Value>(&row.record.expect("record")).expect("JSON"))
            .collect::<Vec<_>>();
        assert_eq!(records[0]["tail"], Value::Null);
        assert_eq!(records[1]["tail"], "legacy");
        assert_eq!(records[2]["tail"], "current");
        assert_eq!(STATEMENTS.get(), 1, "one joined SELECT for all agents");
    }
}
