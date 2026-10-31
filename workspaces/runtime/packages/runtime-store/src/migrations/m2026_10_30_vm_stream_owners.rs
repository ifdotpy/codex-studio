//! Index the VM owners whose stream lanes are persisted on each journal batch.
use crate::StoreResult;
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    crate::schema::prepare_receipt_holds(tx)?;
    tx.execute_batch("CREATE INDEX IF NOT EXISTS runtime_agent_vm_stream_owner ON runtime_agents(account_key,id) WHERE execution_mode='vm' AND hold_status='clear'")?;
    Ok(())
}
