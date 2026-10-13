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
        let mut statement = connection.prepare(&sql)?;
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
        read_records(connection, "runtime_agents")
    }

    pub fn snapshot_tasks(
        connection: &Connection,
        root: Option<&str>,
    ) -> StoreResult<Vec<StoredRecord>> {
        let root_filter = root
            .map(|_| " AND json_extract(a.record,'$.rootId')=?")
            .unwrap_or("");
        let sql = format!(
            "SELECT t.id,t.record FROM runtime_tasks t JOIN runtime_agents a \
             ON json_extract(t.record,'$.agent')=a.id \
             WHERE json_extract(a.record,'$.deletedAt') IS NULL {root_filter} \
             AND json_extract(t.record,'$.status')='running' \
             UNION ALL SELECT id,record FROM (SELECT t.id,t.record FROM runtime_tasks t \
             JOIN runtime_agents a ON json_extract(t.record,'$.agent')=a.id \
             WHERE json_extract(a.record,'$.deletedAt') IS NULL {root_filter} \
             AND json_extract(t.record,'$.status')!='running' \
             ORDER BY json_extract(t.record,'$.created') DESC LIMIT 100)"
        );
        let mut statement = connection.prepare(&sql)?;
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
            .map(|_| " AND json_extract(record,'$.agent') IN (SELECT id FROM runtime_agents WHERE json_extract(record,'$.rootId')=? AND json_extract(record,'$.deletedAt') IS NULL)")
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
        let mut statement = connection.prepare(&sql)?;
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

    pub fn agent_ids(connection: &Connection) -> StoreResult<Vec<String>> {
        Ok(Table::<AgentRecord>::new(connection)
            .list()?
            .into_iter()
            .map(|record| record.id().as_ref().to_owned())
            .collect())
    }

    pub fn validate_task_records(connection: &Connection) -> StoreResult<()> {
        Table::<TaskRecord>::new(connection).list()?;
        Ok(())
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
            "SELECT id \
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
        let mut statement = connection.prepare(&sql)?;
        statement
            .query_map(params_from_iter(args.iter()), |row| row.get::<_, String>(0))?
            .map(|row| row.map(|id| WorkspaceTaskRow { id }))
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
    let mut statement = connection.prepare(&format!(
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
