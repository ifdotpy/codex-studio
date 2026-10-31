//! Remove old index dependencies before retiring the agent start columns.
use crate::startup_recovery::DELETED_UNSETTLED_INDEX;
use crate::{StoreResult, repositories::agent_indexed_reads::ACTIVE_INDEX};
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    let columns = serde_json::to_string(&super::m2026_10_30_unused_agent_columns::RETIRED)?;
    let has_retired_columns: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_xinfo('runtime_agents') WHERE name IN (SELECT value FROM json_each(?1)))",
        [columns], |row| row.get(0),
    )?;
    if !has_retired_columns {
        return Ok(());
    }
    // Startup recreates the retained index names from their current owners.
    tx.execute_batch(&format!(
        "DROP INDEX IF EXISTS runtime_agent_account_wake_candidates_v5;
         DROP INDEX IF EXISTS runtime_agent_callback_context_v2;
         DROP INDEX IF EXISTS {DELETED_UNSETTLED_INDEX};
         DROP INDEX IF EXISTS {ACTIVE_INDEX};"
    ))?;
    Ok(())
}
