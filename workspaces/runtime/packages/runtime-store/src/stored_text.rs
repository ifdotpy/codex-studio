//! Owner of compressed text values in SQLite TEXT columns.
//!
//! A column owned by this module holds either plain TEXT, as older releases
//! wrote it, or a BLOB in stored format 1: one version byte
//! ([`STORED_TEXT_FORMAT_ZSTD`]) followed by exactly one zstd frame that
//! declares its content size and carries a content checksum. Readers accept
//! both forms and return the exact text that was written. SQLite keeps a BLOB
//! as a BLOB in a TEXT-affinity column, so a reader that does not use this
//! module fails with a type error instead of reading compressed bytes as text.
//!
//! A frame may name a trained dictionary (`stored_text_dictionary`); the
//! dictionary must be loaded to decode it.
//!
//! Owned columns: `runtime_item_fulltext.body` (no dictionary) and
//! `analytics_items.record` (with a dictionary). Writers of the first store
//! stored format 1, and `stored_text_compaction` converts older plain values.

use std::cell::RefCell;
use std::sync::OnceLock;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use thiserror::Error;
use zstd_safe::{CCtx, CParameter, DCtx};

/// First byte of a stored-format-1 BLOB: a zstd frame follows.
pub const STORED_TEXT_FORMAT_ZSTD: u8 = 1;

/// Largest decoded value that a reader accepts, in bytes. This is SQLite's
/// default maximum length of one value (`SQLITE_MAX_LENGTH`), so no valid
/// plain TEXT value was ever larger. A frame that declares more is refused
/// before any memory is reserved for it.
pub const STORED_TEXT_MAX_DECODED_BYTES: u64 = 1_000_000_000;

/// zstd compression level for new values. Measured on 2026-10-23 transcript
/// bodies: level 3 kept 25% of the bytes at about 300 MB/s; level 19 kept 24%
/// at about 3 MB/s.
pub const STORED_TEXT_ZSTD_LEVEL: i32 = 3;

/// Environment variable of the test mode that compresses every value that a
/// writer stores in an owned column, including writers that still write plain
/// TEXT by default. The value `1` enables it. Run a suite with it to check that
/// every reader decodes compressed values.
pub const COMPRESS_EVERY_WRITE_ENV: &str = "STUDIO_STORE_COMPRESS_EVERY_WRITE";

/// Byte offset of the zstd frame header descriptor in a frame.
const FRAME_HEADER_DESCRIPTOR_OFFSET: usize = 4;
/// Content checksum bit of the zstd frame header descriptor.
const FRAME_CHECKSUM_FLAG: u8 = 0b0000_0100;

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum StoredTextError {
    #[error("stored text has SQLite type {0}, not TEXT or BLOB")]
    WrongType(&'static str),
    #[error("stored text BLOB is empty")]
    Empty,
    #[error("stored text BLOB has unknown format {0}")]
    UnknownFormat(u8),
    #[error("stored text frame is damaged")]
    Damaged,
    #[error("stored text frame does not declare its content size")]
    UnknownSize,
    #[error("stored text frame has no content checksum")]
    NoChecksum,
    #[error("stored text frame declares {0} bytes, more than the limit")]
    TooLarge(u64),
    #[error("stored text frame needs dictionary {0}, which is not loaded")]
    DictionaryRequired(u32),
    #[error("stored text dictionary {0} is invalid or conflicts with a loaded one")]
    BadDictionary(u32),
    #[error("stored text is not UTF-8")]
    NotUtf8,
}

thread_local! {
    static COMPRESSOR: RefCell<Option<CCtx<'static>>> = const { RefCell::new(None) };
    static DECOMPRESSOR: RefCell<Option<DCtx<'static>>> = const { RefCell::new(None) };
}

/// True when the compress-every-write test mode is enabled for this process.
pub fn compress_every_write() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var(COMPRESS_EVERY_WRITE_ENV).is_ok_and(|value| value == "1"))
}

/// Encode `text` in stored format 1.
pub fn encode(text: &str) -> Vec<u8> {
    COMPRESSOR.with_borrow_mut(|slot| {
        let context = slot.get_or_insert_with(new_compressor);
        let mut frame = Vec::with_capacity(zstd_safe::compress_bound(text.len()) + 1);
        // The bound is large enough for any input, so compression cannot run
        // out of room; any other error is a defect in the zstd context.
        let written = context
            .compress2(&mut frame, text.as_bytes())
            .unwrap_or_else(|code| {
                panic!(
                    "zstd compression failed: {}",
                    zstd_safe::get_error_name(code)
                )
            });
        debug_assert_eq!(written, frame.len());
        frame.insert(0, STORED_TEXT_FORMAT_ZSTD);
        frame
    })
}

fn new_compressor() -> CCtx<'static> {
    let mut context = CCtx::create();
    for parameter in [
        CParameter::CompressionLevel(STORED_TEXT_ZSTD_LEVEL),
        CParameter::ContentSizeFlag(true),
        CParameter::ChecksumFlag(true),
    ] {
        context.set_parameter(parameter).unwrap_or_else(|code| {
            panic!(
                "zstd parameter refused: {}",
                zstd_safe::get_error_name(code)
            )
        });
    }
    context
}

/// Decode a stored-format-1 BLOB to its text.
pub fn decode_blob(bytes: &[u8]) -> Result<String, StoredTextError> {
    let (&format, frame) = bytes.split_first().ok_or(StoredTextError::Empty)?;
    if format != STORED_TEXT_FORMAT_ZSTD {
        return Err(StoredTextError::UnknownFormat(format));
    }
    if zstd_safe::find_frame_compressed_size(frame) != Ok(frame.len()) {
        return Err(StoredTextError::Damaged);
    }
    let descriptor = frame
        .get(FRAME_HEADER_DESCRIPTOR_OFFSET)
        .ok_or(StoredTextError::Damaged)?;
    if descriptor & FRAME_CHECKSUM_FLAG == 0 {
        return Err(StoredTextError::NoChecksum);
    }
    let dictionary = match zstd_safe::get_dict_id_from_frame(frame) {
        Some(id) => Some(
            crate::stored_text_dictionary::dictionary(id.get())
                .ok_or(StoredTextError::DictionaryRequired(id.get()))?,
        ),
        None => None,
    };
    let size = zstd_safe::get_frame_content_size(frame)
        .map_err(|_| StoredTextError::Damaged)?
        .ok_or(StoredTextError::UnknownSize)?;
    if size > STORED_TEXT_MAX_DECODED_BYTES {
        return Err(StoredTextError::TooLarge(size));
    }
    let size = usize::try_from(size).map_err(|_| StoredTextError::TooLarge(size))?;
    let mut text = Vec::with_capacity(size);
    let written = DECOMPRESSOR.with_borrow_mut(|slot| {
        let context = slot.get_or_insert_with(DCtx::create);
        match dictionary {
            Some(dictionary) => {
                context.decompress_using_ddict(&mut text, frame, dictionary.decompressor())
            }
            None => context.decompress(&mut text, frame),
        }
        .map_err(|_| StoredTextError::Damaged)
    })?;
    if written != size || text.len() != size {
        return Err(StoredTextError::Damaged);
    }
    String::from_utf8(text).map_err(|_| StoredTextError::NotUtf8)
}

/// Decode an owned column value: plain TEXT or a stored-format-1 BLOB.
pub fn decode(value: ValueRef<'_>) -> Result<String, StoredTextError> {
    match value {
        ValueRef::Text(text) => std::str::from_utf8(text)
            .map(str::to_owned)
            .map_err(|_| StoredTextError::NotUtf8),
        ValueRef::Blob(bytes) => decode_blob(bytes),
        ValueRef::Null => Err(StoredTextError::WrongType("NULL")),
        ValueRef::Integer(_) => Err(StoredTextError::WrongType("INTEGER")),
        ValueRef::Real(_) => Err(StoredTextError::WrongType("REAL")),
    }
}

/// Text read from an owned column, decoded from either stored form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredText(pub String);

impl StoredText {
    pub fn into_string(self) -> String {
        self.0
    }
}

impl FromSql for StoredText {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value {
            ValueRef::Text(_) | ValueRef::Blob(_) => decode(value)
                .map(Self)
                .map_err(|error| FromSqlError::Other(Box::new(error))),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}

/// A value to write to an owned column.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoredTextValue<'a> {
    Plain(&'a str),
    Compressed(Vec<u8>),
}

impl<'a> StoredTextValue<'a> {
    /// Stored format 1. `runtime_item_fulltext.body` writers always store it.
    pub fn compressed(text: &str) -> Self {
        Self::Compressed(encode(text))
    }

    /// The form a writer stores now: plain TEXT, or stored format 1 in the
    /// compress-every-write test mode.
    pub fn for_write(text: &'a str) -> Self {
        if compress_every_write() {
            Self::Compressed(encode(text))
        } else {
            Self::Plain(text)
        }
    }
}

impl ToSql for StoredTextValue<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self {
            Self::Plain(text) => ToSqlOutput::Borrowed(ValueRef::Text(text.as_bytes())),
            Self::Compressed(bytes) => ToSqlOutput::Borrowed(ValueRef::Blob(bytes)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_text_round_trips_exact_bytes() {
        for text in [
            String::new(),
            "a".to_owned(),
            "h\u{e9}llo \u{2713} \u{1f600}\n\0tail".to_owned(),
            "{\"aggregatedOutput\":\"line\\n\"}".repeat(4_000),
        ] {
            let stored = encode(&text);
            assert_eq!(stored[0], STORED_TEXT_FORMAT_ZSTD);
            assert_eq!(decode_blob(&stored), Ok(text.clone()));
            assert_eq!(decode(ValueRef::Blob(&stored)), Ok(text.clone()));
            assert_eq!(decode(ValueRef::Text(text.as_bytes())), Ok(text));
        }
    }

    #[test]
    fn stored_text_compresses_repetitive_text() {
        let text = "{\"aggregatedOutput\":\"line\\n\"}".repeat(4_000);
        assert!(encode(&text).len() * 10 < text.len());
    }

    #[test]
    fn stored_text_refuses_damaged_and_unknown_values() {
        let stored = encode("refuse damaged frames");
        let mut flipped = stored.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 0xff;
        assert_eq!(decode_blob(&flipped), Err(StoredTextError::Damaged));
        assert_eq!(
            decode_blob(&stored[..stored.len() - 1]),
            Err(StoredTextError::Damaged)
        );
        let mut trailing = stored.clone();
        trailing.push(0);
        assert_eq!(decode_blob(&trailing), Err(StoredTextError::Damaged));
        let mut unknown = stored.clone();
        unknown[0] = 2;
        assert_eq!(
            decode_blob(&unknown),
            Err(StoredTextError::UnknownFormat(2))
        );
        assert_eq!(decode_blob(&[]), Err(StoredTextError::Empty));
        assert_eq!(
            decode(ValueRef::Null),
            Err(StoredTextError::WrongType("NULL"))
        );
        assert_eq!(
            decode(ValueRef::Integer(1)),
            Err(StoredTextError::WrongType("INTEGER"))
        );
        assert_eq!(
            decode(ValueRef::Text(&[0xff])),
            Err(StoredTextError::NotUtf8)
        );
    }

    #[test]
    fn stored_text_refuses_frames_without_checksum_size_or_with_a_dictionary() {
        let mut plain = CCtx::create();
        plain
            .set_parameter(CParameter::ChecksumFlag(false))
            .expect("checksum parameter");
        let mut frame = Vec::with_capacity(256);
        plain.compress2(&mut frame, b"no checksum").expect("frame");
        frame.insert(0, STORED_TEXT_FORMAT_ZSTD);
        assert_eq!(decode_blob(&frame), Err(StoredTextError::NoChecksum));

        plain
            .set_parameter(CParameter::ChecksumFlag(true))
            .expect("checksum parameter");
        plain
            .set_parameter(CParameter::ContentSizeFlag(false))
            .expect("content size parameter");
        let mut frame = Vec::with_capacity(256);
        plain.compress2(&mut frame, b"unknown size").expect("frame");
        frame.insert(0, STORED_TEXT_FORMAT_ZSTD);
        assert_eq!(decode_blob(&frame), Err(StoredTextError::UnknownSize));

        // Hand-made frames (RFC 8878 section 3.1.1): magic, header descriptor,
        // optional fields, one empty last raw block, and a 4-byte checksum.
        const MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
        const EMPTY_LAST_RAW_BLOCK: [u8; 3] = [0x01, 0x00, 0x00];
        let over_limit = STORED_TEXT_MAX_DECODED_BYTES + 1;
        // Single segment, 4-byte content size, checksum.
        let mut huge = vec![STORED_TEXT_FORMAT_ZSTD];
        huge.extend_from_slice(&MAGIC);
        huge.push(0b1010_0100);
        huge.extend_from_slice(
            &u32::try_from(over_limit)
                .expect("4-byte size")
                .to_le_bytes(),
        );
        huge.extend_from_slice(&EMPTY_LAST_RAW_BLOCK);
        huge.extend_from_slice(&[0; 4]);
        assert_eq!(
            decode_blob(&huge),
            Err(StoredTextError::TooLarge(over_limit))
        );

        // Single segment, 1-byte dictionary id 7, 1-byte content size 0, checksum.
        let mut with_dictionary = vec![STORED_TEXT_FORMAT_ZSTD];
        with_dictionary.extend_from_slice(&MAGIC);
        with_dictionary.extend_from_slice(&[0b0010_0101, 7, 0]);
        with_dictionary.extend_from_slice(&EMPTY_LAST_RAW_BLOCK);
        with_dictionary.extend_from_slice(&[0; 4]);
        assert_eq!(
            decode_blob(&with_dictionary),
            Err(StoredTextError::DictionaryRequired(7))
        );
    }

    #[test]
    fn stored_text_value_reads_through_sqlite_in_both_forms() {
        let connection = rusqlite::Connection::open_in_memory().expect("database");
        connection
            .execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, body TEXT NOT NULL);")
            .expect("table");
        let text = "body \u{2713}".repeat(100);
        connection
            .execute(
                "INSERT INTO t(id,body) VALUES(1,?1),(2,?2)",
                rusqlite::params![
                    StoredTextValue::Plain(&text),
                    StoredTextValue::Compressed(encode(&text))
                ],
            )
            .expect("rows");
        let kinds: Vec<String> = connection
            .prepare("SELECT typeof(body) FROM t ORDER BY id")
            .expect("statement")
            .query_map([], |row| row.get(0))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("kinds");
        assert_eq!(kinds, ["text", "blob"]);
        for id in [1, 2] {
            let stored: StoredText = connection
                .query_row("SELECT body FROM t WHERE id=?1", [id], |row| row.get(0))
                .expect("stored text");
            assert_eq!(stored.into_string(), text);
        }
    }
}
