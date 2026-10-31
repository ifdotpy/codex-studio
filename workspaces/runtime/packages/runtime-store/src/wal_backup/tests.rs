use super::*;
use crate::sqlite_durability::{CheckpointMode, checkpoint};
use rusqlite::Connection;

struct Source {
    dir: tempfile::TempDir,
    writer: Connection,
    backup: Arc<WalBackup>,
}

impl Source {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let writer = Connection::open(dir.path().join("canvas.sqlite3")).unwrap();
        crate::sqlite_durability::configure_writer(&writer).unwrap();
        writer
            .execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY, body BLOB NOT NULL);")
            .unwrap();
        let backup = WalBackup::open(dir.path()).unwrap();
        Self {
            dir,
            writer,
            backup,
        }
    }

    fn insert(&self, rows: usize) {
        let transaction = self.writer.unchecked_transaction().unwrap();
        for _ in 0..rows {
            transaction
                .execute("INSERT INTO items(body) VALUES (zeroblob(2000))", [])
                .unwrap();
        }
        transaction.commit().unwrap();
    }

    fn count(&self) -> i64 {
        self.writer
            .query_row("SELECT count(*) FROM items", [], |row| row.get(0))
            .unwrap()
    }

    /// What the writer actor does around a flush.
    fn flush(&self) -> bool {
        if !self.backup.before_checkpoint(CheckpointMode::Passive) {
            return false;
        }
        let outcome = checkpoint(&self.writer, CheckpointMode::Passive).unwrap();
        self.backup.after_checkpoint(outcome.is_complete());
        true
    }

    /// Copy snapshot and outbox to a folder like the paired server holds.
    fn receive(&self, snapshot: &Path) -> PathBuf {
        let folder = self.dir.path().join("received");
        fs::create_dir_all(&folder).unwrap();
        write_snapshot_file(snapshot, &folder.join(SNAPSHOT_FILE)).unwrap();
        for segment in self.backup.segments() {
            fs::copy(&segment.path, folder.join(&segment.name)).unwrap();
        }
        folder
    }

    /// Begin a generation and take the snapshot the way the shipper does.
    fn start(&self) -> PathBuf {
        let snapshot = self.begin();
        assert!(self.backup.end_snapshot());
        snapshot
    }

    /// Begin a generation and copy the database file; the read mark is still held.
    fn begin(&self) -> PathBuf {
        self.backup.begin_generation(&self.writer).unwrap();
        let snapshot = self.dir.path().join("snapshot-copy");
        fs::copy(self.backup.database_path(), &snapshot).unwrap();
        snapshot
    }
}

#[test]
fn restore_rebuilds_the_database_across_wal_restarts() {
    let source = Source::new();
    source.insert(20);
    let snapshot = source.start();
    source.insert(30);
    assert!(source.flush());
    // The flush finished the WAL, so this write restarts it: a second WAL index.
    source.insert(40);
    assert!(source.flush());
    source.insert(10);
    assert!(source.backup.before_checkpoint(CheckpointMode::Passive));
    let received = source.receive(&snapshot);
    let names: Vec<_> = source
        .backup
        .segments()
        .iter()
        .map(|s| s.name.clone())
        .collect();
    assert!(
        names.iter().any(|name| name.starts_with("w00000001-")),
        "{names:?}"
    );
    let out = source.dir.path().join("restored.sqlite3");
    let restored = restore(&received, &out).unwrap();
    assert_eq!(restored.wal_files, 3);
    let count: i64 = Connection::open(&out)
        .unwrap()
        .query_row("SELECT count(*) FROM items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, source.count());
    assert_eq!(count, 100);
}

#[test]
fn uncommitted_and_torn_frames_are_not_shipped() {
    let source = Source::new();
    let snapshot = source.start();
    source.insert(5);
    let open = source.writer.unchecked_transaction().unwrap();
    open.execute("INSERT INTO items(body) VALUES (zeroblob(100000))", [])
        .unwrap();
    assert!(source.backup.before_checkpoint(CheckpointMode::Passive));
    let received = source.receive(&snapshot);
    open.rollback().unwrap();
    let out = source.dir.path().join("restored.sqlite3");
    restore(&received, &out).unwrap();
    let count: i64 = Connection::open(&out)
        .unwrap()
        .query_row("SELECT count(*) FROM items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 5);
}

#[test]
fn a_lost_segment_fails_the_restore() {
    let source = Source::new();
    let snapshot = source.start();
    source.insert(5);
    assert!(source.backup.before_checkpoint(CheckpointMode::Passive));
    source.insert(5);
    assert!(source.backup.before_checkpoint(CheckpointMode::Passive));
    source.insert(5);
    assert!(source.backup.before_checkpoint(CheckpointMode::Passive));
    let received = source.receive(&snapshot);
    let segments = source.backup.segments();
    assert!(segments.len() >= 3);
    fs::remove_file(received.join(&segments[1].name)).unwrap();
    let error = restore(&received, &source.dir.path().join("restored.sqlite3")).unwrap_err();
    assert!(error.contains("gap"), "{error}");
}

#[test]
fn a_flipped_byte_fails_the_restore() {
    let source = Source::new();
    let snapshot = source.start();
    source.insert(5);
    assert!(source.backup.before_checkpoint(CheckpointMode::Passive));
    let received = source.receive(&snapshot);
    let segment = &source.backup.segments()[0];
    let mut bytes = fs::read(received.join(&segment.name)).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    fs::write(received.join(&segment.name), bytes).unwrap();
    let error = restore(&received, &source.dir.path().join("restored.sqlite3")).unwrap_err();
    assert!(error.contains("checksum"), "{error}");
}

#[test]
fn a_restart_outside_the_hook_breaks_the_generation() {
    let source = Source::new();
    source.start();
    source.insert(5);
    assert!(source.flush());
    source.insert(5);
    // A checkpoint outside the hook finishes the WAL; the next write restarts it.
    assert_eq!(source.backup.status().broken, None);
    checkpoint(&source.writer, CheckpointMode::Passive).unwrap();
    source.insert(5);
    assert!(source.backup.before_checkpoint(CheckpointMode::Passive));
    assert!(source.backup.status().broken.is_some());
}

// The checkpoints keep running while the snapshot is copied, so the copy holds pages that
// a checkpoint wrote during the copy; the WAL from its start fixes them.
#[test]
fn checkpoints_run_during_the_snapshot_and_the_copy_restores() {
    let source = Source::new();
    source.insert(20);
    // A complete checkpoint first: the WAL is empty of unflushed frames when the snapshot begins.
    assert!(source.flush());
    source.begin();
    assert!(source.backup.status().snapshot_running);
    source.insert(30);
    assert!(source.flush(), "the checkpoint is not deferred");
    let snapshot = source.dir.path().join("snapshot-late");
    fs::copy(source.backup.database_path(), &snapshot).unwrap();
    source.insert(10);
    assert!(source.flush());
    assert!(source.backup.end_snapshot());
    assert_eq!(source.backup.status().broken, None);
    source.insert(5);
    assert!(source.backup.before_checkpoint(CheckpointMode::Passive));
    let received = source.receive(&snapshot);
    let out = source.dir.path().join("restored.sqlite3");
    restore(&received, &out).unwrap();
    let count: i64 = Connection::open(&out)
        .unwrap()
        .query_row("SELECT count(*) FROM items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, source.count());
    assert_eq!(count, 65);
}

#[test]
fn a_shutdown_checkpoint_or_a_restart_during_the_snapshot_breaks_the_generation() {
    let source = Source::new();
    source.begin();
    assert!(source.backup.before_checkpoint(CheckpointMode::Truncate));
    assert!(source.backup.status().broken.is_some());
    assert!(!source.backup.status().snapshot_running);
    assert!(!source.backup.end_snapshot());

    let other = Source::new();
    other.begin();
    let reopened = WalBackup::open(other.dir.path()).unwrap();
    assert!(reopened.status().broken.is_some());
}

#[test]
fn the_snapshot_file_round_trips_through_zstd_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let raw = dir.path().join("raw");
    let bytes: Vec<u8> = (0..BACKUP_SNAPSHOT_BLOCK_BYTES * 2 + 77)
        .map(|index| (index / 3) as u8)
        .collect();
    fs::write(&raw, &bytes).unwrap();
    let packed = dir.path().join("packed");
    write_snapshot_file(&raw, &packed).unwrap();
    assert!(fs::metadata(&packed).unwrap().len() < bytes.len() as u64);
    let out = dir.path().join("out");
    decode_snapshot(&packed, &out).unwrap();
    assert_eq!(fs::read(out).unwrap(), bytes);
}

#[test]
fn a_new_generation_drops_the_old_outbox() {
    let source = Source::new();
    source.start();
    source.insert(5);
    assert!(source.backup.before_checkpoint(CheckpointMode::Passive));
    let first = source.backup.status();
    assert_eq!(first.pending_segments, 2);
    source.start();
    let second = source.backup.status();
    assert_ne!(first.generation, second.generation);
    assert_eq!(second.pending_segments, 1);
}

#[test]
fn state_survives_a_restart_of_the_shipper() {
    let source = Source::new();
    source.start();
    source.insert(5);
    assert!(source.flush());
    let reopened = WalBackup::open(source.dir.path()).unwrap();
    assert_eq!(
        reopened.status().generation,
        source.backup.status().generation
    );
    source.insert(5);
    // The WAL restarted after the clean checkpoint, and the reopened shipper expects it.
    assert!(reopened.before_checkpoint(CheckpointMode::Passive));
    assert_eq!(reopened.status().broken, None);
}

#[test]
fn segment_names_round_trip() {
    let name = segment_name(3, 32, 8224, [0xdead_beef, 7], 12);
    assert_eq!(
        parse_segment_name(&name),
        Some(SegmentName {
            index: 3,
            start: 32,
            end: 8224,
            checksum: [0xdead_beef, 7],
            pages: 12
        })
    );
    assert_eq!(parse_segment_name("snapshot.sqlite3"), None);
    assert_eq!(parse_segment_name("w1-2-3.seg"), None);
}
