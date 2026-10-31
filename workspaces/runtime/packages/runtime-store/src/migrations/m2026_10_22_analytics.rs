//! Analytics projection retirement and metadata-only column transitions.
//! Remove after supported installs/restores no longer admit these source layouts.
use crate::StoreResult;
#[cfg(test)]
use crate::Transaction;
use rusqlite::{Connection, OptionalExtension};
use std::collections::HashSet;

pub(super) fn preflight(db: &Connection) -> StoreResult<()> {
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(crate::repositories::cost_aggregate::TABLES)?;
    for table in ["analytics_cost_rows", "analytics_cost_groups"] {
        let kind: Option<String> = db
            .query_row(
                "SELECT type FROM sqlite_master WHERE name=?1",
                [table],
                |row| row.get(0),
            )
            .optional()?;
        match kind.as_deref() {
            None => continue,
            Some("table") => {}
            Some(_) => {
                return Err(crate::StoreError::RecordContract(
                    "aggregate source is not a table",
                ));
            }
        }
        let names = |connection: &Connection| -> StoreResult<HashSet<String>> {
            Ok(connection
                .prepare("SELECT name FROM pragma_table_info(?1)")?
                .query_map([table], |row| row.get(0))?
                .collect::<Result<_, _>>()?)
        };
        let actual = names(db)?;
        for name in names(&expected)? {
            if !actual.contains(&name)
                && !matches!(
                    name.as_str(),
                    "eligible" | "final_evidence" | "cache_write_1h"
                )
            {
                return Err(crate::StoreError::RecordContract(
                    "unsupported aggregate source columns",
                ));
            }
        }
    }
    // A missing derived aggregate will read these original facts during backfill.
    if !crate::schema::table_exists(db, "analytics_cost_rows")? {
        let mut statement =
            db.prepare("SELECT record FROM analytics_usage WHERE root IS NOT NULL")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let value: serde_json::Value = serde_json::from_str(&row.get::<_, String>(0)?)?;
            studio_runtime_schema::usage::persisted::validate_legacy_analytics_cost_fields(&value)
                .map_err(|e| crate::StoreError::SchemaValidation(e.to_string()))?;
            if let Some(typed) = value.get("studioUsage") {
                serde_json::from_value::<studio_runtime_schema::usage::UsageObservation>(
                    typed.clone(),
                )?;
            }
        }
    }
    Ok(())
}

/// Retire the Python JSON projection after Rust took ownership of cost writes.
/// The projection table owns its three triggers and index; the six usage
/// triggers must go first because SQLite does not remove cross-table triggers.
/// IF EXISTS makes a retry after a committed open perform no schema changes.
#[cfg(test)]
pub(crate) fn migrate_2026_10_22_drop_python_cost_projection(
    db: &Transaction<'_>,
) -> StoreResult<()> {
    db.execute_batch(
        "DROP TRIGGER IF EXISTS analytics_cost_usage_before_insert_v1;
         DROP TRIGGER IF EXISTS analytics_cost_usage_before_update_v1;
         DROP TRIGGER IF EXISTS analytics_cost_usage_before_delete_v1;
         DROP TRIGGER IF EXISTS analytics_cost_usage_insert_v1;
         DROP TRIGGER IF EXISTS analytics_cost_usage_update_v1;
         DROP TRIGGER IF EXISTS analytics_cost_usage_delete_v1;
         DROP TABLE IF EXISTS analytics_cost_projection_v1;",
    )?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn aggregate_columns(db: &Connection) -> StoreResult<()> {
    if crate::schema::table_exists(db, "analytics_cost_rows")? {
        let columns = db
            .prepare("PRAGMA table_info(analytics_cost_rows)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<HashSet<_>, _>>()?;
        if !columns.contains("eligible") {
            db.execute_batch("ALTER TABLE analytics_cost_rows ADD COLUMN eligible INTEGER")?;
        }
        if !columns.contains("final_evidence") {
            db.execute_batch("ALTER TABLE analytics_cost_rows ADD COLUMN final_evidence INTEGER NOT NULL DEFAULT 0")?;
        }
        // Nullable columns are metadata-only additions: preserve every old fact/group.
        for table in ["analytics_cost_rows", "analytics_cost_groups"] {
            let has_split = db
                .prepare(&format!("PRAGMA table_info({table})"))?
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<HashSet<_>, _>>()?
                .contains("cache_write_1h");
            if !has_split {
                db.execute_batch(&format!(
                    "ALTER TABLE {table} ADD COLUMN cache_write_1h REAL"
                ))?;
            }
        }
    }
    Ok(())
}
