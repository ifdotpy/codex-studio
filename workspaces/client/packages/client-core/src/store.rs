//! Durable client state. This database is separate from the server's canvas DB.
use crate::protocol::{Request, Response};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreferenceCommand {
    key: String,
    value: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftCommand {
    workspace: String,
    room: String,
    text: String,
    base_revision: u64,
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(
            "BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS client_drafts (
                workspace TEXT NOT NULL, room TEXT NOT NULL,
                text TEXT NOT NULL, revision INTEGER NOT NULL,
                PRIMARY KEY(workspace, room));
            CREATE TABLE IF NOT EXISTS client_receipts (
                request_id TEXT PRIMARY KEY, method TEXT NOT NULL,
                params TEXT NOT NULL, response TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS client_preferences (
                key TEXT PRIMARY KEY, value TEXT NOT NULL);
            COMMIT;",
        )?;
        crate::outbox::initialize(&connection)?;
        crate::projections::initialize(&connection)?;
        crate::draft_store::initialize(&connection)?;
        crate::progress::initialize(&connection)?;
        crate::uploads::initialize(&connection)?;
        crate::dictation::initialize(&connection)?;
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
            method if method.starts_with("uploads.") || method.starts_with("files.") => {
                Ok(crate::uploads::stored(&mut self.connection, request))
            }
            method if method.starts_with("dictation.") => {
                Ok(crate::dictation::stored(&mut self.connection, request))
            }
            method if method.starts_with("file_link.") => Ok(crate::file_links::dispatch(request)),
            "markdown.parse" => Ok(crate::markdown::dispatch(request)),
            method if method.starts_with("progress.cache_") => {
                crate::progress::stored(&self.connection, request)
            }
            method
                if method == "system.uuid"
                    || method == "text.sentences"
                    || method == "locale.language_name"
                    || method.starts_with("url.") =>
            {
                Ok(crate::platform::dispatch(request))
            }
            method if method.starts_with("drafts.") => {
                Ok(crate::draft_store::dispatch(&mut self.connection, request))
            }
            method if method.starts_with("projection.") || method.starts_with("identity.") => {
                Ok(crate::projections::dispatch(&self.connection, request))
            }
            method if method.starts_with("actions.") => {
                Ok(crate::actions::stored(&mut self.connection, request))
            }
            method if method.starts_with("outbox.") => {
                Ok(crate::outbox::dispatch(&mut self.connection, request))
            }
            "system.info" => Ok(Response::success(
                id,
                json!({
                    "name": "Codex Studio", "core": "rust", "protocol": 2,
                    "migration": "in_progress"
                }),
            )),
            "draft.get" => {
                let (Some(workspace), Some(room)) = (
                    request.params.get("workspace").and_then(Value::as_str),
                    request.params.get("room").and_then(Value::as_str),
                ) else {
                    return Ok(Response::failure(
                        id,
                        "invalid_params",
                        "Workspace and room are required.",
                    ));
                };
                let (text, revision) = self
                    .connection
                    .query_row(
                        "SELECT text, revision FROM client_drafts WHERE workspace=?1 AND room=?2",
                        params![workspace, room],
                        |row| Ok((row.get::<_, String>(0)?, crate::store::unsigned(row, 1)?)),
                    )
                    .optional()?
                    .unwrap_or_default();
                Ok(Response::success(
                    id,
                    json!({"text":text,"revision":revision}),
                ))
            }
            "draft.set" => self.set_draft(request),
            "preferences.get" => {
                let mut statement = self
                    .connection
                    .prepare("SELECT key,value FROM client_preferences ORDER BY key")?;
                let rows = statement.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                let mut values = serde_json::Map::new();
                for row in rows {
                    let (key, encoded) = row?;
                    if let Ok(value) = serde_json::from_str(&encoded) {
                        values.insert(key, value);
                    }
                }
                Ok(Response::success(id, Value::Object(values)))
            }
            "preferences.set" => self.set_preference(request),
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
        if request.params["version"] != 1
            || request
                .params
                .as_object()
                .is_none_or(|params| params.len() != 2)
            || entries.len() > 10_000
            || entries.iter().any(|(key, value)| {
                key.is_empty()
                    || key.len() > max_preference_key_bytes()
                    || value.as_str().is_none_or(|raw| raw.len() > 8 * 1024 * 1024)
            })
            || serde_json::to_vec(&request.params).unwrap().len() > 16 * 1024 * 1024
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
        for (key, value) in entries {
            let existing: Option<String> = transaction
                .query_row(
                    "SELECT value FROM client_preferences WHERE key=?1",
                    [key],
                    |row| row.get(0),
                )
                .optional()?;
            if existing
                .as_deref()
                .is_some_and(|raw| Some(raw) != value.as_str())
            {
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
        for (key, value) in entries {
            imported += transaction.execute(
                "INSERT OR IGNORE INTO client_preferences(key,value) VALUES(?1,?2)",
                params![key, value.as_str().unwrap()],
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
        let key = request.params["key"]
            .as_str()
            .filter(|v| !v.is_empty() && v.len() <= max_preference_key_bytes());
        let raw = request.params["raw"]
            .as_str()
            .filter(|v| v.len() <= 8 * 1024 * 1024);
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
        if request.method == "preferences.raw_remove" {
            transaction.execute(
                "DELETE FROM client_preferences WHERE key=?1",
                [key.unwrap()],
            )?;
        } else {
            transaction.execute("INSERT INTO client_preferences(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key.unwrap(),raw.unwrap()])?;
        }
        let response = Response::success(request.id.clone(), json!({"saved":true}));
        record(&transaction, request, &response)?;
        transaction.commit()?;
        Ok(response)
    }

    fn set_preference(&mut self, request: &Request) -> rusqlite::Result<Response> {
        let command: PreferenceCommand = match serde_json::from_value(request.params.clone()) {
            Ok(value) => value,
            Err(error) => {
                return Ok(Response::failure(
                    request.id.clone(),
                    "invalid_params",
                    error,
                ));
            }
        };
        let value = serde_json::to_string(&command.value).expect("JSON value serialization");
        if command.key.is_empty()
            || command.key.len() > max_preference_key_bytes()
            || value.len() > 512 * 1024
        {
            return Ok(Response::failure(
                request.id.clone(),
                "invalid_params",
                "Preference fields exceed their limits.",
            ));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(response) = replay(&transaction, request)? {
            return Ok(response);
        }
        transaction.execute(
            "INSERT INTO client_preferences(key,value) VALUES(?1,?2)
            ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![command.key, value],
        )?;
        let response = Response::success(request.id.clone(), json!({"saved":true}));
        record(&transaction, request, &response)?;
        transaction.commit()?;
        Ok(response)
    }

    fn set_draft(&mut self, request: &Request) -> rusqlite::Result<Response> {
        let id = request.id.clone();
        let command: DraftCommand = match serde_json::from_value(request.params.clone()) {
            Ok(value) => value,
            Err(error) => return Ok(Response::failure(id, "invalid_params", error)),
        };
        if command.workspace.is_empty()
            || command.room.is_empty()
            || command.workspace.len() > 512
            || command.room.len() > 512
            || command.text.len() > 512 * 1024
            || command.base_revision >= i64::MAX as u64
        {
            return Ok(Response::failure(
                id,
                "invalid_params",
                "Draft fields exceed their limits.",
            ));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(response) = replay(&transaction, request)? {
            return Ok(response);
        }
        let current = transaction
            .query_row(
                "SELECT revision FROM client_drafts WHERE workspace=?1 AND room=?2",
                params![command.workspace, command.room],
                |row| crate::store::unsigned(row, 0),
            )
            .optional()?
            .unwrap_or(0);
        let response = if current != command.base_revision {
            Response::failure(
                id.clone(),
                "revision_conflict",
                "The draft changed. Read its current version before saving.",
            )
        } else {
            let revision = current + 1;
            transaction.execute(
                "INSERT INTO client_drafts(workspace,room,text,revision) VALUES (?1,?2,?3,?4)
                 ON CONFLICT(workspace,room) DO UPDATE SET text=excluded.text,revision=excluded.revision",
                params![command.workspace, command.room, command.text, i64::try_from(revision).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?],
            )?;
            Response::success(
                id.clone(),
                json!({"text": command.text, "revision":revision}),
            )
        };
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

    #[test]
    fn old_preference_retry_cannot_replace_a_later_selection() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let set = |id: &str, value: &str| Request {
            id: id.into(),
            method: "preferences.set".into(),
            params: json!({"key":"selected-chat","value":value}),
        };
        assert!(store.dispatch(set("first", "one")).error.is_none());
        assert!(store.dispatch(set("second", "two")).error.is_none());
        assert!(store.dispatch(set("first", "one")).error.is_none());
        let result = store.dispatch(Request {
            id: "read".into(),
            method: "preferences.get".into(),
            params: Value::Null,
        });
        assert_eq!(result.result.unwrap()["selected-chat"], "two");
        assert_eq!(
            store.dispatch(set("first", "changed")).error.unwrap().code,
            "identity_conflict"
        );
    }
    fn save(id: &str, text: &str, revision: u64) -> Request {
        Request {
            id: id.into(),
            method: "draft.set".into(),
            params: json!({
                "workspace":"one", "room":"chat", "text":text,"base_revision":revision,
            }),
        }
    }

    #[test]
    fn lost_response_retry_survives_restart_without_second_write() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let first = Store::open(&path)
            .unwrap()
            .dispatch(save("request-1", "Привет 🦀", 0));
        let mut restarted = Store::open(&path).unwrap();
        let repeated = restarted.dispatch(save("request-1", "Привет 🦀", 0));
        assert_eq!(first.result, repeated.result);
        assert_eq!(repeated.result.unwrap()["revision"], 1);
        assert_eq!(
            restarted
                .dispatch(save("request-1", "changed", 0))
                .error
                .unwrap()
                .code,
            "identity_conflict"
        );
        assert_eq!(
            restarted
                .dispatch(save("request-2", "stale", 0))
                .error
                .unwrap()
                .code,
            "revision_conflict"
        );
        assert_eq!(
            restarted
                .dispatch(save("request-3", "next", 1))
                .result
                .unwrap()["revision"],
            2
        );
        // Retrying a rejected stale command must not become a new write.
        assert_eq!(
            restarted
                .dispatch(save("request-2", "stale", 0))
                .error
                .unwrap()
                .code,
            "revision_conflict"
        );
    }

    #[test]
    fn workspace_scope_prevents_cross_workspace_draft_updates() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        store.dispatch(save("one", "workspace one", 0));
        let mut other = save("two", "workspace two", 0);
        other.params["workspace"] = json!("two");
        assert_eq!(store.dispatch(other).result.unwrap()["revision"], 1);
    }
}
