use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};

const RUN_LIMIT: usize = 25;
const RELATED_LIMIT: usize = 100;

/// Read the same bounded execution chain as the Python diagnostics collector.
/// The caller owns the read connection; this function never writes.
pub fn execution_ledger(connection: &Connection) -> Result<Value, rusqlite::Error> {
    if !table_exists(connection, "runtime_execution_runs")? {
        return Ok(json!({"runs": [], "limit": RUN_LIMIT, "relatedLimit": RELATED_LIMIT}));
    }
    let mut statement = connection
        .prepare("SELECT record FROM runtime_execution_runs ORDER BY created DESC LIMIT ?1")?;
    let raw_runs = statement
        .query_map([RUN_LIMIT as i64], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut runs = Vec::with_capacity(raw_runs.len());
    for raw in raw_runs {
        let mut run: Value = serde_json::from_str(&raw).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        let run_id = run
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        for table in ["attempts", "nodes", "effects"] {
            run[table] = related_records(connection, table, &run_id)?;
        }
        let attempts = run["attempts"].as_array().cloned().unwrap_or_default();
        let effects = run["effects"].as_array().cloned().unwrap_or_default();
        run["attemptIds"] = json!(
            attempts
                .iter()
                .filter_map(|value| value.get("id").and_then(Value::as_str))
                .collect::<Vec<_>>()
        );
        let mut request_ids = BTreeSet::new();
        for attempt in &attempts {
            for field in ["actionRequestId", "nativeOperationId"] {
                if let Some(value) = attempt.get(field).and_then(Value::as_str) {
                    request_ids.insert(value.to_owned());
                }
            }
        }
        for effect in &effects {
            if let Some(value) = effect.get("requestId").and_then(Value::as_str) {
                request_ids.insert(value.to_owned());
            }
        }
        run["requestIds"] = json!(request_ids.into_iter().collect::<Vec<_>>());
        run["inputEventIds"] = json!(related_inputs(connection, &run_id)?);
        runs.push(run);
    }
    Ok(json!({"runs": runs, "limit": RUN_LIMIT, "relatedLimit": RELATED_LIMIT}))
}

/// Count durable input rows that can still be delivered: unsettled rows of agents that
/// are not deleted. Rows of deleted agents are settled by `settle_inputs` and the startup sweep.
pub fn durable_input_pending(connection: &Connection) -> Result<i64, rusqlite::Error> {
    if !table_exists(connection, "runtime_events")? {
        return Ok(0);
    }
    let deleted = if table_exists(connection, "runtime_agents")? {
        format!(
            "AND agent NOT IN (SELECT id FROM runtime_agents WHERE {})",
            studio_runtime_store::agent_management::DELETED_AGENT_SQL
        )
    } else {
        String::new()
    };
    connection.query_row(
        &format!("SELECT COUNT(*) FROM runtime_events WHERE status IN ('pending','reserved','dispatching','uncertain') {deleted}"),
        [],
        |row| row.get(0),
    )
}

/// Read durable migration cursors using the Python diagnostics field names.
pub fn migration_status(connection: &Connection) -> Result<Value, rusqlite::Error> {
    let mut status = serde_json::Map::new();
    if table_exists(connection, "runtime_search_rollout")? {
        let row = connection
            .query_row(
                "SELECT phase,cursor,updated FROM runtime_search_rollout WHERE id=1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, f64>(2)?,
                    ))
                },
            )
            .optional()?;
        if let Some((phase, cursor, updated)) = row {
            status.insert(
                "search".to_owned(),
                json!({"phase": phase, "cursor": cursor, "updated": updated,
                    "error": null, "lastBatchBytes": null}),
            );
        }
    }
    if table_exists(connection, "runtime_payload_migrations")? {
        let columns = table_columns(connection, "runtime_payload_migrations")?;
        let row = if columns.contains("status") {
            connection
                .query_row(
                    "SELECT cursor,complete,status,updated,error FROM runtime_payload_migrations WHERE name='payload-v1:tasks'",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?, row.get::<_, String>(2)?, row.get::<_, Option<f64>>(3)?, row.get::<_, Option<String>>(4)?)),
                )
                .optional()?
        } else {
            connection
                .query_row(
                    "SELECT cursor,complete FROM runtime_payload_migrations WHERE name='payload-v1:tasks'",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?, String::new(), None, None)),
                )
                .optional()?
        };
        let payload = row.map_or_else(
            || json!({"cursor": 0, "complete": false, "status": "pending", "updated": null, "error": null}),
            |(cursor, complete, saved_status, updated, error)| json!({
                "cursor": cursor,
                "complete": complete,
                "status": if saved_status.is_empty() { if complete { "complete" } else { "pending" } } else { saved_status.as_str() },
                "updated": updated,
                "error": error,
            }),
        );
        status.insert("payloads".to_owned(), json!({"tasks": payload}));
    } else {
        status.insert("payloads".to_owned(), json!({
            "tasks": {"cursor": 0, "complete": false, "status": "pending", "updated": null, "error": null}
        }));
    }
    if table_exists(connection, "sync_entity_meta")? {
        let mut values = connection.prepare(
            "SELECT key,value FROM sync_entity_meta WHERE key IN ('entity_tombstone_count','entity_tombstone_floor')",
        )?;
        let rows = values
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<std::collections::BTreeMap<_, _>, _>>()?;
        status.insert(
            "entityTombstones".to_owned(),
            json!({
                "count": rows.get("entity_tombstone_count").and_then(|value| value.parse::<i64>().ok()).unwrap_or_default(),
                "floor": rows.get("entity_tombstone_floor").and_then(|value| value.parse::<i64>().ok()).unwrap_or_default(),
                "pruning": {"status": "notStarted", "updated": 0, "deleted": 0},
            }),
        );
    }
    Ok(Value::Object(status))
}

fn table_columns(
    connection: &Connection,
    table: &str,
) -> Result<std::collections::BTreeSet<String>, rusqlite::Error> {
    // Both identifiers are closed constants passed by this module.
    let sql = format!("PRAGMA table_info({table})");
    let mut statement = connection.prepare(&sql)?;
    statement.query_map([], |row| row.get(1))?.collect()
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, rusqlite::Error> {
    connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )
}

fn related_records(
    connection: &Connection,
    table: &str,
    run_id: &str,
) -> Result<Value, rusqlite::Error> {
    let table = match table {
        "attempts" => "runtime_execution_attempts",
        "nodes" => "runtime_execution_nodes",
        "effects" => "runtime_execution_effects",
        _ => unreachable!("closed table set"),
    };
    if !table_exists(connection, table)? {
        return Ok(json!([]));
    }
    let sql = format!("SELECT record FROM {table} WHERE run=?1 LIMIT ?2");
    let mut statement = connection.prepare(&sql)?;
    let records = statement
        .query_map((run_id, RELATED_LIMIT as i64), |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    records
        .into_iter()
        .map(|raw| {
            serde_json::from_str::<Value>(&raw).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|records| json!(records))
}

fn related_inputs(connection: &Connection, run_id: &str) -> Result<Vec<String>, rusqlite::Error> {
    if !table_exists(connection, "runtime_execution_inputs")? {
        return Ok(Vec::new());
    }
    let mut statement =
        connection.prepare("SELECT event FROM runtime_execution_inputs WHERE run=?1 LIMIT ?2")?;
    statement
        .query_map((run_id, RELATED_LIMIT as i64), |row| row.get(0))?
        .collect()
}
