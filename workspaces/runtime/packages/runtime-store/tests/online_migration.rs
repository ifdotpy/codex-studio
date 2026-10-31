//! The driver stops, resumes from the stored mark and completes, while command
//! writes keep running between its batches.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rusqlite::{Connection, OptionalExtension, params};
use studio_runtime_store::online_migration::{self, OnlineMigration, Step};
use studio_runtime_store::{RuntimeStore, StoreError, StoreResult};

struct Copy;

impl OnlineMigration for Copy {
    type Row = i64;
    type Out = i64;
    const NAME: &'static str = "copy_test";
    const SOURCE_TABLE: &'static str = "online_test_src";
    const SOURCE_KEY: &'static str = "id";

    fn read_after(
        &self,
        db: &Connection,
        after: i64,
        limit: usize,
    ) -> StoreResult<Vec<(i64, i64)>> {
        Ok(db
            .prepare("SELECT id,v FROM online_test_src WHERE id>?1 ORDER BY id LIMIT ?2")?
            .query_map(params![after, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?)
    }
    fn read_one(&self, db: &Connection, key: i64) -> StoreResult<Option<i64>> {
        Ok(db
            .query_row("SELECT v FROM online_test_src WHERE id=?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }
    fn transform(&self, row: i64) -> StoreResult<i64> {
        Ok(row + 1)
    }
    fn write(&self, db: &Connection, key: i64, out: i64) -> StoreResult<()> {
        db.execute(
            "INSERT OR REPLACE INTO online_test_dst VALUES(?1,?2)",
            params![key, out],
        )?;
        Ok(())
    }
    fn remove(&self, db: &Connection, key: i64) -> StoreResult<()> {
        db.execute("DELETE FROM online_test_dst WHERE id=?1", [key])?;
        Ok(())
    }
}

#[tokio::test]
async fn driver_stops_resumes_and_reports_progress() {
    let state = tempfile::tempdir().expect("state");
    let store = RuntimeStore::open(Some(state.path())).await.expect("store");
    store
        .transaction(|tx| {
            tx.execute_batch(
                "CREATE TABLE online_test_src(id INTEGER PRIMARY KEY, v INTEGER NOT NULL);
                 CREATE TABLE online_test_dst(id INTEGER PRIMARY KEY, v INTEGER NOT NULL);
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<1500)
                 INSERT INTO online_test_src SELECT i,i FROM n;",
            )?;
            Ok(())
        })
        .await
        .expect("seed");

    // Stop after the first batch: the second `stop` poll returns true.
    let polls = AtomicUsize::new(0);
    let first = online_migration::run(&store, Arc::new(Copy), || {
        polls.fetch_add(1, Ordering::SeqCst) >= 1
    })
    .await
    .expect("first run");
    assert_eq!(first, Step::Progress);
    let progress = store
        .read_async("test", |db| {
            Ok::<_, StoreError>(online_migration::progress(db)?)
        })
        .await
        .expect("progress");
    assert_eq!(progress.len(), 1);
    assert_eq!(progress[0].name, "copy_test");
    assert!(!progress[0].complete && progress[0].cursor > 0);

    // A command write goes through between batches.
    store
        .transaction(|tx| {
            tx.execute("UPDATE online_test_src SET v=-5 WHERE id=1", [])?;
            Ok(())
        })
        .await
        .expect("command write");

    let second = online_migration::run(&store, Arc::new(Copy), || false)
        .await
        .expect("second run");
    assert_eq!(second, Step::Complete);
    let (count, first_value) = store
        .read_async("test", |db| {
            Ok::<_, StoreError>((
                db.query_row("SELECT COUNT(*) FROM online_test_dst", [], |r| {
                    r.get::<_, i64>(0)
                })?,
                db.query_row("SELECT v FROM online_test_dst WHERE id=1", [], |r| {
                    r.get::<_, i64>(0)
                })?,
            ))
        })
        .await
        .expect("target");
    assert_eq!((count, first_value), (1500, -4));
    let progress = store
        .read_async("test", |db| {
            Ok::<_, StoreError>(online_migration::progress(db)?)
        })
        .await
        .expect("progress");
    assert!(progress[0].complete && progress[0].dirty == 0);
}
