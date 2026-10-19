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

/// Agent statuses that mean a turn may still run under the agent.
pub const ACTIVE_AGENT_STATUSES: [&str; 3] = ["running", "starting", "approval"];

const MONITOR_INDEX: &str = "runtime_monitor_startup_active";
const REQUEST_INDEX: &str = "runtime_request_startup_unsettled";
const TOOL_REQUEST_INDEX: &str = "runtime_tool_request_startup_unsettled";
const CLEANUP_INTENT_INDEX: &str = "runtime_agent_cleanup_intent";
const DELETED_UNSETTLED_INDEX: &str = "runtime_agent_deleted_unsettled";

/// SQL: the stored value at `path` is neither absent, null, false nor zero.
/// A superset of the Rust `truthy` test (an empty string or container passes),
/// so selections through it are refined by the exact Rust test.
fn nonfalsy(path: &str) -> String {
    format!("COALESCE(json_extract(record,'{path}'),0)!=0")
}

/// SQL predicate: an agent record carries a workspace cleanup intent object.
fn cleanup_intent_predicate() -> String {
    "CASE WHEN json_valid(record) THEN json_type(record,'$.workspaceCleanupIntent')='object' \
     ELSE 0 END"
        .to_owned()
}

/// SQL predicate: a deleted agent that may still hold an active turn, the
/// rows whose deletion settlement a restart must finish.
fn deleted_unsettled_predicate() -> String {
    let statuses = ACTIVE_AGENT_STATUSES
        .iter()
        .map(|status| format!("'{status}'"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "CASE WHEN json_valid(record) THEN ({} AND ({} OR {} OR {} OR \
         json_extract(record,'$.status') IN ({statuses}))) ELSE 0 END",
        nonfalsy("$.deletedAt"),
        nonfalsy("$.inFlight"),
        nonfalsy("$.turnId"),
        nonfalsy("$.startAttempt.activeAtReservation"),
    )
}

/// One owner for an agent partial index and the query that must match it.
struct AgentSelection {
    index: &'static str,
    predicate: fn() -> String,
}
const CLEANUP_INTENTS: AgentSelection = AgentSelection {
    index: CLEANUP_INTENT_INDEX,
    predicate: cleanup_intent_predicate,
};
const DELETED_UNSETTLED: AgentSelection = AgentSelection {
    index: DELETED_UNSETTLED_INDEX,
    predicate: deleted_unsettled_predicate,
};
impl AgentSelection {
    fn definition(&self) -> OwnedRuntimeIndexDefinition {
        OwnedRuntimeIndexDefinition {
            name: self.index,
            table: "runtime_agents",
            create_sql: format!(
                "CREATE INDEX IF NOT EXISTS {} ON runtime_agents(id) WHERE {}",
                self.index,
                (self.predicate)()
            ),
        }
    }
    fn rows(&self, connection: &Connection) -> StoreResult<Vec<(String, String)>> {
        if !crate::schema::table_exists(connection, "runtime_agents")? {
            return Ok(Vec::new());
        }
        let mut statement = connection.prepare_cached(&format!(
            "SELECT id,record FROM runtime_agents WHERE {} ORDER BY id",
            (self.predicate)()
        ))?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }
}

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

/// The predicate of the active-monitor partial index.
pub(crate) fn active_monitor_predicate() -> String {
    MONITORS.predicate()
}

/// The predicate of the unsettled tool-request partial index.
pub(crate) fn unsettled_tool_request_predicate() -> String {
    TOOL_REQUESTS.predicate()
}

pub struct StartupRecoveryRepository;

impl StartupRecoveryRepository {
    /// Canonical DDL for the startup recovery partial indexes.
    #[doc(hidden)]
    pub fn index_definitions() -> Vec<OwnedRuntimeIndexDefinition> {
        [MONITORS, REQUESTS, TOOL_REQUESTS]
            .iter()
            .map(Selection::definition)
            .chain(
                [CLEANUP_INTENTS, DELETED_UNSETTLED]
                    .iter()
                    .map(AgentSelection::definition),
            )
            .collect()
    }

    #[doc(hidden)]
    pub fn prepare_indexes(connection: &Connection) -> StoreResult<()> {
        for selection in [MONITORS, REQUESTS, TOOL_REQUESTS] {
            if crate::schema::table_exists(connection, selection.table)? {
                connection.execute_batch(&selection.definition().create_sql)?;
            }
        }
        if crate::schema::table_exists(connection, "runtime_agents")? {
            for selection in [CLEANUP_INTENTS, DELETED_UNSETTLED] {
                connection.execute_batch(&selection.definition().create_sql)?;
            }
        }
        Ok(())
    }

    /// `(id, record)` of every agent carrying a workspace cleanup intent, ordered by id.
    pub fn agents_with_cleanup_intent(
        connection: &Connection,
    ) -> StoreResult<Vec<(String, String)>> {
        CLEANUP_INTENTS.rows(connection)
    }

    /// `(id, record)` of every deleted agent that may still hold an active turn,
    /// ordered by id. A superset: the caller applies the exact settlement test.
    pub fn deleted_agents_maybe_unsettled(
        connection: &Connection,
    ) -> StoreResult<Vec<(String, String)>> {
        DELETED_UNSETTLED.rows(connection)
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
        for (selection, rows, expected) in [
            (
                MONITORS,
                [
                    ("a", r#"{"status":"running"}"#),
                    ("b", r#"{"status":"starting"}"#),
                    ("c", r#"{"status":"approval"}"#),
                    ("d", r#"{"status":"completed"}"#),
                    ("e", r#"{"status":"pending"}"#),
                    ("f", "{"),
                    ("g", r#"["running"]"#),
                ]
                .as_slice(),
                ["a", "b", "c"].as_slice(),
            ),
            (
                REQUESTS,
                [
                    ("a", r#"{"status":"answering"}"#),
                    ("b", r#"{"status":"pending"}"#),
                    ("c", r#"{"status":"answered"}"#),
                    ("d", r#"{"status":"running"}"#),
                    ("e", "{"),
                    ("f", r#"["pending"]"#),
                ]
                .as_slice(),
                ["a", "b"].as_slice(),
            ),
            (
                TOOL_REQUESTS,
                [
                    ("a", r#"{"stage":"queued"}"#),
                    ("b", r#"{"stage":"running"}"#),
                    ("c", r#"{"stage":"completed"}"#),
                    ("d", r#"{"status":"queued"}"#),
                    ("e", "{"),
                    ("f", r#"["queued"]"#),
                ]
                .as_slice(),
                ["a", "b"].as_slice(),
            ),
        ] {
            for (id, record) in rows {
                connection
                    .execute(
                        &format!("INSERT INTO {}(id,record) VALUES(?1,?2)", selection.table),
                        [id, record],
                    )
                    .expect("row");
            }
            let selected = selection.rows(&connection).expect("rows");
            let ids = selected
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>();
            assert_eq!(ids, expected, "{}", selection.table);
            for (id, record) in &selected {
                assert!(rows.contains(&(id.as_str(), record.as_str())), "{id}");
            }
        }
    }

    #[test]
    fn agent_selections_read_only_their_partial_index_and_exact_rows() {
        let connection = Connection::open_in_memory().expect("memory SQLite opens");
        connection
            .execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);")
            .expect("table");
        StartupRecoveryRepository::prepare_indexes(&connection).expect("indexes");
        for (selection, index) in [
            (&super::CLEANUP_INTENTS, "runtime_agent_cleanup_intent"),
            (&super::DELETED_UNSETTLED, "runtime_agent_deleted_unsettled"),
        ] {
            let plan = connection
                .prepare(&format!(
                    "EXPLAIN QUERY PLAN SELECT id,record FROM runtime_agents WHERE {} ORDER BY id",
                    (selection.predicate)()
                ))
                .expect("plan prepares")
                .query_map([], |row| row.get::<_, String>(3))
                .expect("plan runs")
                .collect::<Result<Vec<_>, _>>()
                .expect("plan rows")
                .join(" ");
            assert!(plan.contains(&format!("INDEX {index}")), "{plan}");
        }
        for (id, record) in [
            (
                "a-intent",
                r#"{"workspaceCleanupIntent":{"phase":"queued"}}"#,
            ),
            ("b-null-intent", r#"{"workspaceCleanupIntent":null}"#),
            ("c-running", r#"{"deletedAt":5,"status":"running"}"#),
            ("d-turn", r#"{"deletedAt":5,"turnId":"t"}"#),
            (
                "e-reserved",
                r#"{"deletedAt":5,"startAttempt":{"activeAtReservation":true}}"#,
            ),
            (
                "f-tombstone",
                r#"{"deletedAt":5,"inFlight":false,"turnId":null,"status":"idle"}"#,
            ),
            ("g-live", r#"{"status":"running"}"#),
            ("h-malformed", "{"),
        ] {
            connection
                .execute("INSERT INTO runtime_agents VALUES(?1,?2)", [id, record])
                .expect("row");
        }
        let ids =
            |rows: Vec<(String, String)>| rows.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
        assert_eq!(
            ids(StartupRecoveryRepository::agents_with_cleanup_intent(&connection).expect("rows")),
            ["a-intent"]
        );
        assert_eq!(
            ids(
                StartupRecoveryRepository::deleted_agents_maybe_unsettled(&connection)
                    .expect("rows")
            ),
            ["c-running", "d-turn", "e-reserved"]
        );
    }
}
