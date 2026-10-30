//! One schema-checked boundary for the persisted system and tool readers.
use crate::schema::SchemaRequirements;
use crate::{StoreError, StoreResult};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

#[cfg(test)]
std::thread_local! { pub(crate) static VALIDATE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

pub fn invalid(table: &str, id: &str) -> StoreError {
    StoreError::InvalidSyncObject {
        scope: table.to_owned(),
        id: id.to_owned(),
    }
}

pub fn validate(table: &str, id: &str, value: Value) -> StoreResult<Value> {
    #[cfg(test)]
    VALIDATE_CALLS.with(|count| count.set(count.get() + 1));
    let schema = studio_runtime_schema::table_schema(table)
        .ok_or_else(|| StoreError::SchemaValidation(format!("no storage schema for {table}")))?;
    let value = crate::records::validate_value(schema, value, || invalid(table, id))?;
    validate_row_key(table, id, &value)?;
    Ok(value)
}

/// Check only row identity after the caller has validated the table schema.
pub(crate) fn validate_row_key(table: &str, id: &str, value: &Value) -> StoreResult<()> {
    let key = if table == "sync_entities" {
        value.pointer("/value/id")
    } else {
        value.get("id")
    };
    if !matches!(table, "runtime_event_meta") && key.and_then(Value::as_str) != Some(id) {
        return Err(invalid(table, id));
    }
    Ok(())
}

pub fn decode(table: &str, id: &str, raw: &str) -> StoreResult<Value> {
    let value = serde_json::from_str(raw).map_err(|_| invalid(table, id))?;
    validate(table, id, value)
}

/// [`decode`] for a row read here: an agent gets its volatile fields first.
fn decode_row(connection: &Connection, table: &str, id: &str, raw: &str) -> StoreResult<Value> {
    let mut value = serde_json::from_str(raw).map_err(|_| invalid(table, id))?;
    crate::agent_volatile::merge_if_agent(connection, table, id, &mut value)?;
    validate(table, id, value)
}

pub use crate::schema::table_exists as exists;

pub fn get(connection: &Connection, table: &'static str, id: &str) -> StoreResult<Option<Value>> {
    if !exists(connection, table)? {
        return Ok(None);
    }
    let column = "record";
    let raw: Option<String> = connection
        .query_row(
            &format!("SELECT {column} FROM {table} WHERE id=?1"),
            [id],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw| decode_row(connection, table, id, &raw))
        .transpose()
}

pub fn all(connection: &Connection, table: &'static str) -> StoreResult<Vec<Value>> {
    if !exists(connection, table)? {
        return Ok(Vec::new());
    }
    let mut statement =
        connection.prepare_cached(&format!("SELECT id,record FROM {table} ORDER BY rowid"))?;
    statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .map(|row| {
            let (id, raw) = row?;
            decode_row(connection, table, &id, &raw)
        })
        .collect()
}

/// Schema-checked rows of `table` whose `agent` is one of `agents`, so a
/// broken row of another chat is neither read nor parsed.
pub fn owned_by(
    connection: &Connection,
    table: &'static str,
    agents: &[&str],
) -> StoreResult<Vec<Value>> {
    if !exists(connection, table)? {
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare_cached(&format!(
        "SELECT id,record FROM {table} WHERE json_extract(record,'$.agent')=?1 ORDER BY rowid"
    ))?;
    let mut rows = Vec::new();
    for agent in agents {
        for row in statement.query_map([agent], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (id, raw) = row?;
            rows.push(decode_row(connection, table, &id, &raw)?);
        }
    }
    Ok(rows)
}

/// Schema-checked agents that are not deleted, in rowid order. A live agent
/// has no `deletedAt`; the `runtime_agent_live_root_v2` partial index serves the
/// predicate, so tombstones are neither read nor parsed.
pub fn live_agents(connection: &Connection) -> StoreResult<Vec<Value>> {
    if !exists(connection, "runtime_agents")? {
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare_cached(
        "SELECT id,record FROM runtime_agents INDEXED BY runtime_agent_live_root_v2 \
         WHERE deleted_at IS NULL ORDER BY rowid",
    )?;
    statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .map(|row| {
            let (id, raw) = row?;
            decode_row(connection, "runtime_agents", &id, &raw)
        })
        .collect()
}

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[crate::schema::ConditionalTableRequirement {
        table_exists: "runtime_tool_requests",
        tables: &[crate::schema::TableRequirement {
            name: "runtime_tool_requests",
            columns: &["id", "record"],
        }],
    }],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};
inventory::submit! {crate::repositories::SchemaRegistration{repository:"system_reads",requirements:&REQUIREMENTS}}
