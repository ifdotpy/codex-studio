//! Owner of large item text kept outside SQLite.
//!
//! A transcript item whose text is longer than [`TEXT_BLOB_MIN_CHARS`] keeps
//! a preview in its record. The full text is one file,
//! `<state>/blobs/text/<hash>`: a stored-format-1 zstd frame
//! ([`crate::stored_text`]) named by the SHA-256 of the exact text. The row
//! `runtime_item_blobs(id, hash, size)` points the item at it.
//!
//! Write order: write the file, fsync it and its directory, then write the
//! row in the same transaction. A crash in between leaves an unreferenced file
//! and never a row without a file. [`sweep`] deletes unreferenced files that
//! are older than [`TEXT_BLOB_SWEEP_DELAY`]. It runs in a writer transaction, so
//! no writer sits between its file write and its row insert while it deletes.
//!
//! The directory lies next to the database file, so every function takes the
//! connection and needs a file database.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::cached_sql::CachedSql;
use crate::{StoreError, StoreResult, stored_text};

/// Item text longer than this many characters goes to a blob. Unit: characters.
/// It is also the length of the preview that stays in the item record.
pub const TEXT_BLOB_MIN_CHARS: usize = 20_000;

/// An unreferenced blob younger than this is never deleted. It covers the
/// time between a blob write and its row commit, and a stream that rewrites a
/// growing item. Unit: seconds.
pub const TEXT_BLOB_SWEEP_DELAY: Duration = Duration::from_secs(60 * 60);

/// Address of one blob: the SHA-256 of the exact text and its length.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextBlobRef {
    /// Lowercase hexadecimal SHA-256 of the UTF-8 text.
    pub hash: String,
    /// Length of the UTF-8 text. Unit: bytes.
    pub size: u64,
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

fn directory(connection: &Connection) -> StoreResult<PathBuf> {
    let database = connection
        .path()
        .filter(|path| !path.is_empty())
        .ok_or_else(|| unavailable("text blobs need a file database"))?;
    let parent = std::path::Path::new(database)
        .parent()
        .ok_or_else(|| unavailable("database has no directory"))?;
    Ok(parent.join("blobs").join("text"))
}

fn hash_of(text: &str) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(text.as_bytes()))
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Write `text` durably and return its address. Writing the same text again
/// refreshes the age of the file so a sweep keeps it.
pub fn put(connection: &Connection, text: &str) -> StoreResult<TextBlobRef> {
    let dir = directory(connection)?;
    fs::create_dir_all(&dir)?;
    let reference = TextBlobRef {
        hash: hash_of(text),
        size: text.len() as u64,
    };
    let target = dir.join(&reference.hash);
    if target.is_file() {
        File::options()
            .write(true)
            .open(&target)?
            .set_modified(SystemTime::now())?;
        return Ok(reference);
    }
    let mut temporary = tempfile::NamedTempFile::new_in(&dir)?;
    temporary.write_all(&stored_text::encode(text))?;
    temporary.as_file().sync_all()?;
    temporary.persist(&target).map_err(|error| error.error)?;
    #[cfg(unix)]
    {
        File::open(&dir)?.sync_all()?;
        if let Some(parent) = dir.parent() {
            File::open(parent)?.sync_all()?;
        }
    }
    Ok(reference)
}

/// Read a blob by hash and check it against its name.
pub fn read(connection: &Connection, hash: &str) -> StoreResult<String> {
    if !valid_hash(hash) {
        return Err(invalid(hash));
    }
    let bytes = fs::read(directory(connection)?.join(hash))?;
    let text = stored_text::decode_blob(&bytes).map_err(|_| invalid(hash))?;
    if hash_of(&text) != hash {
        return Err(invalid(hash));
    }
    Ok(text)
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

/// Point item `id` at `blob`. Call it after [`put`] returned.
pub fn set_reference(
    connection: &Connection,
    id: &str,
    blob: &TextBlobRef,
) -> rusqlite::Result<()> {
    connection
        .execute_cached(
            "INSERT INTO runtime_item_blobs(id,hash,size) VALUES(?1,?2,?3) \
             ON CONFLICT(id) DO UPDATE SET hash=excluded.hash,size=excluded.size",
            params![id, blob.hash, blob.size as i64],
        )
        .map(drop)
}

/// Stop pointing item `id` at a blob.
pub fn clear_reference(connection: &Connection, id: &str) -> rusqlite::Result<()> {
    if !has_table(connection)? {
        return Ok(());
    }
    connection
        .execute_cached("DELETE FROM runtime_item_blobs WHERE id=?1", [id])
        .map(drop)
}

/// Delete blobs that no row references and that are older than
/// [`TEXT_BLOB_SWEEP_DELAY`] at `now`. Call it inside a writer transaction.
/// Returns the number of deleted files.
pub fn sweep(connection: &Connection, now: SystemTime) -> StoreResult<usize> {
    let dir = directory(connection)?;
    if !dir.is_dir() {
        return Ok(0);
    }
    let referenced: HashSet<String> = connection
        .prepare("SELECT hash FROM runtime_item_blobs")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let mut deleted = 0;
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| referenced.contains(name))
        {
            continue;
        }
        let metadata = entry.metadata()?;
        let age = now
            .duration_since(metadata.modified()?)
            .unwrap_or(Duration::ZERO);
        if metadata.is_file() && age >= TEXT_BLOB_SWEEP_DELAY {
            fs::remove_file(entry.path())?;
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// Move every `runtime_item_fulltext` row to a blob and point its item at it.
/// The table keeps its rows. Returns the number of rows moved.
pub fn move_fulltext(connection: &Connection) -> StoreResult<usize> {
    let ids: Vec<String> = connection
        .prepare(
            "SELECT id FROM runtime_item_fulltext \
             WHERE id NOT IN (SELECT id FROM runtime_item_blobs)",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    for id in &ids {
        let body: stored_text::StoredText = connection.query_row(
            "SELECT body FROM runtime_item_fulltext WHERE id=?1",
            [id],
            |row| row.get(0),
        )?;
        let blob = put(connection, &body.0)?;
        set_reference(connection, id, &blob)?;
    }
    Ok(ids.len())
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
