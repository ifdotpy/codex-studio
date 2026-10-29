//! Agent column and volatile format transitions, introduced 2026-10-20 and 2026-10-21.
//! Remove after supported installations and restores require these formats.
use crate::{
    StoreResult,
    agent_volatile::{AGENT_TABLE, AGENT_VOLATILE_FIELDS, AGENT_VOLATILE_TABLE, columns},
};
use rusqlite::Transaction;

pub(super) fn preflight(db: &rusqlite::Connection) -> StoreResult<()> {
    let names = db
        .prepare("SELECT name FROM pragma_table_xinfo('runtime_agents')")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for name in names {
        if name != "id"
            && name != "record"
            && name != "start_attempt_capacity_retry_id"
            && !crate::agent_columns::AGENT_COLUMNS
                .iter()
                .any(|column| column.name == name)
            && !crate::schema::receipt_owner_columns()
                .iter()
                .any(|(table, column, _)| *table == "runtime_agents" && *column == name)
        {
            return Err(crate::StoreError::RecordContract(
                "unsupported agent source column",
            ));
        }
    }
    if let Some(finding) = crate::bootstrap::inspect_sync_tables(db)?
        .into_iter()
        .next()
    {
        return Err(crate::StoreError::SchemaObjectMismatch {
            database: "canvas.sqlite3".into(),
            object: finding.object,
            reason: finding.reason,
        });
    }
    if crate::schema::table_exists(db, AGENT_VOLATILE_TABLE)? {
        let columns = db
            .prepare("SELECT name FROM pragma_table_xinfo(?1)")?
            .query_map([AGENT_VOLATILE_TABLE], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if columns
            .iter()
            .map(String::as_str)
            .ne(std::iter::once("id").chain(AGENT_VOLATILE_FIELDS))
        {
            return Err(crate::StoreError::RecordContract(
                "unsupported volatile source columns",
            ));
        }
    }
    Ok(())
}

/// Bring `runtime_agents` to this format; the first startup step, before any
/// index on the columns is created. `RuntimeStore::open` calls it.
pub fn prepare_columns(transaction: &Transaction<'_>) -> StoreResult<()> {
    if crate::schema::table_exists(transaction, AGENT_TABLE)? {
        migrate_2026_10_21_capacity_retry_column(transaction)?;
        // 2026-10-26: identity leaves were migrated before rebuilding strict columns.
        if !crate::agent_columns::has_columns(transaction)?
            || !crate::agent_columns::has_explicit_provider_column(transaction)?
        {
            rebuild_columns(transaction)?;
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

/// Shared rebuild for the 2026-10-20 columns and 2026-10-26 explicit provider: rebuild `runtime_agents` with the generated
/// columns of [`crate::agent_columns::AGENT_COLUMNS`]. Rows keep their rowid, id and record; the
/// table's triggers are recreated unchanged. Its JSON expression indexes are
/// dropped with the old table; their owners create the column indexes that
/// replace them later in the same startup transaction. No trigger fires, so
/// no change journal entry, transcript revision or sync projection follows.
/// It runs in the caller's startup transaction, so a crash leaves either the
/// old table or the complete new one. Keep this helper while either dated
/// startup upgrade still supports old states or restorable backups.
fn rebuild_columns(transaction: &Transaction<'_>) -> StoreResult<()> {
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
        crate::agent_columns::table_sql(STAGING)
    ))?;
    for trigger in triggers {
        transaction.execute_batch(&trigger)?;
    }
    Ok(())
}

/// Tables that `runtime_agents` triggers advance on every record change. The
/// 2026-10-20 move changes no agent's content, so it restores them: no change
/// journal entry, transcript revision or sync projection follows from it.
pub(crate) const MIGRATION_PRESERVED_TABLES: [&str; 4] = [
    "runtime_agent_record_generation",
    "runtime_scheduler_agent_change_state",
    "runtime_scheduler_agent_changes",
    "runtime_transcript_revisions",
];

/// Data migration of 2026-10-20: move the volatile fields of every valid
/// agent record into [`AGENT_VOLATILE_TABLE`] and remove them from the record.
/// It runs once, in the transaction that creates the table, so a crash leaves
/// either the old format or the complete new one; a test oracle that writes
/// old-format records into a migrated state runs it again. Delete it once
/// every state has opened with a build of 2026-10-20 or later.
pub fn migrate_2026_10_20(transaction: &Transaction<'_>) -> StoreResult<()> {
    let present = AGENT_VOLATILE_FIELDS
        .iter()
        .map(|field| format!("json_type(record,'$.{field}') IS NOT NULL"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let values = AGENT_VOLATILE_FIELDS
        .iter()
        .map(|field| format!("record->'$.{field}'"))
        .collect::<Vec<_>>()
        .join(",");
    let paths = AGENT_VOLATILE_FIELDS
        .iter()
        .map(|field| format!("'$.{field}'"))
        .collect::<Vec<_>>()
        .join(",");
    let moved = format!("json_valid(record) AND json_type(record)='object' AND ({present})");
    let mut preserved = Vec::new();
    for table in MIGRATION_PRESERVED_TABLES {
        if crate::schema::table_exists(transaction, table)? {
            transaction.execute_batch(&format!(
                "CREATE TEMP TABLE migration_2026_10_20_{table} AS SELECT * FROM main.{table};"
            ))?;
            preserved.push(table);
        }
    }
    // A field still in a record is newer than its volatile column: only a
    // writer without this format (a test oracle) puts it there.
    let newer = AGENT_VOLATILE_FIELDS
        .iter()
        .map(|field| format!("\"{field}\"=COALESCE(excluded.\"{field}\",\"{field}\")"))
        .collect::<Vec<_>>()
        .join(",");
    transaction.execute_batch(&format!(
        "INSERT INTO {AGENT_VOLATILE_TABLE}(id,{}) SELECT id,{values} FROM runtime_agents WHERE {moved}
         ON CONFLICT(id) DO UPDATE SET {newer};
         UPDATE runtime_agents SET record=json_remove(record,{paths}) WHERE {moved};",
        columns("")
    ))?;
    for table in preserved {
        transaction.execute_batch(&format!(
            "DELETE FROM main.{table};
             INSERT INTO main.{table} SELECT * FROM temp.migration_2026_10_20_{table};
             DROP TABLE temp.migration_2026_10_20_{table};"
        ))?;
    }
    Ok(())
}
