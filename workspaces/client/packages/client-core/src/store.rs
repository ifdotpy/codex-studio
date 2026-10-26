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

pub(crate) fn valid_raw_preference(key: &str, raw: &str) -> bool {
    !key.is_empty()
        && key.len() <= max_preference_key_bytes()
        && raw.len()
            <= studio_client_domain::preferences::contract()["maxRawBytes"]
                .as_u64()
                .expect("raw byte limit") as usize
}

pub struct Store {
    pub(crate) connection: Connection,
    // This open has not yet recovered its former owner's interrupted waits.
    migration_held: bool,
}

/// Private host preflight. The replay owner also uses this exact parser.
pub fn prepare_original_preferences(scope: &str, writes: &Value) -> Result<Vec<Request>, String> {
    crate::mutation_journal::prepare_original_preferences(scope, writes)
}

/// Read the original file without schema creation, restart recovery, native IPC,
/// or background tasks. Only the trusted host chooses this path.
pub fn read_original_migration_source(path: &Path) -> Result<Option<Value>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("The original native store metadata cannot be read.".into()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("The original native store is not a regular source file.".into());
    }
    let connection = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "The original native store cannot be opened read-only.")?;
    fn schema(
        connection: &Connection,
    ) -> Result<std::collections::BTreeMap<String, (String, String)>, String> {
        let mut query=connection.prepare("SELECT name,type,coalesce(sql,'') FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' ORDER BY name").map_err(|_|"The original native schema cannot be read.")?;
        query
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (row.get::<_, String>(1)?, row.get::<_, String>(2)?),
                ))
            })
            .map_err(|_| "The original native schema cannot be listed.")?
            .map(|row| row.map_err(|_| "The original native schema entry cannot be read.".into()))
            .collect()
    }
    // The actual Store initializers own the schema names. This memory-only
    // reference has no socket, backend, profile file, or native side effects.
    let reference = Store::open(Path::new(":memory:"))
        .map_err(|_| "The native schema reference cannot be opened.")?;
    let expected = schema(&reference.connection)?;
    let transaction = connection
        .unchecked_transaction()
        .map_err(|_| "The original native read snapshot cannot be held.")?;
    let actual = schema(&transaction)?;
    if actual.iter().any(|(name, (kind, _))| {
        expected
            .get(name)
            .is_none_or(|(expected_kind, _)| expected_kind != kind)
    }) {
        return Err(
            "The original native store has an unknown schema owner. Its source remains held."
                .into(),
        );
    }
    for (name, (kind, _)) in &actual {
        if kind != "table" {
            continue;
        }
        fn columns(
            connection: &Connection,
            name: &str,
        ) -> Result<Vec<(String, String, bool, i64)>, String> {
            let quoted = name.replace('"', "\"\"");
            let mut query = connection
                .prepare(&format!("PRAGMA table_info(\"{quoted}\")"))
                .map_err(|_| "The native source columns cannot be read.")?;
            query
                .query_map([], |row| {
                    Ok((row.get(1)?, row.get(2)?, row.get(3)?, row.get(5)?))
                })
                .map_err(|_| "The native source columns cannot be listed.")?
                .map(|row| row.map_err(|_| "The native source column metadata is invalid.".into()))
                .collect()
        }
        if columns(&transaction, name)? != columns(&reference.connection, name)? {
            return Err(
                "The original native store has an unknown column layout. Its source remains held."
                    .into(),
            );
        }
    }
    let mut source = crate::mutation_journal::original_native_source(&transaction)?;
    source["schema"] = serde_json::to_value(actual)
        .map_err(|_| "The original native schema cannot be encoded.")?;
    transaction
        .commit()
        .map_err(|_| "The original native read snapshot could not finish.")?;
    Ok(Some(source))
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
        Self::open_mode(path, false)
    }

    pub fn open_for_migration(path: &Path) -> rusqlite::Result<Self> {
        Self::open_mode(path, true)
    }

    fn open_mode(path: &Path, migration_held: bool) -> rusqlite::Result<Self> {
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
        if migration_held {
            crate::outbox::initialize_for_migration(&connection)?;
            crate::projections::initialize_for_migration(&connection)?;
        } else {
            crate::outbox::initialize(&connection)?;
            crate::projections::initialize(&connection)?;
        }
        crate::draft_store::initialize(&connection)?;
        crate::uploads::initialize(&connection)?;
        crate::dictation::initialize(&connection)?;
        crate::voice::initialize(&connection)?;
        crate::mutation_journal::initialize(&connection)?;
        if migration_held {
            crate::preferences::initialize_for_migration(&connection)?;
        } else {
            crate::preferences::initialize(&connection)?;
        }
        Ok(Self {
            connection,
            migration_held,
        })
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
            method
                if method
                    == crate::mutation_journal::migration_policy()["admitMethod"]
                        .as_str()
                        .unwrap() =>
            {
                let response = crate::mutation_journal::migration(&mut self.connection, request);
                if response.error.is_none()
                    && response.result.as_ref() == Some(&json!({"admitted":true}))
                    && self.migration_held
                {
                    // The serialized worker and native effect gate remain held
                    // through each owner's existing recovery transaction.
                    crate::outbox::initialize(&self.connection)?;
                    crate::projections::initialize(&self.connection)?;
                    crate::preferences::initialize(&self.connection)?;
                    self.migration_held = false;
                }
                Ok(response)
            }
            "preferences.view"
            | "preferences.edit"
            | "preferences.adopt_fields"
            | "preferences.raw_compare_exchange" => {
                Ok(crate::preferences::stored(&mut self.connection, request))
            }
            method if method.starts_with("preference_sync.") => {
                Ok(crate::preferences::stored(&mut self.connection, request))
            }
            method if method.starts_with("voice_journal.") => {
                Ok(crate::voice::stored(&mut self.connection, request))
            }
            method if method.starts_with("attachments.") => Ok(
                crate::attachment_selections::stored(&mut self.connection, request),
            ),
            method if method.starts_with("client_storage.") => Ok(
                crate::mutation_journal::migration(&mut self.connection, request),
            ),
            method if method.starts_with("monitor_input.") || method.starts_with("mutation.") => {
                Ok(crate::mutation_journal::stored(
                    &mut self.connection,
                    request,
                ))
            }
            method if method.starts_with("uploads.") || method.starts_with("files.") => {
                Ok(crate::uploads::stored(&mut self.connection, request))
            }
            method if method.starts_with("dictation.") => {
                Ok(crate::dictation::stored(&mut self.connection, request))
            }
            method if method.starts_with("file_link.") => Ok(crate::file_links::dispatch(request)),
            "markdown.parse" => Ok(crate::markdown::dispatch(request)),
            method
                if method == "system.uuid"
                    || method == "system.uuidBatch"
                    || method == "system.sha256"
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
        if imported > 0 {
            crate::preferences::bump(&transaction, "preferences")
                .map_err(|e| rusqlite::Error::InvalidParameterName(e))?;
        }
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
        let next = if request.method == "preferences.raw_remove" {
            None
        } else {
            raw
        };
        let previous = crate::preferences::raw(&transaction, key.unwrap())
            .map_err(rusqlite::Error::InvalidParameterName)?;
        crate::preferences::capture_raw(&transaction, key.unwrap(), previous.as_deref(), next)
            .map_err(rusqlite::Error::InvalidParameterName)?;
        let changed = crate::preferences::put_raw(&transaction, key.unwrap(), next)
            .map_err(rusqlite::Error::InvalidParameterName)?;
        if changed {
            crate::preferences::bump(&transaction, "preferences")
                .map_err(rusqlite::Error::InvalidParameterName)?;
        }
        let response = Response::success(
            request.id.clone(),
            json!({"saved":true,"revision":crate::preferences::revision(&transaction,"preferences").map_err(rusqlite::Error::InvalidParameterName)?}),
        );
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
        if crate::preferences::put_raw(&transaction, &command.key, Some(&value))
            .map_err(rusqlite::Error::InvalidParameterName)?
        {
            crate::preferences::bump(&transaction, "preferences")
                .map_err(rusqlite::Error::InvalidParameterName)?;
        }
        let response = Response::success(
            request.id.clone(),
            json!({"saved":true,"revision":crate::preferences::revision(&transaction,"preferences").map_err(rusqlite::Error::InvalidParameterName)?}),
        );
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
    fn journal(store: &mut Store, method: &str, params: Value) -> Value {
        let response = store.dispatch(Request {
            id: crate::server_keys::random_id().unwrap(),
            method: method.into(),
            params,
        });
        assert!(response.error.is_none(), "{method}: {:?}", response.error);
        response.result.unwrap()
    }

    #[test]
    fn native_monitor_journal_survives_restart_without_input_text_and_refuses_changed_input() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let mut store = Store::open(&path).unwrap();
        let begin = json!({"monitorId":"monitor","serializedPayload":"{\"id\":\"monitor\",\"text\":\"private-input\"}"});
        let first = journal(&mut store, "monitor_input.begin", begin.clone());
        assert_eq!(first["uncertain"], false);
        drop(store);
        let mut store = Store::open(&path).unwrap();
        let retry = journal(&mut store, "monitor_input.begin", begin);
        assert_eq!(first["requestId"], retry["requestId"]);
        assert_eq!(retry["uncertain"], true);
        let changed = store.dispatch(Request {
            id: "changed".into(), method: "monitor_input.begin".into(),
            params: json!({"monitorId":"monitor","serializedPayload":"{\"id\":\"monitor\",\"text\":\"different\"}"}),
        });
        assert!(changed.error.is_some());
        let durable: String = store
            .connection
            .query_row(
                "SELECT group_concat(document) FROM client_mutation_journal",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!durable.contains("private-input"));
        let receipt_params: String = store
            .connection
            .query_row(
                "SELECT coalesce(group_concat(params),'') FROM client_receipts",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!receipt_params.contains("private-input"));
        let key = json!({"monitorId":"monitor","requestId":retry["requestId"],"payloadDigest":retry["payloadDigest"]});
        journal(&mut store, "monitor_input.reject", {
            let mut p = key.clone();
            p["status"] = json!(400);
            p
        });
        assert!(
            !journal(
                &mut store,
                "monitor_input.pending",
                json!({"monitorId":"monitor"})
            )
            .is_null()
        );
        journal(&mut store, "monitor_input.settle", key.clone());
        let next = journal(
            &mut store,
            "monitor_input.begin",
            json!({"monitorId":"monitor","serializedPayload":"{\"id\":\"monitor\",\"text\":\"new\"}"}),
        );
        assert_ne!(next["requestId"], first["requestId"]);
        journal(&mut store, "monitor_input.settle", key);
        assert_eq!(
            journal(
                &mut store,
                "monitor_input.pending",
                json!({"monitorId":"monitor"})
            )["requestId"],
            next["requestId"]
        );
    }

    #[test]
    fn native_generic_journal_commits_exact_bytes_and_retains_uncertainty() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let begin = json!({"path":"/api/rename","serializedPayload":"{\"b\":2,\"a\":1}"});
        let first = journal(&mut store, "mutation.begin", begin.clone());
        use sha2::{Digest, Sha256};
        let expected = Sha256::digest(b"/api/rename\n{\"b\":2,\"a\":1}")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(first["payloadDigest"], expected);
        let retry = journal(&mut store, "mutation.begin", begin);
        assert_eq!(first["requestId"], retry["requestId"]);
        let key = json!({"requestId":retry["requestId"],"payloadDigest":retry["payloadDigest"]});
        let mut rejected = key.clone();
        rejected["status"] = json!(400);
        journal(&mut store, "mutation.reject", rejected);
        let again = journal(
            &mut store,
            "mutation.begin",
            json!({"path":"/api/rename","serializedPayload":"{\"b\":2,\"a\":1}"}),
        );
        assert_eq!(again["requestId"], first["requestId"]);
        journal(&mut store, "mutation.settle", key);
        let fresh = journal(
            &mut store,
            "mutation.begin",
            json!({"path":"/api/rename","serializedPayload":"{\"b\":2,\"a\":1}"}),
        );
        assert_ne!(fresh["requestId"], first["requestId"]);
    }

    #[test]
    fn native_client_storage_migration_preserves_receipts_outbox_and_later_native_edits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let mut store = Store::open(&path).unwrap();
        let workspace = "a".repeat(32);
        journal(&mut store, "identity.set", json!({"workspaceId":workspace}));
        let original = Request {
            id: "old-pref".into(),
            method: "preferences.raw_set".into(),
            params: json!({"key":"browser:\"root\":theme","raw":"old"}),
        };
        let response = store.dispatch(original);
        assert!(response.error.is_none());
        journal(
            &mut store,
            "preferences.raw_set",
            json!({"key":"browser:\"root\":theme","raw":"later"}),
        );
        let mut source = journal(
            &mut store,
            "client_storage.export_preferences",
            json!({"scope":"root"}),
        );
        source["pendingPreferences"] = json!([{ "id":"old-pref","method":"preferences.raw_set","params":{"key":"browser:\"root\":theme","raw":"old"}}]);
        source["pendingDrafts"] =
            json!([{ "workspace":workspace,"id":"old-draft","changes":{"room":"unsent Ж🦀"}}]);
        source["monitorInputs"] = json!([{ "monitorId":"monitor","requestId":"old-input","payloadDigest":"a".repeat(64),"uncertain":true}]);
        source["outbox"] = json!([{ "workspace":workspace,"id":"old-message","body":{"id":"old-message","room":"room","text":"original"},"status":"queued","attempted":true,"created":7,"displayPending":true,"attachments":[],"error":"lost reply"}]);
        let import = json!({"manifestId":"legacy","serializedSource":source.to_string()});
        let receipt = journal(&mut store, "client_storage.import", import.clone());
        assert_eq!(raw(&mut store)["theme"], "later");
        assert_eq!(
            journal(&mut store, "drafts.view", json!({"workspace":workspace}))["drafts"]["room"],
            "unsent Ж🦀"
        );
        let outbox = journal(&mut store, "outbox.list", json!({"workspace":workspace}));
        assert_eq!(outbox[0]["id"], "old-message");
        assert_eq!(outbox[0]["attempted"], true);
        assert_eq!(outbox[0]["error"], "lost reply");
        journal(
            &mut store,
            "preferences.raw_set",
            json!({"key":"theme","raw":"new native"}),
        );
        drop(store);
        let mut store = Store::open(&path).unwrap();
        assert_eq!(
            journal(&mut store, "client_storage.import", import),
            receipt
        );
        assert_eq!(raw(&mut store)["theme"], "new native");
        let mut changed = source.clone();
        changed["preferences"]["extra"] = json!("must not appear");
        let rejected = store.dispatch(Request {
            id: "changed-manifest".into(),
            method: "client_storage.import".into(),
            params: json!({"manifestId":"legacy","serializedSource":changed.to_string()}),
        });
        assert!(rejected.error.is_some());
        assert!(raw(&mut store).get("extra").is_none());
        assert_eq!(
            journal(
                &mut store,
                "monitor_input.pending",
                json!({"monitorId":"monitor"})
            )["requestId"],
            "old-input"
        );
    }

    #[test]
    fn native_stable_identity_rejects_stale_clear_and_keeps_the_new_request() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let first = journal(&mut store, "mutation.stable_id", json!({"key":"login"}));
        assert_eq!(
            journal(&mut store, "mutation.stable_id", json!({"key":"login"})),
            first
        );
        assert_eq!(
            journal(
                &mut store,
                "mutation.clear_stable_id",
                json!({"key":"login","requestId":first})
            ),
            true
        );
        let next = journal(&mut store, "mutation.stable_id", json!({"key":"login"}));
        assert_ne!(next, first);
        assert_eq!(
            journal(
                &mut store,
                "mutation.clear_stable_id",
                json!({"key":"login","requestId":first})
            ),
            false
        );
        assert_eq!(
            journal(&mut store, "mutation.stable_id", json!({"key":"login"})),
            next
        );
    }
    #[test]
    fn native_monitor_journal_serializes_connections_and_refuses_sibling_identity_reuse() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        drop(Store::open(&path).unwrap());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads=(0..2).map(|_| {
            let path=path.clone(); let barrier=barrier.clone();
            std::thread::spawn(move|| {
                let mut store=Store::open(&path).unwrap(); barrier.wait();
                journal(&mut store,"monitor_input.begin",json!({"monitorId":"same","serializedPayload":"{\"id\":\"same\",\"text\":\"input\"}"}))
            })
        }).collect::<Vec<_>>();
        let results = threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results[0]["requestId"], results[1]["requestId"]);
        assert_ne!(results[0]["uncertain"], results[1]["uncertain"]);
        let mut store = Store::open(&path).unwrap();
        let first = journal(
            &mut store,
            "monitor_input.begin",
            json!({"monitorId":"a","serializedPayload":"{\"id\":\"a\"}","explicitRequestId":"exact"}),
        );
        journal(
            &mut store,
            "monitor_input.settle",
            json!({"monitorId":"a","requestId":"exact","payloadDigest":first["payloadDigest"]}),
        );
        let sibling=store.dispatch(Request{id:"sibling".into(),method:"monitor_input.begin".into(),params:json!({"monitorId":"b","serializedPayload":"{\"id\":\"b\"}","explicitRequestId":"exact"})});
        assert!(sibling.error.is_some());
        assert!(
            journal(
                &mut store,
                "monitor_input.pending",
                json!({"monitorId":"b"})
            )
            .is_null()
        );
    }

    #[test]
    fn native_original_bulk_preference_replay_keeps_one_receipt_before_scoped_export() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let writes = json!([{ "id":"original-import","method":"preferences.import","params":{"version":1,"entries":{"theme":"dark",":server:remote:theme":"light"}}}]);
        let replay = json!({"scope":"root","writes":writes});
        assert_eq!(
            journal(
                &mut store,
                "client_storage.replay_preferences",
                replay.clone()
            )["requestIds"],
            json!(["original-import"])
        );
        journal(
            &mut store,
            "preferences.raw_set",
            json!({"key":"browser:\"root\"::server:remote:theme","raw":"newer"}),
        );
        journal(&mut store, "client_storage.replay_preferences", replay);
        let export = journal(
            &mut store,
            "client_storage.export_preferences",
            json!({"scope":"server:remote"}),
        );
        assert_eq!(export["preferences"], json!({"theme":"newer"}));
        let root = journal(
            &mut store,
            "client_storage.export_preferences",
            json!({"scope":"root"}),
        );
        assert_eq!(root["preferences"], json!({"theme":"dark"}));
        let wrong=store.dispatch(Request{id:"wrong-bulk".into(),method:"client_storage.replay_preferences".into(),params:json!({"scope":"root","writes":[{"id":"original-import","method":"preferences.import","params":{"version":1,"entries":{"theme":"wrong"}}}]})});
        assert!(wrong.error.is_some());
        assert_eq!(raw(&mut store)["browser:\"root\":theme"], "dark");
    }

    #[test]
    fn native_client_storage_failure_rolls_back_every_imported_row_and_manifest() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        store.connection.execute_batch("CREATE TRIGGER fail_journal_import BEFORE INSERT ON client_mutation_journal BEGIN SELECT RAISE(ABORT,'disk fixture'); END;").unwrap();
        let failed=store.dispatch(Request{id:"failure".into(),method:"client_storage.import".into(),params:json!({"manifestId":"failed","serializedSource":json!({"version":1,"preferences":{"theme":"must roll back"},"monitorInputs":[{"monitorId":"m","requestId":"old","payloadDigest":"a".repeat(64),"uncertain":true}]}).to_string()})});
        assert!(failed.error.is_some());
        assert!(raw(&mut store).get("theme").is_none());
        assert!(
            journal(
                &mut store,
                "client_storage.status",
                json!({"manifestId":"failed"})
            )
            .is_null()
        );
    }

    #[test]
    fn native_cutover_requires_every_expected_byte_and_metadata_receipt() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let source = "a".repeat(64);
        let imported = journal(
            &mut store,
            "client_storage.import",
            json!({"manifestId":"metadata","serializedSource":json!({"version":1,"sourceIdentity":source,"expectedByteImports":["unstarted-byte"]}).to_string()}),
        );
        let finish = json!({"sourceIdentity":source,"serverId":"local","metadata":[{"manifestId":"metadata","sourceSha256":imported["sourceSha256"]}],"bytes":[]});
        let missing = store.dispatch(Request {
            id: "missing-byte".into(),
            method: "client_storage.finish_cutover".into(),
            params: finish.clone(),
        });
        assert!(missing.error.is_some());
        let mut fabricated = finish;
        fabricated["bytes"] = json!([{"manifestId":"unstarted-byte","receipt":{"complete":true}}]);
        let missing = store.dispatch(Request {
            id: "fake-byte".into(),
            method: "client_storage.finish_cutover".into(),
            params: fabricated,
        });
        assert!(missing.error.is_some());
        assert_eq!(
            journal(
                &mut store,
                "client_storage.cutover_status",
                json!({"sourceIdentity":source,"serverId":"local"})
            ),
            json!({"complete":false})
        );
    }

    #[test]
    fn native_cutover_seal_survives_later_edits_and_refuses_an_omitted_archive() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let source = "b".repeat(64);
        let imported = journal(
            &mut store,
            "client_storage.import",
            json!({"manifestId":"metadata","serializedSource":json!({"version":1,"sourceIdentity":source,"expectedByteImports":[]}).to_string()}),
        );
        let finish = json!({"sourceIdentity":source,"serverId":"local","metadata":[{"manifestId":"metadata","sourceSha256":imported["sourceSha256"]}],"bytes":[]});
        let mut with_archive = finish.clone();
        with_archive["archive"] = json!({"id":"sealed","owner":"root","sha256":"c".repeat(64)});
        assert!(
            store
                .dispatch(Request {
                    id: "missing-archive".into(),
                    method: "client_storage.finish_cutover".into(),
                    params: with_archive
                })
                .error
                .is_some()
        );
        let receipt = journal(&mut store, "client_storage.finish_cutover", finish.clone());
        assert_eq!(receipt["complete"], true);
        journal(
            &mut store,
            "preferences.raw_set",
            json!({"key":"theme","raw":"after cutover"}),
        );
        assert_eq!(
            journal(&mut store, "client_storage.finish_cutover", finish),
            receipt
        );
        assert_eq!(raw(&mut store)["theme"], "after cutover");
        assert_eq!(
            journal(
                &mut store,
                "client_storage.cutover_status",
                json!({"sourceIdentity":source,"serverId":"local"})
            ),
            json!({"complete":true})
        );
    }

    #[test]
    fn native_draft_document_import_keeps_tombstone_bytes_dirty_without_server_ack() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let workspace = "a".repeat(32);
        journal(&mut store, "identity.set", json!({"workspaceId":workspace}));
        let payload = "{ \"id\":\"phone:room\",\"device\":\"phone\",\"session\":\"room\",\"text\":\"\",\"updated\":1,\"metadata\":{\"keep\":true} }";
        let source = json!({"version":1,"draftDocuments":[{"workspace":workspace,"documents":[{"id":"phone:room","payload":payload,"seq":8,"_deleted":true}]}]});
        journal(
            &mut store,
            "client_storage.import",
            json!({"manifestId":"draft-documents","serializedSource":source.to_string()}),
        );
        let pending = journal(&mut store, "drafts.pending", json!({"workspace":workspace}));
        assert_eq!(pending["rows"][0]["newDocumentState"]["payload"], payload);
        assert_eq!(pending["rows"][0]["newDocumentState"]["seq"], 8);
        assert_eq!(pending["rows"][0]["newDocumentState"]["_deleted"], true);
        assert!(pending["rows"][0]["assumedMasterState"].is_null());
        assert!(
            journal(&mut store, "drafts.view", json!({"workspace":workspace}))["drafts"]
                .get("room")
                .is_none()
        );
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
