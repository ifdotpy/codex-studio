//! Conversion of the values in an owned stored-text column between plain TEXT
//! and stored format 1, in short resumable batches.
//!
//! [`Direction::Compress`] is the background pass. [`Direction::Inflate`] is
//! its reverse for an operator who must return the state to a release that
//! reads only plain TEXT. A reader scans one bounded rowid window and converts
//! the values outside the writer; each converted value must decode to the
//! exact original text before it is used. The writer batch then replaces a
//! value only when the row still holds the scanned value (compare and set),
//! reads every replaced value back inside the same transaction, and commits
//! only when the read-back values are the converted values and decode to the
//! originals. The batch records a SHA-256 of the original texts in
//! `runtime_stored_text_batches`. Nothing is deleted: a value is replaced only
//! by another form of the same text.

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};

use crate::stored_text;
use crate::{CachedSql, StoreError, StoreResult};

/// A column whose values the conversion passes rewrite.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactedColumn {
    /// Table that holds the column.
    pub table: &'static str,
    /// Unique text key of a row, compared before a value is replaced.
    pub key: &'static str,
    /// The owned stored-text column.
    pub column: &'static str,
    /// Name of the compaction progress row in `runtime_payload_migrations`
    /// and of its batch rows in `runtime_stored_text_batches`.
    pub progress: &'static str,
}

/// `runtime_item_fulltext.body`, owned by [`stored_text`].
pub const FULLTEXT_BODY: CompactedColumn = CompactedColumn {
    table: "runtime_item_fulltext",
    key: "id",
    column: "body",
    progress: "stored-text-v1:runtime_item_fulltext.body",
};

/// Batch records of the compaction and inflate passes, in each database that
/// holds a compacted column.
pub(crate) const BATCHES_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS runtime_stored_text_batches (
        name TEXT NOT NULL, batch INTEGER NOT NULL, first_rowid INTEGER NOT NULL,
        last_rowid INTEGER NOT NULL, rows INTEGER NOT NULL, plain_bytes INTEGER NOT NULL,
        stored_bytes INTEGER NOT NULL, sha256 TEXT NOT NULL, created REAL NOT NULL,
        PRIMARY KEY(name, batch));
";

/// Rowids that one reader window inspects. Unit: rows.
pub const COMPACTION_WINDOW_ROWS: i64 = 512;
/// Values that one batch converts at most. Unit: rows.
pub const COMPACTION_BATCH_ROWS: usize = 64;
/// Text bytes that one batch converts at most; a single larger value is
/// still converted alone. Unit: bytes.
pub const COMPACTION_BATCH_BYTES: usize = 8 * 1024 * 1024;

/// Which form a pass writes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    /// Plain TEXT to stored format 1.
    Compress,
    /// Stored format 1 to plain TEXT.
    Inflate,
}

impl Direction {
    fn source_type(self) -> &'static str {
        match self {
            Self::Compress => "text",
            Self::Inflate => "blob",
        }
    }

    fn batch_name(self, column: CompactedColumn) -> String {
        match self {
            Self::Compress => column.progress.to_owned(),
            Self::Inflate => format!("{}:inflate", column.progress),
        }
    }
}

/// Saved progress of the compaction pass of one column.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompactionProgress {
    /// Highest rowid already inspected.
    pub cursor: i64,
    /// True when one complete pass reached the end of the table.
    pub complete: bool,
}

/// One scanned value, converted and checked in memory.
#[derive(Clone, Debug)]
pub struct CompactionCandidate {
    rowid: i64,
    key: String,
    text: String,
    from: SqlValue,
    to: SqlValue,
}

/// The values of one reader window that a pass converts.
#[derive(Clone, Debug)]
pub struct CompactionWindow {
    pub direction: Direction,
    pub candidates: Vec<CompactionCandidate>,
    /// The next window starts after this rowid.
    pub next_cursor: i64,
    /// True when the window reached the end of the table.
    pub done: bool,
}

/// Result of one writer batch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CompactionBatch {
    pub converted: usize,
    /// Rows that changed after the scan; the next pass inspects them again.
    pub skipped: usize,
    /// Bytes of the converted texts.
    pub plain_bytes: u64,
    /// Bytes of their compressed forms.
    pub stored_bytes: u64,
}

pub fn progress(
    connection: &Connection,
    column: CompactedColumn,
) -> StoreResult<CompactionProgress> {
    Ok(connection
        .query_row_cached(
            "SELECT cursor,complete FROM runtime_payload_migrations WHERE name=?1",
            [column.progress],
            |row| {
                Ok(CompactionProgress {
                    cursor: row.get(0)?,
                    complete: row.get::<_, i64>(1)? != 0,
                })
            },
        )
        .optional()?
        .unwrap_or_default())
}

/// Read and convert the values after `cursor` that `direction` rewrites,
/// bounded by [`COMPACTION_WINDOW_ROWS`], [`COMPACTION_BATCH_ROWS`] and
/// [`COMPACTION_BATCH_BYTES`].
pub fn scan_window(
    connection: &Connection,
    column: CompactedColumn,
    direction: Direction,
    cursor: i64,
) -> StoreResult<CompactionWindow> {
    let end = cursor.saturating_add(COMPACTION_WINDOW_ROWS);
    let last: Option<i64> = connection.query_row_cached(
        &format!("SELECT max(rowid) FROM {}", column.table),
        [],
        |row| row.get(0),
    )?;
    let mut statement = connection.prepare_cached(&format!(
        "SELECT rowid,{key},{column} FROM {table} \
         WHERE rowid>?1 AND rowid<=?2 AND typeof({column})=?3 ORDER BY rowid",
        key = column.key,
        column = column.column,
        table = column.table,
    ))?;
    let mut rows = statement.query(params![cursor, end, direction.source_type()])?;
    let mut window = CompactionWindow {
        direction,
        candidates: Vec::new(),
        next_cursor: end,
        done: false,
    };
    let mut bytes = 0_usize;
    while let Some(row) = rows.next()? {
        let rowid: i64 = row.get(0)?;
        if window.candidates.len() == COMPACTION_BATCH_ROWS
            || (!window.candidates.is_empty() && bytes >= COMPACTION_BATCH_BYTES)
        {
            // Resume at this row in the next batch.
            window.next_cursor = rowid - 1;
            return Ok(window);
        }
        let key: String = row.get(1)?;
        let candidate = match direction {
            Direction::Compress => {
                let text: String = row.get(2)?;
                let stored = stored_text::encode(&text);
                if stored_text::decode_blob(&stored).as_deref() != Ok(text.as_str()) {
                    return Err(round_trip_failure(column, &key));
                }
                CompactionCandidate {
                    rowid,
                    key,
                    from: SqlValue::Text(text.clone()),
                    to: SqlValue::Blob(stored),
                    text,
                }
            }
            Direction::Inflate => {
                let stored: Vec<u8> = row.get(2)?;
                let text = stored_text::decode_blob(&stored)
                    .map_err(|_| round_trip_failure(column, &key))?;
                CompactionCandidate {
                    rowid,
                    key,
                    from: SqlValue::Blob(stored),
                    to: SqlValue::Text(text.clone()),
                    text,
                }
            }
        };
        bytes += candidate.text.len();
        window.candidates.push(candidate);
    }
    window.done = last.is_none_or(|last| end >= last);
    Ok(window)
}

/// Replace the scanned values that are unchanged, verify every replaced value
/// by reading it back, record the batch hash, and save compaction progress.
/// Any verification failure returns an error, so the caller's transaction
/// rolls back and no value changes.
pub fn apply_window(
    transaction: &Transaction<'_>,
    column: CompactedColumn,
    window: &CompactionWindow,
    now: f64,
) -> StoreResult<CompactionBatch> {
    let replace = format!(
        "UPDATE {table} SET {column}=?1 WHERE rowid=?2 AND {key}=?3 AND {column}=?4",
        table = column.table,
        column = column.column,
        key = column.key,
    );
    let read_back = format!(
        "SELECT {column} FROM {table} WHERE rowid=?1",
        table = column.table,
        column = column.column,
    );
    let mut batch = CompactionBatch::default();
    let mut originals = Sha256::new();
    let mut decoded = Sha256::new();
    let mut first = None;
    let mut last = None;
    for candidate in &window.candidates {
        let changed = transaction.execute_cached(
            &replace,
            params![candidate.to, candidate.rowid, candidate.key, candidate.from],
        )?;
        if changed == 0 {
            batch.skipped += 1;
            continue;
        }
        let stored: SqlValue = transaction
            .query_row_cached(&read_back, [candidate.rowid], |row| row.get(0))
            .map_err(|_| round_trip_failure(column, &candidate.key))?;
        let text = match &stored {
            SqlValue::Text(text) => text.clone(),
            SqlValue::Blob(bytes) => stored_text::decode_blob(bytes)
                .map_err(|_| round_trip_failure(column, &candidate.key))?,
            _ => return Err(round_trip_failure(column, &candidate.key)),
        };
        if stored != candidate.to || text != candidate.text {
            return Err(round_trip_failure(column, &candidate.key));
        }
        hash_value(&mut originals, candidate.rowid, candidate.text.as_bytes());
        hash_value(&mut decoded, candidate.rowid, text.as_bytes());
        first.get_or_insert(candidate.rowid);
        last = Some(candidate.rowid);
        batch.converted += 1;
        batch.plain_bytes += candidate.text.len() as u64;
        batch.stored_bytes += match (&candidate.from, &candidate.to) {
            (_, SqlValue::Blob(bytes)) | (SqlValue::Blob(bytes), _) => bytes.len() as u64,
            _ => 0,
        };
    }
    if let (Some(first), Some(last)) = (first, last) {
        let originals = data_encoding::HEXLOWER.encode(&originals.finalize());
        if originals != data_encoding::HEXLOWER.encode(&decoded.finalize()) {
            return Err(round_trip_failure(column, "batch"));
        }
        transaction.execute_cached(
            "INSERT INTO runtime_stored_text_batches\
             (name,batch,first_rowid,last_rowid,rows,plain_bytes,stored_bytes,sha256,created) \
             VALUES(?1,(SELECT coalesce(max(batch),0)+1 FROM runtime_stored_text_batches WHERE name=?1),\
             ?2,?3,?4,?5,?6,?7,?8)",
            params![
                window.direction.batch_name(column),
                first,
                last,
                batch.converted as i64,
                batch.plain_bytes as i64,
                batch.stored_bytes as i64,
                originals,
                now
            ],
        )?;
    }
    if window.direction == Direction::Compress {
        save_progress(
            transaction,
            column,
            window.next_cursor,
            window.done,
            "running",
            now,
        )?;
    }
    Ok(batch)
}

/// Save compaction progress, for example at the end of a pass whose last
/// windows held no plain values. A finished pass is saved as `complete`.
pub fn save_progress(
    transaction: &Transaction<'_>,
    column: CompactedColumn,
    cursor: i64,
    done: bool,
    status: &str,
    now: f64,
) -> StoreResult<()> {
    let status = if done { "complete" } else { status };
    transaction.execute_cached(
        "INSERT INTO runtime_payload_migrations(name,cursor,complete,status,updated,error) \
         VALUES(?1,?2,?3,?4,?5,NULL) ON CONFLICT(name) DO UPDATE SET \
         cursor=excluded.cursor,complete=excluded.complete,status=excluded.status,\
         updated=excluded.updated,error=NULL",
        params![column.progress, cursor, i64::from(done), status, now],
    )?;
    Ok(())
}

/// Forget compaction progress, so that a later compaction pass inspects the
/// whole column again. An inflate pass ends with this.
pub fn reset_progress(transaction: &Transaction<'_>, column: CompactedColumn) -> StoreResult<()> {
    transaction.execute_cached(
        "DELETE FROM runtime_payload_migrations WHERE name=?1",
        [column.progress],
    )?;
    Ok(())
}

/// Keep the saved cursor and record why the compaction pass stopped.
pub fn record_error(
    transaction: &Transaction<'_>,
    column: CompactedColumn,
    message: &str,
    now: f64,
) -> StoreResult<()> {
    transaction.execute_cached(
        "INSERT INTO runtime_payload_migrations(name,cursor,complete,status,updated,error) \
         VALUES(?1,0,0,'error',?2,?3) ON CONFLICT(name) DO UPDATE SET \
         status='error',updated=excluded.updated,error=excluded.error",
        params![column.progress, now, message],
    )?;
    Ok(())
}

/// One value in a batch hash: rowid and byte length as little-endian 64-bit
/// integers, then the text bytes.
fn hash_value(hash: &mut Sha256, rowid: i64, bytes: &[u8]) {
    hash.update(rowid.to_le_bytes());
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn round_trip_failure(column: CompactedColumn, key: &str) -> StoreError {
    StoreError::InvalidStoredObject {
        table: column.table,
        id: key.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stored_text::StoredText;

    const NOW: f64 = 2_000_000_000.0;

    fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("database");
        connection
            .execute_batch(
                "CREATE TABLE runtime_item_fulltext (id TEXT PRIMARY KEY, body TEXT NOT NULL);
                 CREATE TABLE runtime_payload_migrations (name TEXT PRIMARY KEY,
                     cursor INTEGER NOT NULL DEFAULT 0, complete INTEGER NOT NULL DEFAULT 0,
                     status TEXT NOT NULL DEFAULT 'pending', updated REAL, error TEXT);",
            )
            .expect("column and progress tables");
        connection
            .execute_batch(BATCHES_SCHEMA)
            .expect("batch table");
        connection
    }

    fn insert(connection: &Connection, id: &str, body: rusqlite::types::Value) {
        connection
            .execute(
                "INSERT INTO runtime_item_fulltext(id,body) VALUES(?1,?2)",
                params![id, body],
            )
            .expect("row");
    }

    fn body(id: usize) -> String {
        format!("{{\"aggregatedOutput\":\"line {id} \u{2713}\\n\"}}").repeat(1_000)
    }

    fn stored(connection: &Connection, id: &str) -> (String, String) {
        connection
            .query_row(
                "SELECT typeof(body),body FROM runtime_item_fulltext WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get::<_, StoredText>(1)?.into_string())),
            )
            .expect("stored row")
    }

    fn expected_hash(values: &[(i64, &str)]) -> String {
        let mut hash = Sha256::new();
        for (rowid, text) in values {
            hash.update(rowid.to_le_bytes());
            hash.update((text.len() as u64).to_le_bytes());
            hash.update(text.as_bytes());
        }
        data_encoding::HEXLOWER.encode(&hash.finalize())
    }

    #[test]
    fn compaction_converts_plain_values_and_records_their_hash() {
        let mut connection = database();
        let compressed = crate::stored_text::encode(&body(9));
        insert(&connection, "a", rusqlite::types::Value::Text(body(1)));
        insert(
            &connection,
            "b",
            rusqlite::types::Value::Blob(compressed.clone()),
        );
        insert(&connection, "c", rusqlite::types::Value::Text(body(3)));

        let window = scan_window(&connection, FULLTEXT_BODY, Direction::Compress, 0).expect("scan");
        assert_eq!(
            window.candidates.len(),
            2,
            "only plain values are candidates"
        );
        assert!(window.done);
        let transaction = connection.transaction().expect("transaction");
        let batch = apply_window(&transaction, FULLTEXT_BODY, &window, NOW).expect("apply");
        transaction.commit().expect("commit");

        assert_eq!((batch.converted, batch.skipped), (2, 0));
        assert_eq!(batch.plain_bytes, (body(1).len() + body(3).len()) as u64);
        assert!(batch.stored_bytes * 10 < batch.plain_bytes);
        assert_eq!(stored(&connection, "a"), ("blob".to_owned(), body(1)));
        assert_eq!(stored(&connection, "c"), ("blob".to_owned(), body(3)));
        let untouched: Vec<u8> = connection
            .query_row(
                "SELECT body FROM runtime_item_fulltext WHERE id='b'",
                [],
                |row| row.get(0),
            )
            .expect("compressed row");
        assert_eq!(untouched, compressed);
        let (rows, first, last, hash): (i64, i64, i64, String) = connection
            .query_row(
                "SELECT rows,first_rowid,last_rowid,sha256 FROM runtime_stored_text_batches \
                 WHERE name=?1 AND batch=1",
                [FULLTEXT_BODY.progress],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("batch row");
        assert_eq!((rows, first, last), (2, 1, 3));
        assert_eq!(hash, expected_hash(&[(1, &body(1)), (3, &body(3))]));
        assert_eq!(
            progress(&connection, FULLTEXT_BODY).expect("progress"),
            CompactionProgress {
                cursor: COMPACTION_WINDOW_ROWS,
                complete: true
            }
        );
        let window =
            scan_window(&connection, FULLTEXT_BODY, Direction::Compress, 0).expect("rescan");
        assert!(
            window.candidates.is_empty(),
            "a repeated window converts nothing"
        );
    }

    #[test]
    fn inflate_restores_plain_text_for_an_older_release() {
        let mut connection = database();
        insert(&connection, "a", rusqlite::types::Value::Text(body(1)));
        insert(&connection, "b", rusqlite::types::Value::Text(body(2)));
        for direction in [Direction::Compress, Direction::Inflate] {
            let window = scan_window(&connection, FULLTEXT_BODY, direction, 0).expect("scan");
            assert_eq!(window.candidates.len(), 2, "{direction:?}");
            let transaction = connection.transaction().expect("transaction");
            let batch = apply_window(&transaction, FULLTEXT_BODY, &window, NOW).expect("apply");
            assert_eq!(batch.converted, 2, "{direction:?}");
            transaction.commit().expect("commit");
        }
        assert_eq!(stored(&connection, "a"), ("text".to_owned(), body(1)));
        assert_eq!(stored(&connection, "b"), ("text".to_owned(), body(2)));
        let hashes: Vec<(String, String)> = connection
            .prepare("SELECT name,sha256 FROM runtime_stored_text_batches ORDER BY name")
            .expect("statement")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("batch rows");
        let expected = expected_hash(&[(1, &body(1)), (2, &body(2))]);
        assert_eq!(
            hashes,
            vec![
                (FULLTEXT_BODY.progress.to_owned(), expected.clone()),
                (format!("{}:inflate", FULLTEXT_BODY.progress), expected)
            ],
            "both directions hash the same texts"
        );
        let transaction = connection.transaction().expect("transaction");
        reset_progress(&transaction, FULLTEXT_BODY).expect("reset");
        transaction.commit().expect("commit");
        assert_eq!(
            progress(&connection, FULLTEXT_BODY).expect("progress"),
            CompactionProgress::default()
        );
    }

    #[test]
    fn compaction_skips_a_value_changed_after_the_scan() {
        let mut connection = database();
        insert(&connection, "a", rusqlite::types::Value::Text(body(1)));
        let window = scan_window(&connection, FULLTEXT_BODY, Direction::Compress, 0).expect("scan");
        connection
            .execute(
                "UPDATE runtime_item_fulltext SET body=?1 WHERE id='a'",
                [body(2)],
            )
            .expect("writer changes the row");
        let transaction = connection.transaction().expect("transaction");
        let batch = apply_window(&transaction, FULLTEXT_BODY, &window, NOW).expect("apply");
        transaction.commit().expect("commit");
        assert_eq!((batch.converted, batch.skipped), (0, 1));
        assert_eq!(stored(&connection, "a"), ("text".to_owned(), body(2)));
        let batches: i64 = connection
            .query_row(
                "SELECT count(*) FROM runtime_stored_text_batches",
                [],
                |row| row.get(0),
            )
            .expect("batch count");
        assert_eq!(batches, 0);
    }

    #[test]
    fn compaction_rolls_back_when_a_read_back_value_differs() {
        let mut connection = database();
        insert(&connection, "a", rusqlite::types::Value::Text(body(1)));
        // A damaged write: the stored value is replaced after the update.
        connection
            .execute_batch(
                "CREATE TRIGGER damage AFTER UPDATE ON runtime_item_fulltext BEGIN \
                 UPDATE runtime_item_fulltext SET body=x'01' WHERE rowid=NEW.rowid; END;",
            )
            .expect("trigger");
        let window = scan_window(&connection, FULLTEXT_BODY, Direction::Compress, 0).expect("scan");
        let transaction = connection.transaction().expect("transaction");
        let error = apply_window(&transaction, FULLTEXT_BODY, &window, NOW)
            .expect_err("a damaged value refuses the batch");
        drop(transaction);
        assert!(matches!(
            error,
            StoreError::InvalidStoredObject { table: "runtime_item_fulltext", id } if id == "a"
        ));
        assert_eq!(stored(&connection, "a"), ("text".to_owned(), body(1)));
        assert_eq!(
            progress(&connection, FULLTEXT_BODY).expect("progress"),
            CompactionProgress::default()
        );
    }

    #[test]
    fn compaction_windows_are_bounded_and_resume_after_the_last_converted_row() {
        let mut connection = database();
        for index in 0..COMPACTION_BATCH_ROWS + 5 {
            insert(
                &connection,
                &format!("r{index:03}"),
                rusqlite::types::Value::Text(body(index)),
            );
        }
        let first =
            scan_window(&connection, FULLTEXT_BODY, Direction::Compress, 0).expect("first window");
        assert_eq!(first.candidates.len(), COMPACTION_BATCH_ROWS);
        assert_eq!(first.next_cursor, COMPACTION_BATCH_ROWS as i64);
        assert!(!first.done);
        let transaction = connection.transaction().expect("transaction");
        apply_window(&transaction, FULLTEXT_BODY, &first, NOW).expect("first batch");
        transaction.commit().expect("commit");
        assert_eq!(
            progress(&connection, FULLTEXT_BODY).expect("progress"),
            CompactionProgress {
                cursor: COMPACTION_BATCH_ROWS as i64,
                complete: false
            }
        );
        let second = scan_window(
            &connection,
            FULLTEXT_BODY,
            Direction::Compress,
            first.next_cursor,
        )
        .expect("second");
        assert_eq!(second.candidates.len(), 5);
        assert!(second.done);

        // One value above the byte budget is converted alone.
        let connection = database();
        let large = "x".repeat(COMPACTION_BATCH_BYTES + 1);
        insert(&connection, "large", rusqlite::types::Value::Text(large));
        insert(&connection, "next", rusqlite::types::Value::Text(body(1)));
        let window =
            scan_window(&connection, FULLTEXT_BODY, Direction::Compress, 0).expect("large window");
        assert_eq!(window.candidates.len(), 1);
        assert_eq!(window.next_cursor, 1);
    }
}
