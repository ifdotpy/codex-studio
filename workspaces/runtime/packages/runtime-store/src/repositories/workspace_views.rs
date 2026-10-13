use rusqlite::{Connection, OptionalExtension, params_from_iter, types::Value as SqlValue};
use serde_json::Value;

use crate::StoreResult;
use crate::StoredRecord;
use crate::records::get_json_record;

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
            Ok(StoredRecord {
                id: row.get(0)?,
                record: Some(row.get(1)?),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
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
            Ok(StoredRecord {
                id: row.get(0)?,
                record: Some(row.get(1)?),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
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
            Ok(StoredRecord {
                id: row.get(0)?,
                record: Some(row.get(1)?),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn task(connection: &Connection, id: &str) -> StoreResult<Option<Value>> {
        get_json_record(connection, "runtime_tasks", id)
    }

    pub fn agent(connection: &Connection, id: &str) -> StoreResult<Option<Value>> {
        get_json_record(connection, "runtime_agents", id)
    }

    pub fn agent_ids(connection: &Connection) -> StoreResult<Vec<String>> {
        Ok(connection
            .prepare("SELECT id FROM runtime_agents")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn first_invalid_task_identity(connection: &Connection) -> StoreResult<Option<String>> {
        Ok(connection
            .query_row(
                "SELECT id FROM runtime_tasks WHERE CASE WHEN json_valid(record)=0 THEN 1 \
                 ELSE json_type(record,'$.id') IS NOT 'text' \
                 OR json_extract(record,'$.id') IS NOT id \
                 OR json_type(record,'$.agent') IS NOT 'text' END LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?)
    }

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
        // Bind each predicate's values in the same order as its placeholders.
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
    let mut statement =
        connection.prepare(&format!("SELECT id,record FROM {table} ORDER BY rowid"))?;
    let rows = statement.query_map([], |row| {
        Ok(StoredRecord {
            id: row.get(0)?,
            record: Some(row.get(1)?),
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn table_exists(connection: &Connection, table: &str) -> StoreResult<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
}
