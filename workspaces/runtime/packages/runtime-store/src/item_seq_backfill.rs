//! Online numbering of the rows of `runtime_items` that predate its `seq` column.
//! The installer adds the column and the triggers (`durable_seq`); this migration gives
//! each old row its number in small batches, so the single writer keeps serving commands.

use rusqlite::{Connection, OptionalExtension};

use crate::StoreResult;
use crate::durable_seq::number_row;
use crate::online_migration::OnlineMigration;

pub struct ItemSeqBackfill;

impl OnlineMigration for ItemSeqBackfill {
    type Row = ();
    type Out = ();
    const NAME: &'static str = "runtime_items_seq";
    const SOURCE_TABLE: &'static str = "runtime_items";
    const SOURCE_KEY: &'static str = "rowid";

    fn read_after(&self, db: &Connection, after: i64, limit: usize) -> StoreResult<Vec<(i64, ())>> {
        Ok(db
            .prepare_cached(
                "SELECT rowid FROM runtime_items WHERE rowid>?1 ORDER BY rowid LIMIT ?2",
            )?
            .query_map(rusqlite::params![after, limit as i64], |row| {
                Ok((row.get(0)?, ()))
            })?
            .collect::<Result<_, _>>()?)
    }

    fn read_one(&self, db: &Connection, key: i64) -> StoreResult<Option<()>> {
        Ok(db
            .query_row("SELECT 1 FROM runtime_items WHERE rowid=?1", [key], |_| {
                Ok(())
            })
            .optional()?)
    }

    fn transform(&self, row: ()) -> StoreResult<()> {
        Ok(row)
    }

    /// A row that already has a number is left alone, so a re-copy of a changed key does
    /// not touch the source table again.
    fn write(&self, db: &Connection, key: i64, _: ()) -> StoreResult<()> {
        number_row(db, Self::SOURCE_TABLE, key)
    }

    fn remove(&self, _: &Connection, _: i64) -> StoreResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::online_migration::{Step, SystemClock, begin, step};

    #[test]
    fn old_items_get_increasing_entries_and_numbered_ones_keep_theirs() {
        let mut db = crate::fresh_state::test_canvas();
        // Items written before the source existed have no entry.
        db.execute_batch(
            "DROP TRIGGER durable_runtime_items_insert;
             INSERT INTO runtime_items(id,agent,record,created) VALUES('a','x','{}',1),('b','x','{}',2);",
        )
        .expect("old rows");
        let seqs = |db: &Connection| -> Vec<i64> {
            db.prepare("SELECT seq FROM durable_rows WHERE tbl='runtime_items' ORDER BY seq")
                .expect("query")
                .query_map([], |row| row.get(0))
                .expect("rows")
                .collect::<Result<_, _>>()
                .expect("seqs")
        };
        assert!(seqs(&db).is_empty());
        let tx = db.transaction().expect("transaction");
        begin::<ItemSeqBackfill>(&tx).expect("begin");
        while step(&tx, &ItemSeqBackfill, &SystemClock::new()).expect("step") == Step::Progress {}
        tx.commit().expect("commit");
        let numbered = seqs(&db);
        assert!(
            numbered.len() == 2 && numbered[0] > 0 && numbered[1] > numbered[0],
            "{numbered:?}"
        );
        // A repeat changes nothing.
        number_row(&db, "runtime_items", 1).expect("repeat");
        assert_eq!(seqs(&db), numbered);
    }
}
