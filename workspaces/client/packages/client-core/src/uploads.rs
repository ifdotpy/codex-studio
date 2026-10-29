//! Durable file bytes and upload identities. Cancellation wins over late replies.
use crate::{
    backend::Backend,
    delivery::{StoreSender, state},
    protocol::{Request, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
// The durable receipt historically accepts any object with the matching id.
// Other fields belong to the server or older providers and remain opaque.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Asset {
    id: String,
    #[serde(flatten)]
    fields: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
enum UploadState {
    Pending {},
    ExplicitNull { asset: () },
    Acknowledged { asset: Asset },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Upload {
    id: String,
    workspace: String,
    state_dir: String,
    agent: String,
    name: String,
    mime: String,
    size: usize,
    created: serde_json::Number,
    #[serde(flatten)]
    state: UploadState,
    #[serde(flatten)]
    extensions: BTreeMap<String, Value>,
}

#[derive(Default)]
enum AssetField {
    #[default]
    Missing,
    Null,
    Present(Asset),
}
impl<'de> Deserialize<'de> for AssetField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Option::<Asset>::deserialize(deserializer)? {
            Some(asset) => Self::Present(asset),
            None => Self::Null,
        })
    }
}

impl<'de> Deserialize<'de> for Upload {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            id: String,
            workspace: String,
            state_dir: String,
            agent: String,
            name: String,
            mime: String,
            size: usize,
            created: serde_json::Number,
            #[serde(default)]
            asset: AssetField,
            #[serde(flatten)]
            extensions: BTreeMap<String, Value>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            id: wire.id,
            workspace: wire.workspace,
            state_dir: wire.state_dir,
            agent: wire.agent,
            name: wire.name,
            mime: wire.mime,
            size: wire.size,
            created: wire.created,
            extensions: wire.extensions,
            state: match wire.asset {
                AssetField::Present(asset) => UploadState::Acknowledged { asset },
                AssetField::Null => UploadState::ExplicitNull { asset: () },
                AssetField::Missing => UploadState::Pending {},
            },
        })
    }
}
fn encode<T: Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|e| e.to_string())
}
fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|e| e.to_string())
}
pub const LIMIT: usize = 20 * 1024 * 1024;
pub fn initialize(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS client_file_sources(id TEXT PRIMARY KEY,name TEXT NOT NULL,mime TEXT NOT NULL,bytes BLOB NOT NULL); CREATE TABLE IF NOT EXISTS client_uploads(id TEXT PRIMARY KEY,workspace TEXT NOT NULL,state_dir TEXT NOT NULL,agent TEXT NOT NULL,document TEXT NOT NULL,bytes BLOB NOT NULL); CREATE TABLE IF NOT EXISTS client_upload_tombstones(id TEXT PRIMARY KEY,cancelled INTEGER NOT NULL);")
}
fn text<'a>(p: &'a Value, key: &str) -> Result<&'a str, String> {
    p[key]
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 4096)
        .ok_or_else(|| format!("{key} is required."))
}
fn uuid() -> Result<String, String> {
    crate::platform::dispatch(&Request {
        id: "file-identity".into(),
        method: "system.uuid".into(),
        params: Value::Null,
    })
    .result
    .and_then(|v| v.as_str().map(str::to_owned))
    .ok_or("Cannot create a file identity.".into())
}
fn row(db: &Connection, id: &str) -> Result<Option<Upload>, String> {
    let raw: Option<String> = db
        .query_row(
            "SELECT document FROM client_uploads WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    raw.map(|s| {
        serde_json::from_str::<Value>(&s)
            .map_err(|e| e.to_string())
            .and_then(decode)
    })
    .transpose()
}
fn status(db: &Connection, id: &str) -> Result<Value, String> {
    let cancelled: Option<bool> = db
        .query_row(
            "SELECT cancelled FROM client_upload_tombstones WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(json!({"cancelled":cancelled==Some(true),"settled":cancelled.is_some()}))
}
fn apply(db: &Connection, request: &Request) -> Result<Value, String> {
    let p = &request.params;
    match request.method.as_str() {
        "files.stage_many" => {
            let files = p["files"]
                .as_array()
                .filter(|v| v.len() <= 20)
                .ok_or("Select at most 20 files.")?;
            let mut result = vec![];
            for file in files {
                result.push(apply(
                    db,
                    &Request {
                        id: request.id.clone(),
                        method: "files.stage".into(),
                        params: file.clone(),
                    },
                )?);
            }
            Ok(json!(result))
        }
        "files.stage" | "files.text" => {
            let name = text(p, "name")?;
            if name.contains(['\0', '/', '\\']) {
                return Err("The file name must not contain a path.".into());
            }
            let bytes = if request.method == "files.text" {
                p["text"]
                    .as_str()
                    .ok_or("File text is required.")?
                    .as_bytes()
                    .to_vec()
            } else {
                let value = p["base64"]
                    .as_str()
                    .filter(|v| v.len() <= LIMIT.div_ceil(3) * 4)
                    .ok_or("Invalid file data.")?;
                STANDARD.decode(value).map_err(|e| e.to_string())?
            };
            if bytes.is_empty() || bytes.len() > LIMIT {
                return Err("Files must contain 1 byte to 20 MiB.".into());
            }
            let (count, total): (i64, i64) = db
                .query_row(
                    "SELECT count(*),COALESCE(sum(length(bytes)),0) FROM client_file_sources",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(|e| e.to_string())?;
            if count >= 100 || total + bytes.len() as i64 > 256 * 1024 * 1024 {
                return Err("Save or remove the selected files before adding more files.".into());
            }
            let id = uuid()?;
            let mime = p["mime"]
                .as_str()
                .filter(|v| v.len() <= 255)
                .unwrap_or("application/octet-stream");
            db.execute(
                "INSERT INTO client_file_sources VALUES(?1,?2,?3,?4)",
                params![id, name, mime, bytes],
            )
            .map_err(|e| e.to_string())?;
            Ok(json!({"nativeHandle":id,"name":name,"type":mime,"size":bytes.len()}))
        }
        "uploads.queue" => {
            let workspace = text(p, "workspace")?;
            if !crate::projections::valid_workspace(workspace) {
                return Err("Invalid workspace identity.".into());
            }
            let state_dir = text(p, "stateDir")?;
            let agent = text(p, "agent")?;
            let files = p["files"]
                .as_array()
                .filter(|v| !v.is_empty() && v.len() <= 8)
                .ok_or("Attach one to eight files.")?;
            let mut result = vec![];
            for (index, file) in files.iter().enumerate() {
                let source = text(file, "nativeHandle")?;
                let (name,mime,bytes):(String,String,Vec<u8>)=db.query_row("SELECT name,mime,bytes FROM client_file_sources WHERE id=?1",[source],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).map_err(|_|"The selected file is unavailable. Check saved uploads before selecting it again.")?;
                let id = uuid()?;
                let created = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| e.to_string())?
                    .as_millis() as u64
                    + index as u64;
                let document = Upload {
                    id: id.clone(),
                    workspace: workspace.into(),
                    state_dir: state_dir.into(),
                    agent: agent.into(),
                    name,
                    mime,
                    size: bytes.len(),
                    created: created.into(),
                    extensions: BTreeMap::new(),
                    state: UploadState::Pending {},
                };
                db.execute(
                    "INSERT INTO client_uploads VALUES(?1,?2,?3,?4,?5,?6)",
                    params![
                        id,
                        workspace,
                        state_dir,
                        agent,
                        serde_json::to_string(&document).map_err(|e| e.to_string())?,
                        bytes
                    ],
                )
                .map_err(|e| e.to_string())?;
                db.execute("DELETE FROM client_file_sources WHERE id=?1", [source])
                    .map_err(|e| e.to_string())?;
                result.push(document);
            }
            encode(result)
        }
        "uploads.list" => {
            let mut query=db.prepare("SELECT document FROM client_uploads WHERE workspace=?1 AND state_dir=?2 ORDER BY json_extract(document,'$.created'),id").map_err(|e|e.to_string())?;
            let rows = query
                .query_map(params![text(p, "workspace")?, text(p, "stateDir")?], |r| {
                    r.get::<_, String>(0)
                })
                .map_err(|e| e.to_string())?;
            let mut result = vec![];
            for row in rows {
                result.push(decode::<Upload>(
                    serde_json::from_str::<Value>(&row.map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?,
                )?);
            }
            encode(result)
        }
        "uploads.status" => status(db, text(p, "id")?),
        "uploads.get" => encode(row(db, text(p, "id")?)?),
        "uploads.content" => {
            let id = text(p, "id")?;
            let bytes: Option<Vec<u8>> = db
                .query_row("SELECT bytes FROM client_uploads WHERE id=?1", [id], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(|e| e.to_string())?;
            Ok(bytes
                .map(|v| json!(STANDARD.encode(v)))
                .unwrap_or(Value::Null))
        }
        "uploads.cancel" | "uploads.finish" => {
            let id = text(p, "id")?;
            let cancelled = request.method == "uploads.cancel";
            db.execute("INSERT INTO client_upload_tombstones VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET cancelled=max(cancelled,excluded.cancelled)",params![id,cancelled]).map_err(|e|e.to_string())?;
            db.execute("DELETE FROM client_uploads WHERE id=?1", [id])
                .map_err(|e| e.to_string())?;
            Ok(Value::Null)
        }
        "uploads.receipt" => {
            let id = text(p, "id")?;
            if p["asset"]["id"] != id {
                return Err("The upload response does not match the saved file.".into());
            }
            let asset: Asset = decode(p["asset"].clone())?;
            let Some(mut current) = row(db, id)? else {
                return Ok(Value::Null);
            };
            if status(db, id)?["settled"] == true {
                return Ok(Value::Null);
            }
            current.state = UploadState::Acknowledged {
                asset: asset.clone(),
            };
            db.execute(
                "UPDATE client_uploads SET document=?2 WHERE id=?1",
                params![
                    id,
                    serde_json::to_string(&current).map_err(|e| e.to_string())?
                ],
            )
            .map_err(|e| e.to_string())?;
            encode(asset)
        }
        _ => Err("Unknown upload command.".into()),
    }
}
pub fn stored(db: &mut Connection, request: &Request) -> Response {
    let result = (|| -> Result<Response, String> {
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let durable = matches!(
            request.method.as_str(),
            "files.stage_many" | "files.stage" | "files.text" | "uploads.queue"
        );
        if durable
            && let Some(reply) = crate::store::replay(&tx, request).map_err(|e| e.to_string())?
        {
            return Ok(reply);
        }
        let value = apply(&tx, request)?;
        let response = Response::success(request.id.clone(), value);
        if durable {
            crate::store::record(&tx, request, &response).map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(response)
    })();
    result.unwrap_or_else(|e| Response::failure(request.id.clone(), "upload_state", e))
}
fn upload_lock(id: &str) -> Result<std::sync::Arc<tokio::sync::Mutex<()>>, String> {
    use std::sync::{Arc, Mutex, OnceLock, Weak};
    static LOCKS: OnceLock<Mutex<std::collections::HashMap<String, Weak<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|e| e.to_string())?;
    locks.retain(|_, value| value.strong_count() > 0);
    if let Some(lock) = locks.get(id).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    if locks.len() >= 64 {
        return Err("The upload queue is busy. Retry after an upload completes.".into());
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(id.to_owned(), Arc::downgrade(&lock));
    Ok(lock)
}
pub async fn dispatch(backend: &Backend, sender: &StoreSender, request: Request) -> Response {
    let result=async {
        let p=&request.params;
        if request.method=="uploads.pending"||request.method=="uploads.enqueue"{
            if request.method=="uploads.enqueue" {text(p,"workspace")?;}
            let identity=crate::sync::open(backend,sender).await.map_err(|e|e.message)?;
            let workspace=identity["workspaceId"].as_str().ok_or("Workspace identity is unavailable.")?;
            if p["workspace"].as_str().is_some_and(|expected| !expected.is_empty()&&expected!=workspace){return Err("The workspace changed. Saved files remain on this device.".into());}
            let mut params=p.clone();params["workspace"]=json!(workspace);
            if request.method=="uploads.pending"{return state(sender,"uploads.list",params).await;}
            let (reply,receive)=tokio::sync::oneshot::channel();
            sender.send(crate::delivery::StoreCommand::Request(Request{id:request.id.clone(),method:"uploads.queue".into(),params},reply)).await.map_err(|e|e.to_string())?;
            let response=receive.await.map_err(|e|e.to_string())?;
            return response.result.ok_or_else(||response.error.map(|e|e.message).unwrap_or("Missing file state response.".into()));
        }
        let expected = &p["row"];
        let id = text(expected, "id")?;
        let lock=upload_lock(id)?;let _guard=lock.lock().await;
        let current: Option<Upload> = decode(state(sender,"uploads.get",json!({"id":id})).await?)?;
        let Some(current) = current else { return Ok(Value::Null); };
        if expected["workspace"] != current.workspace || expected["agent"] != current.agent || expected["stateDir"] != current.state_dir {
            return Err("The saved upload belongs to another workspace or chat.".into());
        }
        let identity=crate::sync::identity(backend).await.map_err(|e|e.message)?;
        if identity["workspaceId"]!=current.workspace{return Err("The server workspace changed. Saved files remain on this device.".into());}
        if let UploadState::Acknowledged { .. } = current.state {
            let latest: Option<Upload> = decode(state(sender,"uploads.get",json!({"id":id})).await?)?;
            return match latest { Some(Upload { state: UploadState::Acknowledged { asset }, .. }) => encode(asset), _ => Ok(Value::Null) };
        }
        let bytes=state(sender,"uploads.content",json!({"id":id})).await?;
        if bytes.is_null(){return Ok(Value::Null);}
        let response=backend.dispatch(Request{id:request.id.clone(),method:"api.request".into(),params:json!({"path":"/api/assets","workspaceId":current.workspace,"timeoutMs":60000,"body":{"id":id,"agent":current.agent,"name":current.name,"base64":bytes}})}).await;
        let value=response.result.ok_or_else(||response.error.map(|e|e.message).unwrap_or("Missing upload response.".into()))?;
        state(sender,"uploads.receipt",json!({"id":id,"asset":value.get("asset").unwrap_or(&value)})).await
    }.await;
    match result {
        Ok(value) => Response::success(request.id, value),
        Err(error) => Response::failure(request.id, "upload", error),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    fn command(id: &str, method: &str, params: Value) -> Request {
        Request {
            id: id.into(),
            method: method.into(),
            params,
        }
    }
    #[test]
    fn queue_is_atomic_and_cancelled_upload_cannot_return_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("client.sqlite3");
        let mut store = Store::open(&path).unwrap();
        let file = store
            .dispatch(command(
                "file",
                "files.text",
                json!({"name":"message.txt","text":"Привет 🦀","mime":"text/plain"}),
            ))
            .result
            .unwrap();
        let inputs =
            json!({"workspace":"a".repeat(32),"stateDir":"/state","agent":"lead","files":[file]});
        let mut invalid = inputs.clone();
        invalid["files"]
            .as_array_mut()
            .unwrap()
            .push(json!({"nativeHandle":"missing"}));
        assert!(
            store
                .dispatch(command("invalid", "uploads.queue", invalid))
                .error
                .is_some()
        );
        let rows = store
            .dispatch(command("queue", "uploads.queue", inputs.clone()))
            .result
            .unwrap();
        let id = rows[0]["id"].clone();
        assert_eq!(rows[0]["size"], "Привет 🦀".len());
        assert_eq!(
            store
                .dispatch(command("queue", "uploads.queue", inputs))
                .result,
            Some(rows)
        );
        assert!(
            store
                .dispatch(command("cancel", "uploads.cancel", json!({"id":id})))
                .error
                .is_none()
        );
        drop(store);
        let mut store = Store::open(&path).unwrap();
        assert_eq!(
            store
                .dispatch(command(
                    "late",
                    "uploads.receipt",
                    json!({"id":id,"asset":{"id":id,"name":"message.txt"}})
                ))
                .result,
            Some(Value::Null)
        );
        assert_eq!(
            store
                .dispatch(command("status", "uploads.status", json!({"id":id})))
                .result
                .unwrap()["cancelled"],
            true
        );
        assert_eq!(
            store
                .dispatch(command("bytes", "uploads.content", json!({"id":id})))
                .result,
            Some(Value::Null)
        );
    }
    #[test]
    fn receipt_round_trips_through_list_get_and_rejects_invalid_assets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.sqlite3");
        let mut store = Store::open(&path).unwrap();
        let file = store
            .dispatch(command(
                "file",
                "files.text",
                json!({"name":"x.txt","text":"hello"}),
            ))
            .result
            .unwrap();
        let workspace = "a".repeat(32);
        let queued = store
            .dispatch(command(
                "queue",
                "uploads.queue",
                json!({"workspace":workspace,"stateDir":"/state","agent":"lead","files":[file]}),
            ))
            .result
            .unwrap();
        let id = &queued[0]["id"];
        let asset = json!({"id":id,"agent":"lead","name":"x.txt","mime":"text/plain","image":false,"size":5,"hash":"h","created":1.0,"vendor":{"a":1}});
        assert!(
            store
                .dispatch(command(
                    "bad",
                    "uploads.receipt",
                    json!({"id":id,"asset":{"id":"wrong"}})
                ))
                .error
                .is_some()
        );
        assert_eq!(
            store
                .dispatch(command(
                    "receipt",
                    "uploads.receipt",
                    json!({"id":id,"asset":asset})
                ))
                .result,
            Some(asset.clone())
        );
        drop(store);
        let mut store = Store::open(&path).unwrap();
        let got = store
            .dispatch(command("get", "uploads.get", json!({"id":id})))
            .result
            .unwrap();
        assert_eq!(got["asset"], asset);
        let listed = store
            .dispatch(command(
                "list",
                "uploads.list",
                json!({"workspace":workspace,"stateDir":"/state"}),
            ))
            .result
            .unwrap();
        assert_eq!(listed[0], got);
        assert!(
            store
                .dispatch(command("finish", "uploads.finish", json!({"id":id})))
                .error
                .is_none()
        );
        assert_eq!(
            store
                .dispatch(command(
                    "late",
                    "uploads.receipt",
                    json!({"id":id,"asset":asset})
                ))
                .result,
            Some(Value::Null)
        );
    }
    #[test]
    fn legacy_partial_receipt_and_unknown_fields_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("client.sqlite3")).unwrap();
        let file = store
            .dispatch(command(
                "file",
                "files.text",
                json!({"name":"x","text":"x"}),
            ))
            .result
            .unwrap();
        let row = store.dispatch(command("queue", "uploads.queue", json!({"workspace":"a".repeat(32),"stateDir":"/state","agent":"lead","files":[file]}))).result.unwrap()[0].clone();
        let id = &row["id"];
        let asset = json!({"id":id,"created":1,"vendor":{"n":2}});
        assert_eq!(
            store
                .dispatch(command(
                    "receipt",
                    "uploads.receipt",
                    json!({"id":id,"asset":asset})
                ))
                .result,
            Some(asset.clone())
        );
        let saved = store
            .dispatch(command("get", "uploads.get", json!({"id":id})))
            .result
            .unwrap();
        assert_eq!(saved["asset"], asset);
        assert!(saved["asset"]["created"].as_u64().is_some());
        let mut extended = saved;
        extended["future"] = json!({"opaque":true});
        extended["created"] = json!(1.5);
        store
            .connection
            .execute(
                "UPDATE client_uploads SET document=?2 WHERE id=?1",
                params![id.as_str().unwrap(), extended.to_string()],
            )
            .unwrap();
        assert_eq!(
            store
                .dispatch(command("get-extended", "uploads.get", json!({"id":id})))
                .result,
            Some(extended.clone())
        );
        let replacement = json!({"id":id,"created":1.5});
        assert_eq!(
            store
                .dispatch(command(
                    "replace",
                    "uploads.receipt",
                    json!({"id":id,"asset":replacement})
                ))
                .result,
            Some(replacement.clone())
        );
        extended["asset"] = replacement;
        let listed = store
            .dispatch(command(
                "list-extended",
                "uploads.list",
                json!({"workspace":"a".repeat(32),"stateDir":"/state"}),
            ))
            .result
            .unwrap();
        assert_eq!(listed[0], extended);
        store.dispatch(command("cancel", "uploads.cancel", json!({"id":id})));
        assert_eq!(
            store
                .dispatch(command(
                    "late",
                    "uploads.receipt",
                    json!({"id":id,"asset":{"id":id,"name":"x"}})
                ))
                .result,
            Some(Value::Null)
        );
    }
    #[test]
    fn stored_duplicate_keys_and_null_asset_keep_legacy_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("client.sqlite3")).unwrap();
        let file = store
            .dispatch(command(
                "file",
                "files.text",
                json!({"name":"x","text":"x"}),
            ))
            .result
            .unwrap();
        let row = store.dispatch(command("queue", "uploads.queue", json!({"workspace":"a".repeat(32),"stateDir":"/state","agent":"lead","files":[file]}))).result.unwrap()[0].clone();
        let id = row["id"].as_str().unwrap();
        let raw = format!(
            r#"{{"id":"bad","id":"{id}","workspace":"{}","stateDir":"/state","agent":"lead","name":"x","mime":"text/plain","size":1,"created":1,"asset":null}}"#,
            "a".repeat(32)
        );
        store
            .connection
            .execute(
                "UPDATE client_uploads SET document=?2 WHERE id=?1",
                params![id, raw],
            )
            .unwrap();
        let got = store
            .dispatch(command("get", "uploads.get", json!({"id":id})))
            .result
            .unwrap();
        assert_eq!(got["id"], id);
        assert!(got.as_object().unwrap().contains_key("asset"));
        assert!(got["asset"].is_null());
        let listed = store
            .dispatch(command(
                "list-null",
                "uploads.list",
                json!({"workspace":"a".repeat(32),"stateDir":"/state"}),
            ))
            .result
            .unwrap();
        assert_eq!(listed[0], got);
        let raw = format!(
            r#"{{"id":"{id}","workspace":"{}","stateDir":"/state","agent":"lead","name":"x","mime":"text/plain","size":1,"created":1,"asset":{{"id":"wrong","id":"{id}"}}}}"#,
            "a".repeat(32)
        );
        store
            .connection
            .execute(
                "UPDATE client_uploads SET document=?2 WHERE id=?1",
                params![id, raw],
            )
            .unwrap();
        let got = store
            .dispatch(command("get2", "uploads.get", json!({"id":id})))
            .result
            .unwrap();
        assert_eq!(got["asset"]["id"], id);
    }
    #[tokio::test]
    async fn delivery_persists_receipt_and_reuses_it() {
        use crate::backend::tests::{read_http, reply};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut uploaded = false;
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (headers, body) = read_http(&mut stream).await;
                if headers.starts_with("GET /api/sync/identity ") {
                    reply(
                        &mut stream,
                        &json!({"workspaceId":"a".repeat(32)}).to_string(),
                    )
                    .await;
                } else if headers.starts_with("GET /api/session ") {
                    reply(&mut stream, r#"{"token":"test"}"#).await;
                } else {
                    assert!(headers.starts_with("POST /api/assets "));
                    assert!(!uploaded, "receipt reuse must not post again");
                    uploaded = true;
                    reply(&mut stream, &json!({"id":body["id"],"agent":"lead","name":"x.txt","mime":"text/plain","image":false,"size":5,"hash":"h","created":1.0,"vendor":true}).to_string()).await;
                }
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.sqlite3");
        let row = {
            let mut store = Store::open(&path).unwrap();
            let file = store
                .dispatch(command(
                    "file",
                    "files.text",
                    json!({"name":"x.txt","text":"hello"}),
                ))
                .result
                .unwrap();
            store.dispatch(command("queue", "uploads.queue", json!({"workspace":"a".repeat(32),"stateDir":"/state","agent":"lead","files":[file]}))).result.unwrap()[0].clone()
        };
        let (sender, mut receive) = tokio::sync::mpsc::channel::<crate::delivery::StoreCommand>(64);
        let thread = std::thread::spawn(move || {
            let mut store = Store::open(&path).unwrap();
            while let Some(command) = receive.blocking_recv() {
                command.execute(&mut store);
            }
        });
        let backend = Backend::new(&origin).unwrap();
        let missing = dispatch(
            &backend,
            &sender,
            command(
                "missing",
                "uploads.deliver",
                json!({"row":{"id":"missing"}}),
            ),
        )
        .await;
        assert_eq!(missing.result, Some(Value::Null));
        let minimal = json!({"id":row["id"],"workspace":row["workspace"],"stateDir":row["stateDir"],"agent":row["agent"]});
        let first = dispatch(
            &backend,
            &sender,
            command("deliver", "uploads.deliver", json!({"row":minimal})),
        )
        .await;
        let asset = first.result.unwrap();
        assert_eq!(asset["vendor"], true);
        let second = dispatch(
            &backend,
            &sender,
            command("again", "uploads.deliver", json!({"row":row})),
        )
        .await;
        assert_eq!(second.result, Some(asset));
        drop(sender);
        thread.join().unwrap();
        server.abort();
    }
    #[tokio::test]
    async fn upload_retry_keeps_exact_bytes_and_cancel_wins_over_a_late_http_receipt() {
        use crate::backend::tests::{read_http, reply};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (arrived, arrival) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut first = None;
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (headers, body) = read_http(&mut stream).await;
                if headers.starts_with("GET /api/sync/identity ") {
                    reply(
                        &mut stream,
                        &json!({"workspaceId":"a".repeat(32)}).to_string(),
                    )
                    .await;
                } else if headers.starts_with("GET /api/session ") {
                    reply(&mut stream, r#"{"token":"test"}"#).await;
                } else {
                    assert!(headers.starts_with("POST /api/assets "));
                    assert!(
                        headers
                            .to_ascii_lowercase()
                            .contains(&format!("x-canvas-workspace: {}", "a".repeat(32)))
                    );
                    assert_eq!(
                        STANDARD.decode(body["base64"].as_str().unwrap()).unwrap(),
                        "Привет 🦀".as_bytes()
                    );
                    if first.is_none() {
                        first = Some(body);
                        drop(stream);
                        continue;
                    }
                    assert_eq!(first.as_ref(), Some(&body));
                    arrived.send(()).unwrap();
                    released.await.unwrap();
                    reply(&mut stream,&json!({"asset":{"id":body["id"],"name":"message.txt","mime":"text/plain","size":17,"image":false}}).to_string()).await;
                    break;
                }
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let row = {
            let mut store = Store::open(&path).unwrap();
            let file = store
                .dispatch(command(
                    "file",
                    "files.text",
                    json!({"name":"message.txt","text":"Привет 🦀"}),
                ))
                .result
                .unwrap();
            store.dispatch(command("queue","uploads.queue",json!({"workspace":"a".repeat(32),"stateDir":"/state","agent":"lead","files":[file]}))).result.unwrap()[0].clone()
        };
        let start = |path: std::path::PathBuf| {
            let (sender, mut receive) =
                tokio::sync::mpsc::channel::<crate::delivery::StoreCommand>(64);
            let thread = std::thread::spawn(move || {
                let mut store = Store::open(&path).unwrap();
                while let Some(command) = receive.blocking_recv() {
                    command.execute(&mut store);
                }
            });
            (sender, thread)
        };
        let (sender, thread) = start(path.clone());
        let backend = Backend::new(&origin).unwrap();
        assert!(
            dispatch(
                &backend,
                &sender,
                command("first", "uploads.deliver", json!({"row":row}))
            )
            .await
            .error
            .is_some()
        );
        drop(sender);
        thread.join().unwrap();
        let (sender, thread) = start(path);
        let delivery_sender = sender.clone();
        let delivery_row = row.clone();
        let delivery = tokio::spawn(async move {
            dispatch(
                &backend,
                &delivery_sender,
                command("retry", "uploads.deliver", json!({"row":delivery_row})),
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), arrival)
            .await
            .unwrap()
            .unwrap();
        state(&sender, "uploads.cancel", json!({"id":row["id"]}))
            .await
            .unwrap();
        release.send(()).unwrap();
        let response = delivery.await.unwrap();
        assert!(response.error.is_none());
        assert_eq!(response.result, Some(Value::Null));
        assert!(
            state(&sender, "uploads.get", json!({"id":row["id"]}))
                .await
                .unwrap()
                .is_null()
        );
        drop(sender);
        thread.join().unwrap();
        server.await.unwrap();
    }
}
