use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, functions::FunctionFlags, params};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::{StoreError, StoreResult};

const SCHEMA_SNAPSHOT: &str =
    include_str!("../../../apps/server/tests/fixtures/runtime-state-schema.json");

pub(crate) fn verify(canvas_path: &Path, analytics_path: &Path) -> StoreResult<()> {
    let expected: Value = serde_json::from_str(SCHEMA_SNAPSHOT)?;
    let canvas = open_read_only(canvas_path)?;
    let analytics = open_read_only(analytics_path)?;
    let actual = json!({
        "canvas.sqlite3": database_snapshot(&canvas)?,
        "analytics.sqlite3": database_snapshot(&analytics)?,
        "pragmas": {
            "main": connection_pragmas(&canvas)?,
            "analytics": connection_pragmas(&analytics)?,
        }
    });

    for database in ["canvas.sqlite3", "analytics.sqlite3"] {
        if expected.get(database) != actual.get(database) {
            return Err(StoreError::SchemaMismatch(format!(
                "{database} sqlite_master rows, user_version, or migration markers differ"
            )));
        }
    }
    if expected.get("pragmas") != actual.get("pragmas") {
        return Err(StoreError::SchemaMismatch(
            "database journal or synchronous pragma differs".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn register_python_trigger_functions(connection: &Connection) -> StoreResult<()> {
    connection.create_scalar_function(
        "sync_entity_payload",
        4,
        FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let collection: String = context.get(0)?;
            if collection != "event" {
                return Err(rusqlite::Error::UserFunctionError(Box::new(
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "unsupported sync entity trigger collection",
                    ),
                )));
            }
            let key: String = context.get(1)?;
            let raw: String = context.get(2)?;
            let deleted: i64 = context.get(3)?;
            event_payload(&key, &raw, deleted != 0)
                .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))
        },
    )?;
    connection.create_scalar_function(
        "sync_entity_hash",
        1,
        FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let payload: String = context.get(0)?;
            Ok(sha256_hex(payload.as_bytes()))
        },
    )?;
    Ok(())
}

fn sha256_hex(value: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(value);
    let mut result = String::with_capacity(digest.len() * 2);
    for byte in digest.iter() {
        result.push(char::from(HEX[usize::from(byte >> 4)]));
        result.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    result
}

fn event_payload(key: &str, raw: &str, deleted: bool) -> Result<String, serde_json::Error> {
    let mut value = if deleted {
        Map::new()
    } else {
        serde_json::from_str::<Value>(raw)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default()
    };
    if !deleted {
        value.retain(|name, _| {
            matches!(
                name.as_str(),
                "id" | "agent" | "kind" | "status" | "created" | "error"
            )
        });
    }
    serde_json::to_string(&json!({
        "collection": "event",
        "id": key,
        "value": value,
    }))
}

fn open_read_only(path: &Path) -> StoreResult<Connection> {
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
}

fn database_snapshot(connection: &Connection) -> StoreResult<Value> {
    let mut objects = connection.prepare(
        "SELECT type, name, tbl_name, sql FROM sqlite_master \
         WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
    )?;
    let sqlite_master = objects
        .query_map([], |row| {
            Ok(json!({
                "type": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "table": row.get::<_, String>(2)?,
                "sql": row.get::<_, Option<String>>(3)?,
            }))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let user_version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let payload_migrations = if has_table(connection, "runtime_payload_migrations")? {
        let mut statement = connection.prepare(
            "SELECT name, cursor, complete, status FROM runtime_payload_migrations ORDER BY name",
        )?;
        statement
            .query_map([], |row| {
                Ok(json!({
                    "name": row.get::<_, String>(0)?,
                    "cursor": row.get::<_, i64>(1)?,
                    "complete": row.get::<_, i64>(2)?,
                    "status": row.get::<_, String>(3)?,
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    let file_migration = if has_table(connection, "analytics_meta")? {
        connection
            .query_row(
                "SELECT value FROM analytics_meta WHERE key='fileMigrationV1'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|value| serde_json::from_str::<Value>(&value))
            .transpose()?
            .map_or_else(Vec::new, |value| vec![value])
    } else {
        Vec::new()
    };
    Ok(json!({
        "pragma_user_version": user_version,
        "payload_migrations": payload_migrations,
        "file_migration_v1": file_migration,
        "sqlite_master": sqlite_master,
    }))
}

fn has_table(connection: &Connection, name: &str) -> StoreResult<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        params![name],
        |row| row.get(0),
    )?)
}

fn connection_pragmas(connection: &Connection) -> StoreResult<Value> {
    let journal_mode: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    let synchronous: i64 = connection.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
    Ok(json!({
        "journal_mode": journal_mode,
        "synchronous": synchronous,
    }))
}

#[cfg(test)]
mod tests {
    use super::SCHEMA_SNAPSHOT;
    use serde_json::Value;

    #[test]
    fn committed_snapshot_has_both_database_catalogs_and_expected_markers() {
        let snapshot: Value =
            serde_json::from_str(SCHEMA_SNAPSHOT).expect("schema snapshot parses");
        assert_eq!(snapshot["canvas.sqlite3"]["pragma_user_version"], 0);
        assert_eq!(
            snapshot["canvas.sqlite3"]["payload_migrations"],
            serde_json::json!([])
        );
        assert_eq!(
            snapshot["analytics.sqlite3"]["file_migration_v1"],
            serde_json::json!([])
        );
        assert_eq!(snapshot["pragmas"]["main"]["journal_mode"], "wal");
        assert_eq!(snapshot["pragmas"]["analytics"]["synchronous"], 2);
    }
}
