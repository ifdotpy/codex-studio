//! Server-wide orchestration settings that the scheduler and the account
//! workers read on every use.

use crate::schema::{SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::time::Duration;
use studio_runtime_schema::shared_values::{
    CHILD_RESULT_COALESCE_DEFAULT_SECONDS, CHILD_RESULT_COALESCE_MAX_SECONDS,
    CHILD_RESULT_COALESCE_MIN_SECONDS, IDLE_SESSION_UNLOAD_DEFAULT_MINUTES,
    IDLE_SESSION_UNLOAD_MAX_MINUTES, IDLE_SESSION_UNLOAD_MIN_MINUTES,
};

const TABLE: &str = "runtime_orchestration_settings";
const ROW: &str = "global";
const SECONDS_PER_MINUTE: u64 = 60;

/// One whole-number field of the settings record, with its unset value and
/// accepted range; the shared values own the numbers and their unit.
#[derive(Clone, Copy, Debug)]
pub struct Setting {
    pub field: &'static str,
    pub default: u64,
    pub min: u64,
    pub max: u64,
}

impl Setting {
    pub fn valid(&self, value: u64) -> bool {
        (self.min..=self.max).contains(&value)
    }
}

/// Seconds a lead's child result waits for sibling subagents that are still
/// queued or running; zero delivers every child result at once.
pub const CHILD_RESULT_COALESCE_SETTING: Setting = Setting {
    field: "childResultCoalesceSeconds",
    default: CHILD_RESULT_COALESCE_DEFAULT_SECONDS,
    min: CHILD_RESULT_COALESCE_MIN_SECONDS,
    max: CHILD_RESULT_COALESCE_MAX_SECONDS,
};
/// Minutes a lead's or ordinary chat's idle thread stays loaded and an
/// account's provider process runs after its last thread unloaded; managed
/// workers unload as soon as their turn settles; zero never unloads.
pub const IDLE_SESSION_UNLOAD_SETTING: Setting = Setting {
    field: "idleSessionUnloadMinutes",
    default: IDLE_SESSION_UNLOAD_DEFAULT_MINUTES,
    min: IDLE_SESSION_UNLOAD_MIN_MINUTES,
    max: IDLE_SESSION_UNLOAD_MAX_MINUTES,
};

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
    /// The stored value of `setting`. A state without the table, the row, or
    /// the field uses the setting's default; a stored value out of range is refused.
    pub fn value(connection: &Connection, setting: Setting) -> StoreResult<u64> {
        if !crate::schema::table_exists(connection, TABLE)? {
            return Ok(setting.default);
        }
        let raw: Option<String> = connection
            .query_row(
                &format!("SELECT record FROM {TABLE} WHERE id=?1"),
                [ROW],
                |row| row.get(0),
            )
            .optional()?;
        let Some(raw) = raw else {
            return Ok(setting.default);
        };
        let invalid = || {
            StoreError::SchemaMismatch(format!(
                "invalid stored object {TABLE}/{ROW}: {}",
                setting.field
            ))
        };
        let record = serde_json::from_str::<Value>(&raw).map_err(|_| invalid())?;
        match record.get(setting.field) {
            None => Ok(setting.default),
            Some(value) => value
                .as_u64()
                .filter(|value| setting.valid(*value))
                .ok_or_else(invalid),
        }
    }

    /// Store a validated value of `setting`, keeping the record's other fields.
    pub fn set(connection: &Connection, setting: Setting, value: u64) -> StoreResult<()> {
        let value = i64::try_from(value)
            .ok()
            .filter(|_| setting.valid(value))
            .ok_or_else(|| {
                StoreError::SchemaMismatch(format!(
                    "{} must be between {} and {}",
                    setting.field, setting.min, setting.max
                ))
            })?;
        connection.execute(
            &format!(
                "INSERT INTO {TABLE}(id,record) VALUES(?1,json_object(?2,?3)) \
                 ON CONFLICT(id) DO UPDATE SET record=json_set(record,?4,?3)"
            ),
            params![ROW, setting.field, value, format!("$.{}", setting.field)],
        )?;
        Ok(())
    }

    /// Claude idle compaction is off until the server setting enables it.
    pub fn idle_compaction(
        connection: &Connection,
    ) -> StoreResult<studio_runtime_schema::idle_compaction::Settings> {
        if !crate::schema::table_exists(connection, TABLE)? {
            return Ok(Default::default());
        }
        let raw: Option<String> = connection
            .query_row(
                &format!("SELECT json_extract(record,'$.idleCompaction') FROM {TABLE} WHERE id=?1"),
                [ROW],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let settings: studio_runtime_schema::idle_compaction::Settings = raw
            .map(|raw| serde_json::from_str(&raw))
            .transpose()
            .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?
            .unwrap_or_default();
        if !settings.valid() {
            return Err(StoreError::SchemaMismatch(
                "idleCompaction.minTokens is below the minimum".into(),
            ));
        }
        Ok(settings)
    }

    pub fn set_idle_compaction(
        connection: &Connection,
        settings: &studio_runtime_schema::idle_compaction::Settings,
    ) -> StoreResult<()> {
        if !settings.valid() {
            return Err(StoreError::SchemaMismatch(
                "idleCompaction.minTokens is below the minimum".into(),
            ));
        }
        connection.execute(
            &format!("INSERT INTO {TABLE}(id,record) VALUES(?1,json_object('idleCompaction',json(?2))) ON CONFLICT(id) DO UPDATE SET record=json_set(record,'$.idleCompaction',json(?2))"),
            params![ROW, serde_json::to_string(settings).map_err(|error| StoreError::SchemaMismatch(error.to_string()))?],
        )?;
        Ok(())
    }

    /// The effective child-result hold.
    pub fn child_result_coalesce(connection: &Connection) -> StoreResult<Duration> {
        Self::value(connection, CHILD_RESULT_COALESCE_SETTING).map(Duration::from_secs)
    }

    /// Store a validated child-result hold in whole seconds.
    pub fn set_child_result_coalesce(connection: &Connection, seconds: u64) -> StoreResult<()> {
        Self::set(connection, CHILD_RESULT_COALESCE_SETTING, seconds)
    }

    /// How long an idle lead or chat thread stays loaded and an idle provider
    /// process runs; `None` never unloads.
    pub fn idle_session_unload(connection: &Connection) -> StoreResult<Option<Duration>> {
        let minutes = Self::value(connection, IDLE_SESSION_UNLOAD_SETTING)?;
        Ok((minutes > 0).then(|| Duration::from_secs(minutes * SECONDS_PER_MINUTE)))
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

    fn unload(connection: &Connection) -> Option<Duration> {
        OrchestrationSettingsRepository::idle_session_unload(connection).expect("reads")
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

    #[test]
    fn idle_unload_persists_beside_the_hold_and_zero_never_unloads() {
        let connection = database();
        let default = Duration::from_secs(IDLE_SESSION_UNLOAD_DEFAULT_MINUTES * SECONDS_PER_MINUTE);
        assert_eq!(unload(&connection), Some(default));
        OrchestrationSettingsRepository::set_child_result_coalesce(&connection, 0).expect("hold");
        assert_eq!(
            unload(&connection),
            Some(default),
            "a row written before the field existed uses the default"
        );
        let max = IDLE_SESSION_UNLOAD_SETTING.max;
        OrchestrationSettingsRepository::set(&connection, IDLE_SESSION_UNLOAD_SETTING, max)
            .expect("bound is accepted");
        assert_eq!(
            unload(&connection),
            Some(Duration::from_secs(max * SECONDS_PER_MINUTE))
        );
        assert_eq!(
            OrchestrationSettingsRepository::child_result_coalesce(&connection).expect("reads"),
            Duration::ZERO,
            "writing one field keeps the other"
        );
        assert!(
            OrchestrationSettingsRepository::set(&connection, IDLE_SESSION_UNLOAD_SETTING, max + 1)
                .is_err()
        );
        OrchestrationSettingsRepository::set(
            &connection,
            IDLE_SESSION_UNLOAD_SETTING,
            IDLE_SESSION_UNLOAD_SETTING.min,
        )
        .expect("zero is accepted");
        assert_eq!(unload(&connection), None);
    }
}
