//! Add bounded scheduler lookups without changing stored records.
use crate::StoreResult;
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    tx.execute_batch("CREATE INDEX IF NOT EXISTS runtime_accepted_archive_due ON runtime_operation_receipts(COALESCE(json_extract(result,'$.nextAttemptAt'),0),id) WHERE signature='accepted-archive-v1' AND json_extract(result,'$.status') IN ('pending','retryable','unknown');")?;
    Ok(())
}
