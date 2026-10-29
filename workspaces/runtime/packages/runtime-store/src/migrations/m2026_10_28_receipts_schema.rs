//! Receipt destinations and current-owner query schema, before any family moves.
use crate::{StoreResult, receipt_migration_contract};
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    receipt_migration_contract::prepare_schema(tx)
        .map_err(|error| crate::StoreError::SchemaMismatch(error.to_string()))?;
    crate::schema::prepare_receipt_holds(tx)
}
