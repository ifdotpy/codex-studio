//! Indexed agent fields as typed columns of `runtime_agents`.
//!
//! Storage format introduced on 2026-10-20; the dated migrations apply the explicit-provider cutover of 2026-10-26. Every agent
//! field that an index or an indexed query reads is a STORED generated column
//! of `runtime_agents`, computed by SQLite from the record when the row is
//! written. Indexes are plain column indexes, and queries filter and sort on
//! the columns. The record stays the one source of truth: a column cannot
//! disagree with its record, and every writer (including raw test fixtures)
//! keeps writing `(id, record)` only.
//!
//! Before this format, about twenty JSON expression indexes re-evaluated
//! their `json_extract` keys and predicates for the old and the new record on
//! every agent write. A generated column is evaluated once, for the new record;
//! the old key comes from the stored row.
//!
//! [`AGENT_COLUMNS`] owns the column list: name, SQLite type, value and
//! constraint. A column holds exactly what its value expression yields:
//! `record->>'$.path'` is `json_extract(record,'$.path')` (JSON true and false
//! read as 1 and 0, JSON null and an absent field as SQL NULL). A column
//! declared without a type keeps the JSON value's own SQL type, for fields
//! whose stored JSON type is not fixed. There are no CHECK constraints: the
//! record schema owns field types and values (`AgentStatus::Other` keeps a
//! status of a newer build), and readers refuse an invalid record by name; a
//! CHECK would instead make such a row unwritable. Malformed JSON is refused
//! at write.

use std::sync::LazyLock;

use rusqlite::{Connection, Transaction};

use crate::StoreResult;

/// The agents table.
pub const AGENT_TABLE: &str = "runtime_agents";

/// One generated column of [`AGENT_TABLE`].
pub struct AgentColumn {
    /// Column name, used verbatim in SQL.
    pub name: &'static str,
    /// Declared SQLite type (affinity); empty keeps the value's own type.
    pub sql_type: &'static str,
    /// SQL expression over `record` that computes the column.
    pub value: &'static str,
    /// Column constraint (`NOT NULL`), or empty.
    pub constraint: &'static str,
}

const fn column(
    name: &'static str,
    sql_type: &'static str,
    value: &'static str,
    constraint: &'static str,
) -> AgentColumn {
    AgentColumn {
        name,
        sql_type,
        value,
        constraint,
    }
}

/// SQL truth test of the record field at JSON path `$path` with the
/// truthiness of the agent writers: absent, null, false, 0, "", [] and {} are
/// false. The value of `agent_archived` and `review_archive_failed`.
macro_rules! truthy {
    ($path:literal) => {
        concat!(
            "NOT (json_type(record,'",
            $path,
            "') IS NULL",
            " OR json_type(record,'",
            $path,
            "') IN ('null','false')",
            " OR (json_type(record,'",
            $path,
            "') IN ('integer','real') AND record->>'",
            $path,
            "'=0)",
            " OR (json_type(record,'",
            $path,
            "')='text' AND record->>'",
            $path,
            "'='')",
            " OR (json_type(record,'",
            $path,
            "')='array' AND json_array_length(record,'",
            $path,
            "')=0)",
            " OR (json_type(record,'",
            $path,
            "')='object' AND record->>'",
            $path,
            "'='{}'))"
        )
    };
}

/// The explicit provider column is also the Canvas cutover evidence for 2026-10-26.
const PROVIDER_COLUMN: AgentColumn =
    column("provider", "TEXT", "record->>'$.provider'", "NOT NULL");

/// The indexed agent fields; see the module documentation. Changing this list
/// changes the table and needs a new dated migration.
pub const AGENT_COLUMNS: [AgentColumn; 34] = [
    column("root_id", "TEXT", "record->>'$.rootId'", ""),
    column("parent_id", "TEXT", "record->>'$.parentId'", ""),
    column("status", "TEXT", "record->>'$.status'", ""),
    column(
        "account_key",
        "TEXT",
        "COALESCE(record->>'$.accountKey','default')",
        "NOT NULL",
    ),
    PROVIDER_COLUMN,
    column("role", "TEXT", "record->>'$.role'", ""),
    column("deleted_at", "", "record->>'$.deletedAt'", ""),
    column("turn_id", "TEXT", "record->>'$.turnId'", ""),
    column("cwd", "TEXT", "record->>'$.cwd'", ""),
    column(
        "workspace_operation",
        "",
        "record->>'$.workspaceOperation'",
        "",
    ),
    column("in_flight", "INTEGER", "record->>'$.inFlight'", ""),
    column("remote_worker", "", "record->>'$.remoteWorker'", ""),
    column("remote_anchor", "", "record->>'$.remoteAnchor'", ""),
    column(
        "native_failure_hold",
        "",
        "record->>'$.nativeFailureHold'",
        "",
    ),
    column(
        "steer_rejected_turn_id",
        "TEXT",
        "record->>'$.steerRejectedTurnId'",
        "",
    ),
    column(
        "native_tool_refresh_id",
        "TEXT",
        "record->>'$.nativeToolRefreshId'",
        "",
    ),
    column(
        "context_repair_phase",
        "TEXT",
        "record->>'$.contextRepair.phase'",
        "",
    ),
    column(
        "source_cleanup_phase",
        "TEXT",
        "record->>'$.contextRepair.sourceCleanup.phase'",
        "",
    ),
    column(
        "source_cleanup_account_key",
        "TEXT",
        "record->>'$.contextRepair.sourceCleanup.accountKey'",
        "",
    ),
    column(
        "source_cleanup_thread_id",
        "TEXT",
        "record->>'$.contextRepair.sourceCleanup.threadId'",
        "",
    ),
    column(
        "has_disconnect_recovery",
        "INTEGER",
        "COALESCE(json_type(record,'$.disconnectRecovery')='object',0)",
        "NOT NULL",
    ),
    column(
        "disconnect_recovery_stage",
        "TEXT",
        "record->>'$.disconnectRecovery.stage'",
        "",
    ),
    column(
        "has_restart_recovery",
        "INTEGER",
        "COALESCE(json_type(record,'$.restartRecovery')='object',0)",
        "NOT NULL",
    ),
    column(
        "restart_recovery_stage",
        "TEXT",
        "record->>'$.restartRecovery.stage'",
        "",
    ),
    column(
        "has_workspace_cleanup_intent",
        "INTEGER",
        "COALESCE(json_type(record,'$.workspaceCleanupIntent')='object',0)",
        "NOT NULL",
    ),
    column(
        "agent_archived",
        "INTEGER",
        truthy!("$.agentArchive"),
        "NOT NULL",
    ),
    column(
        "review_archive_failed",
        "INTEGER",
        truthy!("$.reviewArchiveError"),
        "NOT NULL",
    ),
    column(
        "review_archive_marked",
        "INTEGER",
        "record->>'$.reviewArchiveScheduled' IS NOT NULL",
        "NOT NULL",
    ),
    column(
        "review_archive_scheduled",
        "INTEGER",
        "COALESCE(json_type(record,'$.reviewArchiveScheduled')='text' \
         AND record->>'$.reviewArchiveScheduled'<>'',0)",
        "NOT NULL",
    ),
    column(
        "review_archive_next_at",
        "",
        "record->>'$.reviewArchiveNextAt'",
        "",
    ),
    // Episode presence, deadline in Unix seconds, and durable exclusion identity.
    column(
        "idle_episode_type",
        "TEXT",
        "json_type(record,'$.idleCompactionEpisode')",
        "",
    ),
    column(
        "idle_episode_fire_at",
        "",
        "record->>'$.idleCompactionEpisode.fireAt'",
        "",
    ),
    column(
        "idle_episode_id",
        "",
        "record->>'$.idleCompactionEpisode.id'",
        "",
    ),
    // A legacy image workspace that startup has not yet finished.
    column(
        "legacy_unsettled",
        "INTEGER",
        "COALESCE(record->>'$.workspaceMode'='image' OR record->>'$.imageWorkspace'=1,0) \
         AND COALESCE(record->>'$.legacyWorkspaceSettled',0)=0",
        "NOT NULL",
    ),
];

/// The `CREATE TABLE` statement of [`AGENT_TABLE`] named `name`. The columns
/// precede `record`: a row's large record spills to overflow pages, and a
/// column stored after it could be read only by walking that chain.
fn column_sql(column: &AgentColumn) -> String {
    format!(
        "\"{}\" {} GENERATED ALWAYS AS ({}) STORED {}",
        column.name, column.sql_type, column.value, column.constraint
    )
}

pub(crate) fn has_explicit_provider_column(connection: &Connection) -> StoreResult<bool> {
    Ok(connection.query_row(
        "SELECT instr(sql,?1)>0 FROM sqlite_master WHERE type='table' AND name='runtime_agents'",
        [column_sql(&PROVIDER_COLUMN)],
        |row| row.get(0),
    )?)
}

pub(crate) fn table_sql(name: &str) -> String {
    let columns = AGENT_COLUMNS
        .iter()
        .map(column_sql)
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "CREATE TABLE IF NOT EXISTS \"{name}\"(id TEXT PRIMARY KEY,{columns},record TEXT NOT NULL,{})",
        crate::schema::receipt_owner_columns()
            .into_iter()
            .filter(|(table, _, _)| *table == AGENT_TABLE)
            .map(|(_, name, definition)| format!("{name} {definition}"))
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// The table as a fresh schema-only test fixture declares it.
pub fn agent_table_sql() -> &'static str {
    static SQL: LazyLock<String> = LazyLock::new(|| table_sql(AGENT_TABLE));
    &SQL
}

/// Whether `table` has exactly the columns of this format, in order.
pub(crate) fn has_columns(transaction: &Transaction<'_>) -> StoreResult<bool> {
    let mut statement = transaction.prepare(&format!("PRAGMA table_xinfo({AGENT_TABLE})"))?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    let expected = std::iter::once("id")
        .chain(AGENT_COLUMNS.iter().map(|column| column.name))
        .chain(std::iter::once("record"));
    let receipt_names: Vec<_> = crate::schema::receipt_owner_columns()
        .into_iter()
        .filter(|(table, _, _)| *table == AGENT_TABLE)
        .map(|(_, name, _)| name)
        .collect();
    Ok(names
        .iter()
        .map(String::as_str)
        .filter(|name| !receipt_names.contains(name))
        .eq(expected))
}

/// `batch` with its `CREATE TABLE runtime_agents(...)` statement replaced by
/// this format's table, for test fixtures that declare their own schema.
#[doc(hidden)]
pub fn fixture_sql(batch: &str) -> String {
    let Some(start) = batch.find("CREATE TABLE runtime_agents") else {
        return batch.to_owned();
    };
    let end = batch[start..]
        .find(';')
        .map_or(batch.len(), |offset| start + offset);
    format!("{}{}{}", &batch[..start], agent_table_sql(), &batch[end..])
}

/// `SELECT` of the declared (not generated) columns of `table`, for test
/// oracles that compare whole rows with states written before 2026-10-20.
#[doc(hidden)]
pub fn select_declared_sql(connection: &rusqlite::Connection, table: &str) -> StoreResult<String> {
    let quoted = table.replace('"', "\"\"");
    let columns = connection
        .prepare(&format!("PRAGMA table_info(\"{quoted}\")"))?
        .query_map([], |row| row.get::<_, String>(1))?
        .map(|name| name.map(|name| format!("\"{}\"", name.replace('"', "\"\""))))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(format!("SELECT {} FROM \"{quoted}\"", columns.join(",")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RuntimeStore;
    use rusqlite::Connection;
    use serde_json::{Value, json};

    fn agents() -> Vec<Value> {
        vec![
            json!({"compactions":0,"id":"lead","rootId":"lead","epoch":2,"status":"running","accountKey":"acct",
                "provider":"claude","threadId":"t1","turnId":"u1","inFlight":true,"autoWake":true,
                "cwd":"/w","workspaceOperation":"branch","startAttempt":{"submitted":true,
                "connectionGeneration":4,"activeAtReservation":false},"prompt":"keep"}),
            json!({"compactions":0,"id":"child","provider":"codex","rootId":"lead","parentId":"lead","epoch":0,"status":"completed","accountKey":"default",
                "role":"reviewer","reviewArchiveScheduled":"",
                "restartRecovery":{"stage":"held"},"deletedAt":5.5}),
            json!({"compactions":0,"id":"bare","provider":"codex","rootId":"bare","epoch":0,"status":"idle","accountKey":"default"}),
        ]
    }

    fn rows(connection: &Connection) -> Vec<(i64, String, String)> {
        connection
            .prepare("SELECT rowid,id,record FROM runtime_agents ORDER BY rowid")
            .expect("rows prepare")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("rows read")
            .collect::<Result<_, _>>()
            .expect("rows decode")
    }

    fn catalog(connection: &Connection, kind: &str) -> Vec<String> {
        connection
            .prepare(
                "SELECT name FROM sqlite_master WHERE tbl_name='runtime_agents' AND type=?1 \
                 AND sql IS NOT NULL ORDER BY name",
            )
            .expect("catalog prepares")
            .query_map([kind], |row| row.get(0))
            .expect("catalog reads")
            .collect::<Result<_, _>>()
            .expect("catalog decodes")
    }

    fn journal(connection: &Connection) -> Vec<String> {
        [
            "runtime_agent_record_generation",
            "runtime_scheduler_agent_change_state",
            "runtime_scheduler_agent_changes",
            "runtime_transcript_revisions",
        ]
        .iter()
        .flat_map(|table| {
            let mut statement = connection
                .prepare(&format!("SELECT * FROM {table} ORDER BY 1"))
                .expect("journal table");
            let count = statement.column_count();
            statement
                .query_map([], |row| {
                    (0..count)
                        .map(|index| row.get::<_, rusqlite::types::Value>(index))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .expect("journal rows")
                .map(|row| format!("{table}: {:?}", row.expect("journal row")))
                .collect::<Vec<_>>()
        })
        .collect()
    }

    #[test]
    fn unused_agent_columns_are_not_computed_on_write() {
        let connection = Connection::open_in_memory().expect("memory SQLite opens");
        connection
            .execute_batch(agent_table_sql())
            .expect("table creates");
        for column in [
            "native_reset_pending",
            "capacity_retry_id",
            "capacity_retry_status",
            "capacity_retry_due_at",
            "start_attempt_action",
        ] {
            let exists: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_xinfo('runtime_agents') WHERE name=?1)",
                [column], |row| row.get(0),
            ).expect("schema reads");
            assert!(
                !exists,
                "unused generated column {column} must not be computed"
            );
        }
    }

    #[tokio::test]
    async fn retired_start_columns_migrate_with_all_legacy_dependent_indexes() {
        let root = tempfile::tempdir().expect("state directory");
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("store opens");
        store.transaction(|tx| {
            for agent in agents() {
                tx.execute("INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                    rusqlite::params![agent["id"].as_str(), agent.to_string()])?;
            }
            let triggers = tx.prepare("SELECT sql FROM sqlite_master WHERE type='trigger' AND tbl_name='runtime_agents'")?
                .query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            let old_table = table_sql("runtime_agents_legacy_start_fixture").replace(
                "record TEXT NOT NULL",
                "auto_wake AS (record->>'$.autoWake') STORED,\
                 thread_id AS (record->>'$.threadId') STORED,\
                 start_attempt_submitted AS (record->>'$.startAttempt.submitted') STORED,\
                 start_attempt_connection_generation AS (record->>'$.startAttempt.connectionGeneration') STORED,\
                 start_attempt_active_at_reservation AS (record->>'$.startAttempt.activeAtReservation') STORED,record TEXT NOT NULL",
            );
            tx.execute_batch(&old_table)?;
            tx.execute_batch(
                "INSERT INTO runtime_agents_legacy_start_fixture(rowid,id,record) SELECT rowid,id,record FROM runtime_agents;\
                 ALTER TABLE runtime_agents RENAME TO runtime_agents_before_start_fixture;\
                 DROP TABLE runtime_agents_before_start_fixture;\
                 ALTER TABLE runtime_agents_legacy_start_fixture RENAME TO runtime_agents;\
                 CREATE INDEX runtime_agent_account_wake_candidates_v5 ON runtime_agents(account_key,id) WHERE start_attempt_submitted=1 AND start_attempt_connection_generation IS NOT NULL;\
                 CREATE INDEX runtime_agent_callback_context_v2 ON runtime_agents(account_key,start_attempt_connection_generation,thread_id,turn_id) WHERE status='running' AND auto_wake=1;\
                 CREATE INDEX runtime_agent_deleted_unsettled_v2 ON runtime_agents(id) WHERE COALESCE(deleted_at,0)!=0 AND COALESCE(start_attempt_active_at_reservation,0)!=0;\
                 CREATE INDEX runtime_agent_active ON runtime_agents(root_id) WHERE in_flight=1 OR start_attempt_active_at_reservation=1;",
            )?;
            for sql in triggers { tx.execute_batch(&sql)?; }
            Ok(())
        }).await.expect("legacy start columns and indexes");
        let before = {
            let db = store.test_reader().expect("reader");
            (rows(&db), journal(&db), catalog(&db, "trigger"))
        };
        drop(store);
        let mut previous_indexes = None;
        for _ in 0..2 {
            let store = RuntimeStore::open(Some(root.path()))
                .await
                .expect("legacy state migrates");
            let db = store.test_reader().expect("reader");
            assert_eq!(rows(&db), before.0);
            assert_eq!(journal(&db), before.1);
            assert_eq!(catalog(&db, "trigger"), before.2);
            let dependent: i64 = db.query_row("SELECT count(*) FROM sqlite_master WHERE type='index' AND tbl_name='runtime_agents' AND sql LIKE '%start_attempt_%'", [], |row| row.get(0)).expect("index definitions");
            assert_eq!(dependent, 0);
            let indexes = catalog(&db, "index");
            assert!(
                indexes.contains(
                    &crate::SchedulerRepository::account_wake_index_definition()
                        .name
                        .to_owned()
                )
            );
            for definition in crate::StartupRecoveryRepository::index_definitions()
                .into_iter()
                .filter(|definition| definition.table == AGENT_TABLE)
            {
                let sql: String = db
                    .query_row(
                        "SELECT sql FROM sqlite_master WHERE type='index' AND name=?1",
                        [definition.name],
                        |row| row.get(0),
                    )
                    .expect("current startup index exists");
                assert_eq!(sql, definition.create_sql.replace("IF NOT EXISTS ", ""));
            }
            if let Some(previous) = &previous_indexes {
                assert_eq!(&indexes, previous);
            }
            previous_indexes = Some(indexes);
        }
    }

    /// A record write updates the columns; malformed JSON is refused.
    #[test]
    fn columns_follow_the_record_and_refuse_malformed_json() {
        let connection = Connection::open_in_memory().expect("memory SQLite opens");
        connection
            .execute_batch(agent_table_sql())
            .expect("table creates");
        connection
            .execute(
                "INSERT INTO runtime_agents(id,record) VALUES('a',?1)",
                [agents()[2].to_string()],
            )
            .expect("agent inserts");
        connection
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.status','queued') WHERE id='a'",
                [],
            )
            .expect("agent updates");
        let status: String = connection
            .query_row("SELECT status FROM runtime_agents", [], |row| row.get(0))
            .expect("columns");
        assert_eq!(status, "queued");
        connection
            .execute(
                "INSERT INTO runtime_agents(id,record) VALUES('b','{bad json')",
                [],
            )
            .expect_err("malformed JSON is refused");
    }
}
