//! The one SQLite durability policy of Studio's Rust databases
//! (docs/decisions/2026-10-24-sqlite-normal-with-periodic-flush.md).
//!
//! Every writer connection uses WAL with `synchronous=NORMAL`: a commit is not
//! synced to disk, so it survives a process crash but an OS crash or power
//! loss may roll it back. A checkpoint syncs the WAL and the database file, so
//! each long-lived database is flushed at most once per
//! [`WAL_FLUSH_INTERVAL`] while it has unflushed commits, and once more when
//! its owner stops gracefully. The interval and the WAL size limit are owned by
//! `studio_runtime_schema::shared_values`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Weak};
use std::time::Duration;

use rusqlite::Connection;
use studio_runtime_schema::shared_values::{
    SQLITE_WAL_FLUSH_INTERVAL_SECONDS, SQLITE_WAL_JOURNAL_SIZE_LIMIT_BYTES,
};

/// Longest wait of a committed change for its flush, and the shortest time
/// between two periodic flushes of one database.
pub const WAL_FLUSH_INTERVAL: Duration = Duration::from_secs(SQLITE_WAL_FLUSH_INTERVAL_SECONDS);

/// The value `PRAGMA synchronous` reports for NORMAL.
pub const SYNCHRONOUS_NORMAL: i64 = 1;

/// Apply the shared writer pragmas: WAL, `synchronous=NORMAL`, and the WAL size
/// limit. `synchronous` is connection-local, so every writer connection calls this.
pub fn configure_writer(connection: &Connection) -> rusqlite::Result<()> {
    connection.pragma_update(None, "journal_mode", "WAL")?;
    configure_synchronous(connection)
}

/// Apply the connection-local pragmas to a writer of a database already in WAL mode.
pub fn configure_synchronous(connection: &Connection) -> rusqlite::Result<()> {
    connection.pragma_update(None, "synchronous", SYNCHRONOUS_NORMAL)?;
    connection.pragma_update(
        None,
        "journal_size_limit",
        SQLITE_WAL_JOURNAL_SIZE_LIMIT_BYTES,
    )
}

/// How a checkpoint treats readers and the WAL file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckpointMode {
    /// Copy what no reader still needs; never wait.
    Passive,
    /// Wait for readers, copy every frame, and truncate the WAL file to zero bytes.
    Truncate,
}

/// The result row of `PRAGMA wal_checkpoint`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CheckpointOutcome {
    /// 1 when a reader or writer prevented a complete checkpoint.
    pub busy: i64,
    /// Frames in the WAL, or -1 when the database is not in WAL mode.
    pub log_frames: i64,
    /// Frames copied into the database file, or -1 when not in WAL mode.
    pub checkpointed_frames: i64,
}

impl CheckpointOutcome {
    /// Whether every committed frame reached the database file. SQLite syncs
    /// the WAL before it copies frames and the database file after, so every
    /// commit before this checkpoint is then on disk.
    pub fn is_complete(&self) -> bool {
        self.busy == 0 && self.log_frames == self.checkpointed_frames
    }
}

/// Run one checkpoint on `connection`.
pub fn checkpoint(
    connection: &Connection,
    mode: CheckpointMode,
) -> rusqlite::Result<CheckpointOutcome> {
    let sql = match mode {
        CheckpointMode::Passive => "PRAGMA wal_checkpoint(PASSIVE)",
        CheckpointMode::Truncate => "PRAGMA wal_checkpoint(TRUNCATE)",
    };
    connection.query_row(sql, [], |row| {
        Ok(CheckpointOutcome {
            busy: row.get(0)?,
            log_frames: row.get(1)?,
            checkpointed_frames: row.get(2)?,
        })
    })
}

/// When the next flush of one database may run: requested by commits, and at
/// most once per [`WAL_FLUSH_INTERVAL`]. Time is Tokio's clock so tests can
/// control it; outside a paused test runtime it is the monotonic clock.
#[derive(Debug)]
pub struct WalFlushSchedule {
    requested: AtomicBool,
    last_flush: Mutex<tokio::time::Instant>,
    /// Signalled when a request arrives while none was pending.
    requested_wake: tokio::sync::Notify,
}

impl Default for WalFlushSchedule {
    fn default() -> Self {
        Self {
            requested: AtomicBool::new(false),
            last_flush: Mutex::new(tokio::time::Instant::now()),
            requested_wake: tokio::sync::Notify::new(),
        }
    }
}

impl WalFlushSchedule {
    /// Record a commit that the next flush must cover.
    pub fn request(&self) {
        if !self.requested.swap(true, Ordering::AcqRel) {
            self.requested_wake.notify_one();
        }
    }

    /// Whether a commit is waiting for a flush.
    pub fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    /// Wait until a request arrives while none was pending.
    pub async fn requested(&self) {
        self.requested_wake.notified().await;
    }

    /// The remaining wait before a requested flush may run, or `None` when
    /// nothing waits for a flush.
    pub fn retry_after_at(&self, now: tokio::time::Instant) -> Option<Duration> {
        if !self.is_requested() {
            return None;
        }
        let last = *self
            .last_flush
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Some(WAL_FLUSH_INTERVAL.saturating_sub(now.saturating_duration_since(last)))
    }

    /// Take the pending request if its flush is due at `now`. The flush runs
    /// while the claim is held; a claim dropped without [`WalFlushClaim::complete`]
    /// restores the request.
    pub fn claim_if_due(&self, now: tokio::time::Instant) -> Option<WalFlushClaim<'_>> {
        if !self.retry_after_at(now)?.is_zero() {
            return None;
        }
        self.requested
            .swap(false, Ordering::AcqRel)
            .then_some(WalFlushClaim {
                schedule: self,
                completed: false,
            })
    }
}

/// A taken flush request; see [`WalFlushSchedule::claim_if_due`].
pub struct WalFlushClaim<'a> {
    schedule: &'a WalFlushSchedule,
    completed: bool,
}

impl WalFlushClaim<'_> {
    /// Record a finished flush; the next one is due one interval later.
    pub fn complete(mut self) {
        *self
            .schedule
            .last_flush
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = tokio::time::Instant::now();
        self.completed = true;
    }
}

impl Drop for WalFlushClaim<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.schedule.requested.store(true, Ordering::Release);
        }
    }
}

/// Flush `connection` when it changed rows since `flushed_changes`, its
/// `total_changes()` at the previous flush. Returns whether a checkpoint ran.
pub fn flush_if_changed(
    connection: &Connection,
    flushed_changes: &mut u64,
) -> rusqlite::Result<bool> {
    let changes = connection.total_changes();
    if changes == *flushed_changes {
        return Ok(false);
    }
    checkpoint(connection, CheckpointMode::Passive)?;
    *flushed_changes = changes;
    Ok(true)
}

/// Flush the single writer connection of a database owned by its own process
/// (the process supervisor journal, the Mac `host_exec` service) once per
/// [`WAL_FLUSH_INTERVAL`] while it has new changes. The thread ends when the
/// owner drops the connection; closing the last connection checkpoints it.
pub fn spawn_periodic_flush(
    name: &str,
    connection: Weak<Mutex<Connection>>,
) -> std::io::Result<()> {
    let label = name.to_owned();
    std::thread::Builder::new()
        .name(format!("{label}-wal-flush"))
        .spawn(move || {
            let mut flushed_changes = 0;
            loop {
                #[allow(clippy::disallowed_methods)] // A dedicated thread, outside any runtime.
                std::thread::sleep(WAL_FLUSH_INTERVAL);
                let Some(connection) = connection.upgrade() else {
                    return;
                };
                let Ok(connection) = connection.lock() else {
                    return;
                };
                if let Err(error) = flush_if_changed(&connection, &mut flushed_changes) {
                    eprintln!("{label} WAL flush failed: {error}");
                }
            }
        })
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every writer must get NORMAL and the WAL size limit from the one helper.
    #[test]
    fn configure_writer_sets_wal_normal_and_the_size_limit() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let connection = Connection::open(directory.path().join("db.sqlite3")).expect("open");
        configure_writer(&connection).expect("pragmas");
        let journal: String = connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("journal mode");
        let synchronous: i64 = connection
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .expect("synchronous");
        let limit: i64 = connection
            .query_row("PRAGMA journal_size_limit", [], |row| row.get(0))
            .expect("size limit");
        assert_eq!(
            (journal.as_str(), synchronous, limit),
            (
                "wal",
                SYNCHRONOUS_NORMAL,
                SQLITE_WAL_JOURNAL_SIZE_LIMIT_BYTES
            )
        );
    }

    // The schedule runs a flush only for pending commits and at most once
    // per interval, so a busy database is not synced on every commit.
    #[tokio::test(start_paused = true)]
    async fn a_flush_is_due_once_per_interval_and_only_after_a_request() {
        let schedule = WalFlushSchedule::default();
        tokio::time::advance(WAL_FLUSH_INTERVAL).await;
        assert!(schedule.claim_if_due(tokio::time::Instant::now()).is_none());

        schedule.request();
        schedule
            .claim_if_due(tokio::time::Instant::now())
            .expect("due after an interval")
            .complete();
        schedule.request();
        assert_eq!(
            schedule.retry_after_at(tokio::time::Instant::now()),
            Some(WAL_FLUSH_INTERVAL)
        );
        assert!(schedule.claim_if_due(tokio::time::Instant::now()).is_none());
        tokio::time::advance(WAL_FLUSH_INTERVAL - Duration::from_millis(1)).await;
        assert!(schedule.claim_if_due(tokio::time::Instant::now()).is_none());
        tokio::time::advance(Duration::from_millis(1)).await;
        drop(
            schedule
                .claim_if_due(tokio::time::Instant::now())
                .expect("due one interval after the last flush"),
        );
        assert!(
            schedule.is_requested(),
            "an unfinished claim keeps the request"
        );
    }

    // Only a request that finds nothing pending wakes the flush owner; the
    // owner then arms one timer instead of reacting to every commit.
    #[tokio::test]
    async fn only_the_first_pending_request_wakes_the_owner() {
        let schedule = WalFlushSchedule::default();
        schedule.request();
        schedule.request();
        tokio::time::timeout(Duration::from_secs(1), schedule.requested())
            .await
            .expect("first request wakes");
        assert!(
            tokio::time::timeout(Duration::from_millis(10), schedule.requested())
                .await
                .is_err(),
            "a second request while one is pending must not wake again"
        );
    }

    // Periodic flushing of standalone databases skips a checkpoint when the
    // connection wrote nothing since the previous one.
    #[test]
    fn flush_if_changed_runs_only_after_new_changes() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let connection = Connection::open(directory.path().join("db.sqlite3")).expect("open");
        configure_writer(&connection).expect("pragmas");
        connection
            .execute_batch("CREATE TABLE t(v INTEGER); INSERT INTO t VALUES (1);")
            .expect("write");
        let mut flushed = 0;
        assert!(flush_if_changed(&connection, &mut flushed).expect("first flush"));
        assert!(!flush_if_changed(&connection, &mut flushed).expect("no new change"));
        connection
            .execute("INSERT INTO t VALUES (2)", [])
            .expect("write");
        assert!(flush_if_changed(&connection, &mut flushed).expect("new change"));
    }
}
