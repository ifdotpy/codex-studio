//! Durable server projections. Sequence numbers include deletion tombstones.
use crate::protocol::{Request, Response};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_client_domain::MAX_SAFE_INTEGER;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Document {
    pub id: String,
    pub payload: String,
    pub seq: u64,
    #[serde(default, rename = "_deleted")]
    pub deleted: bool,
}

// Local entity cache format, independent of the server's sync protocol version.
const ENTITY_CACHE_VERSION: i64 = 4;
// Largest local projection key, measured in UTF-8 bytes.
const MAX_PROJECTION_SCOPE_BYTES: usize = 512;

pub fn initialize(connection: &Connection) -> rusqlite::Result<()> {
    let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
    transaction.execute_batch("CREATE TABLE IF NOT EXISTS client_projections (
        workspace TEXT NOT NULL, scope TEXT NOT NULL, document TEXT NOT NULL,
        seq INTEGER NOT NULL, PRIMARY KEY(workspace,scope));
        CREATE TABLE IF NOT EXISTS client_identity (singleton INTEGER PRIMARY KEY CHECK(singleton=1), value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS client_projection_cache_version (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1), version INTEGER NOT NULL);")?;
    let version: Option<i64> = transaction
        .query_row(
            "SELECT version FROM client_projection_cache_version WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if version != Some(ENTITY_CACHE_VERSION) {
        // Derived state and its cursors must be cleared together, for every workspace.
        // Keep transcripts, identity, drafts, receipts and other durable local data.
        transaction.execute("DELETE FROM client_projections WHERE scope LIKE 'entity:%' OR scope LIKE 'state:entities:%' OR scope='state'", [])?;
        transaction.execute("INSERT INTO client_projection_cache_version VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET version=excluded.version", [ENTITY_CACHE_VERSION])?;
    }
    transaction.commit()
}

fn entity_scope(change: &Value) -> Result<String, String> {
    let collection = change["collection"]
        .as_str()
        .ok_or("Missing entity collection.")?;
    let id = change["id"].as_str().ok_or("Missing entity identity.")?;
    let seq = change["seq"].as_u64().ok_or("Missing entity sequence.")?;
    if collection.is_empty()
        || collection.contains(':')
        || id.is_empty()
        || seq > MAX_SAFE_INTEGER as u64
    {
        return Err("Invalid entity identity or sequence.".into());
    }
    match change["op"].as_str() {
        Some("upsert") if change["value"].is_object() => {}
        Some("delete") if change.get("value").is_none() => {}
        _ => return Err("Invalid entity change operation or value.".into()),
    }
    let scope = format!("entity:{collection}:{id}");
    if scope.len() > MAX_PROJECTION_SCOPE_BYTES {
        return Err("Invalid projection identity.".into());
    }
    Ok(scope)
}

fn get(connection: &Connection, workspace: &str, scope: &str) -> Result<Option<Value>, String> {
    let encoded: Option<String> = connection
        .query_row(
            "SELECT document FROM client_projections WHERE workspace=?1 AND scope=?2",
            params![workspace, scope],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    encoded
        .map(|value| serde_json::from_str(&value).map_err(|e| e.to_string()))
        .transpose()
}

fn entities(connection: &Connection, workspace: &str) -> Result<Vec<Value>, String> {
    let mut statement = connection
        .prepare("SELECT document FROM client_projections WHERE workspace=?1 AND scope LIKE 'entity:%' ORDER BY scope")
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([workspace], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    rows.map(|row| {
        serde_json::from_str(&row.map_err(|e| e.to_string())?).map_err(|e| e.to_string())
    })
    .collect()
}

pub fn valid_workspace(workspace: &str) -> bool {
    workspace.len() == 32
        && workspace
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn execute(connection: &Connection, request: &Request) -> Result<Value, String> {
    if request.method == "identity.get" {
        let value: Option<String> = connection
            .query_row(
                "SELECT value FROM client_identity WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        return value
            .map(|value| serde_json::from_str(&value).map_err(|e| e.to_string()))
            .transpose()
            .map(|value| value.unwrap_or(Value::Null));
    }
    if request.method == "identity.set" {
        let workspace = request.params["workspaceId"]
            .as_str()
            .ok_or("Missing workspace identity.")?;
        if !valid_workspace(workspace) {
            return Err("Invalid workspace identity.".into());
        }
        let identity = json!({"workspaceId":workspace,"chatState":request.params["chatState"].as_bool().unwrap_or(false)});
        // Bind the store atomically, including concurrent opens. A changed
        // backend must not silently replace the identity used for offline data.
        let changed = connection
            .execute(
                "INSERT INTO client_identity VALUES(1,?1)
            ON CONFLICT(singleton) DO UPDATE SET value=excluded.value
            WHERE json_extract(client_identity.value, '$.workspaceId') = ?2",
                params![identity.to_string(), workspace],
            )
            .map_err(|e| e.to_string())?;
        if changed == 0 {
            return Err(
                "The server workspace changed. Use a separate client store for the new workspace."
                    .into(),
            );
        }
        return Ok(identity);
    }
    let workspace = request.params["workspace"]
        .as_str()
        .ok_or("Missing workspace identity.")?;
    if !valid_workspace(workspace) {
        return Err("Invalid projection identity.".into());
    }
    if request.method == "projection.putEntity" {
        let change = &request.params["change"];
        let scope = entity_scope(change)?;
        let seq = change["seq"].as_u64().ok_or("Missing entity sequence.")?;
        let changed = connection
            .execute(
                "INSERT INTO client_projections(workspace,scope,document,seq) VALUES(?1,?2,?3,?4)
            ON CONFLICT(workspace,scope) DO UPDATE SET document=excluded.document,seq=excluded.seq
            WHERE excluded.seq>client_projections.seq",
                params![workspace, scope, change.to_string(), seq as i64],
            )
            .map_err(|error| error.to_string())?;
        return Ok(Value::Bool(changed != 0));
    }
    let scope = request.params["scope"]
        .as_str()
        .ok_or("Missing projection scope.")?;
    if scope.is_empty() || scope.len() > MAX_PROJECTION_SCOPE_BYTES {
        return Err("Invalid projection identity.".into());
    }
    match request.method.as_str() {
        "projection.entities" => Ok(Value::Array(entities(connection, workspace)?)),
        "projection.get" => Ok(get(connection, workspace, scope)?.unwrap_or(Value::Null)),
        "projection.put" => {
            let document: Document = serde_json::from_value(request.params["document"].clone())
                .map_err(|e| e.to_string())?;
            if document.id != scope || document.seq > MAX_SAFE_INTEGER as u64 {
                return Err("Invalid projection document.".into());
            }
            if !document.deleted {
                serde_json::from_str::<Value>(&document.payload).map_err(|e| e.to_string())?;
            }
            connection.execute("INSERT INTO client_projections(workspace,scope,document,seq) VALUES(?1,?2,?3,?4)
                ON CONFLICT(workspace,scope) DO UPDATE SET document=excluded.document,seq=excluded.seq
                WHERE excluded.seq>client_projections.seq", params![workspace,scope,serde_json::to_string(&document).map_err(|e|e.to_string())?,document.seq as i64])
                .map_err(|e|e.to_string())?;
            Ok(serde_json::to_value(get(connection, workspace, scope)?)
                .map_err(|e| e.to_string())?)
        }
        _ => Err("Unknown projection command.".into()),
    }
}

pub fn dispatch(connection: &Connection, request: &Request) -> Response {
    match execute(connection, request) {
        Ok(value) => Response::success(request.id.clone(), value),
        Err(error) => Response::failure(request.id.clone(), "projection_error", error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn entity_v4_upsert_delete_and_stale_replay_survive_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let workspace = "a".repeat(32);
        let put = |change: Value| Request {
            id: "entity".into(),
            method: "projection.putEntity".into(),
            params: json!({"workspace":workspace,"change":change}),
        };
        let upsert = json!({"op":"upsert","collection":"agent","id":"one","seq":10,"value":{"id":"one","name":"First"}});
        let deleted = json!({"op":"delete","collection":"agent","id":"one","seq":12});
        {
            let mut store = crate::store::Store::open(&path).unwrap();
            let applied = store.dispatch(put(upsert.clone()));
            assert!(applied.error.is_none(), "{:?}", applied.error);
            assert_eq!(applied.result, Some(json!(true)));
            assert_eq!(
                store.dispatch(put(upsert.clone())).result,
                Some(json!(false))
            );
            assert_eq!(
                store.dispatch(put(deleted.clone())).result,
                Some(json!(true))
            );
        }
        let mut store = crate::store::Store::open(&path).unwrap();
        let mut stale = upsert.clone();
        stale["seq"] = json!(11);
        assert_eq!(store.dispatch(put(stale)).result, Some(json!(false)));
        let read = store.dispatch(Request {
            id: "read".into(),
            method: "projection.get".into(),
            params: json!({"workspace":workspace,"scope":"entity:agent:one"}),
        });
        assert_eq!(read.result, Some(deleted));
        let mut fresh = upsert;
        fresh["seq"] = json!(13);
        assert_eq!(store.dispatch(put(fresh.clone())).result, Some(json!(true)));
        let rows = store.dispatch(Request {
            id: "read".into(),
            method: "projection.entities".into(),
            params: json!({"workspace":workspace,"scope":"entity:*"}),
        });
        assert_eq!(rows.result, Some(json!([fresh])));
    }

    #[test]
    fn entity_v4_rejects_legacy_and_malformed_changes_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let mut store =
            crate::store::Store::open(&directory.path().join("client.sqlite3")).unwrap();
        let workspace = "c".repeat(32);
        for change in [
            json!({"id":"entity:agent:one","seq":1,"payload":"{}"}),
            json!({"op":"upsert","collection":"agent","id":"one","seq":1,"value":"{}"}),
            json!({"op":"delete","collection":"agent","id":"one","seq":1,"value":{}}),
            json!({"op":"replace","collection":"agent","id":"one","seq":1,"value":{}}),
            json!({"op":"delete","collection":"agent","id":"","seq":1}),
            json!({"op":"delete","collection":"agent","id":"one","seq":MAX_SAFE_INTEGER as u64 + 1}),
        ] {
            let response = store.dispatch(Request {
                id: "invalid".into(),
                method: "projection.putEntity".into(),
                params: json!({"workspace":workspace,"change":change}),
            });
            assert!(
                response.error.is_some(),
                "accepted invalid change: {change}"
            );
        }
        let rows = store.dispatch(Request {
            id: "read".into(),
            method: "projection.entities".into(),
            params: json!({"workspace":workspace,"scope":"entity:*"}),
        });
        assert_eq!(rows.result, Some(json!([])));
    }

    #[test]
    fn entity_v4_discards_old_cache_and_cursors_but_preserves_other_local_data() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let workspace = "a".repeat(32);
        {
            let mut store = crate::store::Store::open(&path).unwrap();
            for scope in [
                "entity:agent:one",
                "state",
                "state:entities:checkpoint",
                "state:entities:initial",
                "state:entities:ready",
                "transcript:one",
            ] {
                let result = store.dispatch(Request {
                    id: "seed".into(), method: "projection.put".into(),
                    params: json!({"workspace":workspace,"scope":scope,"document":{"id":scope,"seq":99,"payload":"{}"}}),
                });
                assert!(result.error.is_none());
            }
            assert!(
                store
                    .dispatch(Request {
                        id: "identity".into(),
                        method: "identity.set".into(),
                        params: json!({"workspaceId":workspace,"chatState":true}),
                    })
                    .error
                    .is_none()
            );
            // An unversioned database is the format shipped before entity v4.
            store
                .connection
                .execute_batch("DROP TABLE IF EXISTS client_projection_cache_version")
                .unwrap();
        }
        let mut store = crate::store::Store::open(&path).unwrap();
        for scope in [
            "entity:agent:one",
            "state",
            "state:entities:checkpoint",
            "state:entities:initial",
            "state:entities:ready",
        ] {
            let result = store.dispatch(Request {
                id: "read".into(),
                method: "projection.get".into(),
                params: json!({"workspace":workspace,"scope":scope}),
            });
            assert_eq!(
                result.result,
                Some(Value::Null),
                "old cache retained: {scope}"
            );
        }
        assert!(
            store
                .dispatch(Request {
                    id: "read".into(),
                    method: "projection.get".into(),
                    params: json!({"workspace":workspace,"scope":"transcript:one"}),
                })
                .result
                .unwrap()
                .is_object()
        );
        assert_eq!(
            store
                .dispatch(Request {
                    id: "identity".into(),
                    method: "identity.get".into(),
                    params: Value::Null,
                })
                .result
                .unwrap()["workspaceId"],
            workspace
        );
    }

    #[test]
    fn workspace_rebind_is_rejected_without_changing_durable_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let set = |workspace: &str, chat_state| Request {
            id: "identity".into(),
            method: "identity.set".into(),
            params: json!({"workspaceId":workspace.repeat(32),"chatState":chat_state}),
        };
        {
            let mut store = crate::store::Store::open(&path).unwrap();
            assert!(store.dispatch(set("a", false)).error.is_none());
            assert!(store.dispatch(set("a", true)).error.is_none());
            assert!(store.dispatch(set("b", false)).error.is_some());
        }
        let mut store = crate::store::Store::open(&path).unwrap();
        assert!(store.dispatch(set("b", false)).error.is_some());
        let result = store.dispatch(Request {
            id: "read".into(),
            method: "identity.get".into(),
            params: Value::Null,
        });
        assert_eq!(
            result.result,
            Some(json!({"workspaceId":"a".repeat(32),"chatState":true}))
        );
    }

    #[test]
    fn tombstone_and_workspace_survive_late_replies_and_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let workspace = "a".repeat(32);
        let put = |seq, deleted, payload: &str| Request {
            id: "put".into(),
            method: "projection.put".into(),
            params: json!({"workspace":workspace,"scope":"transcript:one","document":{"id":"transcript:one","seq":seq,"payload":payload,"_deleted":deleted}}),
        };
        {
            let mut store = crate::store::Store::open(&path).unwrap();
            assert!(
                store
                    .dispatch(put(10, false, "{\"items\":[]}"))
                    .error
                    .is_none()
            );
            assert!(store.dispatch(put(12, true, "")).error.is_none());
        }
        let mut store = crate::store::Store::open(&path).unwrap();
        let late = store.dispatch(put(11, false, "{\"items\":[1]}"));
        assert_eq!(late.result.unwrap()["_deleted"], true);
        let other = store.dispatch(Request {
            id: "get".into(),
            method: "projection.get".into(),
            params: json!({"workspace":"b".repeat(32),"scope":"transcript:one"}),
        });
        assert_eq!(other.result, Some(Value::Null));
        let mut wrong = put(13, false, "{}");
        wrong.params["document"]["id"] = json!("transcript:other");
        assert!(store.dispatch(wrong).error.is_some());
    }
}
