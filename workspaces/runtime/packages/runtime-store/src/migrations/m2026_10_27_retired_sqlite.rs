use rusqlite::Transaction;

use crate::StoreResult;

pub(super) fn canvas(transaction: &Transaction<'_>) -> StoreResult<()> {
    transaction.execute_batch(
        "DROP TRIGGER IF EXISTS runtime_turn_item_link_insert_v1;
         DROP TRIGGER IF EXISTS runtime_turn_item_link_update_v1;
         DROP TRIGGER IF EXISTS runtime_turn_item_link_delete_v1;
         DROP TRIGGER IF EXISTS runtime_payload_checkpoints_insert;
         DROP TRIGGER IF EXISTS runtime_payload_checkpoints_update;
         DROP TRIGGER IF EXISTS runtime_payload_checkpoints_delete;
         DROP TRIGGER IF EXISTS runtime_payload_requests_insert;
         DROP TRIGGER IF EXISTS runtime_payload_requests_update;
         DROP TRIGGER IF EXISTS runtime_payload_requests_delete;
         DROP TRIGGER IF EXISTS runtime_payload_results_insert;
         DROP TRIGGER IF EXISTS runtime_payload_results_update;
         DROP TRIGGER IF EXISTS runtime_payload_results_delete;
         DROP TABLE IF EXISTS runtime_turn_item_links_v1;
         DROP TABLE IF EXISTS runtime_turn_item_link_state_v1;
         DROP TABLE IF EXISTS runtime_budget_usage;
         DROP TABLE IF EXISTS runtime_model_modes;
         DROP TABLE IF EXISTS runtime_native_catalog_updates;
         DROP TABLE IF EXISTS runtime_native_limit_refreshes;
         DROP TABLE IF EXISTS runtime_native_sweeps;
         DROP TABLE IF EXISTS runtime_supervisor_cursor;
         DROP TABLE IF EXISTS runtime_server_sequence;
         DROP TABLE IF EXISTS runtime_payload_refs;
         DROP TABLE IF EXISTS runtime_search;
         DROP TABLE IF EXISTS runtime_search_indexed;
         DROP TABLE IF EXISTS runtime_search_rows;
         DROP TABLE IF EXISTS runtime_search_rows_rollout;",
    )?;
    Ok(())
}

pub(super) fn analytics(transaction: &Transaction<'_>) -> StoreResult<()> {
    transaction.execute_batch(
        "DROP TRIGGER IF EXISTS analytics_cost_usage_before_insert_v1;
         DROP TRIGGER IF EXISTS analytics_cost_usage_before_update_v1;
         DROP TRIGGER IF EXISTS analytics_cost_usage_before_delete_v1;
         DROP TRIGGER IF EXISTS analytics_cost_usage_insert_v1;
         DROP TRIGGER IF EXISTS analytics_cost_usage_update_v1;
         DROP TRIGGER IF EXISTS analytics_cost_usage_delete_v1;
         DROP TABLE IF EXISTS analytics_cost_changes_v1;
         DROP TABLE IF EXISTS analytics_cost_roots_v1;
         DROP TABLE IF EXISTS analytics_cost_state_v1;
         DROP TABLE IF EXISTS analytics_cost_projection_v1;",
    )?;
    Ok(())
}
