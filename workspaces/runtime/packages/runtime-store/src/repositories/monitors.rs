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

/// Partial index, by creation time, of the monitors that may be a
/// Studio-caused ending. The Studio error list reads them in nearly every
/// Canvas writer transaction; through the status index that read parsed 6,771
/// records (17 MB) on 25 October 2026, mostly commands that ended with their
/// own exit code, to keep about 680.
const STUDIO_ERROR_INDEX: &str = "runtime_monitor_studio_error";

/// SQL predicate of `STUDIO_ERROR_INDEX` and of the read that must use it.
/// It mirrors `studio_errors::monitor_kind` in `studio-runtime-services`, which
/// stays the exact classifier: a `lost` monitor, or a `failed` monitor with no
/// (or a null) `exitCode` and a non-empty text `error`. The owner-stop test is
/// left to the classifier, so this is a superset of its accepted rows. A
/// malformed record is never selected, so it cannot refuse the index build or
/// the read.
const STUDIO_ERROR_PREDICATE: &str = "CASE WHEN json_valid(record) THEN \
     json_extract(record,'$.status')='lost' OR (json_extract(record,'$.status')='failed' \
     AND json_extract(record,'$.exitCode') IS NULL AND json_type(record,'$.error')='text' \
     AND json_extract(record,'$.error')<>'') ELSE 0 END";

/// Creation time of a monitor, in Unix seconds; the key of `STUDIO_ERROR_INDEX`.
const CREATED_EXPRESSION: &str = "json_extract(record,'$.created')";

fn studio_error_candidates_sql() -> String {
    format!(
        "SELECT id,record FROM runtime_monitors WHERE {STUDIO_ERROR_PREDICATE} \
         AND {CREATED_EXPRESSION}>=?1"
    )
}

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

    /// Create the queued-stream and Studio-error partial indexes on a state
    /// that has monitors.
    /// `IF NOT EXISTS` makes a reopen a no-op; the guarded predicate lets the
    /// build pass over malformed rows of an older state.
    pub(crate) fn prepare_indexes(connection: &Connection) -> StoreResult<()> {
        if crate::schema::table_exists(connection, MONITOR_TABLE.name)? {
            connection.execute_batch(&format!(
                "CREATE INDEX IF NOT EXISTS {QUEUED_STREAM_INDEX} ON runtime_monitors(id) \
                 WHERE {QUEUED_STREAM_PREDICATE};
                 CREATE INDEX IF NOT EXISTS {STUDIO_ERROR_INDEX} \
                 ON runtime_monitors({CREATED_EXPRESSION}) WHERE {STUDIO_ERROR_PREDICATE}"
            ))?;
        }
        Ok(())
    }

    /// `(id, record)` of every monitor created at or after `since` (Unix
    /// seconds) that may be a Studio-caused ending, read through
    /// `STUDIO_ERROR_INDEX`. The caller classifies each row exactly.
    pub fn studio_error_candidates(
        connection: &Connection,
        since: f64,
    ) -> StoreResult<Vec<(String, String)>> {
        Ok(connection
            .prepare_cached(&studio_error_candidates_sql())?
            .query_map([since], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?)
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
    use super::{MonitorsRepository, QUEUED_STREAM_INDEX, STUDIO_ERROR_INDEX};
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

    /// The Studio error list runs this read in nearly every Canvas writer
    /// transaction. On 25 October 2026 it parsed 6,771 records (17 MB) per
    /// call, mostly commands that ended with their own exit code, to keep
    /// about 680. It must select only rows the classifier can accept, through
    /// a narrow index search, and a malformed row must not refuse it.
    #[test]
    fn studio_error_candidates_skip_own_exits_and_use_their_index() {
        let connection = Connection::open_in_memory().expect("memory SQLite opens");
        connection
            .execute_batch(
                r#"CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 INSERT INTO runtime_monitors VALUES
                   ('a-malformed','{'),
                   ('b-lost','{"status":"lost","created":100,"exitCode":null}'),
                   ('c-lost-old','{"status":"lost","created":10}'),
                   ('d-not-recorded','{"status":"failed","created":100,"exitCode":null,"error":"Server restarted"}'),
                   ('e-no-exit-key','{"status":"failed","created":100,"error":"No live provider session","ruleId":"r"}'),
                   ('f-own-exit','{"status":"failed","created":100,"exitCode":1,"error":"","ruleId":"r"}'),
                   ('g-own-exit-error','{"status":"failed","created":100,"exitCode":0,"error":"boom"}'),
                   ('h-empty-error','{"status":"failed","created":100,"exitCode":null,"error":""}'),
                   ('i-no-error','{"status":"failed","created":100,"exitCode":null}'),
                   ('j-number-error','{"status":"failed","created":100,"exitCode":null,"error":7}'),
                   ('k-completed','{"status":"completed","created":100,"exitCode":null,"error":"x"}'),
                   ('l-running','{"status":"running","created":100}');"#,
            )
            .expect("monitor rows");
        MonitorsRepository::prepare_indexes(&connection).expect("index builds");
        MonitorsRepository::prepare_indexes(&connection).expect("repeated build is a no-op");
        let mut ids = MonitorsRepository::studio_error_candidates(&connection, 50.0)
            .expect("candidate read")
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(ids, ["b-lost", "d-not-recorded", "e-no-exit-key"]);
        let plan = connection
            .prepare(&format!(
                "EXPLAIN QUERY PLAN {}",
                super::studio_error_candidates_sql()
            ))
            .expect("plan")
            .query_map([50.0], |row| row.get::<_, String>(3))
            .expect("plan rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("plan rows");
        assert_eq!(
            plan,
            [format!(
                "SEARCH runtime_monitors USING INDEX {STUDIO_ERROR_INDEX} (<expr>>?)"
            )]
        );
    }
}
