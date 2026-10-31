//! Durable client state. This database is separate from the server's canvas DB.
use crate::protocol::{Request, Response};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

// Shared contract owns UTF-8 key bytes, including the browser-owner prefix.
fn max_preference_key_bytes() -> usize {
    static LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        serde_json::from_str::<Value>(include_str!("../../../contracts/preference-storage.json"))
            .expect("valid preference storage contract")["maxKeyBytes"]
            .as_u64()
            .expect("positive preference key byte limit") as usize
    })
}

pub struct Store {
    pub(crate) connection: Connection,
}

#[derive(Default, Deserialize)]
struct RawKey<'a> {
    #[serde(borrow)]
    key: Option<&'a str>,
}
#[derive(Default, Deserialize)]
struct RawText<'a> {
    #[serde(borrow)]
    raw: Option<&'a str>,
}
#[derive(Default, Deserialize)]
struct SnapshotVersion {
    #[serde(default)]
    version: Value,
}
fn project<'a, T: Deserialize<'a> + Default>(value: &'a Value) -> T {
    if value.is_object() {
        T::deserialize(value).unwrap_or_default()
    } else {
        T::default()
    }
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(
            "BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS client_receipts (
                request_id TEXT PRIMARY KEY, method TEXT NOT NULL,
                params TEXT NOT NULL, response TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS client_preferences (
                key TEXT PRIMARY KEY, value TEXT NOT NULL);
            DROP TABLE IF EXISTS client_drafts;
            DROP TABLE IF EXISTS client_outbox;
            COMMIT;",
        )?;
        crate::projections::initialize(&connection)?;
        crate::draft_store::initialize(&connection)?;
        Ok(Self { connection })
    }

    pub fn dispatch(&mut self, request: Request) -> Response {
        if request.id.is_empty() || request.id.len() > 128 {
            return Response::failure(request.id, "invalid_request", "Invalid request identity.");
        }
        match self.execute(&request) {
            Ok(response) => response,
            Err(error) => Response::failure(request.id, "storage_error", error),
        }
    }

    fn execute(&mut self, request: &Request) -> rusqlite::Result<Response> {
        let id = request.id.clone();
        match request.method.as_str() {
            method if method.starts_with("drafts.") => {
                Ok(crate::draft_store::dispatch(&mut self.connection, request))
            }
            method if method.starts_with("projection.") || method.starts_with("identity.") => {
                Ok(crate::projections::dispatch(&self.connection, request))
            }
            "preferences.raw_all" => {
                let mut query = self
                    .connection
                    .prepare("SELECT key,value FROM client_preferences ORDER BY key")?;
                let rows = query.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                let mut values = serde_json::Map::new();
                for row in rows {
                    let (key, raw) = row?;
                    values.insert(key, json!(raw));
                }
                Ok(Response::success(id, Value::Object(values)))
            }
            "preferences.import" => self.import_preferences(request),
            "preferences.raw_set" | "preferences.raw_remove" => self.set_raw_preference(request),
            _ => Ok(Response::failure(
                id,
                "unknown_method",
                "This command has not been ported to Rust.",
            )),
        }
    }

    // Import a complete legacy snapshot in one durable transaction. Never replace
    // a value already changed by the native client. The receipt survives restart.
    fn import_preferences(&mut self, request: &Request) -> rusqlite::Result<Response> {
        let Some(entries) = request.params.get("entries").and_then(Value::as_object) else {
            return Ok(Response::failure(
                request.id.clone(),
                "invalid_params",
                "A storage snapshot is required.",
            ));
        };
        let version: SnapshotVersion = project(&request.params);
        if version.version != 1
            || request
                .params
                .as_object()
                .is_none_or(|params| params.len() != 2)
            || entries.len() > 10_000
        {
            return Ok(Response::failure(
                request.id.clone(),
                "invalid_params",
                "Invalid or oversized storage snapshot.",
            ));
        }
        let entries: BTreeMap<&str, &str> = match BTreeMap::deserialize(&request.params["entries"])
        {
            Ok(entries) => entries,
            Err(_) => {
                return Ok(Response::failure(
                    request.id.clone(),
                    "invalid_params",
                    "Invalid or oversized storage snapshot.",
                ));
            }
        };
        if entries.iter().any(|(key, raw)| {
            key.is_empty() || key.len() > max_preference_key_bytes() || raw.len() > 8 * 1024 * 1024
        }) || serde_json::to_vec(&request.params)
            .expect("JSON value serializes")
            .len()
            > 16 * 1024 * 1024
        {
            return Ok(Response::failure(
                request.id.clone(),
                "invalid_params",
                "Invalid or oversized storage snapshot.",
            ));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(response) = replay(&transaction, request)? {
            return Ok(response);
        }
        // Check every key before writing. A conflict rejects the entire snapshot.
        for (key, value) in &entries {
            let existing: Option<String> = transaction
                .query_row(
                    "SELECT value FROM client_preferences WHERE key=?1",
                    [key],
                    |row| row.get(0),
                )
                .optional()?;
            if existing.as_deref().is_some_and(|raw| raw != *value) {
                let response = Response::failure(
                    request.id.clone(),
                    "migration_conflict",
                    "Existing native data differs from the storage snapshot. No values were imported.",
                );
                record(&transaction, request, &response)?;
                transaction.commit()?;
                return Ok(response);
            }
        }
        let mut imported = 0;
        for (key, value) in &entries {
            imported += transaction.execute(
                "INSERT OR IGNORE INTO client_preferences(key,value) VALUES(?1,?2)",
                params![key, value],
            )?;
        }
        let response = Response::success(
            request.id.clone(),
            json!({"imported":imported,"total":entries.len()}),
        );
        record(&transaction, request, &response)?;
        transaction.commit()?;
        Ok(response)
    }

    fn set_raw_preference(&mut self, request: &Request) -> rusqlite::Result<Response> {
        let key: RawKey<'_> = project(&request.params);
        let raw: RawText<'_> = project(&request.params);
        let key = key
            .key
            .filter(|v| !v.is_empty() && v.len() <= max_preference_key_bytes());
        let raw = raw.raw.filter(|v| v.len() <= 8 * 1024 * 1024);
        if key.is_none() || (request.method == "preferences.raw_set" && raw.is_none()) {
            return Ok(Response::failure(
                request.id.clone(),
                "invalid_params",
                "Invalid stored value.",
            ));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(response) = replay(&transaction, request)? {
            return Ok(response);
        }
        // The validation above requires a key, and raw_set also requires its value.
        if request.method == "preferences.raw_remove" {
            transaction.execute(
                "DELETE FROM client_preferences WHERE key=?1",
                [key.expect("validated preference key")],
            )?;
        } else {
            transaction.execute("INSERT INTO client_preferences(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key.expect("validated preference key"),raw.expect("validated raw preference")])?;
        }
        let response = Response::success(request.id.clone(), json!({"saved":true}));
        record(&transaction, request, &response)?;
        transaction.commit()?;
        Ok(response)
    }
}

pub(crate) fn replay(
    connection: &Connection,
    request: &Request,
) -> rusqlite::Result<Option<Response>> {
    let receipt: Option<(String, String, String)> = connection
        .query_row(
            "SELECT method, params, response FROM client_receipts WHERE request_id=?1",
            [&request.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((method, encoded, response)) = receipt else {
        return Ok(None);
    };
    Ok(Some(
        if method == request.method
            && encoded == serde_json::to_string(&request.params).expect("JSON value serialization")
        {
            serde_json::from_str(&response).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?
        } else {
            Response::failure(
                request.id.clone(),
                "identity_conflict",
                "The request identity already has different content.",
            )
        },
    ))
}

pub(crate) fn record(
    connection: &Connection,
    request: &Request,
    response: &Response,
) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO client_receipts(request_id,method,params,response) VALUES (?1,?2,?3,?4)",
        params![
            request.id,
            request.method,
            serde_json::to_string(&request.params).expect("JSON value serialization"),
            serde_json::to_string(response).expect("response serialization"),
        ],
    )?;
    Ok(())
}

/// SQLite integers are signed; reject corrupt negative counters explicitly.
pub(crate) fn unsigned(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_tables_are_removed_on_empty_and_populated_databases() {
        for populated in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("client.sqlite3");
            if populated {
                let db = Connection::open(&path).unwrap();
                db.execute_batch("CREATE TABLE client_drafts(workspace TEXT NOT NULL, room TEXT NOT NULL, text TEXT NOT NULL, revision INTEGER NOT NULL, PRIMARY KEY(workspace,room)); INSERT INTO client_drafts VALUES('w','r','saved',1); CREATE TABLE client_outbox(workspace TEXT NOT NULL, id TEXT NOT NULL, room TEXT NOT NULL, payload TEXT NOT NULL, in_flight INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(workspace,id)); INSERT INTO client_outbox VALUES('w','id','r','saved',1); CREATE TABLE client_preferences(key TEXT PRIMARY KEY,value TEXT NOT NULL); INSERT INTO client_preferences VALUES('keep','value'); CREATE TABLE client_receipts(request_id TEXT PRIMARY KEY,method TEXT NOT NULL,params TEXT NOT NULL,response TEXT NOT NULL); INSERT INTO client_receipts VALUES('keep','preferences.import','{}','{}'); INSERT INTO client_receipts VALUES('old','draft.set','{}','{}');").unwrap();
            }
            let mut store = Store::open(&path).unwrap();
            for table in ["client_drafts", "client_outbox"] {
                let exists: bool = store
                    .connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                        [table],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert!(!exists, "{table} still exists");
            }
            if populated {
                let value: String = store
                    .connection
                    .query_row(
                        "SELECT value FROM client_preferences WHERE key='keep'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(value, "value");
                let methods: Vec<String> = store
                    .connection
                    .prepare("SELECT method FROM client_receipts ORDER BY method")
                    .unwrap()
                    .query_map([], |row| row.get(0))
                    .unwrap()
                    .collect::<Result<_, _>>()
                    .unwrap();
                assert_eq!(methods, ["draft.set", "preferences.import"]);
                let reply = store.dispatch(Request {
                    id: "old".into(),
                    method: "preferences.raw_set".into(),
                    params: json!({"key":"keep","raw":"changed"}),
                });
                assert_eq!(reply.error.unwrap().code, "identity_conflict");
            }
            drop(store);
            Store::open(&path).unwrap();
        }
    }

    #[test]
    fn retired_commands_are_unknown() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        for method in [
            "draft.get",
            "draft.set",
            "preferences.get",
            "preferences.set",
            "outbox.enqueue",
            "outbox.list",
            "actions.prepare",
        ] {
            let response = store.dispatch(Request {
                id: "test".into(),
                method: method.into(),
                params: json!({}),
            });
            assert_eq!(response.error.unwrap().code, "unknown_method", "{method}");
        }
    }

    #[test]
    fn retired_upload_commands_leave_existing_files_intact() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE client_file_sources(id TEXT PRIMARY KEY,name TEXT NOT NULL,mime TEXT NOT NULL,bytes BLOB NOT NULL); INSERT INTO client_file_sources VALUES('old','file','text/plain',X'0102'); CREATE TABLE client_uploads(id TEXT PRIMARY KEY,workspace TEXT NOT NULL,state_dir TEXT NOT NULL,agent TEXT NOT NULL,document TEXT NOT NULL,bytes BLOB NOT NULL); INSERT INTO client_uploads VALUES('old','workspace','directory','agent','saved',X'0304'); CREATE TABLE client_upload_tombstones(id TEXT PRIMARY KEY,cancelled INTEGER NOT NULL); INSERT INTO client_upload_tombstones VALUES('old',1);").unwrap();
        drop(connection);
        let mut store = Store::open(&path).unwrap();
        for method in ["uploads.list", "files.stage"] {
            let response = store.dispatch(Request {
                id: method.into(),
                method: method.into(),
                params: json!({}),
            });
            assert_eq!(response.error.unwrap().code, "unknown_method");
        }
        let source: Vec<u8> = store
            .connection
            .query_row(
                "SELECT bytes FROM client_file_sources WHERE id='old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let upload: (String, Vec<u8>) = store
            .connection
            .query_row(
                "SELECT document, bytes FROM client_uploads WHERE id='old'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let cancelled: bool = store
            .connection
            .query_row(
                "SELECT cancelled FROM client_upload_tombstones WHERE id='old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(source, [1, 2]);
        assert_eq!(upload, ("saved".into(), vec![3, 4]));
        assert!(cancelled);
    }

    #[test]
    fn retired_dictation_commands_leave_existing_recordings_intact() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE client_dictation_recordings(id TEXT PRIMARY KEY, document TEXT NOT NULL); INSERT INTO client_dictation_recordings VALUES('old', 'saved'); CREATE TABLE client_dictation_chunks(id TEXT NOT NULL, position INTEGER NOT NULL, pcm BLOB NOT NULL, PRIMARY KEY(id, position)); INSERT INTO client_dictation_chunks VALUES('old', 0, X'0102');").unwrap();
        drop(connection);
        let mut store = Store::open(&path).unwrap();
        let response = store.dispatch(Request {
            id: "retired".into(),
            method: "dictation.list".into(),
            params: json!({"chatId":"chat"}),
        });
        assert_eq!(response.error.unwrap().code, "unknown_method");
        let saved: String = store
            .connection
            .query_row(
                "SELECT document FROM client_dictation_recordings WHERE id='old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let pcm: Vec<u8> = store
            .connection
            .query_row(
                "SELECT pcm FROM client_dictation_chunks WHERE id='old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(saved, "saved");
        assert_eq!(pcm, [1, 2]);
    }

    #[test]
    fn preference_inputs_preserve_shape_order_and_raw_remove_defaults() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let request = |id: &str, method: &str, params: Value| Request {
            id: id.into(),
            method: method.into(),
            params,
        };
        for (params, expected) in [
            (
                json!({"version":2,"entries":null}),
                "A storage snapshot is required.",
            ),
            (
                json!({"version":1.0,"entries":{"a":"value"}}),
                "Invalid or oversized storage snapshot.",
            ),
            (
                json!({"version":1,"entries":{"a":"value"},"extra":true}),
                "Invalid or oversized storage snapshot.",
            ),
            (
                json!({"version":1,"entries":{"a":null}}),
                "Invalid or oversized storage snapshot.",
            ),
        ] {
            let reply = store.dispatch(request("bad", "preferences.import", params));
            assert_eq!(reply.error.unwrap().message, expected);
            assert_eq!(raw(&mut store), json!({}));
        }
        let entries: serde_json::Map<String, Value> = (0..10_001)
            .map(|index| (format!("key-{index}"), json!("value")))
            .collect();
        let reply = store.dispatch(request(
            "too-many",
            "preferences.import",
            json!({"version":1,"entries":entries}),
        ));
        assert_eq!(
            reply.error.unwrap().message,
            "Invalid or oversized storage snapshot."
        );
        assert_eq!(raw(&mut store), json!({}));
        assert!(
            store
                .dispatch(request(
                    "set",
                    "preferences.raw_set",
                    json!({"key":"a","raw":"saved"})
                ))
                .error
                .is_none()
        );
        assert!(
            store
                .dispatch(request(
                    "remove",
                    "preferences.raw_remove",
                    json!({"key":"a","raw":{"ignored":true}})
                ))
                .error
                .is_none()
        );
        assert_eq!(raw(&mut store), json!({}));
    }
    fn import(id: &str, entries: Value) -> Request {
        Request {
            id: id.into(),
            method: "preferences.import".into(),
            params: json!({"version":1,"entries":entries}),
        }
    }
    fn raw(store: &mut Store) -> Value {
        store
            .dispatch(Request {
                id: "read".into(),
                method: "preferences.raw_all".into(),
                params: Value::Null,
            })
            .result
            .unwrap()
    }
    #[test]
    fn long_preference_keys_survive_import_and_keep_byte_limit() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(&directory.path().join("client.sqlite3")).unwrap();
        let key = format!(
            "browser:\"root\":studio-answer-draft:{}",
            "question".repeat(220)
        );
        let mut entries = serde_json::Map::new();
        entries.insert(key.clone(), json!("saved answer"));
        assert!(
            store
                .dispatch(import("long-key", Value::Object(entries)))
                .error
                .is_none()
        );
        assert_eq!(raw(&mut store)[&key], "saved answer");
        let oversized = "Ж".repeat(max_preference_key_bytes() / 2 + 1);
        assert!(
            store
                .dispatch(Request {
                    id: "oversized".into(),
                    method: "preferences.raw_set".into(),
                    params: json!({"key":oversized,"raw":"blocked"})
                })
                .error
                .is_some()
        );
    }

    #[test]
    fn snapshot_import_replay_after_restart_preserves_later_edits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let snapshot = json!({"draft":"Привет 🦀", "empty":"", "encoded":"{\"version\":1}"});
        let first = Store::open(&path)
            .unwrap()
            .dispatch(import("migration", snapshot.clone()));
        assert_eq!(first.result.as_ref().unwrap()["imported"], 3);
        let mut restarted = Store::open(&path).unwrap();
        assert_eq!(raw(&mut restarted), snapshot);
        assert!(
            restarted
                .dispatch(Request {
                    id: "edit".into(),
                    method: "preferences.raw_set".into(),
                    params: json!({"key":"draft","raw":"newer"})
                })
                .error
                .is_none()
        );
        assert_eq!(
            restarted.dispatch(import("migration", snapshot)).result,
            first.result
        );
        assert_eq!(raw(&mut restarted)["draft"], "newer");
        assert_eq!(
            restarted
                .dispatch(import("migration", json!({"draft":"changed"})))
                .error
                .unwrap()
                .code,
            "identity_conflict"
        );
    }
    #[test]
    fn snapshot_conflict_and_storage_failure_leave_no_partial_import() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        assert!(
            store
                .dispatch(import("initial", json!({"z":"existing"})))
                .error
                .is_none()
        );
        let request = import("conflict", json!({"a":"new", "z":"different"}));
        assert_eq!(
            store.dispatch(request).error.unwrap().code,
            "migration_conflict"
        );
        assert_eq!(raw(&mut store), json!({"z":"existing"}));
        store.connection.execute_batch("CREATE TRIGGER fail_import BEFORE INSERT ON client_preferences WHEN NEW.key='b' BEGIN SELECT RAISE(ABORT,'disk fixture'); END;").unwrap();
        assert_eq!(
            store
                .dispatch(import("retry", json!({"a":"first", "b":"second"})))
                .error
                .unwrap()
                .code,
            "storage_error"
        );
        assert_eq!(raw(&mut store), json!({"z":"existing"}));
        store
            .connection
            .execute_batch("DROP TRIGGER fail_import;")
            .unwrap();
        assert_eq!(
            store
                .dispatch(import("retry", json!({"a":"first", "b":"second"})))
                .result
                .unwrap()["imported"],
            2
        );
        assert_eq!(
            store
                .dispatch(import("same-values", json!({"a":"first", "b":"second"})))
                .result
                .unwrap()["imported"],
            0
        );
    }
    #[test]
    fn snapshot_validation_never_partially_writes() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        for entries in [
            json!({"a":"valid","b":null}),
            json!({"":"invalid"}),
            json!([]),
        ] {
            assert_eq!(
                store
                    .dispatch(import("invalid", entries))
                    .error
                    .unwrap()
                    .code,
                "invalid_params"
            );
            assert_eq!(raw(&mut store), json!({}));
        }
        let mut future = import("future", json!({"a":"new"}));
        future.params["version"] = json!(2);
        assert_eq!(store.dispatch(future).error.unwrap().code, "invalid_params");
        assert_eq!(raw(&mut store), json!({}));
    }
}
