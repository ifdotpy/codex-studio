//! Read persisted monitor records.

use rusqlite::{Connection, OptionalExtension};

use crate::StoreResult;
use crate::records::{MonitorRecord, Table};
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};

/// Compatibility name for callers that used the pre-typed monitor wrapper.
pub type StoredMonitor = MonitorRecord;

const MONITOR_TABLE: TableRequirement = TableRequirement {
    name: "runtime_monitors",
    columns: &["id", "record"],
};

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[ConditionalTableRequirement {
        table_exists: "runtime_monitors",
        tables: &[MONITOR_TABLE],
    }],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

/// Partial index of the monitors whose durable result stream holds an
/// undelivered event. The monitor tick reads the stream head once per pass and
/// once per delivered event; without this index that read parsed every
/// historical monitor (93,650 rows, 209 MB, about 0.55 s on 22 October 2026).
const QUEUED_STREAM_INDEX: &str = "runtime_monitor_queued_stream";

/// SQL predicate of `QUEUED_STREAM_INDEX` and of the query that must use it:
/// the record's `streamQueue` holds at least one event. A malformed record is
/// never selected, so it cannot refuse the index build or the stream read.
const QUEUED_STREAM_PREDICATE: &str =
    "CASE WHEN json_valid(record) THEN json_array_length(record,'$.streamQueue')>0 ELSE 0 END";

#[derive(Clone, Copy, Debug, Default)]
pub struct MonitorsRepository;

impl MonitorsRepository {
    pub fn get(connection: &Connection, id: &str) -> StoreResult<Option<MonitorRecord>> {
        let has_table: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_monitors')",
            [],
            |row| row.get(0),
        )?;
        if !has_table {
            return Ok(None);
        }
        Table::<MonitorRecord>::new(connection).get(&id.to_owned())
    }

    /// Create the queued-stream partial index on a state that has monitors.
    /// `IF NOT EXISTS` makes a reopen a no-op; the guarded predicate lets the
    /// build pass over malformed rows of an older state.
    pub(crate) fn prepare_indexes(connection: &Connection) -> StoreResult<()> {
        if crate::schema::table_exists(connection, MONITOR_TABLE.name)? {
            connection.execute_batch(&format!(
                "CREATE INDEX IF NOT EXISTS {QUEUED_STREAM_INDEX} ON runtime_monitors(id) \
                 WHERE {QUEUED_STREAM_PREDICATE}"
            ))?;
        }
        Ok(())
    }

    /// `(id, record)` of the first monitor, by id, whose durable result stream
    /// (`streamQueue`) holds an undelivered event, whatever its status. The
    /// queued-stream index bounds this to monitors with pending events.
    pub fn first_queued_stream(connection: &Connection) -> StoreResult<Option<(String, String)>> {
        Ok(connection
            .prepare_cached(&format!(
                "SELECT id,record FROM runtime_monitors WHERE {QUEUED_STREAM_PREDICATE} \
                 ORDER BY id LIMIT 1"
            ))?
            .query_row([], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?)
    }
}
inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "monitors",
        requirements: &REQUIREMENTS,
    }
}

#[cfg(test)]
mod tests {
    use super::{MonitorsRepository, QUEUED_STREAM_INDEX};
    use rusqlite::Connection;

    /// The index build runs at every store open, also on older states that
    /// may hold malformed monitor rows. It must succeed there, be a no-op when
    /// repeated, and serve the stream read without refusing the valid rows.
    #[test]
    fn queued_stream_index_builds_over_malformed_rows_and_serves_the_stream_read() {
        let connection = Connection::open_in_memory().expect("memory SQLite opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 INSERT INTO runtime_monitors VALUES
                   ('a-malformed','{'),
                   ('b-done','{\"status\":\"completed\",\"streamQueue\":[]}'),
                   ('c-other','{\"status\":\"completed\",\"streamQueue\":{}}'),
                   ('d-queued','{\"status\":\"completed\",\"streamQueue\":[{\"cursor\":\"result:d\"}]}');",
            )
            .expect("monitor rows");
        MonitorsRepository::prepare_indexes(&connection).expect("index builds");
        MonitorsRepository::prepare_indexes(&connection).expect("repeated build is a no-op");
        let indexes: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
                [QUEUED_STREAM_INDEX],
                |row| row.get(0),
            )
            .expect("catalog");
        assert_eq!(indexes, 1);
        let indexed: i64 = connection
            .query_row(
                &format!("SELECT COUNT(*) FROM runtime_monitors INDEXED BY {QUEUED_STREAM_INDEX} WHERE {}", super::QUEUED_STREAM_PREDICATE),
                [],
                |row| row.get(0),
            )
            .expect("indexed rows");
        assert_eq!(
            indexed, 1,
            "only the monitor with a pending event is indexed"
        );
        let head = MonitorsRepository::first_queued_stream(&connection).expect("stream read");
        assert_eq!(head.map(|(id, _)| id).as_deref(), Some("d-queued"));
        connection
            .execute(
                "UPDATE runtime_monitors SET record='{\"streamQueue\":[]}' WHERE id='d-queued'",
                [],
            )
            .expect("acknowledge");
        assert!(
            MonitorsRepository::first_queued_stream(&connection)
                .expect("stream read")
                .is_none()
        );
    }
}
