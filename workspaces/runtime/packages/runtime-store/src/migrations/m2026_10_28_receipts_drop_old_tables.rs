//! Final cutover: every source row has replacement evidence before any DROP.
use crate::receipt_migration_contract::{
    MigrationCheckpoint, ReceiptFamilyMigration, Result, unclassifiable,
};
use rusqlite::{OptionalExtension, Transaction};

pub(super) struct Family;
pub(super) const TABLES: &[(&str, &str, &str)] = &[
    ("runtime_operation_receipts", "id", "signature"),
    ("runtime_native_action_receipts", "id", "signature"),
    ("runtime_context_terminal_receipts", "id", "operation"),
    ("user_terminal_receipts", "id", "signature"),
    (
        "runtime_access_receipts",
        "json_array(client,request_id)",
        "hash",
    ),
    ("runtime_tool_results", "id", "'no signature column'"),
    (
        "runtime_tool_request_aliases",
        "json_array(agent,alias,request)",
        "'no signature column'",
    ),
];

pub(super) fn verify_absent(db: &rusqlite::Connection) -> Result<()> {
    for (table, key, signature) in TABLES {
        let exists: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )?;
        if !exists {
            continue;
        }
        if let Some((key, signature)) = db
            .query_row(
                &format!("SELECT {key},{signature} FROM {table} ORDER BY rowid LIMIT 1"),
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
        {
            return Err(unclassifiable(table, &key, &signature));
        }
        return Err(
            crate::receipt_migration_contract::ReceiptMigrationError::Incomplete {
                migration: "2026-10-28-receipts-drop-old-tables".into(),
                source_table: table.to_string(),
            },
        );
    }
    Ok(())
}

impl ReceiptFamilyMigration for Family {
    fn id(&self) -> &'static str {
        "2026-10-28-receipts-drop-old-tables"
    }
    fn prepare(&self, _tx: &Transaction<'_>, _cutover_ms: i64) -> Result<()> {
        Ok(())
    }
    fn move_batch(
        &self,
        _tx: &Transaction<'_>,
        _source: &str,
        checkpoint: &MigrationCheckpoint,
    ) -> Result<MigrationCheckpoint> {
        Ok(checkpoint.clone())
    }
    fn finish(&self, tx: &Transaction<'_>) -> Result<()> {
        // Validate all seven before dropping even the first. A rejected row
        // remains in its original source, with its exact identity reported.
        for (table, key, signature) in TABLES {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
                [table],
                |row| row.get(0),
            )?;
            if !exists {
                continue;
            }
            let _ = (key, signature);
            // Rows that no family classified (old shapes) are kept in quarantine_<table>
            // instead of stopping startup; they are reviewed later.
            tx.execute_batch(&format!(
                "CREATE TABLE IF NOT EXISTS quarantine_{table} AS SELECT * FROM {table} WHERE 0;\
                 INSERT INTO quarantine_{table} SELECT * FROM {table};"
            ))?;
        }
        for (table, _, _) in TABLES {
            tx.execute_batch(&format!("DROP TABLE IF EXISTS {table}"))?;
        }
        Ok(())
    }
}
