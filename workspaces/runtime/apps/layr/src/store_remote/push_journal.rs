//! Process-crash progress for one push. Remote publications remain authoritative.
use super::{
    format::{Entry, Link},
    zst,
};
use anyhow::{bail, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Completed {
    pub state: String,
    pub attempt: String,
    pub depth: usize,
}
#[derive(Default, Serialize)]
pub(super) struct Stats {
    pub peak_bytes: u64,
    pub commits: usize,
    pub commit_seconds: f64,
    pub state_commits: usize,
    pub state_commit_seconds: f64,
    pub seed_states: usize,
    pub seed_pending_parts: usize,
    pub seed_metadata_lists: usize,
    pub seed_reader_gets: usize,
    pub seed_seconds: f64,
    pub seed_started_ms: i64,
    pub seed_finished_ms: i64,
}
pub(super) struct Pending {
    pub object: String,
    pub entries: Vec<Entry>,
}
pub(super) struct Journal {
    db: Connection,
    path: PathBuf,
    pub stats: Stats,
    pub resuming: bool,
    warned: bool,
    staged_states: usize,
}
impl Journal {
    pub fn open(dir: &Path) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = dir.join("push-journal.sqlite3");
        let resuming = path.exists();
        if !resuming {
            for suffix in ["-wal", "-shm"] {
                Self::remove_optional(&PathBuf::from(format!("{}{suffix}", path.display())))?;
            }
            std::fs::File::open(dir)?.sync_all()?;
        }
        std::fs::OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(&path)?;
        let db = Connection::open(&path)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
            CREATE TABLE IF NOT EXISTS hashes (sha TEXT PRIMARY KEY, links BLOB NOT NULL) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS completed (state TEXT PRIMARY KEY, attempt TEXT NOT NULL, depth INTEGER NOT NULL) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS pending (object TEXT PRIMARY KEY, state TEXT NOT NULL, attempt TEXT NOT NULL, entries BLOB NOT NULL, modified INTEGER) WITHOUT ROWID;")?;
        if db.query_row("SELECT count(*) FROM pragma_table_info('pending') WHERE name='modified'", [], |row| {
            row.get::<_, usize>(0)
        })? == 0
        {
            db.execute_batch("ALTER TABLE pending RENAME COLUMN created TO modified; UPDATE pending SET modified=NULL;")?;
        }
        let peak_bytes = std::fs::metadata(&path)?.len();
        Ok(Self { db, path, stats: Stats { peak_bytes, ..Stats::default() }, resuming, warned: false, staged_states: 0 })
    }
    pub fn hashes(&self) -> Result<BTreeMap<String, Vec<Link>>> {
        let mut result = BTreeMap::new();
        let mut query = self.db.prepare("SELECT sha, links FROM hashes")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let bytes: Vec<u8> = row.get(1)?;
            result.insert(row.get(0)?, serde_json::from_slice(&zst::decompress_with_limit(&bytes, None, 1 << 20)?)?);
        }
        Ok(result)
    }
    pub fn completed(&self) -> Result<Vec<Completed>> {
        let mut query = self.db.prepare("SELECT state, attempt, depth FROM completed")?;
        let completed = query
            .query_map([], |row| Ok(Completed { state: row.get(0)?, attempt: row.get(1)?, depth: row.get(2)? }))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(completed)
    }
    pub fn pending(&self) -> Result<Vec<Pending>> {
        let mut result = Vec::new();
        let mut query = self.db.prepare("SELECT object, entries FROM pending ORDER BY object")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let bytes: Vec<u8> = row.get(1)?;
            result.push(Pending { object: row.get(0)?, entries: serde_json::from_slice(&zst::decompress(&bytes, None)?)? });
        }
        Ok(result)
    }
    fn transaction(&mut self, state: bool, commit: bool, apply: impl FnOnce(&Connection) -> Result<()>) -> Result<()> {
        if self.db.is_autocommit() {
            self.db.execute_batch("BEGIN IMMEDIATE")?;
        }
        apply(&self.db)?;
        self.staged_states += usize::from(state);
        self.observe_size()?;
        // A part prepare commits the preceding published cursor and its own pending
        // row together, before PUT. Runs of states without parts group at 32 states.
        if commit || self.staged_states >= 32 {
            self.flush()?;
        }
        Ok(())
    }
    fn observe_size(&mut self) -> Result<()> {
        let size = std::fs::metadata(&self.path)?.len()
            + std::fs::metadata(self.path.with_file_name("push-journal.sqlite3-wal")).map_or(0, |m| m.len())
            + std::fs::metadata(self.path.with_file_name("push-journal.sqlite3-shm")).map_or(0, |m| m.len());
        self.stats.peak_bytes = self.stats.peak_bytes.max(size);
        if self.stats.peak_bytes > 64 << 20 && !self.warned {
            eprintln!("warning: active push journal exceeds 64 MiB; it is removed after the push completes");
            self.warned = true;
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        if self.db.is_autocommit() {
            return Ok(());
        }
        let started = Instant::now();
        self.db.execute_batch("COMMIT")?;
        let elapsed = started.elapsed().as_secs_f64();
        self.stats.commits += 1;
        self.stats.commit_seconds += elapsed;
        if self.staged_states > 0 {
            self.stats.state_commits += self.staged_states;
            self.stats.state_commit_seconds += elapsed;
        }
        self.staged_states = 0;
        self.observe_size()?;
        Ok(())
    }
    fn insert_hashes(tx: &Connection, hashes: &BTreeMap<String, Vec<Link>>) -> Result<()> {
        let mut insert = tx.prepare_cached("INSERT OR IGNORE INTO hashes VALUES (?1,?2)")?;
        for (sha, links) in hashes {
            insert.execute(params![sha, zst::compress(&serde_json::to_vec(links)?, None)?])?;
        }
        Ok(())
    }
    pub fn prepare_part(&mut self, state: &str, attempt: &str, part: u32, entries: &[Entry]) -> Result<()> {
        let object = super::format::data_name(state, attempt, part);
        let bytes = zst::compress(&serde_json::to_vec(entries)?, None)?;
        self.transaction(false, true, |tx| {
            tx.execute("INSERT OR REPLACE INTO pending VALUES (?1,?2,?3,?4,NULL)", params![object, state, attempt, bytes])?;
            Ok(())
        })
    }
    pub fn validate_part(&mut self, object: &str, modified: i64) -> Result<()> {
        self.transaction(false, true, |tx| {
            tx.execute("UPDATE pending SET modified=?2 WHERE object=?1", params![object, modified])?;
            Ok(())
        })
    }
    pub fn discard_part(&mut self, object: &str) -> Result<()> {
        self.transaction(false, true, |tx| {
            tx.execute("DELETE FROM pending WHERE object=?1", [object])?;
            Ok(())
        })
    }
    pub fn checkpoint(
        &mut self,
        state: &str,
        attempt: &str,
        depth: usize,
        hashes: &BTreeMap<String, Vec<Link>>,
        protected: &[String],
    ) -> Result<()> {
        self.transaction(true, false, |tx| {
            Self::insert_hashes(tx, hashes)?;
            tx.execute("INSERT OR REPLACE INTO completed VALUES (?1,?2,?3)", params![state, attempt, depth])?;
            tx.execute("DELETE FROM pending WHERE state=?1 AND attempt=?2", params![state, attempt])?;
            for object in protected {
                tx.execute("DELETE FROM pending WHERE object=?1", [object])?;
            }
            Ok(())
        })
    }
    pub fn finish(mut self) -> Result<Stats> {
        self.flush()?;
        let busy: i64 = self.db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        if busy != 0 {
            bail!("push journal is still in use; keeping its files for recovery");
        }
        let Self { db, path, stats, .. } = self;
        db.close().map_err(|(_, error)| error)?;
        for suffix in ["-wal", "-shm"] {
            Self::remove_optional(&PathBuf::from(format!("{}{suffix}", path.display())))?;
        }
        std::fs::remove_file(&path)?;
        std::fs::File::open(path.parent().unwrap())?.sync_all()?;
        Ok(stats)
    }
    fn remove_optional(path: &Path) -> Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}
