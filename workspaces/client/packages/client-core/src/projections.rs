//! Durable server projections. Sequence numbers include deletion tombstones.
use crate::protocol::{Request, Response};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Document {
    pub id: String,
    pub payload: String,
    pub seq: u64,
    #[serde(default, rename = "_deleted")]
    pub deleted: bool,
}

pub fn initialize(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS client_projections (
        workspace TEXT NOT NULL, scope TEXT NOT NULL, document TEXT NOT NULL,
        seq INTEGER NOT NULL, PRIMARY KEY(workspace,scope));
        CREATE TABLE IF NOT EXISTS client_identity (singleton INTEGER PRIMARY KEY CHECK(singleton=1), value TEXT NOT NULL);")
}

fn get(connection: &Connection, workspace: &str, scope: &str) -> Result<Option<Document>, String> {
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

fn entities(connection: &Connection, workspace: &str) -> Result<Vec<Document>, String> {
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
    let scope = request.params["scope"]
        .as_str()
        .ok_or("Missing projection scope.")?;
    if !valid_workspace(workspace) || scope.is_empty() || scope.len() > 512 {
        return Err("Invalid projection identity.".into());
    }
    match request.method.as_str() {
        "projection.entities" => Ok(
            serde_json::to_value(entities(connection, workspace)?).map_err(|e| e.to_string())?
        ),
        "projection.get" => {
            Ok(serde_json::to_value(get(connection, workspace, scope)?)
                .map_err(|e| e.to_string())?)
        }
        "projection.put" => {
            let document: Document = serde_json::from_value(request.params["document"].clone())
                .map_err(|e| e.to_string())?;
            if document.id != scope || document.seq > 9_007_199_254_740_991 {
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
