//! Read persisted monitor records.

use rusqlite::{Connection, OptionalExtension};

use crate::StoreResult;
use crate::records::{MonitorRecord, Table};
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};

#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MonitorOperation {
    pub agent: String,
    pub epoch: i64,
    pub account_key: String,
    pub connection_id: String,
    #[serde(flatten)]
    pub evidence: MonitorOperationEvidence,
}

#[derive(Clone, Debug, Default, serde::Deserialize, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MonitorOperationEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub root_request_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub provider_generation: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub provider_request: Option<crate::request_results::AdmittedRequest>,
}

impl PartialEq for MonitorOperationEvidence {
    fn eq(&self, other: &Self) -> bool {
        // The historical thread is retained for reading, not operation identity.
        self.root_request_id == other.root_request_id
            && self.provider_generation == other.provider_generation
            && self.provider_request == other.provider_request
    }
}

fn present<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

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
const STUDIO_ERROR_INDEX: &str = "runtime_monitor_studio_error_v2";
/// Retired derived index; its predicate excluded malformed and unclassified failed records.
const LEGACY_STUDIO_ERROR_INDEX: &str = "runtime_monitor_studio_error";

/// SQL predicate of `STUDIO_ERROR_INDEX` and of the read that must use it.
/// It mirrors `studio_errors::monitor_kind` in `studio-runtime-services`, which
/// stays the exact classifier: a `lost` monitor, or a `failed` monitor with no
/// (or a null) `exitCode`. The owner-stop test is
/// left to the classifier, so this is a superset of its accepted rows. A
/// malformed record is selected as a diagnostic, without parsing it in SQL.
fn studio_error_predicate() -> String {
    let statuses = studio_runtime_schema::studio_errors::MonitorStatus::ALL
        .iter()
        .map(|status| format!("'{}'", status.as_str()))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "CASE WHEN json_valid(record) THEN json_type(record,'$.status') IS NOT 'text' OR json_extract(record,'$.status') NOT IN ({statuses}) OR json_extract(record,'$.status')='lost' OR (json_extract(record,'$.status')='failed' AND json_type(record,'$.exitCode') IS NOT 'integer') ELSE 1 END"
    )
}

/// Creation time of a monitor, in Unix seconds; the key of `STUDIO_ERROR_INDEX`.
const CREATED_EXPRESSION: &str = "CASE WHEN json_valid(record) THEN CASE WHEN json_type(record,'$.created') IN ('integer','real') AND json_extract(record,'$.created')>=0 THEN json_extract(record,'$.created') ELSE 0 END ELSE 0 END";

fn studio_error_candidates_sql() -> String {
    let predicate = studio_error_predicate();
    format!(
        "SELECT id,record FROM runtime_monitors WHERE {predicate} \
         AND {CREATED_EXPRESSION}>=?1 UNION ALL \
         SELECT id,record FROM runtime_monitors WHERE {predicate} AND {CREATED_EXPRESSION}=0"
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
            let predicate = studio_error_predicate();
            connection.execute_batch(&format!(
                "CREATE INDEX IF NOT EXISTS {QUEUED_STREAM_INDEX} ON runtime_monitors(id) \
                 WHERE {QUEUED_STREAM_PREDICATE};
                 CREATE INDEX IF NOT EXISTS {STUDIO_ERROR_INDEX} \
                 ON runtime_monitors({CREATED_EXPRESSION}) WHERE {predicate}; \
                 DROP INDEX IF EXISTS {LEGACY_STUDIO_ERROR_INDEX}"
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

    /// The record of monitor `id` when it was created at or after `since`
    /// (Unix seconds) and may be a Studio-caused ending.
    pub fn studio_error_candidate(
        connection: &Connection,
        id: &str,
        since: f64,
    ) -> StoreResult<Option<String>> {
        let predicate = studio_error_predicate();
        Ok(connection
            .prepare_cached(&format!(
                "SELECT record FROM runtime_monitors WHERE id=?1 AND {predicate} \
                 AND ({CREATED_EXPRESSION}>=?2 OR {CREATED_EXPRESSION}=0)"
            ))?
            .query_row(rusqlite::params![id, since], |row| row.get(0))
            .optional()?)
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

    #[test]
    fn monitor_rows_check_identity_and_preserve_open_operation_evidence() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY, record TEXT NOT NULL)",
        )
        .unwrap();
        let raw = r#"{"id":"m","status":null,"operation":{"agent":"a","epoch":1.0,"accountKey":"default","connectionId":"c","providerRequest":{"future":true}},"streamQueue":[{"opaque":true}]}"#;
        db.execute("INSERT INTO runtime_monitors VALUES ('m',?1)", [raw])
            .unwrap();
        assert_eq!(
            MonitorsRepository::get(&db, "m").unwrap().unwrap().value(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );
        for raw in [
            r#"{"id":"wrong"}"#,
            r#"{"id":"m","status":"unknown"}"#,
            r#"{"id":"m","id":"m"}"#,
        ] {
            db.execute("UPDATE runtime_monitors SET record=?1", [raw])
                .unwrap();
            assert!(MonitorsRepository::get(&db, "m").is_err());
        }
    }

    #[test]
    fn historical_thread_is_readable_but_does_not_change_operation_identity() {
        let base = r#"{"agent":"a","epoch":1,"accountKey":"default","connectionId":"c"}"#;
        let historical = r#"{"agent":"a","epoch":1,"accountKey":"default","connectionId":"c","threadId":"thread"}"#;
        let original: super::MonitorOperation = serde_json::from_str(base).unwrap();
        let saved: super::MonitorOperation = serde_json::from_str(historical).unwrap();
        assert_eq!(original, saved);
        assert_eq!(
            crate::projection_value(&saved),
            serde_json::from_str::<serde_json::Value>(historical).unwrap()
        );
        assert_eq!(
            serde_json::from_str::<super::MonitorOperation>(r#"{"agent":"a","epoch":1,"accountKey":"default","connectionId":"c","threadId":null}"#).unwrap(),
            original
        );
        assert!(serde_json::from_str::<super::MonitorOperation>(r#"{"agent":"a","epoch":1,"accountKey":"default","connectionId":"c","unexpected":true}"#).is_err());
    }

    #[test]
    fn operation_evidence_rejects_unknown_fields_directly_and_when_flattened() {
        use serde::Deserialize;
        use serde_json::json;

        let evidence = json!({"threadId":"t", "rootRequestId":"r", "providerGeneration":3,
            "providerRequest":{"id":"i", "signature":"s", "created":1}});
        assert!(super::MonitorOperationEvidence::deserialize(evidence.clone()).is_ok());
        let mut invalid = evidence.clone();
        invalid["unexpected"] = json!(true);
        assert!(super::MonitorOperationEvidence::deserialize(invalid.clone()).is_err());
        let mut operation =
            json!({"agent":"a", "epoch":1, "accountKey":"default", "connectionId":"c"});
        operation
            .as_object_mut()
            .unwrap()
            .extend(evidence.as_object().unwrap().clone());
        assert!(super::MonitorOperation::deserialize(operation.clone()).is_ok());
        operation["unexpected"] = json!(true);
        assert!(super::MonitorOperation::deserialize(operation).is_err());
    }

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
        assert_eq!(
            ids,
            [
                "a-malformed",
                "b-lost",
                "d-not-recorded",
                "e-no-exit-key",
                "h-empty-error",
                "i-no-error",
                "j-number-error"
            ]
        );
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
            plan.iter()
                .filter(|step| step.contains(STUDIO_ERROR_INDEX))
                .count(),
            2,
            "both dated and undated ranges use the derived index: {plan:?}"
        );
        assert!(
            !plan
                .iter()
                .any(|step| step.contains("SCAN runtime_monitors")),
            "{plan:?}"
        );
    }
}
