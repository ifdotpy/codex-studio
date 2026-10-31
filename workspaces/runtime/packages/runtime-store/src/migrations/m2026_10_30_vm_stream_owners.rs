//! Hold-status columns for the VM owners whose stream lanes are persisted on each journal batch.
//! The owner index is `chats_vm` (`domain`).
use crate::StoreResult;
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    crate::schema::prepare_receipt_holds(tx)?;
    Ok(())
}
