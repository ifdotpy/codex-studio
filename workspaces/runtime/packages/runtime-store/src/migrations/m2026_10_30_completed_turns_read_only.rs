//! The completed-turn table becomes read only (issue 630, slice 3).
//!
//! Every reader and writer uses the `turns` table. This step drops the mirror
//! trigger and makes the old table refuse writes. Its rows stay: no data is deleted.
use rusqlite::Transaction;

use crate::StoreResult;

pub(super) fn apply(transaction: &Transaction<'_>) -> StoreResult<()> {
    crate::domain::make_completed_turns_read_only(transaction)
}
