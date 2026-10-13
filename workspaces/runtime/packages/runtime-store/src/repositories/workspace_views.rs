use rusqlite::{Connection, OptionalExtension, params_from_iter, types::Value as SqlValue};
use serde_json::Value;

use crate::StoreResult;
use crate::records::get_json_record;

#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceTaskRow {
    pub id: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WorkspaceViewsRepository;

impl WorkspaceViewsRepository {
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
