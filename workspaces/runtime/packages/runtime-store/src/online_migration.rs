//! Online migration of a large table in small batches, while the single writer
//! keeps serving commands. See `docs/online-migrations.md`.
//!
//! One batch is one write transaction. It ends when the batch time limit would
//! be exceeded, and the next batch starts after a pause that lets queued
//! command writes run first. The high-water mark lives in
//! `runtime_payload_migrations` (name `online:<NAME>`), so a restart resumes
//! from the last committed batch.
//!
//! Source rows that change during the migration reach the target through
//! triggers. They record the changed key in `runtime_online_migration_dirty`
//! when the key is at or below the mark (a higher key is copied later anyway).
//! Every batch first re-copies dirty keys, then copies new rows. The last batch
//! sees an empty dirty set and no rows above the mark in one transaction, drops
//! the triggers and records completion. The owner switches to the target after
//! that point and no earlier.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use studio_runtime_schema::persistence::MigrationStatus;

use crate::{RuntimeStore, StoreError, StoreResult};

/// Time limit of one batch transaction, in milliseconds. A batch stops before a
/// row whose estimated cost (the slowest row so far) would cross it. The first
/// row of a batch always runs, so one row slower than the limit still advances.
pub const BATCH_TIME_LIMIT_MS: u64 = 50;

/// Most rows read or re-copied by one batch, in rows.
pub const BATCH_MAX_ROWS: usize = 500;

/// Pause between two batches, in milliseconds. Command writes queue in the
/// writer during the pause and run before the next batch.
pub const BATCH_PAUSE_MS: u64 = 50;

const DIRTY_TABLE: &str = "runtime_online_migration_dirty";

/// A monotonic clock. Tests replace it to control batch timing.
pub trait Clock {
    fn now(&self) -> Duration;
}

/// The real monotonic clock.
pub struct SystemClock(Instant);

impl SystemClock {
    pub fn new() -> Self {
        Self(Instant::now())
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

/// One migration: the source rows, the transform and the target write.
/// Keys are positive integers (a rowid or an integer primary key).
pub trait OnlineMigration: Send + Sync + 'static {
    /// A source row after reading.
    type Row;
    /// The value written to the target.
    type Out;
    /// Unique name made of `[a-z0-9_]`. It names the progress row and triggers.
    const NAME: &'static str;
    /// Source table. Writes to it are tracked by triggers until completion.
    const SOURCE_TABLE: &'static str;
    /// Integer key column of the source table.
    const SOURCE_KEY: &'static str;

    /// Up to `limit` rows with a key above `after`, ordered by key.
    fn read_after(
        &self,
        db: &Connection,
        after: i64,
        limit: usize,
    ) -> StoreResult<Vec<(i64, Self::Row)>>;
    /// The current source row of `key`, or `None` when it is gone.
    fn read_one(&self, db: &Connection, key: i64) -> StoreResult<Option<Self::Row>>;
    /// Pure conversion of one row. No I/O.
    fn transform(&self, row: Self::Row) -> StoreResult<Self::Out>;
    /// Write the target row of `key`. Must be idempotent (upsert): a batch can
    /// run again after a crash or when a dirty key is copied twice.
    fn write(&self, db: &Connection, key: i64, out: Self::Out) -> StoreResult<()>;
    /// Delete the target row of `key` after its source row was deleted.
    fn remove(&self, db: &Connection, key: i64) -> StoreResult<()>;
}

/// Result of one batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {
    /// More work remains.
    Progress,
    /// The migration is complete and its triggers are gone.
    Complete,
}

fn progress_name<M: OnlineMigration>() -> String {
    format!("online:{}", M::NAME)
}

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

fn trigger_names<M: OnlineMigration>() -> [String; 3] {
    ["ins", "upd", "del"].map(|kind| format!("online_{}_{kind}", M::NAME))
}

/// Create the progress row and the change triggers. Safe to call at every start;
/// a completed migration stays complete and gets no triggers.
pub fn begin<M: OnlineMigration>(tx: &Transaction<'_>) -> StoreResult<()> {
    assert!(
        !M::NAME.is_empty()
            && M::NAME
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
        "online migration name must match [a-z0-9_]+"
    );
    let name = progress_name::<M>();
    tx.execute(
        "INSERT OR IGNORE INTO runtime_payload_migrations(name,cursor,complete,status,updated) \
         VALUES(?1,0,0,?2,?3)",
        params![name, MigrationStatus::Running.as_str(), now_seconds()],
    )?;
    let complete: bool = tx.query_row(
        "SELECT complete FROM runtime_payload_migrations WHERE name=?1",
        [&name],
        |row| row.get(0),
    )?;
    if complete {
        return Ok(());
    }
    tx.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {DIRTY_TABLE}(\
            migration TEXT NOT NULL, key INTEGER NOT NULL, PRIMARY KEY(migration,key)) WITHOUT ROWID"
    ))?;
    let [ins, upd, del] = trigger_names::<M>();
    let (table, key) = (M::SOURCE_TABLE, M::SOURCE_KEY);
    let mark = format!("(SELECT cursor FROM runtime_payload_migrations WHERE name='{name}')");
    let record = |row: &str| {
        format!(
            "INSERT OR IGNORE INTO {DIRTY_TABLE}(migration,key) \
             SELECT '{name}',{row}.{key} WHERE {row}.{key} <= {mark};"
        )
    };
    tx.execute_batch(&format!(
        "CREATE TRIGGER IF NOT EXISTS {ins} AFTER INSERT ON {table} BEGIN {} END;\
         CREATE TRIGGER IF NOT EXISTS {upd} AFTER UPDATE ON {table} BEGIN {} {} END;\
         CREATE TRIGGER IF NOT EXISTS {del} AFTER DELETE ON {table} BEGIN {} END;",
        record("NEW"),
        record("OLD"),
        record("NEW"),
        record("OLD"),
    ))?;
    Ok(())
}

/// Stops a batch before it would cross the time limit.
struct Budget<'a> {
    clock: &'a dyn Clock,
    started: Duration,
    slowest: Duration,
    rows: usize,
}

impl Budget<'_> {
    fn allows_next(&self) -> bool {
        let limit = Duration::from_millis(BATCH_TIME_LIMIT_MS);
        self.rows == 0 || self.clock.now() - self.started + self.slowest < limit
    }

    fn timed<T>(&mut self, work: impl FnOnce() -> StoreResult<T>) -> StoreResult<T> {
        let before = self.clock.now();
        let value = work()?;
        self.slowest = self.slowest.max(self.clock.now() - before);
        self.rows += 1;
        Ok(value)
    }
}

/// Run one batch inside the caller's write transaction. `begin` must have run.
pub fn step<M: OnlineMigration>(
    tx: &Transaction<'_>,
    migration: &M,
    clock: &dyn Clock,
) -> StoreResult<Step> {
    let name = progress_name::<M>();
    let mut budget = Budget {
        clock,
        started: clock.now(),
        slowest: Duration::ZERO,
        rows: 0,
    };

    // 1. Re-copy keys that changed at or below the mark.
    let dirty: Vec<i64> = tx
        .prepare_cached(&format!(
            "SELECT key FROM {DIRTY_TABLE} WHERE migration=?1 ORDER BY key LIMIT ?2"
        ))?
        .query_map(params![name, BATCH_MAX_ROWS as i64], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let mut dirty_left = dirty.len() == BATCH_MAX_ROWS;
    for key in &dirty {
        if !budget.allows_next() {
            dirty_left = true;
            break;
        }
        budget.timed(|| match migration.read_one(tx, *key)? {
            Some(row) => migration.write(tx, *key, migration.transform(row)?),
            None => migration.remove(tx, *key),
        })?;
        tx.execute(
            &format!("DELETE FROM {DIRTY_TABLE} WHERE migration=?1 AND key=?2"),
            params![name, key],
        )?;
    }

    // 2. Copy rows above the mark, unless dirty keys still wait.
    let mark: i64 = tx
        .query_row(
            "SELECT cursor FROM runtime_payload_migrations WHERE name=?1",
            [&name],
            |row| row.get(0),
        )
        .optional()?
        .ok_or(StoreError::RecordContract("online migration was not begun"))?;
    let mut new_mark = mark;
    let mut exhausted = false;
    if !dirty_left {
        let rows = migration.read_after(tx, mark, BATCH_MAX_ROWS)?;
        exhausted = rows.is_empty();
        for (key, row) in rows {
            if !budget.allows_next() {
                break;
            }
            budget.timed(|| migration.write(tx, key, migration.transform(row)?))?;
            new_mark = key;
        }
    }

    // 3. Record the mark, or completion when nothing was left to do.
    let done = exhausted && !dirty_left;
    if done {
        for trigger in trigger_names::<M>() {
            tx.execute_batch(&format!("DROP TRIGGER IF EXISTS {trigger}"))?;
        }
    }
    let status = if done {
        MigrationStatus::Complete
    } else {
        MigrationStatus::Running
    };
    tx.execute(
        "UPDATE runtime_payload_migrations SET cursor=?2,complete=?3,status=?4,updated=?5,error=NULL \
         WHERE name=?1",
        params![name, new_mark, i64::from(done), status.as_str(), now_seconds()],
    )?;
    Ok(if done { Step::Complete } else { Step::Progress })
}

/// Drive a migration to completion, one batch per write transaction, with
/// [`BATCH_PAUSE_MS`] between batches. Stop it by dropping the future or by
/// returning when `stop` reports true; run it again to resume from the mark.
/// A failed batch rolls back, is recorded in the progress row and is returned.
pub async fn run<M: OnlineMigration>(
    store: &RuntimeStore,
    migration: std::sync::Arc<M>,
    stop: impl Fn() -> bool + Send,
) -> StoreResult<Step> {
    store.transaction(begin::<M>).await?;
    while !stop() {
        let step_migration = std::sync::Arc::clone(&migration);
        let outcome = store
            .transaction(move |tx| step(tx, step_migration.as_ref(), &SystemClock::new()))
            .await;
        match outcome {
            Ok(Step::Complete) => return Ok(Step::Complete),
            Ok(Step::Progress) => {
                tokio::time::sleep(Duration::from_millis(BATCH_PAUSE_MS)).await;
            }
            Err(error) => {
                let message = error.to_string();
                let name = progress_name::<M>();
                // The original error is the one to return; a failed status write is only logged.
                if let Err(status_error) =
                    store
                        .transaction(move |tx| {
                            tx.execute(
                            "UPDATE runtime_payload_migrations SET status=?2,error=?3,updated=?4 \
                             WHERE name=?1",
                            params![name, MigrationStatus::Error.as_str(), message, now_seconds()],
                        )?;
                            Ok::<(), StoreError>(())
                        })
                        .await
                {
                    eprintln!("The migration failure was not recorded: {status_error}");
                }
                return Err(error);
            }
        }
    }
    Ok(Step::Progress)
}

/// Progress of one online migration, for the migration status endpoint.
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    pub name: String,
    /// High-water mark: the highest source key copied.
    pub cursor: i64,
    pub complete: bool,
    pub status: String,
    pub updated: Option<f64>,
    pub error: Option<String>,
    /// Changed keys waiting for a re-copy.
    pub dirty: i64,
}

/// Read the progress of every online migration.
pub fn progress(db: &Connection) -> rusqlite::Result<Vec<Progress>> {
    let dirty_table: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [DIRTY_TABLE],
        |row| row.get(0),
    )?;
    let dirty_sql = if dirty_table {
        format!("(SELECT COUNT(*) FROM {DIRTY_TABLE} d WHERE d.migration=m.name)")
    } else {
        "0".to_owned()
    };
    let mut statement = db.prepare(&format!(
        "SELECT substr(m.name,8),m.cursor,m.complete,m.status,m.updated,m.error,{dirty_sql} \
         FROM runtime_payload_migrations m WHERE m.name LIKE 'online:%' ORDER BY m.name"
    ))?;
    let rows = statement
        .query_map([], |row| {
            Ok(Progress {
                name: row.get(0)?,
                cursor: row.get(1)?,
                complete: row.get::<_, i64>(2)? != 0,
                status: row.get(3)?,
                updated: row.get(4)?,
                error: row.get(5)?,
                dirty: row.get(6)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// Copies `src(id, v)` to `dst(id, v2)` with v2 = v * 2. Each transform
    /// advances the fake clock by `cost_ms`; `CRASH_AT` makes one write fail.
    struct Plain {
        cost_ms: std::sync::atomic::AtomicU64,
    }

    thread_local! {
        static TEST_CLOCK: Cell<Duration> = const { Cell::new(Duration::ZERO) };
        static CRASH_AT: Cell<Option<i64>> = const { Cell::new(None) };
    }

    impl OnlineMigration for Plain {
        type Row = i64;
        type Out = i64;
        const NAME: &'static str = "double";
        const SOURCE_TABLE: &'static str = "src";
        const SOURCE_KEY: &'static str = "id";

        fn read_after(
            &self,
            db: &Connection,
            after: i64,
            limit: usize,
        ) -> StoreResult<Vec<(i64, i64)>> {
            Ok(db
                .prepare("SELECT id,v FROM src WHERE id>?1 ORDER BY id LIMIT ?2")?
                .query_map(params![after, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?)
        }
        fn read_one(&self, db: &Connection, key: i64) -> StoreResult<Option<i64>> {
            Ok(db
                .query_row("SELECT v FROM src WHERE id=?1", [key], |r| r.get(0))
                .optional()?)
        }
        fn transform(&self, row: i64) -> StoreResult<i64> {
            let cost = self.cost_ms.load(std::sync::atomic::Ordering::Relaxed);
            TEST_CLOCK.with(|c| c.set(c.get() + Duration::from_millis(cost)));
            Ok(row * 2)
        }
        fn write(&self, db: &Connection, key: i64, out: i64) -> StoreResult<()> {
            if CRASH_AT.with(Cell::get) == Some(key) {
                return Err(StoreError::RecordContract("simulated crash"));
            }
            db.execute(
                "INSERT OR REPLACE INTO dst(id,v2) VALUES(?1,?2)",
                params![key, out],
            )?;
            Ok(())
        }
        fn remove(&self, db: &Connection, key: i64) -> StoreResult<()> {
            db.execute("DELETE FROM dst WHERE id=?1", [key])?;
            Ok(())
        }
    }

    struct ThreadClock;
    impl Clock for ThreadClock {
        fn now(&self) -> Duration {
            TEST_CLOCK.with(Cell::get)
        }
    }

    fn database(rows: i64) -> Connection {
        let db = Connection::open_in_memory().expect("memory db");
        db.execute_batch(crate::feature_schema::PAYLOAD_MIGRATIONS_SCHEMA)
            .expect("progress table");
        db.execute_batch(
            "CREATE TABLE src(id INTEGER PRIMARY KEY, v INTEGER NOT NULL);
             CREATE TABLE dst(id INTEGER PRIMARY KEY, v2 INTEGER NOT NULL);",
        )
        .expect("tables");
        for id in 1..=rows {
            db.execute("INSERT INTO src VALUES(?1,?1)", [id])
                .expect("seed");
        }
        db
    }

    fn batch(db: &mut Connection, m: &Plain) -> StoreResult<Step> {
        let tx = db.transaction()?;
        let outcome = step(&tx, m, &ThreadClock)?;
        tx.commit()?;
        Ok(outcome)
    }

    fn begin_now(db: &mut Connection) {
        let tx = db.transaction().expect("tx");
        begin::<Plain>(&tx).expect("begin");
        tx.commit().expect("commit");
    }

    fn plain(cost_ms: u64) -> Plain {
        TEST_CLOCK.with(|c| c.set(Duration::ZERO));
        CRASH_AT.with(|c| c.set(None));
        Plain {
            cost_ms: cost_ms.into(),
        }
    }

    fn run_all(db: &mut Connection, m: &Plain) {
        for _ in 0..10_000 {
            if batch(db, m).expect("batch") == Step::Complete {
                return;
            }
        }
        panic!("migration did not finish");
    }

    fn dst_rows(db: &Connection) -> Vec<(i64, i64)> {
        db.prepare("SELECT id,v2 FROM dst ORDER BY id")
            .expect("prepare")
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
    }

    #[test]
    fn resumes_after_a_crash_in_the_middle_of_a_batch() {
        let mut db = database(1200);
        let m = plain(0);
        begin_now(&mut db);
        assert_eq!(batch(&mut db, &m).expect("first batch"), Step::Progress);
        let committed = progress(&db).expect("progress")[0].cursor;
        assert_eq!(committed, BATCH_MAX_ROWS as i64);
        // The next batch fails at row 700 after writing rows 501..699.
        CRASH_AT.with(|c| c.set(Some(700)));
        assert!(batch(&mut db, &m).is_err());
        // The failed batch rolled back completely.
        assert_eq!(progress(&db).expect("progress")[0].cursor, committed);
        assert_eq!(dst_rows(&db).len(), BATCH_MAX_ROWS);
        // Restart: a fresh `begin` keeps the mark, and the run finishes.
        CRASH_AT.with(|c| c.set(None));
        begin_now(&mut db);
        run_all(&mut db, &m);
        let rows = dst_rows(&db);
        assert_eq!(rows.len(), 1200);
        assert!(rows.iter().all(|(id, v2)| *v2 == id * 2));
        let done = &progress(&db).expect("progress")[0];
        assert!(done.complete);
        assert_eq!(done.status, "complete");
    }

    #[test]
    fn writes_during_the_migration_reach_the_target() {
        let mut db = database(1200);
        let m = plain(0);
        begin_now(&mut db);
        batch(&mut db, &m).expect("first batch");
        // Below the mark: update, delete. Above the mark: insert. Plus a new key.
        db.execute("UPDATE src SET v=1000 WHERE id=3", [])
            .expect("update");
        db.execute("DELETE FROM src WHERE id=4", [])
            .expect("delete");
        db.execute("UPDATE src SET v=7000 WHERE id=900", [])
            .expect("update above");
        db.execute("INSERT INTO src VALUES(5000,5000)", [])
            .expect("insert");
        assert_eq!(
            progress(&db).expect("progress")[0].dirty,
            2,
            "only keys at or below the mark"
        );
        run_all(&mut db, &m);
        let rows = dst_rows(&db);
        assert!(rows.contains(&(3, 2000)));
        assert!(!rows.iter().any(|(id, _)| *id == 4));
        assert!(rows.contains(&(900, 14000)));
        assert!(rows.contains(&(5000, 10000)));
        assert_eq!(rows.len(), 1200);
        let triggers: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(triggers, 0, "completion drops the triggers");
    }

    #[test]
    fn a_batch_stays_within_the_time_limit_with_a_slow_transform() {
        let mut db = database(100);
        let m = plain(20);
        begin_now(&mut db);
        for _ in 0..60 {
            let before = ThreadClock.now();
            let done = batch(&mut db, &m).expect("batch");
            let spent = ThreadClock.now() - before;
            assert!(
                spent <= Duration::from_millis(BATCH_TIME_LIMIT_MS),
                "batch took {spent:?}"
            );
            if done == Step::Complete {
                break;
            }
        }
        assert_eq!(dst_rows(&db).len(), 100);
    }

    #[test]
    fn a_row_slower_than_the_limit_still_advances() {
        let mut db = database(3);
        let m = plain(BATCH_TIME_LIMIT_MS * 2);
        begin_now(&mut db);
        batch(&mut db, &m).expect("batch");
        assert_eq!(progress(&db).expect("progress")[0].cursor, 1);
    }
}
