use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, functions::FunctionFlags, params};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::{StoreError, StoreResult};

#[cfg(test)]
const SCHEMA_SNAPSHOT: &str =
    include_str!("../../../apps/server/tests/fixtures/runtime-state-schema.json");

pub(crate) fn verify(canvas_path: &Path, analytics_path: &Path) -> StoreResult<()> {
    verify_core(canvas_path, analytics_path)?;
    verify_bootstrap(&open_read_only(canvas_path)?)
}

pub(crate) fn verify_core(canvas_path: &Path, analytics_path: &Path) -> StoreResult<()> {
    let canvas = open_read_only(canvas_path)?;
    let analytics = open_read_only(analytics_path)?;
    for registration in crate::repositories::schema_registrations() {
        let requirements = registration.requirements;
        for table in requirements.tables {
            if is_bootstrap_table(requirements, table) {
                continue;
            }
            require_columns(&canvas, "canvas.sqlite3", table.name, table.columns)?;
        }
        for conditional in requirements.conditional_tables {
            if table_exists(&canvas, conditional.table_exists)? {
                for table in conditional.tables {
                    if is_bootstrap_table(requirements, table) {
                        continue;
                    }
                    require_columns(&canvas, "canvas.sqlite3", table.name, table.columns)?;
                }
            }
        }
        for trigger in requirements.triggers {
            require_trigger(&canvas, "canvas.sqlite3", trigger.name, trigger.table)?;
        }
        for function in requirements.connection_functions {
            if !has_registered_connection_function(function) {
                return Err(object_mismatch(
                    "canvas.sqlite3",
                    function,
                    "repository requires an unregistered connection-local SQL function",
                ));
            }
        }
    }
    verify_payload_migration(&canvas)?;
    verify_analytics_migration(&canvas, &analytics)?;

    for (database, connection) in [
        ("canvas.sqlite3", &canvas),
        ("analytics.sqlite3", &analytics),
    ] {
        let (journal, synchronous) = connection_pragmas(connection)?;
        if journal != "wal" {
            return Err(object_mismatch(
                database,
                "PRAGMA journal_mode",
                "expected WAL",
            ));
        }
        if synchronous != 2 {
            return Err(object_mismatch(
                database,
                "PRAGMA synchronous",
                "expected FULL (2)",
            ));
        }
    }
    Ok(())
}

pub(crate) fn verify_bootstrap(canvas: &Connection) -> StoreResult<()> {
    let registrations = crate::repositories::schema_registrations();
    for registration in &registrations {
        let requirements = registration.requirements;
        for table in requirements.bootstrap_tables {
            require_columns(canvas, "canvas.sqlite3", table.name, table.columns)?;
        }
        for trigger in requirements.bootstrap_triggers {
            require_trigger(canvas, "canvas.sqlite3", trigger.name, trigger.table)?;
        }
        for function in requirements.bootstrap_connection_functions {
            if !has_registered_connection_function(function) {
                return Err(object_mismatch(
                    "canvas.sqlite3",
                    function,
                    "repository requires an unregistered connection-local SQL function",
                ));
            }
        }
    }
    for registration in &registrations {
        let requirements = registration.requirements;
        for conditional in requirements.conditional_triggers {
            if table_exists(canvas, conditional.table_exists)? {
                for trigger in conditional.triggers {
                    require_trigger(canvas, "canvas.sqlite3", trigger.name, trigger.table)?;
                }
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) struct TableRequirement {
    pub name: &'static str,
    pub columns: &'static [&'static str],
}

#[derive(Clone, Copy)]
pub(crate) struct TriggerRequirement {
    pub name: &'static str,
    pub table: &'static str,
}

#[derive(Clone, Copy)]
pub(crate) struct ConditionalTriggerRequirement {
    pub table_exists: &'static str,
    pub triggers: &'static [TriggerRequirement],
}

#[derive(Clone, Copy)]
pub(crate) struct ConditionalTableRequirement {
    pub table_exists: &'static str,
    pub tables: &'static [TableRequirement],
}

pub(crate) struct SchemaRequirements {
    pub tables: &'static [TableRequirement],
    pub conditional_tables: &'static [ConditionalTableRequirement],
    pub triggers: &'static [TriggerRequirement],
    pub conditional_triggers: &'static [ConditionalTriggerRequirement],
    pub connection_functions: &'static [&'static str],
    pub bootstrap_tables: &'static [TableRequirement],
    pub bootstrap_triggers: &'static [TriggerRequirement],
    pub bootstrap_connection_functions: &'static [&'static str],
}

struct ConnectionFunctionRegistration(&'static str);

inventory::collect!(ConnectionFunctionRegistration);

inventory::submit! {
    ConnectionFunctionRegistration("sync_entity_hash")
}

inventory::submit! {
    ConnectionFunctionRegistration("sync_entity_payload")
}

#[cfg(test)]
fn registered_connection_functions() -> Vec<&'static str> {
    let mut functions = inventory::iter::<ConnectionFunctionRegistration>
        .into_iter()
        .map(|registration| registration.0)
        .collect::<Vec<_>>();
    functions.sort_unstable();
    functions
}

fn has_registered_connection_function(name: &str) -> bool {
    inventory::iter::<ConnectionFunctionRegistration>
        .into_iter()
        .any(|registration| registration.0 == name)
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

pub fn sha256_hex(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    data_encoding::HEXLOWER.encode(&digest)
}

pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(values)) => !values.is_empty(),
        Some(Value::Object(values)) => !values.is_empty(),
        Some(Value::Bool(true)) => true,
    }
}

/// Python-compatible sorted-key JSON used for persisted signatures.
pub fn python_json(value: &Value) -> String {
    python_json_separated(value, ", ", ": ", true)
}

/// Compact Python-compatible JSON for content-addressed signatures.
pub fn python_json_compact(value: &Value) -> String {
    python_json_separated(value, ",", ":", false)
}

/// Sorted ASCII JSON with explicitly selected separators for provider catalog digests.
pub fn python_json_with_separators(value: &Value, item: &str, key: &str) -> String {
    python_json_separated(value, item, key, true)
}

fn python_json_separated(
    value: &Value,
    item_separator: &str,
    key_separator: &str,
    ensure_ascii: bool,
) -> String {
    match value {
        Value::Null | Value::Bool(_) => value.to_string(),
        Value::Number(number) if number.is_f64() => number
            .as_f64()
            .map(python_float_json)
            .unwrap_or_else(|| number.to_string()),
        Value::Number(number) => number.to_string(),
        Value::String(text) => python_quote(text, ensure_ascii),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(|item| python_json_separated(
                    item,
                    item_separator,
                    key_separator,
                    ensure_ascii
                ))
                .collect::<Vec<_>>()
                .join(item_separator)
        ),
        Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|key| format!(
                        "{}{key_separator}{}",
                        python_quote(key, ensure_ascii),
                        python_json_separated(
                            &values[key],
                            item_separator,
                            key_separator,
                            ensure_ascii,
                        )
                    ))
                    .collect::<Vec<_>>()
                    .join(item_separator)
            )
        }
    }
}

fn python_quote(value: &str, ensure_ascii: bool) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{8}' => output.push_str("\\b"),
            '\u{c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            c if c < ' ' || (ensure_ascii && c as u32 == 0x7f) => {
                output.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if ensure_ascii && c as u32 > 0x7f => {
                let n = c as u32;
                if n <= 0xffff {
                    output.push_str(&format!("\\u{n:04x}"));
                } else {
                    let n = n - 0x10000;
                    output.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xd800 + (n >> 10),
                        0xdc00 + (n & 0x3ff)
                    ));
                }
            }
            c => output.push(c),
        }
    }
    output.push('"');
    output
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
    let mut fields = value.into_iter().collect::<Vec<_>>();
    fields.sort_by(|left, right| left.0.cmp(&right.0));
    for (index, (name, field)) in fields.iter().enumerate() {
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

/// Serialize a finite float using the spelling emitted by Python's `json.dumps`.
pub fn python_float_json(value: f64) -> String {
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

fn require_columns(
    connection: &Connection,
    database: &str,
    object: &str,
    required: &[&str],
) -> StoreResult<()> {
    if !table_exists(connection, object)? {
        return Err(object_mismatch(
            database,
            object,
            "required table is missing",
        ));
    }
    let columns = table_columns(connection, object)?;
    if let Some(missing) = required.iter().find(|column| !columns.contains(**column)) {
        return Err(object_mismatch(
            database,
            object,
            &format!("required column {missing} is missing"),
        ));
    }
    Ok(())
}

fn require_trigger(
    connection: &Connection,
    database: &str,
    trigger: &str,
    table: &str,
) -> StoreResult<()> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='trigger' AND name=?1 AND tbl_name=?2)",
        params![trigger, table],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(object_mismatch(
            database,
            trigger,
            "required trigger is missing",
        ));
    }
    Ok(())
}

fn object_mismatch(database: &str, object: &str, reason: &str) -> StoreError {
    StoreError::SchemaObjectMismatch {
        database: database.to_owned(),
        object: object.to_owned(),
        reason: reason.to_owned(),
    }
}

fn is_bootstrap_table(requirements: &SchemaRequirements, table: &TableRequirement) -> bool {
    requirements
        .bootstrap_tables
        .iter()
        .any(|bootstrap| bootstrap.name == table.name)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PayloadMigrationState {
    NotStarted,
    InProgress,
    Complete,
}

fn verify_payload_migration(connection: &Connection) -> StoreResult<PayloadMigrationState> {
    if !table_exists(connection, "runtime_payload_migrations")? {
        return Ok(PayloadMigrationState::NotStarted);
    }
    require_columns(
        connection,
        "canvas.sqlite3",
        "runtime_payload_migrations",
        &["name", "cursor", "complete"],
    )?;
    let columns = table_columns(connection, "runtime_payload_migrations")?;
    let has_status = columns.contains("status");
    let select = if has_status {
        "SELECT cursor,complete,status FROM runtime_payload_migrations WHERE name='payload-v1:tasks'"
    } else {
        "SELECT cursor,complete,NULL FROM runtime_payload_migrations WHERE name='payload-v1:tasks'"
    };
    let marker = connection
        .query_row(select, [], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .optional()?;
    let Some((cursor, complete, status)) = marker else {
        return Ok(PayloadMigrationState::NotStarted);
    };
    if cursor < 0 || !matches!(complete, 0 | 1) {
        return Err(object_mismatch(
            "canvas.sqlite3",
            "runtime_payload_migrations.payload-v1:tasks",
            "marker has invalid cursor or complete value",
        ));
    }
    if complete == 1 {
        if has_status && status.as_deref() != Some("complete") {
            return Err(object_mismatch(
                "canvas.sqlite3",
                "runtime_payload_migrations.payload-v1:tasks",
                "complete marker has a non-complete status",
            ));
        }
        Ok(PayloadMigrationState::Complete)
    } else {
        if has_status
            && !matches!(
                status.as_deref(),
                Some("pending" | "running" | "waitingForSpace" | "error")
            )
        {
            return Err(object_mismatch(
                "canvas.sqlite3",
                "runtime_payload_migrations.payload-v1:tasks",
                "incomplete marker has an unknown status",
            ));
        }
        Ok(PayloadMigrationState::InProgress)
    }
}

fn verify_analytics_migration(canvas: &Connection, analytics: &Connection) -> StoreResult<()> {
    let legacy_analytics_table: Option<String> = canvas
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name GLOB 'analytics_*' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(table) = legacy_analytics_table {
        return Err(object_mismatch(
            "canvas.sqlite3",
            &table,
            "legacy analytics table remains in canvas during file migration",
        ));
    }
    if !table_exists(analytics, "analytics_meta")? {
        return Ok(());
    }
    require_columns(
        analytics,
        "analytics.sqlite3",
        "analytics_meta",
        &["key", "value"],
    )?;
    let marker = analytics
        .query_row(
            "SELECT value FROM analytics_meta WHERE key='fileMigrationV1'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(marker) = marker else {
        return Ok(());
    };
    let decoded: Value = serde_json::from_str(&marker).map_err(|error| {
        object_mismatch(
            "analytics.sqlite3",
            "analytics_meta.fileMigrationV1",
            &format!("marker is invalid JSON: {error}"),
        )
    })?;
    if decoded.get("phase").and_then(Value::as_str) != Some("complete") {
        return Err(object_mismatch(
            "analytics.sqlite3",
            "analytics_meta.fileMigrationV1",
            "file migration is not complete",
        ));
    }
    Ok(())
}

fn table_exists(connection: &Connection, name: &str) -> StoreResult<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        params![name],
        |row| row.get(0),
    )?)
}

fn table_columns(
    connection: &Connection,
    table: &str,
) -> StoreResult<std::collections::HashSet<String>> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    Ok(statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<std::collections::HashSet<_>, _>>()?)
}

fn connection_pragmas(connection: &Connection) -> StoreResult<(String, i64)> {
    let journal_mode: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    let synchronous: i64 = connection.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
    Ok((journal_mode, synchronous))
}

#[cfg(test)]
mod tests {
    use super::{
        PayloadMigrationState, SCHEMA_SNAPSHOT, event_payload, python_float_json, python_json,
        python_json_compact, registered_connection_functions, verify_payload_migration,
    };
    use rusqlite::Connection;
    use serde_json::Value;

    #[test]
    fn connection_functions_are_registered_in_stable_order() {
        let functions = registered_connection_functions();
        assert_eq!(functions.len(), 2);
        assert!(functions.windows(2).all(|pair| pair[0] < pair[1]));
    }

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
    fn persisted_and_federation_json_keep_distinct_unicode_formats() {
        let value = serde_json::json!({"z": 1, "a": "é"});
        assert_eq!(python_json(&value), r#"{"a": "\u00e9", "z": 1}"#,);
        assert_eq!(python_json_compact(&value), r#"{"a":"é","z":1}"#,);
        assert_eq!(
            python_json_compact(&serde_json::json!({"x": 1e-6})),
            r#"{"x":1e-06}"#
        );
    }

    #[test]
    fn absent_in_progress_and_complete_payload_markers_are_distinct() {
        let db = Connection::open_in_memory().expect("in-memory SQLite opens");
        assert_eq!(
            verify_payload_migration(&db).expect("absent marker is accepted"),
            PayloadMigrationState::NotStarted
        );
        db.execute_batch(
            "CREATE TABLE runtime_payload_migrations(name TEXT PRIMARY KEY,cursor INTEGER NOT NULL,complete INTEGER NOT NULL,status TEXT);
             INSERT INTO runtime_payload_migrations VALUES('payload-v1:tasks',7,0,'waitingForSpace');",
        )
        .expect("in-progress marker inserts");
        assert_eq!(
            verify_payload_migration(&db).expect("in-progress marker is accepted"),
            PayloadMigrationState::InProgress
        );
        db.execute(
            "UPDATE runtime_payload_migrations SET complete=1,status='complete' WHERE name='payload-v1:tasks'",
            [],
        )
        .expect("complete marker updates");
        assert_eq!(
            verify_payload_migration(&db).expect("complete marker is accepted"),
            PayloadMigrationState::Complete
        );
        db.execute(
            "UPDATE runtime_payload_migrations SET complete=0,status='unknown' WHERE name='payload-v1:tasks'",
            [],
        )
        .expect("malformed marker updates");
        let error = verify_payload_migration(&db).expect_err("unknown marker state is refused");
        assert!(matches!(
            error,
            crate::StoreError::SchemaObjectMismatch { ref object, .. }
                if object == "runtime_payload_migrations.payload-v1:tasks"
        ));
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
