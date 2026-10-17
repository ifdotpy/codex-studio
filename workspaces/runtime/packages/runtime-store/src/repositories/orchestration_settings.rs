//! Server-wide orchestration settings that the scheduler reads on every use.

use crate::schema::{SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::time::Duration;
use studio_runtime_schema::shared_values::{
    CHILD_RESULT_COALESCE_DEFAULT_SECONDS, CHILD_RESULT_COALESCE_MAX_SECONDS,
    CHILD_RESULT_COALESCE_MIN_SECONDS,
};

const TABLE: &str = "runtime_orchestration_settings";
const ROW: &str = "global";
/// The record field that stores the hold in whole seconds.
const CHILD_RESULT_COALESCE_FIELD: &str = "childResultCoalesceSeconds";

/// How long a lead's child result waits for sibling subagents that are still
/// queued or running, until the setting is changed.
pub const CHILD_RESULT_COALESCE_DEFAULT: Duration =
    Duration::from_secs(CHILD_RESULT_COALESCE_DEFAULT_SECONDS);
/// The shortest accepted hold; zero delivers every child result at once.
pub const CHILD_RESULT_COALESCE_MIN: Duration =
    Duration::from_secs(CHILD_RESULT_COALESCE_MIN_SECONDS);
/// The longest accepted hold.
pub const CHILD_RESULT_COALESCE_MAX: Duration =
    Duration::from_secs(CHILD_RESULT_COALESCE_MAX_SECONDS);

const TABLES: &[TableRequirement] = &[TableRequirement {
    name: TABLE,
    columns: &["id", "record"],
}];
pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: TABLES,
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Copy, Debug, Default)]
pub struct OrchestrationSettingsRepository;

impl OrchestrationSettingsRepository {
    /// Whether `seconds` is an accepted child-result hold.
    pub fn valid_child_result_coalesce(seconds: u64) -> bool {
        (CHILD_RESULT_COALESCE_MIN_SECONDS..=CHILD_RESULT_COALESCE_MAX_SECONDS).contains(&seconds)
    }

    /// The effective child-result hold. A state without the table or the row
    /// uses [`CHILD_RESULT_COALESCE_DEFAULT`]; a stored value out of range is refused.
    pub fn child_result_coalesce(connection: &Connection) -> StoreResult<Duration> {
        if !crate::schema::table_exists(connection, TABLE)? {
            return Ok(CHILD_RESULT_COALESCE_DEFAULT);
        }
        let raw: Option<String> = connection
            .query_row(
                &format!("SELECT record FROM {TABLE} WHERE id=?1"),
                [ROW],
                |row| row.get(0),
            )
            .optional()?;
        let Some(raw) = raw else {
            return Ok(CHILD_RESULT_COALESCE_DEFAULT);
        };
        serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|record| record.get(CHILD_RESULT_COALESCE_FIELD)?.as_u64())
            .filter(|seconds| Self::valid_child_result_coalesce(*seconds))
            .map(Duration::from_secs)
            .ok_or_else(|| {
                StoreError::SchemaMismatch(format!(
                    "invalid stored object {TABLE}/{ROW}: {CHILD_RESULT_COALESCE_FIELD}"
                ))
            })
    }

    /// Store a validated child-result hold in whole seconds.
    pub fn set_child_result_coalesce(connection: &Connection, seconds: u64) -> StoreResult<()> {
        if !Self::valid_child_result_coalesce(seconds) {
            return Err(StoreError::SchemaMismatch(format!(
                "{CHILD_RESULT_COALESCE_FIELD} must be between \
                 {CHILD_RESULT_COALESCE_MIN_SECONDS} and {CHILD_RESULT_COALESCE_MAX_SECONDS} seconds"
            )));
        }
        connection.execute(
            &format!(
                "INSERT INTO {TABLE}(id,record) VALUES(?1,?2) \
                 ON CONFLICT(id) DO UPDATE SET record=excluded.record"
            ),
            params![
                ROW,
                json!({ CHILD_RESULT_COALESCE_FIELD: seconds }).to_string()
            ],
        )?;
        Ok(())
    }
}

inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "orchestration_settings",
        requirements: &REQUIREMENTS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_orchestration_settings(id TEXT PRIMARY KEY, record TEXT NOT NULL)",
            )
            .expect("table creates");
        connection
    }

    #[test]
    fn missing_table_or_row_uses_the_default() {
        let empty = Connection::open_in_memory().expect("memory database opens");
        assert_eq!(
            OrchestrationSettingsRepository::child_result_coalesce(&empty).expect("reads"),
            CHILD_RESULT_COALESCE_DEFAULT
        );
        assert_eq!(
            OrchestrationSettingsRepository::child_result_coalesce(&database()).expect("reads"),
            CHILD_RESULT_COALESCE_DEFAULT
        );
    }

    #[test]
    fn stored_value_persists_and_bounds_are_enforced() {
        let connection = database();
        for duration in [CHILD_RESULT_COALESCE_MIN, CHILD_RESULT_COALESCE_MAX] {
            OrchestrationSettingsRepository::set_child_result_coalesce(
                &connection,
                duration.as_secs(),
            )
            .expect("bound is accepted");
            assert_eq!(
                OrchestrationSettingsRepository::child_result_coalesce(&connection).expect("reads"),
                duration
            );
        }
        let above = CHILD_RESULT_COALESCE_MAX.as_secs() + 1;
        assert!(
            OrchestrationSettingsRepository::set_child_result_coalesce(&connection, above).is_err()
        );
        assert_eq!(
            OrchestrationSettingsRepository::child_result_coalesce(&connection).expect("reads"),
            CHILD_RESULT_COALESCE_MAX,
            "a refused write keeps the stored value"
        );
    }
}
