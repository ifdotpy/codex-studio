//! Indexed reads of `runtime_agents` for hot writer-transaction paths. Each
//! query is written to match an existing index so that no caller reads or
//! parses the whole table (tombstones and finished agents dominate it). The
//! query-plan test below fails when a query stops using its index.

use rusqlite::{Connection, params};

use crate::StoreResult;

/// Statuses that occupy a capacity slot; the unit is a status name. Deleting an
/// agent leaves it `paused`, so a slot also requires `CAPACITY_LIVE`.
const CAPACITY_STATUSES: &str = "('starting','running','waiting','paused','approval')";
/// A deleted agent (tombstone) never occupies a slot. Together with
/// `CAPACITY_STATUSES` this is the one owner of the capacity index predicate and
/// of every slot query.
const CAPACITY_LIVE: &str = "json_extract(record,'$.deletedAt') IS NULL";
/// Name of the covering index for the capacity counts. `_v2` added `CAPACITY_LIVE`.
pub const CAPACITY_INDEX: &str = "runtime_agent_capacity_slots_v2";
/// Index replaced by `CAPACITY_INDEX`; dropped at bootstrap so tombstones leave it.
const RETIRED_CAPACITY_INDEX: &str = "runtime_agent_capacity_slots";
const REMOTE_WORKER_KEY: &str = "COALESCE(json_extract(record,'$.remoteWorker'),0)";

/// Indexed reads of agent records by team root or by slot-occupying status.
pub struct AgentIndexedReads;

impl AgentIndexedReads {
    /// Canonical DDL of the capacity-slot partial index (Rust-owned).
    #[doc(hidden)]
    pub fn index_definition() -> crate::OwnedRuntimeIndexDefinition {
        crate::OwnedRuntimeIndexDefinition {
            name: CAPACITY_INDEX,
            table: "runtime_agents",
            create_sql: format!(
                "CREATE INDEX IF NOT EXISTS {CAPACITY_INDEX} ON runtime_agents({REMOTE_WORKER_KEY},id) \
                 WHERE json_extract(record,'$.status') IN {CAPACITY_STATUSES} AND {CAPACITY_LIVE}"
            ),
        }
    }

    #[doc(hidden)]
    pub fn prepare_index(connection: &Connection) -> StoreResult<()> {
        if crate::schema::table_exists(connection, "runtime_agents")? {
            connection.execute_batch(&format!("DROP INDEX IF EXISTS {RETIRED_CAPACITY_INDEX}"))?;
            connection.execute_batch(&Self::index_definition().create_sql)?;
        }
        Ok(())
    }

    /// SQL listing `(id, record)` of every agent of one team root, in rowid order.
    #[doc(hidden)]
    pub const TEAM_RECORDS_SQL: &'static str = "SELECT id,record FROM runtime_agents \
         WHERE json_extract(record,'$.rootId')=?1 ORDER BY rowid";

    /// Raw `(id, record)` of every agent whose `rootId` is `root_id`.
    pub fn team_records(
        connection: &Connection,
        root_id: &str,
    ) -> StoreResult<Vec<(String, String)>> {
        let mut statement = connection.prepare_cached(Self::TEAM_RECORDS_SQL)?;
        Ok(statement
            .query_map([root_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?)
    }

    /// SQL counting the live (not deleted) team members that are not finished.
    /// A finished member is completed, failed, interrupted, or paused without
    /// auto-wake.
    #[doc(hidden)]
    pub const ACTIVE_TEAM_MEMBERS_SQL: &'static str = "SELECT COUNT(*) FROM runtime_agents INDEXED BY runtime_agent_live_root \
         WHERE json_extract(record,'$.rootId')=?1 AND json_extract(record,'$.deletedAt') IS NULL \
         AND NOT (COALESCE(json_extract(record,'$.status'),'') IN ('completed','failed','interrupted') \
         OR (json_extract(record,'$.status') IS 'paused' AND json_type(record,'$.autoWake') IS 'false'))";

    pub fn active_team_members(connection: &Connection, root_id: &str) -> StoreResult<i64> {
        Ok(connection
            .prepare_cached(Self::ACTIVE_TEAM_MEMBERS_SQL)?
            .query_row([root_id], |row| row.get(0))?)
    }

    /// SQL for the ids of agents holding a workspace operation (same index).
    #[doc(hidden)]
    pub const WORKSPACE_OPERATION_IDS_SQL: &'static str = "SELECT id FROM runtime_agents \
         WHERE json_type(record,'$.workspaceOperation')='text' \
         AND json_extract(record,'$.workspaceOperation')!=''";

    /// SQL for the `id,record` rows of agents holding a workspace operation, in
    /// index order (same index).
    #[doc(hidden)]
    pub const WORKSPACE_OPERATION_RECORDS_SQL: &'static str = "SELECT id,record FROM runtime_agents \
         WHERE json_type(record,'$.workspaceOperation')='text' \
         AND json_extract(record,'$.workspaceOperation')!=''";

    /// SQL for the distinct working directories of live agents and of any agent
    /// (live or not) holding a workspace operation: the directories a message
    /// admission may have to compare. Tombstones cannot start an operation.
    #[doc(hidden)]
    pub const ADMISSION_CWDS_SQL: &'static str = "SELECT json_extract(record,'$.cwd') FROM runtime_agents \
         WHERE json_extract(record,'$.deletedAt') IS NULL \
         UNION SELECT json_extract(record,'$.cwd') FROM runtime_agents \
         WHERE json_type(record,'$.workspaceOperation')='text' \
         AND json_extract(record,'$.workspaceOperation')!=''";

    pub fn admission_cwds(connection: &Connection) -> StoreResult<Vec<Option<String>>> {
        Ok(connection
            .prepare_cached(Self::ADMISSION_CWDS_SQL)?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?)
    }

    #[doc(hidden)]
    pub fn global_slots_sql() -> String {
        format!(
            "SELECT COUNT(*) FROM runtime_agents WHERE id<>?1 \
             AND json_extract(record,'$.status') IN {CAPACITY_STATUSES} AND {CAPACITY_LIVE} AND {REMOTE_WORKER_KEY}=0"
        )
    }

    #[doc(hidden)]
    pub fn team_slots_sql() -> String {
        format!(
            "SELECT COUNT(*) FROM runtime_agents WHERE id<>?1 AND id<>?2 \
             AND json_extract(record,'$.rootId')=?2 \
             AND json_extract(record,'$.status') IN {CAPACITY_STATUSES} AND {CAPACITY_LIVE} AND {REMOTE_WORKER_KEY}=0"
        )
    }

    /// SQL counting the working local agents per account key: live
    /// slot-occupying agents that are not paused. A live paused agent keeps its
    /// slot but sends no provider request, so it must not skew where new workers
    /// are placed (intended; owner semantic). It enumerates the partial
    /// capacity index.
    #[doc(hidden)]
    pub fn account_slots_sql() -> String {
        format!(
            "SELECT COALESCE(json_extract(record,'$.accountKey'),'default'),COUNT(*) \
             FROM runtime_agents WHERE json_extract(record,'$.status') IN {CAPACITY_STATUSES} \
             AND {CAPACITY_LIVE} AND json_extract(record,'$.status')<>'paused' \
             AND {REMOTE_WORKER_KEY}=0 GROUP BY 1"
        )
    }

    /// Working (not paused) local agents per account key; idle accounts are absent.
    pub fn slots_by_account(
        connection: &Connection,
    ) -> StoreResult<std::collections::HashMap<String, i64>> {
        Ok(connection
            .prepare_cached(&Self::account_slots_sql())?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?)
    }

    /// Slot-occupying local agents other than `agent_id`, across all teams.
    pub fn global_slots_in_use(connection: &Connection, agent_id: &str) -> StoreResult<i64> {
        Ok(connection
            .prepare_cached(&Self::global_slots_sql())?
            .query_row([agent_id], |row| row.get(0))?)
    }

    /// Slot-occupying local agents of team `root_id` other than `agent_id` and the root.
    pub fn team_slots_in_use(
        connection: &Connection,
        agent_id: &str,
        root_id: &str,
    ) -> StoreResult<i64> {
        Ok(connection
            .prepare_cached(&Self::team_slots_sql())?
            .query_row(params![agent_id, root_id], |row| row.get(0))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// Runtime-agent indexes as the Python-owned schema defines them, read from
    /// the live-state fixture so the plan test cannot drift from production.
    fn database() -> Connection {
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/live-state-schema.json"))
                .expect("schema fixture parses");
        let connection = Connection::open_in_memory().expect("memory SQLite opens");
        connection
            .execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            .expect("agents table creates");
        for object in fixture["canvas.sqlite3"]["sqlite_master"]
            .as_array()
            .expect("fixture lists schema objects")
        {
            if object["table"] == "runtime_agents"
                && object["type"] == "index"
                && let Some(sql) = object["sql"].as_str()
            {
                connection
                    .execute_batch(sql)
                    .expect("fixture index creates");
            }
        }
        AgentIndexedReads::prepare_index(&connection).expect("capacity index creates");
        connection
    }

    fn insert(connection: &Connection, record: Value) {
        connection
            .execute(
                "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                params![record["id"].as_str().expect("id"), record.to_string()],
            )
            .expect("agent inserts");
    }

    fn plan(connection: &Connection, sql: &str, parameters: &[&str]) -> String {
        let mut statement = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("plan prepares");
        statement
            .query_map(rusqlite::params_from_iter(parameters), |row| {
                row.get::<_, String>(3)
            })
            .expect("plan runs")
            .collect::<Result<Vec<_>, _>>()
            .expect("plan rows")
            .join(" | ")
    }

    fn fixture() -> Connection {
        let connection = database();
        let agents = [
            json!({"id":"lead","rootId":"lead","status":"running","cwd":"/w"}),
            json!({"id":"a","rootId":"lead","status":"waiting","cwd":"/w"}),
            json!({"id":"paused-wake","rootId":"lead","status":"paused","autoWake":true}),
            json!({"id":"paused-off","rootId":"lead","status":"paused","autoWake":false}),
            json!({"id":"done","rootId":"lead","status":"completed"}),
            json!({"id":"failed","rootId":"lead","status":"failed"}),
            json!({"id":"gone","rootId":"lead","status":"running","deletedAt":5}),
            json!({"id":"tombstone","rootId":"lead","status":"paused","deletedAt":6}),
            json!({"id":"no-status","rootId":"lead"}),
            json!({"id":"remote","rootId":"lead","status":"running","remoteWorker":true}),
            json!({"id":"op","rootId":"other","status":"running","cwd":"/x","workspaceOperation":"branch"}),
            json!({"id":"op-empty","rootId":"other","status":"completed","cwd":"/y","workspaceOperation":""}),
            json!({"id":"other-idle","rootId":"other","status":"paused","cwd":"/z"}),
        ];
        for agent in agents {
            insert(&connection, agent);
        }
        connection
    }

    #[test]
    fn indexed_reads_equal_the_full_scan_semantics() {
        let connection = fixture();
        let ids =
            |rows: Vec<(String, String)>| rows.into_iter().map(|row| row.0).collect::<Vec<_>>();
        assert_eq!(
            ids(AgentIndexedReads::team_records(&connection, "lead").expect("team")),
            [
                "lead",
                "a",
                "paused-wake",
                "paused-off",
                "done",
                "failed",
                "gone",
                "tombstone",
                "no-status",
                "remote"
            ]
        );
        // lead, a, paused-wake, no-status, remote; tombstone and finished agents excluded.
        assert_eq!(
            AgentIndexedReads::active_team_members(&connection, "lead").expect("count"),
            5
        );
        assert_eq!(
            AgentIndexedReads::active_team_members(&connection, "other").expect("count"),
            2
        );
        assert_eq!(
            AgentIndexedReads::admission_cwds(&connection).expect("cwds"),
            [
                None,
                Some("/w".to_owned()),
                Some("/x".to_owned()),
                Some("/y".to_owned()),
                Some("/z".to_owned())
            ]
        );
        // Every live status-occupying local agent but the caller; tombstones never count.
        let old_global: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_agents WHERE id<>'a' AND json_extract(record,'$.status') \
                 IN ('starting','running','waiting','paused','approval') \
                 AND COALESCE(json_extract(record,'$.remoteWorker'),0)=0 \
                 AND json_extract(record,'$.deletedAt') IS NULL",
                [],
                |row| row.get(0),
            )
            .expect("reference count");
        assert_eq!(old_global, 5);
        assert_eq!(
            AgentIndexedReads::global_slots_in_use(&connection, "a").expect("global"),
            old_global
        );
        assert_eq!(
            AgentIndexedReads::team_slots_in_use(&connection, "a", "lead").expect("team"),
            2
        );
        insert(
            &connection,
            json!({"id":"on-b","rootId":"o","status":"running","accountKey":"b"}),
        );
        let by_account = AgentIndexedReads::slots_by_account(&connection).expect("by account");
        assert_eq!(by_account.len(), 2);
        // Live paused agents hold slots but are not counted as work; tombstones are not counted.
        assert_eq!(by_account["default"], 3);
        assert_eq!(by_account["b"], 1);
    }

    #[test]
    fn bootstrap_replaces_the_tombstone_counting_capacity_index() {
        let connection = fixture();
        connection
            .execute_batch(
                "DROP INDEX runtime_agent_capacity_slots_v2; \
                 CREATE INDEX runtime_agent_capacity_slots ON runtime_agents(id) \
                 WHERE json_extract(record,'$.status') IN ('starting','running','waiting','paused','approval')",
            )
            .expect("legacy index creates");
        AgentIndexedReads::prepare_index(&connection).expect("migration runs");
        let names: Vec<String> = connection
            .prepare(
                "SELECT name FROM sqlite_master WHERE name LIKE 'runtime_agent_capacity_slots%'",
            )
            .expect("catalog prepares")
            .query_map([], |row| row.get(0))
            .expect("catalog runs")
            .collect::<Result<_, _>>()
            .expect("catalog rows");
        assert_eq!(names, [CAPACITY_INDEX]);
    }

    #[test]
    fn queries_use_their_indexes_instead_of_scanning_the_table() {
        let connection = fixture();
        for (sql, parameters, index) in [
            (
                AgentIndexedReads::TEAM_RECORDS_SQL.to_owned(),
                vec!["lead"],
                "runtime_agent_root",
            ),
            (
                AgentIndexedReads::ACTIVE_TEAM_MEMBERS_SQL.to_owned(),
                vec!["lead"],
                "runtime_agent_live_root",
            ),
            (
                AgentIndexedReads::ADMISSION_CWDS_SQL.to_owned(),
                vec![],
                "runtime_agent_live_root",
            ),
            (
                AgentIndexedReads::WORKSPACE_OPERATION_IDS_SQL.to_owned(),
                vec![],
                "runtime_agent_reservation_cwd",
            ),
            (
                AgentIndexedReads::WORKSPACE_OPERATION_RECORDS_SQL.to_owned(),
                vec![],
                "runtime_agent_reservation_cwd",
            ),
            (
                AgentIndexedReads::global_slots_sql(),
                vec!["a"],
                CAPACITY_INDEX,
            ),
            (
                AgentIndexedReads::account_slots_sql(),
                vec![],
                CAPACITY_INDEX,
            ),
            (
                AgentIndexedReads::team_slots_sql(),
                vec!["a", "lead"],
                "runtime_agent_root",
            ),
        ] {
            let details = plan(&connection, &sql, &parameters);
            assert!(details.contains(index), "{sql}: {details}");
            // A scan is acceptable only through an index (the partial capacity index).
            assert!(
                details
                    .split(" | ")
                    .all(|step| !step.starts_with("SCAN") || step.contains("USING")),
                "{sql}: {details}"
            );
        }
    }
}
