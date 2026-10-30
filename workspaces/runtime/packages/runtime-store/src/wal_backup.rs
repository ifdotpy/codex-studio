//! WAL shipping backup of `canvas.sqlite3`: the mechanics on the source and the restore
//! (docs/backup.md; design in docs/decisions/2026-10-28-target-client-server-architecture.md,
//! "Storage"). Transport and settings are in `studio-runtime-services` (`peer_backup`).
//!
//! A *generation* is one base snapshot (a copy of the database file) and the WAL frames
//! written since, in order. The writer actor calls [`WalBackup::before_checkpoint`] and
//! [`WalBackup::after_checkpoint`] around each Canvas checkpoint. Nothing else writes the
//! database file, and no writer runs while the actor runs that job, so:
//!
//! - `before_checkpoint` copies every new committed WAL frame into the outbox as one segment
//!   (local file I/O, no SQL transaction, no network). A WAL longer than
//!   `BACKUP_WAL_READ_MAX_BYTES` is copied over several pauses; the checkpoint waits until
//!   the outbox has caught up, so a checkpoint never restarts a WAL with unshipped frames.
//! - The base snapshot is a copy of the database file taken while a separate read connection
//!   holds a read mark. The normal checkpoints keep running: they copy only the frames up to
//!   that mark into the file and still sync, and the WAL cannot restart. Every WAL frame from
//!   the start of the WAL is also shipped, and restore applies them over the copy, so a page
//!   that a checkpoint changed during the copy ends with its newest content.
//! - A WAL restart that the shipper did not expect (a checkpoint outside the hook) breaks
//!   the chain. The generation is then marked broken and the owner starts a new one.
//!
//! A segment file is `w<wal index>-<start>-<end>-<chain checksum>-<pages>.seg`: the bytes of
//! the WAL file from `start` to `end` (the first segment of a WAL index includes the 32 byte
//! header), the WAL checksum chain at `end`, and the database size in pages after the last
//! commit. That name is the source position that [`restore`] compares with.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use studio_runtime_schema::shared_values::{
    BACKUP_OUTBOX_MAX_BYTES, BACKUP_SNAPSHOT_BLOCK_BYTES, BACKUP_SNAPSHOT_ZSTD_LEVEL,
    BACKUP_WAL_READ_MAX_BYTES,
};

use crate::sqlite_durability::CheckpointMode;

const WAL_HEADER_BYTES: usize = 32;
const FRAME_HEADER_BYTES: usize = 24;
const WAL_VERSION: u32 = 3_007_000;
/// File name of the base snapshot in a generation folder on the receiving server: blocks of
/// `BACKUP_SNAPSHOT_BLOCK_BYTES` of the database file, each as a 4 byte big-endian length
/// and a zstd frame (see [`encode_snapshot_block`]).
pub const SNAPSHOT_FILE: &str = "snapshot.zstblocks";
const SEGMENT_SUFFIX: &str = ".seg";

/// Position in one WAL file: everything before `offset` is in the outbox.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct WalPosition {
    index: u64,
    salt: [u32; 2],
    offset: u64,
    checksum: [u32; 2],
    pages: u32,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct State {
    generation: Option<String>,
    wal: Option<WalPosition>,
    /// Every frame of the WAL was shipped and the last checkpoint finished the WAL, so
    /// the next write restarts it. A new salt is then expected.
    clean: bool,
    /// The snapshot copy runs: a read connection holds the read mark until `end_snapshot`.
    snapshot_running: bool,
    snapshot_sent: bool,
    broken: Option<String>,
    last_error: Option<String>,
}

/// What the owner and the diagnostics see.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupStatus {
    pub generation: Option<String>,
    pub broken: Option<String>,
    pub snapshot_running: bool,
    pub snapshot_sent: bool,
    pub pending_segments: usize,
    pub pending_bytes: u64,
    pub last_error: Option<String>,
}

/// One segment waiting in the outbox.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Segment {
    pub name: String,
    pub path: PathBuf,
    pub len: u64,
}

pub struct WalBackup {
    db: PathBuf,
    wal: PathBuf,
    folder: PathBuf,
    state: Mutex<State>,
    /// The read connection that holds the read mark while the snapshot is copied.
    reader: Mutex<Option<rusqlite::Connection>>,
    wake: tokio::sync::Notify,
}

impl WalBackup {
    /// Open the backup folder `<state>/backup` of a state directory.
    pub fn open(state_dir: &Path) -> io::Result<Arc<Self>> {
        let folder = state_dir.join("backup");
        fs::create_dir_all(&folder)?;
        let mut state: State = match fs::read(folder.join("state.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => State::default(),
            Err(error) => return Err(error),
        };
        // The read mark does not survive a restart, so the copy cannot be completed.
        if state.snapshot_running {
            state.snapshot_running = false;
            state.broken = Some("the server stopped during the snapshot".into());
        }
        Ok(Arc::new(Self {
            db: state_dir.join("canvas.sqlite3"),
            wal: state_dir.join("canvas.sqlite3-wal"),
            folder,
            state: Mutex::new(state),
            reader: Mutex::new(None),
            wake: tokio::sync::Notify::new(),
        }))
    }

    pub fn database_path(&self) -> &Path {
        &self.db
    }

    /// Resolves when a segment or a state change needs the shipper.
    pub async fn changed(&self) {
        self.wake.notified().await;
    }

    pub fn wake(&self) {
        self.wake.notify_one();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn outbox(&self, generation: &str) -> PathBuf {
        self.folder.join("outbox").join(generation)
    }

    fn save(&self, state: &State) -> io::Result<()> {
        let temporary = self.folder.join("state.json.new");
        fs::write(
            &temporary,
            serde_json::to_vec(state).map_err(io::Error::other)?,
        )?;
        fs::rename(temporary, self.folder.join("state.json"))
    }

    fn mark_broken(&self, state: &mut State, reason: String) {
        eprintln!("backup generation broken: {reason}");
        state.broken = Some(reason);
        let _ = self.save(state);
        self.wake();
    }

    pub fn status(&self) -> BackupStatus {
        let state = self.lock().clone();
        let segments = state
            .generation
            .as_deref()
            .map(|generation| self.segments_of(generation))
            .unwrap_or_default();
        BackupStatus {
            snapshot_running: state.snapshot_running,
            snapshot_sent: state.snapshot_sent,
            pending_bytes: segments.iter().map(|segment| segment.len).sum(),
            pending_segments: segments.len(),
            generation: state.generation,
            broken: state.broken,
            last_error: state.last_error,
        }
    }

    pub fn set_error(&self, error: Option<String>) {
        let mut state = self.lock();
        if state.last_error != error {
            state.last_error = error;
            let _ = self.save(&state);
        }
    }

    /// The segments of the current generation, oldest first.
    pub fn segments(&self) -> Vec<Segment> {
        let generation = self.lock().generation.clone();
        generation
            .map(|generation| self.segments_of(&generation))
            .unwrap_or_default()
    }

    fn segments_of(&self, generation: &str) -> Vec<Segment> {
        let Ok(entries) = fs::read_dir(self.outbox(generation)) else {
            return Vec::new();
        };
        let mut segments: Vec<Segment> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                parse_segment_name(&name)?;
                Some(Segment {
                    len: entry.metadata().ok()?.len(),
                    path: entry.path(),
                    name,
                })
            })
            .collect();
        segments.sort_by(|a, b| a.name.cmp(&b.name));
        segments
    }

    /// Remove a segment that the paired server confirmed.
    pub fn ack(&self, segment: &Segment) {
        let _ = fs::remove_file(&segment.path);
    }

    /// Start a new generation: drop the old outbox, take a read mark on a separate
    /// connection, and copy the WAL so far. Run it on the writer actor with the writer
    /// connection, so no commit and no checkpoint happens meanwhile. The caller copies the
    /// database file and then calls [`Self::end_snapshot`].
    pub fn begin_generation(&self, writer: &rusqlite::Connection) -> io::Result<String> {
        let mut state = self.lock();
        self.release_reader();
        if let Some(old) = state.generation.take() {
            let _ = fs::remove_dir_all(self.outbox(&old));
        }
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let id = format!("{seconds:012}-{}", uuid::Uuid::new_v4().simple());
        fs::create_dir_all(self.outbox(&id))?;
        // Rewriting the user version with its own value adds one WAL frame. The read mark
        // then lies after the last checkpointed frame, so checkpoints go on copying the
        // frames before it while the reader lives.
        let version: i64 = writer
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(io::Error::other)?;
        writer
            .execute_batch(&format!("PRAGMA user_version = {version}"))
            .map_err(io::Error::other)?;
        let reader = rusqlite::Connection::open_with_flags(
            &self.db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(io::Error::other)?;
        reader
            .execute_batch("BEGIN; SELECT count(*) FROM sqlite_master;")
            .map_err(io::Error::other)?;
        *self.reader.lock().unwrap_or_else(PoisonError::into_inner) = Some(reader);
        *state = State {
            generation: Some(id.clone()),
            snapshot_running: true,
            // Frames in the WAL now belong to this generation. A WAL restart is expected
            // only after a checkpoint that finished the WAL.
            clean: true,
            ..State::default()
        };
        self.ship(&mut state, None)?;
        self.save(&state)?;
        Ok(id)
    }

    fn release_reader(&self) {
        // Dropping the connection ends its read transaction.
        drop(
            self.reader
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
    }

    /// The snapshot copy reached the paired server: release the read mark. Returns whether
    /// the copy was running, that is, whether the generation is still valid.
    pub fn end_snapshot(&self) -> bool {
        let mut state = self.lock();
        self.release_reader();
        if !state.snapshot_running || state.broken.is_some() {
            return false;
        }
        state.snapshot_running = false;
        state.snapshot_sent = true;
        let _ = self.save(&state);
        true
    }

    /// Stop the generation and drop its outbox: the owner turned the backup off, or the
    /// paired server lost the snapshot. A new generation starts when the backup runs again.
    pub fn break_generation(&self, reason: &str) {
        let mut state = self.lock();
        self.release_reader();
        state.snapshot_running = false;
        if let Some(generation) = state.generation.clone() {
            let _ = fs::remove_dir_all(self.outbox(&generation));
        }
        self.mark_broken(&mut state, reason.to_owned());
    }

    /// Copy new WAL frames into the outbox. Returns whether the checkpoint may run now.
    /// A failure never blocks the checkpoint: the flush to disk comes first.
    pub fn before_checkpoint(&self, mode: CheckpointMode) -> bool {
        let mut state = self.lock();
        if state.generation.is_none() || state.broken.is_some() {
            return true;
        }
        if state.snapshot_running && mode == CheckpointMode::Truncate {
            // The shutdown checkpoint waits for readers, so the copy cannot go on.
            self.release_reader();
            state.snapshot_running = false;
            self.mark_broken(&mut state, "shutdown checkpoint during the snapshot".into());
            return true;
        }
        let limit = match mode {
            CheckpointMode::Passive => Some(BACKUP_WAL_READ_MAX_BYTES),
            CheckpointMode::Truncate => None,
        };
        let allowed = match self.ship(&mut state, limit) {
            Ok(caught_up) => caught_up,
            Err(error) => {
                self.mark_broken(&mut state, format!("WAL copy failed: {error}"));
                return true;
            }
        };
        // The WAL may get frames again; only a finished checkpoint makes it clean.
        state.clean = false;
        let _ = self.save(&state);
        allowed
    }

    /// Record whether the checkpoint finished the WAL. The next write then restarts it.
    pub fn after_checkpoint(&self, complete: bool) {
        let mut state = self.lock();
        if state.generation.is_none() || state.broken.is_some() {
            return;
        }
        state.clean = complete;
        let _ = self.save(&state);
    }

    /// Copy the committed frames after the current position. Returns whether the copy
    /// reached the end of the WAL.
    fn ship(&self, state: &mut State, limit: Option<u64>) -> io::Result<bool> {
        let Some(generation) = state.generation.clone() else {
            return Ok(true);
        };
        let Some(mut file) = open_if_present(&self.wal)? else {
            return Ok(true);
        };
        let mut head = [0_u8; WAL_HEADER_BYTES];
        let Some(header) = read_header(&mut file, &mut head)? else {
            return Ok(true);
        };
        let position = match state.wal {
            Some(position) if position.salt == header.salt => position,
            // Each restart adds one to the first salt, so two restarts are not one.
            Some(previous) if state.clean && header.salt[0] == previous.salt[0].wrapping_add(1) => {
                WalPosition {
                    index: previous.index + 1,
                    ..fresh_position(&header)
                }
            }
            Some(_) => {
                self.mark_broken(state, "the WAL restarted outside the backup hook".into());
                return Ok(true);
            }
            None => fresh_position(&header),
        };
        let start = position.offset;
        let mut data = Vec::new();
        let mut checksum = position.checksum;
        if start == 0 {
            data.extend_from_slice(&head);
            checksum = header.checksum;
        }
        file.seek(SeekFrom::Start(start.max(WAL_HEADER_BYTES as u64)))?;
        let mut frames = FrameReader::new(&mut file, &header, checksum);
        let mut shipped = 0;
        let mut pages = position.pages;
        let mut caught_up = true;
        let mut frame = Vec::new();
        while let Some(commit_pages) = frames.next(&mut frame)? {
            data.extend_from_slice(&frame);
            if commit_pages != 0 {
                shipped = data.len();
                checksum = frames.checksum;
                pages = commit_pages;
                if limit.is_some_and(|limit| data.len() as u64 >= limit) {
                    caught_up = false;
                    break;
                }
            }
        }
        // Frames after the last commit are not committed yet; they stay in the WAL.
        if shipped == 0 {
            state.wal = Some(position);
            return Ok(caught_up);
        }
        data.truncate(shipped);
        let end = start + shipped as u64;
        if self.outbox_bytes(&generation) + shipped as u64 > BACKUP_OUTBOX_MAX_BYTES {
            self.mark_broken(state, "the backup outbox is full".into());
            return Ok(true);
        }
        let name = segment_name(position.index, start, end, checksum, pages);
        let folder = self.outbox(&generation);
        let partial = folder.join(format!("{name}.part"));
        let mut out = File::create(&partial)?;
        out.write_all(&data)?;
        out.sync_data()?;
        fs::rename(partial, folder.join(name))?;
        state.wal = Some(WalPosition {
            offset: end,
            checksum,
            pages,
            ..position
        });
        self.wake();
        Ok(caught_up)
    }

    fn outbox_bytes(&self, generation: &str) -> u64 {
        self.segments_of(generation).iter().map(|s| s.len).sum()
    }
}

fn fresh_position(header: &WalHeader) -> WalPosition {
    WalPosition {
        index: 0,
        salt: header.salt,
        offset: 0,
        checksum: header.checksum,
        pages: 0,
    }
}

fn open_if_present(path: &Path) -> io::Result<Option<File>> {
    match File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn segment_name(index: u64, start: u64, end: u64, checksum: [u32; 2], pages: u32) -> String {
    format!(
        "w{index:08}-{start:016}-{end:016}-{:08x}{:08x}-{pages:08x}{SEGMENT_SUFFIX}",
        checksum[0], checksum[1]
    )
}

/// The source position that a segment name records.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SegmentName {
    pub index: u64,
    pub start: u64,
    pub end: u64,
    pub checksum: [u32; 2],
    pub pages: u32,
}

/// Parse a segment file name; `None` for any other name.
pub fn parse_segment_name(name: &str) -> Option<SegmentName> {
    let body = name.strip_suffix(SEGMENT_SUFFIX)?.strip_prefix('w')?;
    let mut parts = body.split('-');
    let index = parts.next()?.parse().ok()?;
    let start = parts.next()?.parse().ok()?;
    let end = parts.next()?.parse().ok()?;
    let checksum = parts.next()?;
    let pages = u32::from_str_radix(parts.next()?, 16).ok()?;
    if parts.next().is_some() || checksum.len() != 16 {
        return None;
    }
    Some(SegmentName {
        index,
        start,
        end,
        checksum: [
            u32::from_str_radix(&checksum[..8], 16).ok()?,
            u32::from_str_radix(&checksum[8..], 16).ok()?,
        ],
        pages,
    })
}

struct WalHeader {
    big_endian: bool,
    page_size: usize,
    salt: [u32; 2],
    checksum: [u32; 2],
}

fn be(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// The WAL checksum of `data` (a multiple of 8 bytes) after `seed`.
fn checksum(big_endian: bool, seed: [u32; 2], data: &[u8]) -> [u32; 2] {
    let [mut s1, mut s2] = seed;
    for pair in data.chunks_exact(8) {
        let word = |bytes: &[u8]| {
            let raw = [bytes[0], bytes[1], bytes[2], bytes[3]];
            if big_endian {
                u32::from_be_bytes(raw)
            } else {
                u32::from_le_bytes(raw)
            }
        };
        s1 = s1.wrapping_add(word(&pair[..4])).wrapping_add(s2);
        s2 = s2.wrapping_add(word(&pair[4..])).wrapping_add(s1);
    }
    [s1, s2]
}

/// Read the 32 byte WAL header; `None` when the file is too short or not a valid header.
fn read_header(
    file: &mut File,
    head: &mut [u8; WAL_HEADER_BYTES],
) -> io::Result<Option<WalHeader>> {
    file.seek(SeekFrom::Start(0))?;
    match file.read_exact(head) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    Ok(parse_header(head))
}

fn parse_header(head: &[u8; WAL_HEADER_BYTES]) -> Option<WalHeader> {
    let magic = be(&head[0..4]);
    if magic & !1 != 0x377f_0682 || be(&head[4..8]) != WAL_VERSION {
        return None;
    }
    let big_endian = magic & 1 == 1;
    let stored = [be(&head[24..28]), be(&head[28..32])];
    if checksum(big_endian, [0, 0], &head[..24]) != stored {
        return None;
    }
    let page_size = be(&head[8..12]) as usize;
    (page_size.is_power_of_two() && (512..=65536).contains(&page_size)).then_some(WalHeader {
        big_endian,
        page_size,
        salt: [be(&head[16..20]), be(&head[20..24])],
        checksum: stored,
    })
}

/// Reads valid frames one after the other. A frame is valid when its salt matches the
/// header and its checksum continues the chain.
struct FrameReader<'a, R> {
    source: &'a mut R,
    header: &'a WalHeader,
    checksum: [u32; 2],
    /// The checksum chain after the frame that `next` returned last.
    pending: [u32; 2],
}

impl<'a, R: Read> FrameReader<'a, R> {
    fn new(source: &'a mut R, header: &'a WalHeader, checksum: [u32; 2]) -> Self {
        Self {
            source,
            header,
            checksum,
            pending: checksum,
        }
    }

    /// Replace `frame` with the next valid frame. Returns the database size in pages if
    /// the frame commits (zero otherwise), or `None` at the end of the valid frames.
    /// `self.checksum` is the chain after the last *commit* frame returned.
    fn next(&mut self, frame: &mut Vec<u8>) -> io::Result<Option<u32>> {
        frame.clear();
        frame.resize(FRAME_HEADER_BYTES + self.header.page_size, 0);
        match self.source.read_exact(frame) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error),
        }
        if be(&frame[8..12]) != self.header.salt[0] || be(&frame[12..16]) != self.header.salt[1] {
            return Ok(None);
        }
        let big = self.header.big_endian;
        let mut chain = checksum(big, self.pending, &frame[..8]);
        chain = checksum(big, chain, &frame[FRAME_HEADER_BYTES..]);
        if chain != [be(&frame[16..20]), be(&frame[20..24])] {
            return Ok(None);
        }
        self.pending = chain;
        let commit = be(&frame[4..8]);
        if commit != 0 {
            self.checksum = chain;
        }
        Ok(Some(commit))
    }
}

/// Encode one block of the database file as the snapshot file stores it.
pub fn encode_snapshot_block(raw: &[u8]) -> io::Result<Vec<u8>> {
    let mut frame = Vec::with_capacity(zstd_safe::compress_bound(raw.len()));
    zstd_safe::compress(&mut frame, raw, BACKUP_SNAPSHOT_ZSTD_LEVEL)
        .map_err(|code| io::Error::other(format!("zstd: {}", zstd_safe::get_error_name(code))))?;
    let length = u32::try_from(frame.len()).map_err(io::Error::other)?;
    let mut block = Vec::with_capacity(frame.len() + 4);
    block.extend_from_slice(&length.to_be_bytes());
    block.extend_from_slice(&frame);
    Ok(block)
}

/// Write the snapshot file of `db` as the sender does: for tests and local checks.
pub fn write_snapshot_file(db: &Path, out: &Path) -> io::Result<()> {
    let mut source = File::open(db)?;
    let mut target = File::create(out)?;
    let mut raw = vec![0_u8; BACKUP_SNAPSHOT_BLOCK_BYTES];
    loop {
        let mut filled = 0;
        while filled < raw.len() {
            match source.read(&mut raw[filled..])? {
                0 => break,
                read => filled += read,
            }
        }
        if filled == 0 {
            return Ok(());
        }
        target.write_all(&encode_snapshot_block(&raw[..filled])?)?;
    }
}

/// Rebuild the database file from a snapshot file.
fn decode_snapshot(snapshot: &Path, out: &Path) -> io::Result<()> {
    let mut source = io::BufReader::new(File::open(snapshot)?);
    let mut target = File::create(out)?;
    let mut length = [0_u8; 4];
    loop {
        match source.read_exact(&mut length) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        }
        let length = u32::from_be_bytes(length) as usize;
        if length > zstd_safe::compress_bound(BACKUP_SNAPSHOT_BLOCK_BYTES) {
            return Err(io::Error::other("a snapshot block is too long"));
        }
        let mut frame = vec![0_u8; length];
        source.read_exact(&mut frame)?;
        let mut raw = Vec::with_capacity(BACKUP_SNAPSHOT_BLOCK_BYTES);
        zstd_safe::decompress(&mut raw, &frame).map_err(|code| {
            io::Error::other(format!("zstd: {}", zstd_safe::get_error_name(code)))
        })?;
        target.write_all(&raw)?;
    }
}

/// What [`restore`] rebuilt and checked.
#[derive(Debug, Eq, PartialEq)]
pub struct Restored {
    pub segments: usize,
    pub wal_files: usize,
    /// The database size in pages after the last commit of the source position, or `None`
    /// when the generation has no segment.
    pub pages: Option<u32>,
}

/// Rebuild the database in `out` from a generation folder (`snapshot.sqlite3` and the
/// `.seg` files) and check it: each segment must continue the previous one and end at the
/// checksum its name records, `PRAGMA integrity_check` must say `ok`, and the page count
/// must equal the source position of the last segment.
pub fn restore(generation: &Path, out: &Path) -> Result<Restored, String> {
    let fail = |what: &str, error: &dyn std::fmt::Display| format!("{what}: {error}");
    let mut segments: Vec<(SegmentName, PathBuf)> = fs::read_dir(generation)
        .map_err(|e| fail("read the generation folder", &e))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = parse_segment_name(entry.file_name().to_str()?)?;
            Some((name, entry.path()))
        })
        .collect();
    segments.sort_by_key(|(name, _)| (name.index, name.start));
    if out.exists() {
        return Err(format!("{} exists", out.display()));
    }
    decode_snapshot(&generation.join(SNAPSHOT_FILE), out)
        .map_err(|e| fail("decode the snapshot", &e))?;
    let wal = PathBuf::from(format!("{}-wal", out.display()));
    let mut wal_files = 0;
    let mut position = None;
    for group in segments.chunk_by(|a, b| a.0.index == b.0.index) {
        let mut file = File::create(&wal).map_err(|e| fail("create the WAL", &e))?;
        let mut header = None;
        let mut next = 0;
        let mut chain = [0, 0];
        for (name, path) in group {
            if name.start != next {
                return Err(format!("segment gap before {}", path.display()));
            }
            let mut bytes = Vec::new();
            File::open(path)
                .and_then(|mut source| source.read_to_end(&mut bytes))
                .map_err(|e| fail("read a segment", &e))?;
            if bytes.len() as u64 != name.end - name.start {
                return Err(format!("segment {} has the wrong length", path.display()));
            }
            let mut frames: &[u8] = &bytes;
            if name.start == 0 {
                let mut head = [0_u8; WAL_HEADER_BYTES];
                head.copy_from_slice(bytes.get(..WAL_HEADER_BYTES).ok_or("short segment")?);
                let parsed = parse_header(&head).ok_or("bad WAL header in a segment")?;
                chain = parsed.checksum;
                frames = &bytes[WAL_HEADER_BYTES..];
                header = Some(parsed);
            }
            let parsed = header
                .as_ref()
                .ok_or("segments do not start at the WAL header")?;
            let (committed, pending) = {
                let mut reader = FrameReader::new(&mut frames, parsed, chain);
                let mut frame = Vec::new();
                while reader
                    .next(&mut frame)
                    .map_err(|e| fail("scan a segment", &e))?
                    .is_some()
                {}
                (reader.checksum, reader.pending)
            };
            if !frames.is_empty() || committed != name.checksum || pending != name.checksum {
                return Err(format!(
                    "segment {} does not match its recorded checksum",
                    path.display()
                ));
            }
            chain = committed;
            next = name.end;
            file.write_all(&bytes)
                .map_err(|e| fail("write the WAL", &e))?;
            position = Some(name.pages);
        }
        drop(file);
        fold_wal(out).map_err(|e| fail("apply the WAL", &e))?;
        wal_files += 1;
    }
    let connection = rusqlite::Connection::open(out).map_err(|e| fail("open the restore", &e))?;
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|e| fail("integrity_check", &e))?;
    if integrity != "ok" {
        return Err(format!("integrity_check: {integrity}"));
    }
    let pages: u32 = connection
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .map_err(|e| fail("page_count", &e))?;
    if position.is_some_and(|expected| expected != pages) {
        return Err(format!(
            "page count {pages} differs from the source position {position:?}"
        ));
    }
    Ok(Restored {
        segments: segments.len(),
        wal_files,
        pages: position,
    })
}

/// Open the database, which recovers the WAL next to it, and fold the WAL into the file.
fn fold_wal(db: &Path) -> rusqlite::Result<()> {
    let connection = rusqlite::Connection::open(db)?;
    connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
}

#[cfg(test)]
mod tests;
