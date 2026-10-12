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
        value.retain(|name, field| match name.as_str() {
            "id" => field.is_string(),
            "agent" | "kind" | "status" | "error" => field.is_string() || field.is_null(),
            "created" => {
                field.is_null()
                    || field
                        .as_number()
                        .and_then(serde_json::Number::as_f64)
                        .is_some_and(f64::is_finite)
            }
            _ => false,
        });
    }

    let mut payload = format!(
        "{{\"collection\":\"event\",\"id\":{},\"value\":{{",
        serde_json::to_string(key)?
    );
    for (index, (name, field)) in value.iter().enumerate() {
        if index != 0 {
            payload.push(',');
        }
        payload.push_str(&serde_json::to_string(name)?);
        payload.push(':');
        if name == "created"
            && let Some(number) = field.as_number()
        {
            if let Some(value) = number.as_f64().filter(|value| value.is_finite()) {
                payload.push_str(&python_float_json(value));
            }
            continue;
        }
        payload.push_str(&serde_json::to_string(field)?);
    }
    payload.push_str("}}");
    Ok(payload)
}

fn python_float_json(value: f64) -> String {
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0".to_owned()
        } else {
            "0.0".to_owned()
        };
    }

    let raw = format!("{value:?}");
    let (mantissa, exponent) = match raw.split_once('e') {
        Some((mantissa, exponent)) => match exponent.parse::<i32>() {
            Ok(exponent) => (mantissa, exponent),
            Err(_) => return raw,
        },
        None => (raw.as_str(), 0),
    };
    let negative = mantissa.starts_with('-');
    let unsigned = mantissa.strip_prefix('-').unwrap_or(mantissa);
    let (integer, fractional) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let mut digits = format!("{integer}{fractional}");
    let mut decimal_position = integer.len() as i32 + exponent;
    let leading_zeroes = digits.bytes().take_while(|digit| *digit == b'0').count();
    digits.drain(..leading_zeroes);
    decimal_position -= leading_zeroes as i32;
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    let scientific_exponent = decimal_position - 1;

    let mut result = String::new();
    if negative {
        result.push('-');
    }
    if !(-4..16).contains(&scientific_exponent) {
        result.push(digits.as_bytes()[0] as char);
        if digits.len() > 1 {
            result.push('.');
            result.push_str(&digits[1..]);
        }
        result.push('e');
        result.push(if scientific_exponent < 0 { '-' } else { '+' });
        result.push_str(&format!("{:02}", scientific_exponent.unsigned_abs()));
    } else if decimal_position <= 0 {
        result.push_str("0.");
        result.extend(std::iter::repeat_n(
            '0',
            decimal_position.unsigned_abs() as usize,
        ));
        result.push_str(&digits);
    } else if decimal_position as usize >= digits.len() {
        result.push_str(&digits);
        result.extend(std::iter::repeat_n(
            '0',
            decimal_position as usize - digits.len(),
        ));
        result.push_str(".0");
    } else {
        let position = decimal_position as usize;
        result.push_str(&digits[..position]);
        result.push('.');
        result.push_str(&digits[position..]);
    }
    result
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
    use super::{SCHEMA_SNAPSHOT, event_payload, python_float_json};
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

    #[test]
    fn event_float_json_matches_python_repr_boundaries() {
        for (value, expected) in [
            (1_791_788_985.520_981_8, "1791788985.5209818"),
            (0.1, "0.1"),
            (1e21, "1e+21"),
            (5.0, "5.0"),
            (1_791_788_985.0, "1791788985.0"),
            (1.234_567_890_123_456_7, "1.2345678901234567"),
            (9_007_199_254_740_992.0, "9007199254740992.0"),
            (-0.1, "-0.1"),
            (-1e-7, "-1e-07"),
            (1e-5, "1e-05"),
            (1e-4, "0.0001"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (-0.0, "-0.0"),
        ] {
            assert_eq!(python_float_json(value), expected);
        }
    }

    #[test]
    fn integer_created_values_follow_python_float_coercion() {
        assert_eq!(
            event_payload(
                "fixture",
                r#"{"id":"fixture","created":9007199254740993}"#,
                false,
            )
            .expect("event payload encodes"),
            r#"{"collection":"event","id":"fixture","value":{"created":9007199254740992.0,"id":"fixture"}}"#,
        );
    }

    #[test]
    fn high_precision_event_projection_keeps_the_stored_binary64_value() {
        assert_eq!(
            event_payload(
                "fixture",
                r#"{"id":"fixture","created":1791788985.5209818}"#,
                false,
            )
            .expect("event payload encodes"),
            r#"{"collection":"event","id":"fixture","value":{"created":1791788985.5209818,"id":"fixture"}}"#,
        );
        assert_eq!(
            event_payload(
                "fixture",
                r#"{"id":"fixture","created":1.2345678901234567}"#,
                false,
            )
            .expect("event payload encodes"),
            r#"{"collection":"event","id":"fixture","value":{"created":1.2345678901234567,"id":"fixture"}}"#,
        );
    }
}
