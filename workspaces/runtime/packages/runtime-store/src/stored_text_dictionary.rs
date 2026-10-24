//! Trained zstd dictionaries for stored-text columns, and the SQL access that
//! lets queries read compressed values.
//!
//! A dictionary is trained from this state's own values, so it contains
//! fragments of user data. It lives only in the state database, in
//! `analytics_stored_text_dictionaries`, and never in the repository. A frame
//! names its dictionary by id, so rows of that table are never changed or
//! deleted. A process keeps every dictionary it has loaded for as long as it
//! runs (they are few and immutable): ids come from the dictionary content,
//! and a second dictionary with a known id but other bytes is refused.
//!
//! Owned column with a dictionary: `analytics_items.record`. Every analytics
//! connection that Studio opens gets the `stored_text(value)` SQL function and
//! a temporary view `temp.analytics_items` with the same columns, whose
//! `record` is the decoded text. The view shadows the table for unqualified
//! names, so readers keep their SQL; writers name `main.analytics_items`, and
//! an unqualified write fails because a view cannot be written.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use rusqlite::functions::FunctionFlags;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use zstd_safe::{CCtx, CDict, CParameter, DDict};

use crate::stored_text::{
    self, STORED_TEXT_FORMAT_ZSTD, STORED_TEXT_ZSTD_LEVEL, StoredTextError, StoredTextValue,
};
use crate::{CachedSql, StoreError, StoreResult};

/// Dictionary name of `analytics_items.record`.
pub const ANALYTICS_ITEMS_RECORD: &str = "analytics_items.record";
/// Largest trained dictionary: zstd's default size. Unit: bytes.
pub const DICTIONARY_BYTES: usize = 112_640;
/// Rows that a table needs before its dictionary is trained. Unit: rows.
pub const DICTIONARY_MIN_ROWS: i64 = 2_048;
/// Values sampled to train one dictionary. Unit: rows.
pub const DICTIONARY_SAMPLE_ROWS: usize = 4_096;

const DICTIONARY_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS analytics_stored_text_dictionaries (
        id INTEGER PRIMARY KEY, name TEXT NOT NULL, created REAL NOT NULL,
        sha256 TEXT NOT NULL, dictionary BLOB NOT NULL);
    CREATE INDEX IF NOT EXISTS analytics_stored_text_dictionaries_name
        ON analytics_stored_text_dictionaries(name, created, id);
    CREATE TRIGGER IF NOT EXISTS analytics_stored_text_dictionaries_no_update
        BEFORE UPDATE ON analytics_stored_text_dictionaries BEGIN
        SELECT RAISE(ABORT, 'stored text dictionaries are immutable'); END;
    CREATE TRIGGER IF NOT EXISTS analytics_stored_text_dictionaries_no_delete
        BEFORE DELETE ON analytics_stored_text_dictionaries BEGIN
        SELECT RAISE(ABORT, 'stored text dictionaries are immutable'); END;
    CREATE TRIGGER IF NOT EXISTS analytics_stored_text_dictionaries_no_replace
        BEFORE INSERT ON analytics_stored_text_dictionaries
        WHEN EXISTS(SELECT 1 FROM analytics_stored_text_dictionaries WHERE id=NEW.id) BEGIN
        SELECT RAISE(ABORT, 'stored text dictionaries are immutable'); END;
";

/// Columns of `analytics_items`, in table order. The temporary view repeats
/// them with `record` decoded.
const ANALYTICS_ITEMS_COLUMNS: &[&str] = &[
    "id", "agent", "root", "thread", "turn", "at", "type", "name", "is_tool", "record",
];

/// One loaded dictionary.
pub struct Dictionary {
    pub id: u32,
    bytes: Vec<u8>,
    compress: CDict<'static>,
    decompress: DDict<'static>,
}

impl std::fmt::Debug for Dictionary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Dictionary")
            .field("id", &self.id)
            .field("bytes", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

fn registry() -> &'static RwLock<HashMap<u32, &'static Dictionary>> {
    static REGISTRY: OnceLock<RwLock<HashMap<u32, &'static Dictionary>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// The loaded dictionary with `id`.
pub fn dictionary(id: u32) -> Option<&'static Dictionary> {
    registry().read().ok()?.get(&id).copied()
}

/// Load `bytes` as dictionary `id`. Loading the same dictionary again returns
/// the loaded one.
pub fn register(id: u32, bytes: Vec<u8>) -> Result<&'static Dictionary, StoredTextError> {
    if zstd_safe::get_dict_id_from_dict(&bytes).map(|value| value.get()) != Some(id) {
        return Err(StoredTextError::BadDictionary(id));
    }
    let mut registry = registry()
        .write()
        .map_err(|_| StoredTextError::BadDictionary(id))?;
    if let Some(loaded) = registry.get(&id) {
        return if loaded.bytes == bytes {
            Ok(*loaded)
        } else {
            Err(StoredTextError::BadDictionary(id))
        };
    }
    let compress = CDict::try_create(&bytes, STORED_TEXT_ZSTD_LEVEL)
        .ok_or(StoredTextError::BadDictionary(id))?;
    let decompress = DDict::try_create(&bytes).ok_or(StoredTextError::BadDictionary(id))?;
    // Loaded dictionaries stay for the life of the process: frames refer to
    // them by id, and compression contexts borrow them for 'static.
    let loaded: &'static Dictionary = Box::leak(Box::new(Dictionary {
        id,
        bytes,
        compress,
        decompress,
    }));
    registry.insert(id, loaded);
    Ok(loaded)
}

impl Dictionary {
    pub(crate) fn decompressor(&self) -> &DDict<'static> {
        &self.decompress
    }
}

thread_local! {
    static DICTIONARY_COMPRESSOR: RefCell<Option<CCtx<'static>>> = const { RefCell::new(None) };
    /// The last BLOB that `stored_text()` decoded on this thread and its text.
    /// SQLite calls the function once per reference to the column, so a query
    /// that reads several JSON fields of one row decodes the row once.
    static LAST_DECODED: RefCell<Option<(Vec<u8>, String)>> = const { RefCell::new(None) };
}

/// Encode `text` in stored format 1 with `dictionary`.
pub fn encode_with(text: &str, dictionary: &'static Dictionary) -> Vec<u8> {
    DICTIONARY_COMPRESSOR.with_borrow_mut(|slot| {
        let context = slot.get_or_insert_with(|| {
            let mut context = CCtx::create();
            for parameter in [
                CParameter::ContentSizeFlag(true),
                CParameter::ChecksumFlag(true),
                CParameter::DictIdFlag(true),
            ] {
                context.set_parameter(parameter).unwrap_or_else(|code| {
                    panic!(
                        "zstd parameter refused: {}",
                        zstd_safe::get_error_name(code)
                    )
                });
            }
            context
        });
        context
            .ref_cdict(&dictionary.compress)
            .unwrap_or_else(|code| {
                panic!(
                    "zstd dictionary refused: {}",
                    zstd_safe::get_error_name(code)
                )
            });
        let mut frame = Vec::with_capacity(zstd_safe::compress_bound(text.len()) + 1);
        context
            .compress2(&mut frame, text.as_bytes())
            .unwrap_or_else(|code| {
                panic!(
                    "zstd compression failed: {}",
                    zstd_safe::get_error_name(code)
                )
            });
        frame.insert(0, STORED_TEXT_FORMAT_ZSTD);
        frame
    })
}

/// Train a dictionary from sample values. Returns `None` when zstd cannot
/// build one from these samples.
pub fn train(samples: &[Vec<u8>]) -> Option<Vec<u8>> {
    let sizes = samples.iter().map(Vec::len).collect::<Vec<_>>();
    let joined = samples.concat();
    // zstd needs much more sample text than dictionary space; a small table
    // gets a smaller dictionary.
    let capacity = DICTIONARY_BYTES.min(joined.len() / 8).max(1_024);
    let mut dictionary = Vec::with_capacity(capacity);
    zstd_safe::train_from_buffer(&mut dictionary, &joined, &sizes).ok()?;
    zstd_safe::get_dict_id_from_dict(&dictionary)?;
    Some(dictionary)
}

/// Create the dictionary, progress and batch tables of the analytics
/// database.
pub fn ensure_analytics_schema(connection: &Connection) -> StoreResult<()> {
    connection.execute_batch(DICTIONARY_SCHEMA)?;
    connection.execute_batch(crate::feature_schema::PAYLOAD_MIGRATIONS_SCHEMA)?;
    connection.execute_batch(crate::stored_text_compaction::BATCHES_SCHEMA)?;
    Ok(())
}

/// Load every dictionary stored in this analytics database.
pub fn load(connection: &Connection) -> StoreResult<usize> {
    if !table_exists(connection, "analytics_stored_text_dictionaries")? {
        return Ok(0);
    }
    let mut statement = connection.prepare_cached(
        "SELECT id,dictionary,sha256 FROM main.analytics_stored_text_dictionaries ORDER BY created,id",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (id, bytes, hash) in &rows {
        if crate::sha256_hex(bytes) != *hash {
            return Err(bad_dictionary_row(*id));
        }
        let id = u32::try_from(*id).map_err(|_| bad_dictionary_row(*id))?;
        register(id, bytes.clone()).map_err(|_| bad_dictionary_row(i64::from(id)))?;
    }
    Ok(rows.len())
}

/// The newest dictionary of `name` in this database, loaded. Only the id is
/// read while the dictionary is already loaded.
pub fn current(connection: &Connection, name: &str) -> StoreResult<Option<&'static Dictionary>> {
    if !table_exists(connection, "analytics_stored_text_dictionaries")? {
        return Ok(None);
    }
    let id: Option<i64> = connection
        .query_row_cached(
            "SELECT id FROM main.analytics_stored_text_dictionaries \
             WHERE name=?1 ORDER BY created DESC,id DESC LIMIT 1",
            [name],
            |row| row.get(0),
        )
        .optional()?;
    let Some(id) = id else {
        return Ok(None);
    };
    let id32 = u32::try_from(id).map_err(|_| bad_dictionary_row(id))?;
    if let Some(loaded) = dictionary(id32) {
        return Ok(Some(loaded));
    }
    let bytes: Vec<u8> = connection.query_row_cached(
        "SELECT dictionary FROM main.analytics_stored_text_dictionaries WHERE id=?1",
        [id],
        |row| row.get(0),
    )?;
    register(id32, bytes)
        .map(Some)
        .map_err(|_| bad_dictionary_row(id))
}

/// Store a newly trained dictionary of `name` and load it.
pub fn insert(
    transaction: &Transaction<'_>,
    name: &str,
    bytes: Vec<u8>,
    now: f64,
) -> StoreResult<&'static Dictionary> {
    let id = zstd_safe::get_dict_id_from_dict(&bytes)
        .map(|value| value.get())
        .ok_or_else(|| bad_dictionary_row(0))?;
    let loaded = register(id, bytes).map_err(|_| bad_dictionary_row(i64::from(id)))?;
    transaction.execute_cached(
        "INSERT INTO main.analytics_stored_text_dictionaries(id,name,created,sha256,dictionary) \
         VALUES(?1,?2,?3,?4,?5)",
        params![
            i64::from(id),
            name,
            now,
            crate::sha256_hex(&loaded.bytes),
            loaded.bytes
        ],
    )?;
    Ok(loaded)
}

/// The value that an `analytics_items.record` writer stores: compressed with
/// the newest dictionary of this database, or plain TEXT before the first
/// dictionary exists (compressed without one in the compress-every-write
/// test mode).
pub fn value_for_write<'a>(
    connection: &Connection,
    name: &str,
    text: &'a str,
) -> StoreResult<StoredTextValue<'a>> {
    Ok(match current(connection, name)? {
        Some(dictionary) => StoredTextValue::Compressed(encode_with(text, dictionary)),
        None => StoredTextValue::for_write(text),
    })
}

/// Give an analytics connection the `stored_text()` function and the
/// temporary `analytics_items` view, and load its dictionaries. Call it before
/// `PRAGMA query_only`.
pub fn install_analytics_sql(connection: &Connection) -> StoreResult<()> {
    connection.create_scalar_function(
        "stored_text",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            sql_decode(context.get_raw(0))
                .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))
        },
    )?;
    if table_exists(connection, "analytics_items")? {
        let columns = ANALYTICS_ITEMS_COLUMNS
            .iter()
            .map(|column| {
                if *column == "record" {
                    "stored_text(record) AS record".to_owned()
                } else {
                    format!("\"{column}\"")
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        connection.execute_batch(&format!(
            "CREATE TEMP VIEW IF NOT EXISTS analytics_items AS \
             SELECT {columns} FROM main.analytics_items"
        ))?;
    }
    load(connection)?;
    Ok(())
}

/// `stored_text(value)`: the text of an owned value in either stored form.
fn sql_decode(value: ValueRef<'_>) -> Result<Option<String>, StoredTextError> {
    match value {
        ValueRef::Null => Ok(None),
        ValueRef::Blob(bytes) => LAST_DECODED.with_borrow_mut(|last| {
            if let Some((input, text)) = last.as_ref()
                && input.as_slice() == bytes
            {
                return Ok(Some(text.clone()));
            }
            let text = stored_text::decode_blob(bytes)?;
            *last = Some((bytes.to_vec(), text.clone()));
            Ok(Some(text))
        }),
        other => stored_text::decode(other).map(Some),
    }
}

fn table_exists(connection: &Connection, table: &str) -> StoreResult<bool> {
    Ok(connection.query_row_cached(
        "SELECT EXISTS(SELECT 1 FROM main.sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
}

fn bad_dictionary_row(id: i64) -> StoreError {
    StoreError::InvalidStoredObject {
        table: "analytics_stored_text_dictionaries",
        id: id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(index: usize) -> String {
        serde_json::json!({
            "id": format!("item-{index:06}"),
            "agentId": format!("agent-{:03}", index % 17),
            "rootId": "root-a",
            "threadId": format!("thread-{:03}", index % 29),
            "turnId": format!("turn-{index:05}"),
            "type": if index.is_multiple_of(3) { "commandExecution" } else { "agentMessage" },
            "input": {"bytes": index * 7, "chars": index * 5},
            "output": {"bytes": index * 11, "chars": index * 9, "imageCount": 0},
            "payloadBoundary": "protocol",
            "durationMs": index % 1_000,
            "status": "completed",
        })
        .to_string()
    }

    fn trained() -> Vec<u8> {
        let samples = (0..3_000)
            .map(|index| record(index).into_bytes())
            .collect::<Vec<_>>();
        train(&samples).expect("dictionary trains")
    }

    fn analytics() -> Connection {
        let connection = Connection::open_in_memory().expect("database");
        initialize_analytics(connection)
    }

    fn initialize_analytics(connection: Connection) -> Connection {
        let snapshot: serde_json::Value =
            serde_json::from_str(include_str!("runtime-state-schema.json")).expect("schema source");
        let table = snapshot["analytics.sqlite3"]["sqlite_master"]
            .as_array()
            .expect("schema objects")
            .iter()
            .find(|object| object["name"] == "analytics_items")
            .expect("analytics_items schema owner");
        connection
            .execute_batch(table["sql"].as_str().expect("table schema"))
            .expect("table");
        ensure_analytics_schema(&connection).expect("schema");
        install_analytics_sql(&connection).expect("sql");
        connection
    }

    #[test]
    fn dictionary_records_survive_a_process_restart() {
        const FIXTURE_ENV: &str = "STUDIO_STORED_TEXT_RESTART_FIXTURE";
        let text = record(12_345);
        if let Some(path) = std::env::var_os(FIXTURE_ENV) {
            let path = std::path::PathBuf::from(path);
            let connection =
                Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .expect("persisted database");
            let id: u32 = connection
                .query_row(
                    "SELECT id FROM analytics_stored_text_dictionaries",
                    [],
                    |row| row.get(0),
                )
                .expect("persisted dictionary id");
            assert!(
                dictionary(id).is_none(),
                "the new process starts without a dictionary"
            );
            install_analytics_sql(&connection).expect("load persisted dictionaries and view");
            connection
                .pragma_update(None, "query_only", true)
                .expect("read only");
            let records = connection
                .prepare("SELECT record FROM analytics_items ORDER BY id")
                .expect("view")
                .query_map([], |row| row.get::<_, String>(0))
                .expect("rows")
                .collect::<Result<Vec<_>, _>>()
                .expect("decoded rows");
            assert_eq!(records, vec![text; 3]);
            std::fs::write(path.with_extension("verified"), b"3")
                .expect("record the verified row count");
            return;
        }
        let directory = tempfile::tempdir().expect("temporary state");
        let path = directory.path().join("analytics.sqlite3");
        let connection = initialize_analytics(Connection::open(&path).expect("database"));
        let transaction = connection.unchecked_transaction().expect("transaction");
        let dictionary = insert(&transaction, ANALYTICS_ITEMS_RECORD, trained(), 1.0)
            .expect("persist dictionary");
        transaction.commit().expect("commit");
        insert_item(&connection, "plain", StoredTextValue::Plain(&text));
        insert_item(&connection, "old-blob", StoredTextValue::compressed(&text));
        insert_item(
            &connection,
            "dictionary",
            StoredTextValue::Compressed(encode_with(&text, dictionary)),
        );
        drop(connection);
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "stored_text_dictionary::tests::dictionary_records_survive_a_process_restart",
            ])
            .env(FIXTURE_ENV, &path)
            .output()
            .expect("restart reader");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read(path.with_extension("verified")).expect("the child verified rows"),
            b"3"
        );
    }

    #[test]
    fn decoding_view_columns_match_the_owned_schema() {
        let connection = analytics();
        for table in ["main.analytics_items", "temp.analytics_items"] {
            let (schema, table) = table.split_once('.').expect("qualified name");
            let columns = connection
                .prepare(&format!("PRAGMA {schema}.table_info({table})"))
                .expect("columns")
                .query_map([], |row| row.get::<_, String>(1))
                .expect("rows")
                .collect::<Result<Vec<_>, _>>()
                .expect("column names");
            assert_eq!(
                columns, ANALYTICS_ITEMS_COLUMNS,
                "the view must preserve every column from runtime-state-schema.json"
            );
        }
    }

    fn insert_item(connection: &Connection, id: &str, value: StoredTextValue<'_>) {
        connection
            .execute(
                "INSERT INTO main.analytics_items VALUES (?1,'agent',NULL,NULL,NULL,1.0,'agentMessage',NULL,0,?2)",
                params![id, value],
            )
            .expect("item row");
    }

    #[test]
    fn dictionary_frames_round_trip_and_name_their_dictionary() {
        let bytes = trained();
        let id = zstd_safe::get_dict_id_from_dict(&bytes)
            .expect("dictionary id")
            .get();
        let dictionary = register(id, bytes.clone()).expect("dictionary loads");
        assert!(std::ptr::eq(
            dictionary,
            register(id, bytes.clone()).expect("same again")
        ));
        let text = record(12_345);
        let stored = encode_with(&text, dictionary);
        assert_eq!(
            zstd_safe::get_dict_id_from_frame(&stored[1..]).map(|id| id.get()),
            Some(id)
        );
        assert_eq!(stored_text::decode_blob(&stored), Ok(text.clone()));
        assert!(
            stored.len() < stored_text::encode(&text).len(),
            "the dictionary helps"
        );

        let mut other = bytes;
        let last = other.len() - 1;
        other[last] ^= 0xff;
        assert_eq!(
            register(id, other).map(|_| ()),
            Err(StoredTextError::BadDictionary(id))
        );
    }

    #[test]
    fn analytics_connections_read_compressed_records_through_the_view() {
        let connection = analytics();
        let text = record(7);
        insert_item(&connection, "plain", StoredTextValue::Plain(&text));
        insert_item(
            &connection,
            "compressed",
            StoredTextValue::Compressed(stored_text::encode(&text)),
        );
        let transaction = connection.unchecked_transaction().expect("transaction");
        let dictionary =
            insert(&transaction, ANALYTICS_ITEMS_RECORD, trained(), 1.0).expect("stored");
        transaction.commit().expect("commit");
        insert_item(
            &connection,
            "dictionary",
            StoredTextValue::Compressed(encode_with(&text, dictionary)),
        );

        let rows: Vec<(String, String, String, i64)> = connection
            .prepare(
                "SELECT id,record,json_extract(record,'$.type'),json_extract(record,'$.output.bytes') \
                 FROM analytics_items ORDER BY id",
            )
            .expect("view query")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("rows read");
        assert_eq!(rows.len(), 3);
        for (_, raw, kind, bytes) in rows {
            assert_eq!(raw, text);
            assert_eq!(kind, "agentMessage");
            assert_eq!(bytes, 77);
        }
        let kinds: Vec<String> = connection
            .prepare("SELECT typeof(record) FROM main.analytics_items ORDER BY id")
            .expect("table query")
            .query_map([], |row| row.get(0))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("kinds");
        assert_eq!(kinds, ["blob", "blob", "text"]);
        let error = connection
            .execute(
                "INSERT INTO analytics_items VALUES ('x','agent',NULL,NULL,NULL,1.0,NULL,NULL,0,'{}')",
                [],
            )
            .expect_err("the decoding view refuses writes");
        assert!(error.to_string().contains("view"), "{error}");
    }

    #[test]
    fn dictionary_rows_refuse_changes_and_load_verifies_the_hash() {
        let connection = analytics();
        let transaction = connection.unchecked_transaction().expect("transaction");
        let dictionary =
            insert(&transaction, ANALYTICS_ITEMS_RECORD, trained(), 1.0).expect("dictionary");
        transaction.commit().expect("commit");
        for sql in [
            "UPDATE analytics_stored_text_dictionaries SET dictionary=x'00'",
            "DELETE FROM analytics_stored_text_dictionaries",
            "INSERT OR REPLACE INTO analytics_stored_text_dictionaries SELECT * FROM analytics_stored_text_dictionaries",
        ] {
            assert!(connection.execute(sql, []).is_err(), "{sql} must fail");
        }
        let other = analytics();
        other
            .execute(
                "INSERT INTO analytics_stored_text_dictionaries VALUES (?1,?2,1.0,'wrong',?3)",
                params![dictionary.id, ANALYTICS_ITEMS_RECORD, dictionary.bytes],
            )
            .expect("damaged hash fixture");
        assert!(load(&other).is_err(), "an invalid persisted hash must fail");
    }

    #[test]
    fn writers_use_the_newest_dictionary_of_their_own_database() {
        let connection = analytics();
        let text = record(3);
        assert_eq!(
            value_for_write(&connection, ANALYTICS_ITEMS_RECORD, &text).expect("value"),
            StoredTextValue::for_write(&text),
            "before a dictionary exists the writer keeps its default form"
        );
        let transaction = connection.unchecked_transaction().expect("transaction");
        let dictionary =
            insert(&transaction, ANALYTICS_ITEMS_RECORD, trained(), 1.0).expect("stored");
        transaction.commit().expect("commit");
        let StoredTextValue::Compressed(stored) =
            value_for_write(&connection, ANALYTICS_ITEMS_RECORD, &text).expect("value")
        else {
            panic!("a dictionary compresses the value");
        };
        assert_eq!(
            zstd_safe::get_dict_id_from_frame(&stored[1..]).map(|id| id.get()),
            Some(dictionary.id)
        );
        // Another database without the dictionary keeps its default form.
        let other = analytics();
        assert_eq!(
            value_for_write(&other, ANALYTICS_ITEMS_RECORD, &text).expect("value"),
            StoredTextValue::for_write(&text)
        );
        assert_eq!(load(&connection).expect("load"), 1);
    }
}
