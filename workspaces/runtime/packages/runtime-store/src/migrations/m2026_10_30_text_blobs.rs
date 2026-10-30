//! Move the full text of cut items from `runtime_item_fulltext` to blobs.
//!
//! The table keeps its rows and is read-only from now on. A later migration
//! may drop it once [`crate::text_blobs::fulltext_without_blob`] is empty; this
//! one fails, and so rolls back, when any row lacks its blob after the move.
use crate::text_blobs;
use crate::{StoreError, StoreResult};
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    // Creates `runtime_item_blobs` in a state that predates it.
    crate::feature_schema::prepare(tx)?;
    text_blobs::move_fulltext(tx)?;
    match text_blobs::fulltext_without_blob(tx)?.first() {
        None => Ok(()),
        Some(id) => Err(StoreError::InvalidStoredRecord {
            object: "runtime_item_fulltext",
            id: id.clone(),
        }),
    }
}
