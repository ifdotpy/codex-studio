//! Chat, Turn and Session tables (issue 630, slice 1).
//!
//! One transaction: create the tables, the one-unfinished-turn index and the
//! generated check triggers; copy `runtime_completed_turns` as Turn `Completed`;
//! project every agent record. The live canvas holds about 3.4k agents and 97k
//! completed turns, so one transaction is short. The completed-turn table stays.
use rusqlite::Transaction;

use crate::StoreResult;

pub(super) fn apply(transaction: &Transaction<'_>) -> StoreResult<()> {
    crate::domain::install(transaction)?;
    crate::domain::backfill(transaction)
}
