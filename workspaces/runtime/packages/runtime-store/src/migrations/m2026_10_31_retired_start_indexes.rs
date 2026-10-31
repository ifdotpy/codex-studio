//! Remove old index dependencies before retiring the agent start columns.
use crate::startup_recovery::DELETED_UNSETTLED_INDEX;
use crate::{StoreResult, repositories::agent_indexed_reads::ACTIVE_INDEX};
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    // Startup recreates the retained index names from their current owners.
    tx.execute_batch(&format!(
        "DROP INDEX IF EXISTS runtime_agent_account_wake_candidates_v5;
         DROP INDEX IF EXISTS runtime_agent_callback_context_v2;
         DROP INDEX IF EXISTS {DELETED_UNSETTLED_INDEX};
         DROP INDEX IF EXISTS {ACTIVE_INDEX};"
    ))?;
    Ok(())
}
