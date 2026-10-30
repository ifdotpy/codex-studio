//! Create `runtime_item_blobs`, which points an item at a blob that holds its
//! full text. This migration only creates the table and moves nothing, so it
//! does no file I/O in its transaction. A background pass in `runtime-services`
//! writes a blob for every `runtime_item_fulltext` row. The table keeps its
//! rows; a later migration may drop it once
//! [`crate::text_blobs::fulltext_without_blob`] is empty.
use crate::StoreResult;
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    // Creates `runtime_item_blobs` in a state that predates it.
    crate::feature_schema::prepare(tx)
}
