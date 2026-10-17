//! Indexed selection of the rows startup recovery settles. Each partial index
//! holds only unsettled rows, so a restart reads the unsettled rows instead of
//! whole history tables. Malformed JSON is never selected, which matches the
//! recovery passes that skip rows they cannot decode.
use rusqlite::Connection;

use crate::{OwnedRuntimeIndexDefinition, StoreError, StoreResult};

/// Monitor statuses that mean a monitor process may still be live.
pub const ACTIVE_MONITOR_STATUSES: [&str; 3] = ["running", "starting", "approval"];
/// Request statuses that a restart must settle (`answering`) or reconsider (`pending`).
pub const UNSETTLED_REQUEST_STATUSES: [&str; 2] = ["answering", "pending"];
/// Tool request stages whose dispatcher a restart has lost.
pub const UNSETTLED_TOOL_REQUEST_STAGES: [&str; 2] = ["queued", "running"];

const MONITOR_INDEX: &str = "runtime_monitor_startup_active";
const REQUEST_INDEX: &str = "runtime_request_startup_unsettled";
const TOOL_REQUEST_INDEX: &str = "runtime_tool_request_startup_unsettled";

/// One owner for each index predicate and the query that must match it.
struct Selection {
    index: &'static str,
    table: &'static str,
    field: &'static str,
    values: &'static [&'static str],
}

const MONITORS: Selection = Selection {
    index: MONITOR_INDEX,
    table: "runtime_monitors",
    field: "status",
    values: &ACTIVE_MONITOR_STATUSES,
};
const REQUESTS: Selection = Selection {
    index: REQUEST_INDEX,
    table: "runtime_requests",
    field: "status",
    values: &UNSETTLED_REQUEST_STATUSES,
};
const TOOL_REQUESTS: Selection = Selection {
    index: TOOL_REQUEST_INDEX,
    table: "runtime_tool_requests",
    field: "stage",
    values: &UNSETTLED_TOOL_REQUEST_STAGES,
};

impl Selection {
    fn predicate(&self) -> String {
        let values = self
            .values
            .iter()
            .map(|value| format!("'{value}'"))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "CASE WHEN json_valid(record) THEN json_extract(record,'$.{}') IN ({values}) ELSE 0 END",
            self.field
        )
    }

    fn definition(&self) -> OwnedRuntimeIndexDefinition {
        OwnedRuntimeIndexDefinition {
            name: self.index,
            table: self.table,
            create_sql: format!(
                "CREATE INDEX IF NOT EXISTS {} ON {}(id) WHERE {}",
                self.index,
                self.table,
                self.predicate()
            ),
        }
    }

    fn query(&self) -> String {
        format!(
            "SELECT id,record FROM {} WHERE {} ORDER BY id",
            self.table,
            self.predicate()
        )
    }

    fn rows(&self, connection: &Connection) -> StoreResult<Vec<(String, String)>> {
        if !crate::schema::table_exists(connection, self.table)? {
            return Ok(Vec::new());
        }
        let mut statement = connection.prepare_cached(&self.query())?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }
}

/// The predicate of the unsettled-request partial index, for queries that must use it.
pub(crate) fn unsettled_request_predicate() -> String {
    REQUESTS.predicate()
}

pub struct StartupRecoveryRepository;

impl StartupRecoveryRepository {
    /// Canonical DDL for the startup recovery partial indexes.
    #[doc(hidden)]
    pub fn index_definitions() -> Vec<OwnedRuntimeIndexDefinition> {
        [MONITORS, REQUESTS, TOOL_REQUESTS]
            .iter()
            .map(Selection::definition)
            .collect()
    }

    #[doc(hidden)]
    pub fn prepare_indexes(connection: &Connection) -> StoreResult<()> {
        for selection in [MONITORS, REQUESTS, TOOL_REQUESTS] {
            if crate::schema::table_exists(connection, selection.table)? {
                connection.execute_batch(&selection.definition().create_sql)?;
            }
        }
        Ok(())
    }

    /// `(id, record)` of every monitor in an active status, ordered by id.
    pub fn active_monitors(connection: &Connection) -> StoreResult<Vec<(String, String)>> {
        MONITORS.rows(connection)
    }

    /// `(id, record)` of every answering or pending request, ordered by id.
    pub fn unsettled_requests(connection: &Connection) -> StoreResult<Vec<(String, String)>> {
        REQUESTS.rows(connection)
    }

    /// Ids of every queued or running tool request, ordered by id.
    pub fn unsettled_tool_request_ids(connection: &Connection) -> StoreResult<Vec<String>> {
        Ok(TOOL_REQUESTS
            .rows(connection)?
            .into_iter()
            .map(|(id, _)| id)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::{MONITORS, REQUESTS, StartupRecoveryRepository, TOOL_REQUESTS};
    use rusqlite::Connection;

    fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("memory SQLite opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_requests(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_tool_requests(id TEXT PRIMARY KEY,record TEXT NOT NULL);",
            )
            .expect("tables");
        StartupRecoveryRepository::prepare_indexes(&connection).expect("indexes");
        connection
    }

    #[test]
    fn selections_read_only_their_partial_index() {
        let connection = database();
        for selection in [MONITORS, REQUESTS, TOOL_REQUESTS] {
            let plan = connection
                .prepare(&format!("EXPLAIN QUERY PLAN {}", selection.query()))
                .expect("plan prepares")
                .query_map([], |row| row.get::<_, String>(3))
                .expect("plan runs")
                .collect::<Result<Vec<_>, _>>()
                .expect("plan rows")
                .join(" ");
            assert!(
                plan.contains(&format!("USING COVERING INDEX {}", selection.index))
                    || plan.contains(&format!("USING INDEX {}", selection.index)),
                "{plan}"
            );
        }
    }

    #[test]
    fn selections_return_exactly_the_unsettled_values_and_skip_malformed_rows() {
        let connection = database();
        for selection in [MONITORS, REQUESTS, TOOL_REQUESTS] {
            let field = selection.field;
            let mut rows = selection
                .values
                .iter()
                .map(|value| (format!("0-{value}"), format!(r#"{{"{field}":"{value}"}}"#)))
                .collect::<Vec<_>>();
            rows.extend([
                (
                    "1-settled".to_owned(),
                    format!(r#"{{"{field}":"settled"}}"#),
                ),
                ("2-malformed".to_owned(), "{".to_owned()),
                (
                    "3-array".to_owned(),
                    format!(r#"["{}"]"#, selection.values[0]),
                ),
            ]);
            for (id, record) in &rows {
                connection
                    .execute(
                        &format!("INSERT INTO {}(id,record) VALUES(?1,?2)", selection.table),
                        [id, record],
                    )
                    .expect("row");
            }
            let mut expected = rows[..selection.values.len()].to_vec();
            expected.sort();
            assert_eq!(selection.rows(&connection).expect("rows"), expected);
        }
    }
}
