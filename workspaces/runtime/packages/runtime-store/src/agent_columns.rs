//! Indexed agent fields as typed columns of `runtime_agents`.
//!
//! Storage format of 2026-10-20 ([`migrate_2026_10_20_columns`]). Every agent
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

use rusqlite::Transaction;

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

/// The indexed agent fields; see the module documentation. Changing this list
/// changes the table: it needs a new dated migration that rebuilds the table.
pub const AGENT_COLUMNS: [AgentColumn; 41] = [
    column("root_id", "TEXT", "record->>'$.rootId'", ""),
    column("parent_id", "TEXT", "record->>'$.parentId'", ""),
    column("status", "TEXT", "record->>'$.status'", ""),
    column(
        "account_key",
        "TEXT",
        "COALESCE(record->>'$.accountKey','default')",
        "NOT NULL",
    ),
    column(
        "provider",
        "TEXT",
        "COALESCE(record->>'$.provider','codex')",
        "NOT NULL",
    ),
    column("role", "TEXT", "record->>'$.role'", ""),
    column("deleted_at", "", "record->>'$.deletedAt'", ""),
    column("thread_id", "TEXT", "record->>'$.threadId'", ""),
    column("turn_id", "TEXT", "record->>'$.turnId'", ""),
    column("cwd", "TEXT", "record->>'$.cwd'", ""),
    column(
        "workspace_operation",
        "",
        "record->>'$.workspaceOperation'",
        "",
    ),
    column("in_flight", "INTEGER", "record->>'$.inFlight'", ""),
    column("auto_wake", "INTEGER", "record->>'$.autoWake'", ""),
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
    column("execution_mode", "TEXT", "record->>'$.executionMode'", ""),
    column(
        "native_tool_refresh_id",
        "TEXT",
        "record->>'$.nativeToolRefreshId'",
        "",
    ),
    column(
        "native_reset_pending",
        "",
        "record->>'$.nativeRelease.resetPending'",
        "",
    ),
    column(
        "start_attempt_action",
        "TEXT",
        "record->>'$.startAttempt.action'",
        "",
    ),
    column(
        "start_attempt_submitted",
        "",
        "record->>'$.startAttempt.submitted'",
        "",
    ),
    column(
        "start_attempt_connection_generation",
        "",
        "record->>'$.startAttempt.connectionGeneration'",
        "",
    ),
    column(
        "start_attempt_active_at_reservation",
        "",
        "record->>'$.startAttempt.activeAtReservation'",
        "",
    ),
    column(
        "capacity_retry_id",
        "TEXT",
        "record->>'$.capacityRetry.id'",
        "",
    ),
    column(
        "capacity_retry_status",
        "TEXT",
        "record->>'$.capacityRetry.status'",
        "",
    ),
    column(
        "capacity_retry_due_at",
        "",
        "record->>'$.capacityRetry.dueAt'",
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
];

/// The `CREATE TABLE` statement of [`AGENT_TABLE`] named `name`. The columns
/// precede `record`: a row's large record spills to overflow pages, and a
/// column stored after it could be read only by walking that chain.
fn table_sql(name: &str) -> String {
    let columns = AGENT_COLUMNS
        .iter()
        .map(|column| {
            format!(
                "\"{}\" {} GENERATED ALWAYS AS ({}) STORED {}",
                column.name, column.sql_type, column.value, column.constraint
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "CREATE TABLE IF NOT EXISTS \"{name}\"(id TEXT PRIMARY KEY,{columns},record TEXT NOT NULL)"
    )
}

/// The table as a fresh schema-only test fixture declares it.
pub fn agent_table_sql() -> &'static str {
    static SQL: LazyLock<String> = LazyLock::new(|| table_sql(AGENT_TABLE));
    &SQL
}

/// Whether `table` has exactly the columns of this format, in order.
fn has_columns(transaction: &Transaction<'_>) -> StoreResult<bool> {
    let mut statement = transaction.prepare(&format!("PRAGMA table_xinfo({AGENT_TABLE})"))?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    let expected = std::iter::once("id")
        .chain(AGENT_COLUMNS.iter().map(|column| column.name))
        .chain(std::iter::once("record"));
    Ok(names.iter().map(String::as_str).eq(expected))
}

/// Bring `runtime_agents` to this format; the first startup step, before any
/// index on the columns is created. `RuntimeStore::open` calls it.
pub fn prepare(transaction: &Transaction<'_>) -> StoreResult<()> {
    if crate::schema::table_exists(transaction, AGENT_TABLE)? {
        migrate_2026_10_21_capacity_retry_column(transaction)?;
        if !has_columns(transaction)? {
            migrate_2026_10_20_columns(transaction)?;
        }
    }
    Ok(())
}

/// Remove the obsolete capacity-start column and its dependent v4 wake index.
/// Startup installs the replacement index later in the same transaction. Records,
/// rowids and triggers stay unchanged; reopening after commit is a no-op. Keep
/// this migration until every state has opened with a build of 2026-10-21 or later.
fn migrate_2026_10_21_capacity_retry_column(transaction: &Transaction<'_>) -> StoreResult<()> {
    let has_legacy_column: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_xinfo('runtime_agents') \
         WHERE name='start_attempt_capacity_retry_id')",
        [],
        |row| row.get(0),
    )?;
    if has_legacy_column {
        transaction.execute_batch(
            "DROP INDEX IF EXISTS runtime_agent_account_wake_candidates_v4;
             ALTER TABLE runtime_agents DROP COLUMN start_attempt_capacity_retry_id;",
        )?;
    }
    Ok(())
}

/// Data migration of 2026-10-20: rebuild `runtime_agents` with the generated
/// columns of [`AGENT_COLUMNS`]. Rows keep their rowid, id and record; the
/// table's triggers are recreated unchanged. Its JSON expression indexes are
/// dropped with the old table; their owners create the column indexes that
/// replace them later in the same startup transaction. No trigger fires, so
/// no change journal entry, transcript revision or sync projection follows.
/// It runs in the caller's startup transaction, so a crash leaves either the
/// old table or the complete new one. Delete it once every state has opened
/// with a build of 2026-10-20 or later.
pub fn migrate_2026_10_20_columns(transaction: &Transaction<'_>) -> StoreResult<()> {
    const STAGING: &str = "runtime_agents_2026_10_20";
    const RETIRED: &str = "runtime_agents_before_2026_10_20";
    let triggers = transaction
        .prepare(&format!(
            "SELECT sql FROM sqlite_master WHERE type='trigger' AND tbl_name='{AGENT_TABLE}' \
             AND sql IS NOT NULL ORDER BY rowid"
        ))?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    // The old table is renamed away before it is dropped: the store's
    // authorizer ignores deletes from `runtime_agents` (to keep row-level
    // change hooks), which would make `DROP TABLE runtime_agents` a no-op.
    // Legacy renames neither rewrite nor re-check other schema objects, so a
    // state whose other objects startup has yet to repair still migrates; no
    // object outside this table names it, and its triggers are recreated.
    transaction.execute_batch(&format!(
        "DROP TABLE IF EXISTS \"{STAGING}\";
         DROP TABLE IF EXISTS \"{RETIRED}\";
         {};
         INSERT INTO \"{STAGING}\"(rowid,id,record) SELECT rowid,id,record FROM {AGENT_TABLE} ORDER BY rowid;
         PRAGMA legacy_alter_table=ON;
         ALTER TABLE {AGENT_TABLE} RENAME TO \"{RETIRED}\";
         ALTER TABLE \"{STAGING}\" RENAME TO {AGENT_TABLE};
         PRAGMA legacy_alter_table=OFF;
         DROP TABLE \"{RETIRED}\";",
        table_sql(STAGING)
    ))?;
    for trigger in triggers {
        transaction.execute_batch(&trigger)?;
    }
    Ok(())
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
    use crate::{AgentId, AgentRepository, RuntimeStore, system_reads};
    use rusqlite::Connection;
    use serde_json::{Value, json};

    fn agents() -> Vec<Value> {
        vec![
            json!({"id":"lead","rootId":"lead","epoch":2,"status":"running","accountKey":"acct",
                "provider":"claude","threadId":"t1","turnId":"u1","inFlight":true,"autoWake":true,
                "cwd":"/w","workspaceOperation":"branch","startAttempt":{"submitted":true,
                "connectionGeneration":4,"activeAtReservation":false},"prompt":"keep"}),
            json!({"id":"child","rootId":"lead","parentId":"lead","epoch":0,"status":"completed",
                "role":"reviewer","reviewArchiveScheduled":"",
                "restartRecovery":{"stage":"held"},"deletedAt":5.5}),
            json!({"id":"bare","rootId":"bare","epoch":0,"status":"idle"}),
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

    /// A pre-2026-10-20 state (record-only table with JSON expression
    /// indexes) migrates once at open: rows keep rowid, id and record, the
    /// triggers survive, the JSON indexes are gone, readers see the same
    /// agents, nothing is journaled, and a second open changes nothing.
    #[tokio::test]
    async fn migration_2026_10_20_columns_round_trips_an_old_state_once() {
        let root = tempfile::tempdir().expect("state directory");
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("store opens");
        let triggers = catalog(&store.test_reader().expect("reader"), "trigger");
        store
            .transaction(|tx| {
                // Back to the old format: a record-only table, its triggers and
                // a JSON expression index as the earlier builds created them.
                let trigger_sql = tx
                    .prepare(
                        "SELECT sql FROM sqlite_master WHERE type='trigger' \
                         AND tbl_name='runtime_agents'",
                    )?
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                tx.execute_batch(
                    "ALTER TABLE runtime_agents RENAME TO runtime_agents_new;
                     DROP TABLE runtime_agents_new;
                     CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
                     CREATE INDEX runtime_agent_root ON runtime_agents(json_extract(record,'$.rootId'));",
                )?;
                for agent in agents() {
                    tx.execute(
                        "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                        rusqlite::params![agent["id"].as_str(), agent.to_string()],
                    )?;
                }
                tx.execute("DELETE FROM runtime_agents WHERE id='child'", [])?;
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES('child',?1)",
                    [agents()[1].to_string()],
                )?;
                for sql in trigger_sql {
                    tx.execute_batch(&sql)?;
                }
                Ok(())
            })
            .await
            .expect("old-format state");
        let (rows_before, journal_before) = {
            let reader = store.test_reader().expect("reader");
            (rows(&reader), journal(&reader))
        };
        assert_eq!(
            rows_before.last().map(|row| row.0),
            Some(4),
            "rowid gap kept"
        );
        drop(store);

        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("store migrates");
        let reader = store.test_reader().expect("reader");
        assert_eq!(rows(&reader), rows_before, "rowid, id and record are kept");
        assert_eq!(
            journal(&reader),
            journal_before,
            "the rebuild journals nothing"
        );
        assert_eq!(catalog(&reader, "trigger"), triggers, "triggers survive");
        let indexes = catalog(&reader, "index");
        assert!(
            !indexes.contains(&"runtime_agent_root".to_owned()),
            "{indexes:?}"
        );
        let json_indexes: i64 = reader
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' \
                 AND tbl_name='runtime_agents' AND sql LIKE '%json%'",
                [],
                |row| row.get(0),
            )
            .expect("index catalog");
        assert_eq!(
            json_indexes, 0,
            "no index reads the record JSON: {indexes:?}"
        );
        for agent in agents() {
            let id = agent["id"].as_str().expect("id");
            let typed = AgentRepository::get(&reader, &AgentId::from(id))
                .expect("typed read")
                .expect("agent exists");
            assert_eq!(typed.value(), agent);
            assert_eq!(
                system_reads::get(&reader, "runtime_agents", id).expect("checked read"),
                Some(agent.clone())
            );
        }
        let lead: (String, String, i64, i64, i64, String) = reader
            .query_row(
                "SELECT account_key,provider,in_flight,auto_wake,start_attempt_connection_generation,cwd \
                 FROM runtime_agents WHERE id='lead'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            )
            .expect("lead columns");
        assert_eq!(lead, ("acct".into(), "claude".into(), 1, 1, 4, "/w".into()));
        let child: (String, f64, i64, i64, i64, i64) = reader
            .query_row(
                "SELECT parent_id,deleted_at,agent_archived,review_archive_marked,\
                 review_archive_scheduled,has_restart_recovery FROM runtime_agents WHERE id='child'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            )
            .expect("child columns");
        assert_eq!(child, ("lead".into(), 5.5, 0, 1, 0, 1));
        let bare: (String, String, Option<String>) = reader
            .query_row(
                "SELECT account_key,provider,parent_id FROM runtime_agents WHERE id='bare'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("bare columns");
        assert_eq!(bare, ("default".into(), "codex".into(), None));
        let schema = |connection: &Connection| -> Vec<String> {
            connection
                .prepare(
                    "SELECT type||' '||name||' '||COALESCE(sql,'') FROM sqlite_master ORDER BY 1",
                )
                .expect("schema prepares")
                .query_map([], |row| row.get(0))
                .expect("schema reads")
                .collect::<Result<_, _>>()
                .expect("schema decodes")
        };
        let schema_after = schema(&reader);
        drop(reader);
        drop(store);

        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("store reopens");
        let reader = store.test_reader().expect("reader");
        assert_eq!(
            schema(&reader),
            schema_after,
            "a second open changes nothing"
        );
        assert_eq!(rows(&reader), rows_before);
    }

    #[tokio::test]
    async fn migration_2026_10_21_removes_capacity_column_and_replaces_wake_index_once() {
        let root = tempfile::tempdir().expect("state directory");
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("store opens");
        store
            .transaction(|tx| {
                for agent in agents() {
                    tx.execute(
                        "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                        rusqlite::params![agent["id"].as_str(), agent.to_string()],
                    )?;
                }
                let triggers = tx
                    .prepare(
                        "SELECT sql FROM sqlite_master \
                         WHERE type='trigger' AND tbl_name='runtime_agents'",
                    )?
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                // Recreate the previous generated-column format, including the
                // dependent partial index that must be dropped before its column.
                let old_table = table_sql("runtime_agents_capacity_fixture").replace(
                    "record TEXT NOT NULL",
                    "start_attempt_capacity_retry_id TEXT GENERATED ALWAYS AS \
                 (record->>'$.startAttempt.capacityRetryId') STORED,record TEXT NOT NULL",
                );
                tx.execute_batch(&old_table)?;
                tx.execute_batch(
                    "INSERT INTO runtime_agents_capacity_fixture(rowid,id,record)
                   SELECT rowid,id,record FROM runtime_agents;
                 ALTER TABLE runtime_agents RENAME TO runtime_agents_before_capacity_fixture;
                 DROP TABLE runtime_agents_before_capacity_fixture;
                 ALTER TABLE runtime_agents_capacity_fixture RENAME TO runtime_agents;
                 CREATE INDEX runtime_agent_account_wake_candidates_v4
                   ON runtime_agents(account_key,id)
                   WHERE start_attempt_action='capacity' AND start_attempt_submitted=0
                     AND capacity_retry_id=start_attempt_capacity_retry_id AND status='starting';",
                )?;
                for trigger in triggers {
                    tx.execute_batch(&trigger)?;
                }
                Ok(())
            })
            .await
            .expect("previous format installs");
        let before = {
            let reader = store.test_reader().expect("reader");
            (rows(&reader), journal(&reader), catalog(&reader, "trigger"))
        };
        drop(store);
        let mut indexes_after = None;
        for _ in 0..2 {
            let store = RuntimeStore::open(Some(root.path()))
                .await
                .expect("old state opens");
            let reader = store.test_reader().expect("reader");
            assert_eq!(rows(&reader), before.0);
            assert_eq!(journal(&reader), before.1, "migration journals nothing");
            assert_eq!(catalog(&reader, "trigger"), before.2);
            let column_count: i64 = reader
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_xinfo('runtime_agents') \
                 WHERE name='start_attempt_capacity_retry_id'",
                    [],
                    |row| row.get(0),
                )
                .expect("column catalog");
            assert_eq!(column_count, 0);
            let indexes = catalog(&reader, "index");
            assert!(
                !indexes
                    .iter()
                    .any(|name| name == "runtime_agent_account_wake_candidates_v4")
            );
            assert!(
                indexes.contains(
                    &crate::SchedulerRepository::account_wake_index_definition()
                        .name
                        .to_owned()
                )
            );
            if let Some(previous) = &indexes_after {
                assert_eq!(&indexes, previous, "reopening changes no indexes");
            }
            indexes_after = Some(indexes);
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
                "UPDATE runtime_agents SET record=json_set(record,'$.status','queued','$.autoWake',json('true')) WHERE id='a'",
                [],
            )
            .expect("agent updates");
        let columns: (String, i64) = connection
            .query_row("SELECT status,auto_wake FROM runtime_agents", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .expect("columns");
        assert_eq!(columns, ("queued".into(), 1));
        connection
            .execute(
                "INSERT INTO runtime_agents(id,record) VALUES('b','{bad json')",
                [],
            )
            .expect_err("malformed JSON is refused");
    }
}
