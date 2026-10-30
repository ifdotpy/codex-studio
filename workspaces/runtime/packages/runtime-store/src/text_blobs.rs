//! Owner of large text kept outside SQLite, and of the sweep that collects it.
//!
//! One content-addressed tree holds large text: `<state>/blobs/<aa>/<bb>/<sha256>`,
//! the layout of `payload_blobs` in `runtime-services`, which writes every file
//! durably (temporary file, fsync, rename, directory fsync). The name is the
//! SHA-256 of the stored bytes. A text blob is one stored-format-1 zstd frame
//! ([`crate::stored_text`]); a payload blob is plain JSON. [`read`] accepts both.
//!
//! Item text. A writer transaction only writes rows: it puts the full text of a
//! long item into `runtime_item_fulltext` (an in-place update, so a streaming
//! item costs no file) and clears the item's blob row. Once an item is not
//! streaming, a background pass ([`pending`]) writes the text as a blob
//! outside any transaction, then a short transaction ([`commit`]) points the
//! item at it, only if the blob file exists and the text did not change. The
//! row `runtime_item_blobs(id, hash, size)` is that pointer; readers prefer it.
//! A crash between the file write and the commit leaves an unreferenced blob.
//!
//! Sweep. Only text blobs (first byte [`STORED_TEXT_FORMAT_ZSTD`]) older than
//! [`TEXT_BLOB_SWEEP_DELAY`] are candidates. [`quarantine`] reads the pointers,
//! then moves unreferenced candidates to `<state>/blobs/trash`. A writer
//! transaction ([`referenced_among`]) then re-reads the pointers; because
//! [`commit`] runs in a writer transaction and checks that its file exists,
//! either the pointer is seen here or the commit sees the file missing.
//! [`release`] restores referenced files and deletes the rest. Payload blobs
//! are never collected here: their references live in the JSON of many owners
//! and have no index.

use std::collections::HashSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};

use crate::cached_sql::CachedSql;
use crate::stored_text::{self, STORED_TEXT_FORMAT_ZSTD};
use crate::{StoreError, StoreResult};

/// Item text longer than this many characters goes to a blob. Unit: characters.
/// It is also the length of the preview that stays in the item record. The
/// inline threshold of the design (about 8 KB compressed) is a follow-up.
pub const TEXT_BLOB_MIN_CHARS: usize = 20_000;

/// An unreferenced text blob younger than this is never collected. Unit: seconds.
pub const TEXT_BLOB_SWEEP_DELAY: Duration = Duration::from_secs(60 * 60);

/// Name of the quarantine directory inside `<state>/blobs`.
const TRASH_DIRECTORY: &str = "trash";

/// Address of one blob: the SHA-256 of the stored bytes, and the text length.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextBlobRef {
    /// Lowercase hexadecimal SHA-256 of the stored bytes.
    pub hash: String,
    /// Length of the decoded UTF-8 text. Unit: bytes.
    pub size: u64,
}

/// Text that a pass moved to a blob, ready for [`commit`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MovedText {
    pub id: String,
    /// [`digest`] of the text that was written.
    pub digest: String,
    pub blob: TextBlobRef,
}

fn unavailable(message: &str) -> StoreError {
    StoreError::StateDirectory(std::io::Error::other(message.to_owned()))
}

fn invalid(hash: &str) -> StoreError {
    StoreError::InvalidStoredRecord {
        object: "text_blob",
        id: hash.to_owned(),
    }
}

fn blobs_root(connection: &Connection) -> StoreResult<PathBuf> {
    let database = connection
        .path()
        .filter(|path| !path.is_empty())
        .ok_or_else(|| unavailable("text blobs need a file database"))?;
    Path::new(database)
        .parent()
        .map(|parent| parent.join("blobs"))
        .ok_or_else(|| unavailable("database has no directory"))
}

/// Path of the blob `sha256` under the blobs directory `blobs_root`.
pub fn layout(blobs_root: &Path, sha256: &str) -> PathBuf {
    blobs_root
        .join(&sha256[..2])
        .join(&sha256[2..4])
        .join(sha256)
}

fn hex(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// SHA-256 of the exact text. A commit compares it to detect a changed text.
pub fn digest(text: &str) -> String {
    hex(text.as_bytes())
}

/// The stored bytes of `text` and their address. Write the bytes with
/// `payload_blobs::write_blobs`, which makes them durable.
pub fn encode_blob(text: &str) -> (TextBlobRef, Vec<u8>) {
    let bytes = stored_text::encode(text);
    (
        TextBlobRef {
            hash: hex(&bytes),
            size: text.len() as u64,
        },
        bytes,
    )
}

/// Read a blob by hash, check it against its name and decode it.
pub fn read(connection: &Connection, hash: &str) -> StoreResult<String> {
    if !valid_hash(hash) {
        return Err(invalid(hash));
    }
    let path = layout(&blobs_root(connection)?, hash);
    if !fs::symlink_metadata(&path)?.is_file() {
        return Err(invalid(hash));
    }
    let bytes = fs::read(&path)?;
    if hex(&bytes) != hash {
        return Err(invalid(hash));
    }
    if bytes.first() == Some(&STORED_TEXT_FORMAT_ZSTD) {
        stored_text::decode_blob(&bytes).map_err(|_| invalid(hash))
    } else {
        String::from_utf8(bytes).map_err(|_| invalid(hash))
    }
}

/// True when the reference table exists. Production schemas always have it;
/// a bare database, such as a test fixture, has no blobs yet.
fn has_table(connection: &Connection) -> rusqlite::Result<bool> {
    connection.query_row_cached(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='runtime_item_blobs')",
        [],
        |row| row.get(0),
    )
}

/// The blob that holds the full text of item `id`.
pub fn reference(connection: &Connection, id: &str) -> rusqlite::Result<Option<TextBlobRef>> {
    if !has_table(connection)? {
        return Ok(None);
    }
    connection
        .query_row_cached(
            "SELECT hash,size FROM runtime_item_blobs WHERE id=?1",
            [id],
            |row| {
                Ok(TextBlobRef {
                    hash: row.get(0)?,
                    size: row.get::<_, i64>(1)? as u64,
                })
            },
        )
        .optional()
}

/// Stop pointing item `id` at a blob. A writer calls it whenever it writes the
/// item's full text, so a pointer never outlives the text it was made from.
pub fn clear_reference(connection: &Connection, id: &str) -> rusqlite::Result<()> {
    if !has_table(connection)? {
        return Ok(());
    }
    connection
        .execute_cached("DELETE FROM runtime_item_blobs WHERE id=?1", [id])
        .map(drop)
}

/// Up to `limit` full texts that are final and not yet in a blob: items that
/// are not streaming, in `runtime_item_fulltext` order.
pub fn pending(connection: &Connection, limit: usize) -> StoreResult<Vec<(String, String)>> {
    let ids: Vec<String> = connection
        .prepare(
            "SELECT f.id FROM runtime_item_fulltext f \
             LEFT JOIN runtime_items i ON i.id=f.id \
             WHERE f.id NOT IN (SELECT id FROM runtime_item_blobs) \
             AND COALESCE(json_extract(i.record,'$.streaming'),0)=0 \
             ORDER BY f.rowid LIMIT ?1",
        )?
        .query_map([limit as i64], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    ids.into_iter()
        .map(|id| {
            let body: stored_text::StoredText = connection.query_row(
                "SELECT body FROM runtime_item_fulltext WHERE id=?1",
                [&id],
                |row| row.get(0),
            )?;
            Ok((id, body.0))
        })
        .collect()
}

/// Point each item at its blob. Runs in a writer transaction and writes only
/// rows. A text is skipped, to be retried by a later pass, when its blob file
/// is missing or the item's full text is no longer the text that was written.
/// Returns the number of pointers set.
pub fn commit(transaction: &Transaction<'_>, moved: &[MovedText]) -> StoreResult<usize> {
    let root = blobs_root(transaction)?;
    let mut set = 0;
    for text in moved {
        if !valid_hash(&text.blob.hash) || !layout(&root, &text.blob.hash).is_file() {
            continue;
        }
        let current: Option<stored_text::StoredText> = transaction
            .query_row(
                "SELECT body FROM runtime_item_fulltext WHERE id=?1",
                [&text.id],
                |row| row.get(0),
            )
            .optional()?;
        if current.is_none_or(|body| digest(&body.0) != text.digest) {
            continue;
        }
        transaction.execute_cached(
            "INSERT INTO runtime_item_blobs(id,hash,size) VALUES(?1,?2,?3) \
             ON CONFLICT(id) DO UPDATE SET hash=excluded.hash,size=excluded.size",
            params![text.id, text.blob.hash, text.blob.size as i64],
        )?;
        set += 1;
    }
    Ok(set)
}

/// Ids of `runtime_item_fulltext` rows that lack a readable blob with the
/// right content. A later migration may drop the table only when this is empty.
pub fn fulltext_without_blob(connection: &Connection) -> StoreResult<Vec<String>> {
    let rows: Vec<(String, Option<String>)> = connection
        .prepare(
            "SELECT f.id,b.hash FROM runtime_item_fulltext f \
             LEFT JOIN runtime_item_blobs b ON b.id=f.id",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let mut missing = Vec::new();
    for (id, hash) in rows {
        let body: stored_text::StoredText = connection.query_row(
            "SELECT body FROM runtime_item_fulltext WHERE id=?1",
            [&id],
            |row| row.get(0),
        )?;
        let intact = hash
            .and_then(|hash| read(connection, &hash).ok())
            .is_some_and(|text| text == body.0);
        if !intact {
            missing.push(id);
        }
    }
    Ok(missing)
}

/// Step 1 of the sweep, with no transaction open. Hashes of the pointers
/// (read them from a read transaction with [`pointed_hashes`]) protect their
/// blobs. Moves old unreferenced text blobs to the trash directory and returns
/// their hashes, together with any hash already in the trash from a crashed
/// sweep.
pub fn quarantine(
    blobs_root: &Path,
    pointed: &HashSet<String>,
    now: SystemTime,
) -> StoreResult<Vec<String>> {
    let trash = blobs_root.join(TRASH_DIRECTORY);
    fs::create_dir_all(&trash)?;
    let mut names = Vec::new();
    for entry in fs::read_dir(&trash)? {
        if let Some(name) = entry?.file_name().to_str().filter(|n| valid_hash(n)) {
            names.push(name.to_owned());
        }
    }
    for first in fs::read_dir(blobs_root)? {
        let first = first?;
        if first.file_name().len() != 2 || !first.file_type()?.is_dir() {
            continue;
        }
        for second in fs::read_dir(first.path())? {
            let second = second?;
            if !second.file_type()?.is_dir() {
                continue;
            }
            for file in fs::read_dir(second.path())? {
                let file = file?;
                let Some(name) = file.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let metadata = file.metadata()?;
                let age = now
                    .duration_since(metadata.modified()?)
                    .unwrap_or(Duration::ZERO);
                if !valid_hash(&name)
                    || !metadata.is_file()
                    || age < TEXT_BLOB_SWEEP_DELAY
                    || pointed.contains(&name)
                    || !is_text_blob(&file.path())?
                {
                    continue;
                }
                fs::rename(file.path(), trash.join(&name))?;
                names.push(name);
            }
        }
    }
    Ok(names)
}

fn is_text_blob(path: &Path) -> std::io::Result<bool> {
    use std::io::Read;
    let mut first = [0_u8; 1];
    let read = File::open(path)?.read(&mut first)?;
    Ok(read == 1 && first[0] == STORED_TEXT_FORMAT_ZSTD)
}

/// Hashes of every pointer. Call it in a read transaction.
pub fn pointed_hashes(connection: &Connection) -> StoreResult<HashSet<String>> {
    Ok(connection
        .prepare("SELECT hash FROM runtime_item_blobs")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?)
}

/// Step 2 of the sweep. Call it in a writer transaction, which serializes it
/// with [`commit`]. Returns the hashes among `names` that a pointer names now.
pub fn referenced_among(
    transaction: &Transaction<'_>,
    names: &[String],
) -> StoreResult<HashSet<String>> {
    let pointed = pointed_hashes(transaction)?;
    Ok(names
        .iter()
        .filter(|name| pointed.contains(*name))
        .cloned()
        .collect())
}

/// Step 3 of the sweep, with no transaction open. Puts the referenced files
/// back and deletes the others. Returns the number of deleted files.
pub fn release(
    blobs_root: &Path,
    referenced: &HashSet<String>,
    names: &[String],
) -> StoreResult<usize> {
    let trash = blobs_root.join(TRASH_DIRECTORY);
    let mut deleted = 0;
    for name in names {
        if referenced.contains(name) {
            let target = layout(blobs_root, name);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(trash.join(name), target)?;
        } else {
            fs::remove_file(trash.join(name))?;
            deleted += 1;
        }
    }
    Ok(deleted)
}
