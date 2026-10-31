//! Terminal output history in its own database, `terminal-output.sqlite3`, with
//! its own connection. A chunk append never waits in or blocks the Canvas writer.
//!
//! The terminal row (retained tail, offset, event cursor) commits in Canvas
//! first; the caller then appends the same text here. A crash between the two
//! commits loses that one chunk of older history. The tail in the row is not
//! affected. Rows of a deleted terminal are removed after the Canvas commit.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OpenFlags, TransactionBehavior};

use crate::repositories::terminals::TerminalsRepository;
use crate::{RuntimeStore, StoreError, StoreResult, blocking_pool, sqlite_durability};

/// File name of the terminal output database in the state directory.
pub const TERMINAL_OUTPUT_DATABASE: &str = "terminal-output.sqlite3";

const SCHEMA: &str = "\
    CREATE TABLE IF NOT EXISTS user_terminal_output (terminal TEXT NOT NULL, start INTEGER NOT NULL, end INTEGER NOT NULL, text TEXT NOT NULL, PRIMARY KEY (terminal,start));\
    CREATE INDEX IF NOT EXISTS user_terminal_output_end ON user_terminal_output (terminal,end);";

pub(crate) type SharedArchive = Arc<Mutex<Connection>>;

/// Open or create the archive and move rows left in Canvas by earlier versions.
pub(crate) fn open(root: &Path, canvas_path: &Path) -> StoreResult<SharedArchive> {
    let path = root.join(TERMINAL_OUTPUT_DATABASE);
    let mut archive = crate::open_database(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )?;
    sqlite_durability::configure_writer(&archive)?;
    archive.execute_batch(SCHEMA)?;
    move_from_canvas(&mut archive, canvas_path)?;
    Ok(Arc::new(Mutex::new(archive)))
}

/// Copy, flush, then delete: a crash in between repeats the copy, which ignores
/// rows that already exist. The Canvas table stays (empty) for schema checks.
fn move_from_canvas(archive: &mut Connection, canvas_path: &Path) -> StoreResult<()> {
    let canvas = canvas_path.to_string_lossy().into_owned();
    archive.execute("ATTACH DATABASE ?1 AS canvas", [canvas])?;
    let result = (|| -> StoreResult<()> {
        let present: i64 = archive.query_row(
            "SELECT COUNT(*) FROM canvas.sqlite_master WHERE type='table' AND name='user_terminal_output'",
            [],
            |row| row.get(0),
        )?;
        if present == 0 {
            return Ok(());
        }
        let terminals = archive
            .prepare("SELECT DISTINCT terminal FROM canvas.user_terminal_output")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if terminals.is_empty() {
            return Ok(());
        }
        for terminal in &terminals {
            archive.execute(
                "INSERT OR IGNORE INTO main.user_terminal_output SELECT terminal,start,end,text FROM canvas.user_terminal_output WHERE terminal=?1",
                [terminal],
            )?;
        }
        sqlite_durability::checkpoint(archive, sqlite_durability::CheckpointMode::Truncate)?;
        for terminal in &terminals {
            archive.execute(
                "DELETE FROM canvas.user_terminal_output WHERE terminal=?1",
                [terminal],
            )?;
        }
        Ok(())
    })();
    archive.execute("DETACH DATABASE canvas", [])?;
    result
}

fn locked(archive: &SharedArchive) -> StoreResult<std::sync::MutexGuard<'_, Connection>> {
    archive.lock().map_err(|_| StoreError::WriterPoisoned)
}

impl RuntimeStore {
    /// Append `text` that starts at `start` (Unicode scalars) to the archive of terminal `id`.
    pub async fn terminal_output_append(
        &self,
        id: &str,
        start: i64,
        text: String,
    ) -> StoreResult<()> {
        let archive = Arc::clone(&self.terminal_output);
        let wal_flush = Arc::clone(&self.wal_flush);
        let id = id.to_owned();
        blocking_pool::spawn_blocking("terminal-output/append", move || {
            let mut archive = locked(&archive)?;
            let transaction = archive.transaction_with_behavior(TransactionBehavior::Immediate)?;
            TerminalsRepository::append_output(&transaction, &id, start, &text)?;
            transaction
                .commit()
                .map_err(StoreError::commit_outcome_unknown)?;
            wal_flush.request();
            Ok(())
        })
        .await
        .map_err(|_| StoreError::ReadTaskFailed)?
    }

    /// First archived offset of terminal `id`, if any.
    pub async fn terminal_output_start(&self, id: &str) -> StoreResult<Option<i64>> {
        let archive = Arc::clone(&self.terminal_output);
        let id = id.to_owned();
        blocking_pool::spawn_blocking("terminal-output/start", move || {
            let connection = locked(&archive)?;
            TerminalsRepository::archive_start(&connection, &id)
        })
        .await
        .map_err(|_| StoreError::ReadTaskFailed)?
    }

    /// Archived chunks `(start, end, text)` that overlap `[start, end)`, at most `limit`.
    pub async fn terminal_output_chunks(
        &self,
        id: &str,
        start: i64,
        end: i64,
        limit: i64,
    ) -> StoreResult<Vec<(i64, i64, String)>> {
        let archive = Arc::clone(&self.terminal_output);
        let id = id.to_owned();
        blocking_pool::spawn_blocking("terminal-output/chunks", move || {
            let connection = locked(&archive)?;
            TerminalsRepository::archive_chunks(&connection, &id, start, end, limit)
        })
        .await
        .map_err(|_| StoreError::ReadTaskFailed)?
    }

    /// Remove the archive of a deleted terminal.
    pub async fn terminal_output_delete(&self, id: &str) -> StoreResult<()> {
        let archive = Arc::clone(&self.terminal_output);
        let wal_flush = Arc::clone(&self.wal_flush);
        let id = id.to_owned();
        blocking_pool::spawn_blocking("terminal-output/delete", move || {
            locked(&archive)?
                .execute("DELETE FROM user_terminal_output WHERE terminal=?1", [&id])?;
            wal_flush.request();
            Ok(())
        })
        .await
        .map_err(|_| StoreError::ReadTaskFailed)?
    }

    pub(crate) async fn checkpoint_terminal_output(
        &self,
        mode: sqlite_durability::CheckpointMode,
    ) -> StoreResult<()> {
        let archive = Arc::clone(&self.terminal_output);
        blocking_pool::spawn_blocking("terminal-output/checkpoint", move || {
            let connection = locked(&archive)?;
            sqlite_durability::checkpoint(&connection, mode)?;
            Ok(())
        })
        .await
        .map_err(|_| StoreError::ReadTaskFailed)?
    }

    /// A new connection to the archive, for test assertions only.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_terminal_output(&self) -> StoreResult<Connection> {
        crate::open_database(
            &self.root.join(TERMINAL_OUTPUT_DATABASE),
            OpenFlags::SQLITE_OPEN_READ_WRITE,
        )
    }
}
