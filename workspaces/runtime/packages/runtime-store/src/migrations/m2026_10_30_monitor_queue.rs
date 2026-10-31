//! Materialize whether a monitor has an undelivered stream event.
use crate::StoreResult;
use rusqlite::Transaction;

pub(crate) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if !crate::schema::table_exists(tx, "runtime_monitors")? {
        return Ok(());
    }
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_xinfo('runtime_monitors') WHERE name='stream_pending')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        tx.execute_batch("ALTER TABLE runtime_monitors ADD COLUMN stream_pending INTEGER NOT NULL DEFAULT 0;
            UPDATE runtime_monitors SET stream_pending=CASE WHEN json_valid(record) THEN COALESCE(json_array_length(record,'$.streamQueue')>0,0) ELSE 0 END;
            DROP INDEX IF EXISTS runtime_monitor_queued_stream;")?;
    }
    Ok(())
}
