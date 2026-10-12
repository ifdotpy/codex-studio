use rusqlite::{Connection, OptionalExtension, params_from_iter, types::Value as SqlValue};
use serde_json::Value;

use crate::StoreResult;
use crate::schema::{SchemaRequirements, TableRequirement};

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[
        TableRequirement {
            name: "runtime_tasks",
            columns: &["id", "record"],
        },
        TableRequirement {
            name: "runtime_agents",
            columns: &["id", "record"],
        },
    ],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceTaskRow {
    pub id: String,
    pub sort_value: f64,
    pub record: Value,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WorkspaceViewsRepository;

impl WorkspaceViewsRepository {
    pub fn task(connection: &Connection, id: &str) -> StoreResult<Option<Value>> {
        let record = connection
            .query_row(
                "SELECT record FROM runtime_tasks WHERE id=?1",
                [id],
                |row| {
                    let source: String = row.get(0)?;
                    serde_json::from_str(&source).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })
                },
            )
            .optional()?;
        Ok(record)
    }

    pub fn agent(connection: &Connection, id: &str) -> StoreResult<Option<Value>> {
        let record = connection
            .query_row(
                "SELECT record FROM runtime_agents WHERE id=?1",
                [id],
                |row| {
                    let source: String = row.get(0)?;
                    serde_json::from_str(&source).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })
                },
            )
            .optional()?;
        Ok(record)
    }

    pub fn live_agent_ids(
        connection: &Connection,
        root_id: Option<&str>,
    ) -> StoreResult<Vec<String>> {
        let sql = match root_id {
            Some(_) => {
                "SELECT id FROM runtime_agents WHERE json_extract(record,'$.rootId')=?1 \
                 AND json_extract(record,'$.deletedAt') IS NULL"
            }
            None => {
                "SELECT id FROM runtime_agents WHERE json_extract(record,'$.deletedAt') IS NULL"
            }
        };
        let mut statement = connection.prepare(sql)?;
        let ids = if let Some(root_id) = root_id {
            statement
                .query_map([root_id], |row| row.get(0))?
                .collect::<Result<Vec<String>, _>>()?
        } else {
            statement
                .query_map([], |row| row.get(0))?
                .collect::<Result<Vec<String>, _>>()?
        };
        Ok(ids)
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
        let (sort_value, condition, order, cursor_args) = match (cursor, before) {
            (Some((time, id)), None) => (
                updated.clone(),
                format!("({updated} > ? OR ({updated} = ? AND id > ?))"),
                "ASC",
                vec![
                    SqlValue::Real(time),
                    SqlValue::Real(time),
                    SqlValue::Text(id.to_owned()),
                ],
            ),
            (None, Some((created_at, id))) => (
                created.to_owned(),
                format!("({created} < ? OR ({created} = ? AND id < ?))"),
                "DESC",
                vec![
                    SqlValue::Real(created_at),
                    SqlValue::Real(created_at),
                    SqlValue::Text(id.to_owned()),
                ],
            ),
            (None, None) => (created.to_owned(), "1=1".to_owned(), "DESC", Vec::new()),
            (Some(_), Some(_)) => return Ok(Vec::new()),
        };
        let agents_predicate = format!("json_extract(record,'$.agent') IN ({marks})");
        let sql = format!(
            "SELECT id, {sort_value}, record \
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
        let rows = statement
            .query_map(params_from_iter(args.iter()), |row| {
                let id = row.get(0)?;
                let sort_value = row.get(1)?;
                let record: String = row.get(2)?;
                let record = serde_json::from_str(&record).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
                Ok(WorkspaceTaskRow {
                    id,
                    sort_value,
                    record,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}
