use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, functions::FunctionFlags, params};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::{StoreError, StoreResult};

/// Current receipt-owner query columns. The JSON record owns domain state;
/// only quarantine remains independent so malformed agent JSON can be held.
pub(crate) fn receipt_owner_columns() -> Vec<(&'static str, &'static str, String)> {
    use studio_runtime_schema::persistence::*;
    let generated = |_name: &str, path: &str, check: String| {
        format!(
            "TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.{path}' END) VIRTUAL {check}"
        )
    };
    vec![
        ("runtime_agents", "hold_status", format!("TEXT NOT NULL DEFAULT '{}' {}", HoldStatus::Clear.as_str(), enum_check::<HoldStatus>("hold_status"))),
        ("runtime_agents", "hold_incident", "TEXT".into()),
        ("runtime_agents", "title_status", generated("title_status", "titleJob.status", enum_check::<TitleStatus>("title_status"))),
        ("runtime_agents", "vm_launch_status", generated("vm_launch_status", "vmLaunch.status", enum_check::<WorkEffectStatus>("vm_launch_status"))),
        ("runtime_agents", "idle_episode_outcome", generated("idle_episode_outcome", "idleCompactionEpisode.outcome", enum_check::<IdleEpisodeOutcome>("idle_episode_outcome"))),
        ("runtime_agents", "provider_stream_generation", "INTEGER GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.providerStream.generation' END) VIRTUAL".into()),
        ("runtime_agents", "provider_stream_ack", "INTEGER GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.providerStream.ack' END) VIRTUAL".into()),
        ("runtime_work", "status", generated("status", "status", enum_check::<WorkStatus>("status"))),
        ("runtime_work", "archive_status", generated("archive_status", "archiveIntent.status", enum_check::<WorkEffectStatus>("archive_status"))),
        ("runtime_work", "archive_next_at", "REAL GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN COALESCE(record->>'$.archiveIntent.nextAttemptAt',0) END) VIRTUAL".into()),
        ("runtime_work", "vm_submit_status", generated("vm_submit_status", "vmSubmit.status", enum_check::<WorkEffectStatus>("vm_submit_status"))),
        ("runtime_work", "vm_accept_status", generated("vm_accept_status", "vmAccept.status", enum_check::<WorkEffectStatus>("vm_accept_status"))),
        ("runtime_tool_requests", "agent", "TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN CASE record->>'$.kind' WHEN 'managed_tool' THEN record->>'$.agent' WHEN 'provider_callback' THEN record->>'$.payload.identity.context.agent' END END) VIRTUAL".into()),
        ("runtime_tool_requests", "kind", generated("kind", "kind", enum_check::<ToolRequestKind>("kind"))),
        ("runtime_tool_requests", "status", format!("TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN CASE record->>'$.kind' WHEN '{}' THEN record->>'$.stage' WHEN '{}' THEN record->>'$.status' END END) VIRTUAL {}",ToolRequestKind::ManagedTool.as_str(),ToolRequestKind::ProviderCallback.as_str(),enum_check::<ToolRequestStatus>("status"))),
        ("runtime_tool_requests", "request_id", "TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN CASE record->>'$.kind' WHEN 'managed_tool' THEN record->>'$.admitted.id' WHEN 'provider_callback' THEN record->>'$.id' END END) VIRTUAL".into()),
        ("runtime_tool_requests", "origin_key", generated("origin_key", "originKey", String::new())),
        ("runtime_tool_requests", "updated", "INTEGER GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.updated' END) VIRTUAL".into()),
        ("runtime_execution_runs", "status", generated("status", "status", enum_check::<ExecutionStatus>("status"))),
        ("runtime_execution_attempts", "submission", generated("submission", "submission", enum_check::<AttemptSubmission>("submission"))),
        ("runtime_execution_attempts", "turn_status", generated("turn_status", "turnStatus", enum_check::<ExecutionStatus>("turn_status"))),
        ("runtime_execution_effects", "status", generated("status", "status", enum_check::<ExecutionEffectStatus>("status"))),
        ("runtime_execution_nodes", "status", generated("status", "status", enum_check::<ExecutionNodeStatus>("status"))),
        ("runtime_projects", "removal_reason", generated("removal_reason", "removal.reason", enum_check::<ProjectRemovalReason>("removal_reason"))),
    ]
}

pub(crate) fn prepare_receipt_holds(connection: &Connection) -> StoreResult<()> {
    for (table, name, definition) in receipt_owner_columns()
        .into_iter()
        .filter(|(_, name, _)| matches!(*name, "hold_status" | "hold_incident"))
    {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_xinfo(?1) WHERE name=?2)",
            params![table, name],
            |row| row.get(0),
        )?;
        if !exists {
            connection.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN {name} {definition}"
            ))?;
        }
    }
    Ok(())
}

pub(crate) fn prepare_callback_origin(connection: &Connection) -> rusqlite::Result<()> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='runtime_tool_requests')",
        [], |row| row.get(0),
    )?;
    if !exists {
        return Ok(());
    }
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_xinfo('runtime_tool_requests') WHERE name='origin_key')",
        [], |row| row.get(0),
    )?;
    if !exists {
        let (_, _, definition) = receipt_owner_columns()
            .into_iter()
            .find(|(table, name, _)| *table == "runtime_tool_requests" && *name == "origin_key")
            .expect("callback origin schema");
        connection.execute_batch(&format!(
            "ALTER TABLE runtime_tool_requests ADD COLUMN origin_key {definition}"
        ))?;
    }
    connection.execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS runtime_tool_request_origin ON runtime_tool_requests(origin_key) WHERE origin_key IS NOT NULL")
}

pub(crate) fn prepare_receipt_owners(connection: &Connection) -> StoreResult<()> {
    for (table, name, definition) in receipt_owner_columns() {
        if !table_exists(connection, table)? {
            continue;
        }
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_xinfo(?1) WHERE name=?2)",
            params![table, name],
            |row| row.get(0),
        )?;
        if !exists {
            connection.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN {name} {definition}"
            ))?;
        }
    }
    prepare_callback_origin(connection)?;
    let old_archive_index: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name='runtime_work_archive_due'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if old_archive_index.is_some_and(|sql| !sql.contains("archive_next_at")) {
        connection.execute_batch("DROP INDEX runtime_work_archive_due")?;
    }
    connection.execute_batch(
        "CREATE INDEX IF NOT EXISTS runtime_agent_holds ON runtime_agents(id) WHERE hold_status!='clear';
         CREATE INDEX IF NOT EXISTS runtime_work_archive_due ON runtime_work(archive_next_at,id) WHERE archive_status IN ('pending','retryable','unknown');
         CREATE INDEX IF NOT EXISTS runtime_tool_request_current_actor ON runtime_tool_requests(agent,updated DESC,id);
         CREATE INDEX IF NOT EXISTS runtime_tool_request_current_status ON runtime_tool_requests(status,agent,id);
         CREATE UNIQUE INDEX IF NOT EXISTS runtime_execution_terminal_source ON runtime_execution_attempts(json_extract(record,'$.terminalRepair.source.accountKey'),json_extract(record,'$.terminalRepair.source.threadId'),json_extract(record,'$.terminalRepair.proof.turnId')) WHERE json_type(record,'$.terminalRepair')='object';"
    )?;
    crate::StartupRecoveryRepository::prepare_indexes(connection)?;
    Ok(())
}

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

    // `synchronous` is connection-local, so it is checked on the serving
    // connections by [`verify_serving_pragmas`], not on these verifiers.
    for (database, connection) in [
        ("canvas.sqlite3", &canvas),
        ("analytics.sqlite3", &analytics),
    ] {
        let (journal, _) = connection_pragmas(connection)?;
        if journal != "wal" {
            return Err(object_mismatch(
                database,
                "PRAGMA journal_mode",
                "expected WAL",
            ));
        }
    }
    Ok(())
}

/// Refuse a serving writer connection that is not WAL with `synchronous=NORMAL`
/// (`crate::sqlite_durability`).
pub(crate) fn verify_serving_pragmas(connection: &Connection, database: &str) -> StoreResult<()> {
    let (journal, synchronous) = connection_pragmas(connection)?;
    if journal != "wal" {
        return Err(object_mismatch(
            database,
            "PRAGMA journal_mode",
            "expected WAL",
        ));
    }
    if synchronous != crate::sqlite_durability::SYNCHRONOUS_NORMAL {
        return Err(object_mismatch(
            database,
            "PRAGMA synchronous",
            "expected NORMAL (1)",
        ));
    }
    Ok(())
}

pub(crate) fn ensure_analytics_limits_index(analytics: &Connection) -> StoreResult<()> {
    if table_exists(analytics, "analytics_limits")? {
        analytics.execute_batch(
            "CREATE INDEX IF NOT EXISTS analytics_limits_account_at ON analytics_limits(account, at)",
        )?;
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
inventory::submit! { ConnectionFunctionRegistration(crate::entity_publication::WORK_HISTORY_FUNCTION) }
inventory::submit! { ConnectionFunctionRegistration(crate::entity_publication::WORK_IDENTITY_FUNCTION) }

fn has_registered_connection_function(name: &str) -> bool {
    inventory::iter::<ConnectionFunctionRegistration>
        .into_iter()
        .any(|registration| registration.0 == name)
}

pub(crate) fn register_python_trigger_functions(connection: &Connection) -> StoreResult<()> {
    crate::entity_publication::register_work_change_functions(connection)?;
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

// Keep existing storage callers on the shared legacy receipt serialization owner.
pub use studio_runtime_schema::json_compat::{
    python_float_json, python_json, python_json_compact, python_json_utf8,
    python_json_with_separators,
};

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
    requirements.bootstrap_tables.iter().any(|bootstrap| {
        bootstrap.name == table.name
            && table
                .columns
                .iter()
                .all(|column| bootstrap.columns.contains(column))
    })
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
        &["name", "cursor", "complete", "status", "updated", "error"],
    )?;
    let marker = connection
        .query_row("SELECT cursor,complete,status FROM runtime_payload_migrations WHERE name='payload-v1:tasks'", [], |row| {
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
        if status.as_deref() != Some("complete") {
            return Err(object_mismatch(
                "canvas.sqlite3",
                "runtime_payload_migrations.payload-v1:tasks",
                "complete marker has a non-complete status",
            ));
        }
        Ok(PayloadMigrationState::Complete)
    } else {
        if !matches!(
            status.as_deref(),
            Some("pending" | "running" | "waitingForSpace" | "error")
        ) {
            return Err(object_mismatch(
                "canvas.sqlite3",
                "runtime_payload_migrations.payload-v1:tasks",
                "incomplete marker has an unknown status",
            ));
        }
        Ok(PayloadMigrationState::InProgress)
    }
}

// A partial startup view. Full JSON admission happens first so unknown fields,
// duplicate keys, number overflow and nesting retain their previous behavior.
#[derive(serde::Deserialize)]
struct AnalyticsFileMigrationMarker {
    phase: Option<String>,
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
    let complete = decoded.is_object()
        && serde_json::from_value::<AnalyticsFileMigrationMarker>(decoded)
            .ok()
            .and_then(|marker| marker.phase)
            .as_deref()
            == Some("complete");
    if !complete {
        return Err(object_mismatch(
            "analytics.sqlite3",
            "analytics_meta.fileMigrationV1",
            "file migration is not complete",
        ));
    }
    Ok(())
}

pub fn table_exists(connection: &Connection, name: &str) -> StoreResult<bool> {
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
    let mut statement = connection.prepare(&format!("PRAGMA table_xinfo(\"{table}\")"))?;
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
        PayloadMigrationState, event_payload, python_float_json, python_json, python_json_compact,
        verify_payload_migration,
    };
    use rusqlite::Connection;

    #[test]
    fn analytics_marker_admission_preserves_legacy_json_and_error_order() {
        let canvas = Connection::open_in_memory().expect("fixture operation");
        let analytics = Connection::open_in_memory().expect("fixture operation");
        assert!(super::verify_analytics_migration(&canvas, &analytics).is_ok());
        analytics
            .execute_batch("CREATE TABLE analytics_meta(key TEXT, value TEXT)")
            .expect("fixture operation");
        assert!(super::verify_analytics_migration(&canvas, &analytics).is_ok());
        let missing_column = Connection::open_in_memory().expect("fixture database");
        missing_column
            .execute_batch("CREATE TABLE analytics_meta(key TEXT)")
            .expect("fixture schema");
        assert!(
            super::verify_analytics_migration(&canvas, &missing_column)
                .expect_err("missing value column")
                .to_string()
                .contains("value")
        );
        let depth = format!(
            "{{\"phase\":\"complete\",\"other\":{}0{}}}",
            "[".repeat(130),
            "]".repeat(130)
        );
        for (marker, outcome) in [
            ("{\"phase\":\"complete\"}".to_owned(), "ok"),
            ("{\"phase\":false,\"phase\":\"complete\"}".to_owned(), "ok"),
            (
                "{\"phase\":\"complete\",\"phase\":null}".to_owned(),
                "file migration is not complete",
            ),
            ("null".to_owned(), "file migration is not complete"),
            ("[]".to_owned(), "file migration is not complete"),
            ("{\"phase\":3}".to_owned(), "file migration is not complete"),
            (
                "{\"phase\":\"complete\",\"other\":1e999}".to_owned(),
                "marker is invalid JSON",
            ),
            (depth, "marker is invalid JSON"),
            ("{".to_owned(), "marker is invalid JSON"),
        ] {
            analytics
                .execute("DELETE FROM analytics_meta", [])
                .expect("fixture operation");
            analytics
                .execute(
                    "INSERT INTO analytics_meta VALUES('fileMigrationV1', ?1)",
                    [&marker],
                )
                .expect("fixture operation");
            let result = super::verify_analytics_migration(&canvas, &analytics);
            if outcome == "ok" {
                assert!(result.is_ok(), "{marker}");
            } else {
                assert!(
                    result.unwrap_err().to_string().contains(outcome),
                    "{marker}"
                );
            }
        }
        canvas
            .execute_batch("CREATE TABLE analytics_legacy(id INTEGER)")
            .expect("fixture operation");
        let error = super::verify_analytics_migration(&canvas, &analytics)
            .unwrap_err()
            .to_string();
        assert!(error.contains("legacy analytics table"), "{error}");
    }

    #[test]
    fn receipt_schema_verifier_sees_generated_query_columns() {
        let db = rusqlite::Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE target(record TEXT, status TEXT GENERATED ALWAYS AS (json_extract(record,'$.status')) VIRTUAL)").expect("query column");
        super::require_columns(&db, "canvas.sqlite3", "target", &["record", "status"])
            .expect("generated columns are ordinary target schema requirements");
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
    fn python_payload_marker_layout_is_refused_without_upgrade() {
        let db = Connection::open_in_memory().expect("SQLite");
        db.execute_batch("CREATE TABLE runtime_payload_migrations(name TEXT PRIMARY KEY,cursor INTEGER NOT NULL,complete INTEGER NOT NULL);
            INSERT INTO runtime_payload_migrations VALUES('payload-v1:tasks',7,1);").expect("old layout");
        assert!(
            matches!(verify_payload_migration(&db), Err(crate::StoreError::SchemaObjectMismatch { object, .. }) if object == "runtime_payload_migrations")
        );
        let columns: i64 = db
            .query_row(
                "SELECT count(*) FROM pragma_table_info('runtime_payload_migrations')",
                [],
                |row| row.get(0),
            )
            .expect("unchanged columns");
        assert_eq!(columns, 3, "verification never upgrades old layouts");
    }

    #[test]
    fn absent_in_progress_and_complete_payload_markers_are_distinct() {
        let db = Connection::open_in_memory().expect("in-memory SQLite opens");
        assert_eq!(
            verify_payload_migration(&db).expect("absent marker is accepted"),
            PayloadMigrationState::NotStarted
        );
        db.execute_batch(
            "CREATE TABLE runtime_payload_migrations(name TEXT PRIMARY KEY,cursor INTEGER NOT NULL,complete INTEGER NOT NULL,status TEXT,updated REAL,error TEXT);
             INSERT INTO runtime_payload_migrations VALUES('payload-v1:tasks',7,0,'waitingForSpace',NULL,NULL);",
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
