//! One schema-checked boundary for the persisted system and tool readers.
use crate::schema::SchemaRequirements;
use crate::{StoreError, StoreResult};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

pub fn invalid(table: &str, id: &str) -> StoreError {
    StoreError::InvalidSyncObject {
        scope: table.to_owned(),
        id: id.to_owned(),
    }
}

pub fn validate(table: &str, id: &str, value: Value) -> StoreResult<Value> {
    let schema = studio_runtime_schema::table_schema(table)
        .ok_or_else(|| StoreError::SchemaValidation(format!("no storage schema for {table}")))?;
    let value = crate::records::validate_value(schema, value, || invalid(table, id))?;
    let key = if table == "sync_entities" {
        value.pointer("/value/id")
    } else {
        value.get("id")
    };
    if !matches!(
        table,
        "runtime_tool_results"
            | "runtime_operation_receipts"
            | "runtime_event_meta"
            | "runtime_tool_request_aliases"
    ) && key.and_then(Value::as_str) != Some(id)
    {
        return Err(invalid(table, id));
    }
    Ok(value)
}

pub fn decode(table: &str, id: &str, raw: &str) -> StoreResult<Value> {
    let value = serde_json::from_str(raw).map_err(|_| invalid(table, id))?;
    validate(table, id, value)
}

pub use crate::schema::table_exists as exists;

pub fn get(connection: &Connection, table: &'static str, id: &str) -> StoreResult<Option<Value>> {
    if !exists(connection, table)? {
        return Ok(None);
    }
    let column = if matches!(table, "runtime_tool_results" | "runtime_operation_receipts") {
        "result"
    } else {
        "record"
    };
    let raw: Option<String> = connection
        .query_row(
            &format!("SELECT {column} FROM {table} WHERE id=?1"),
            [id],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw| decode(table, id, &raw)).transpose()
}

pub fn all(connection: &Connection, table: &'static str) -> StoreResult<Vec<Value>> {
    if !exists(connection, table)? {
        return Ok(Vec::new());
    }
    let mut statement =
        connection.prepare(&format!("SELECT id,record FROM {table} ORDER BY rowid"))?;
    statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .map(|row| {
            let (id, raw) = row?;
            decode(table, &id, &raw)
        })
        .collect()
}

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[crate::schema::ConditionalTableRequirement {
        table_exists: "runtime_tool_requests",
        tables: &[
            crate::schema::TableRequirement {
                name: "runtime_tool_requests",
                columns: &["id", "record"],
            },
            crate::schema::TableRequirement {
                name: "runtime_tool_request_aliases",
                columns: &["agent", "alias", "request"],
            },
            crate::schema::TableRequirement {
                name: "runtime_tool_results",
                columns: &["id", "result"],
            },
        ],
    }],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};
inventory::submit! {crate::repositories::SchemaRegistration{repository:"system_reads",requirements:&REQUIREMENTS}}
