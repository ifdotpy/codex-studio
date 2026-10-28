//! Durable file bytes and upload identities. Cancellation wins over late replies.
//! The page stages a file in parts, then queues it. Parts are never receipts.
use crate::{
    backend::Backend,
    delivery::{StoreSender, state},
    protocol::{Request, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
/// Largest user attachment, in bytes. Owner: runtime-schema MAX_USER_ATTACHMENT_BYTES.
pub const LIMIT: usize = 20 * 1024 * 1024;
/// Largest decoded part of one `files.append`, in bytes. The page sends parts of this size.
pub const PART_LIMIT: usize = 1024 * 1024;
const STAGE_COUNT_LIMIT: i64 = 100;
const STAGE_TOTAL_LIMIT: i64 = 256 * 1024 * 1024;

pub fn initialize(db: &Connection) -> rusqlite::Result<()> {
    let legacy: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='client_file_sources')",
        [],
        |row| row.get(0),
    )?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS client_file_stages(id TEXT PRIMARY KEY,name TEXT NOT NULL,mime TEXT NOT NULL,size INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS client_file_parts(id TEXT NOT NULL,position INTEGER NOT NULL,bytes BLOB NOT NULL,PRIMARY KEY(id,position)); CREATE TABLE IF NOT EXISTS client_uploads(id TEXT PRIMARY KEY,workspace TEXT NOT NULL,state_dir TEXT NOT NULL,agent TEXT NOT NULL,document TEXT NOT NULL,bytes BLOB NOT NULL); CREATE TABLE IF NOT EXISTS client_upload_tombstones(id TEXT PRIMARY KEY,cancelled INTEGER NOT NULL);")?;
    if legacy {
        // One-time move of whole-file staging rows from the previous layout.
        db.execute_batch("BEGIN; INSERT INTO client_file_stages SELECT id,name,mime,length(bytes) FROM client_file_sources; INSERT INTO client_file_parts SELECT id,0,bytes FROM client_file_sources; DROP TABLE client_file_sources; COMMIT;")?;
    }
    Ok(())
}
fn text<'a>(p: &'a Value, key: &str) -> Result<&'a str, String> {
    p[key]
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 4096)
        .ok_or_else(|| format!("{key} is required."))
}
fn now_millis() -> Result<u64, String> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis() as u64)
}
fn row(db: &Connection, id: &str) -> Result<Value, String> {
    let raw: Option<String> = db
        .query_row(
            "SELECT document FROM client_uploads WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    raw.map(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
        .transpose()
        .map(|v| v.unwrap_or(Value::Null))
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
fn file_name(p: &Value) -> Result<String, String> {
    let name = text(p, "name")?;
    if name.contains(['\0', '/', '\\']) {
        return Err("The file name must not contain a path.".into());
    }
    Ok(name.to_owned())
}
fn file_mime(p: &Value) -> String {
    p["mime"]
        .as_str()
        .filter(|v| v.len() <= 255)
        .unwrap_or("application/octet-stream")
        .to_owned()
}
/// Creates a staged file with its declared size. No bytes are stored yet.
fn begin(db: &Connection, name: &str, mime: &str, size: usize) -> Result<Value, String> {
    if size == 0 || size > LIMIT {
        return Err("Files must contain 1 byte to 20 MiB.".into());
    }
    let (count, total): (i64, i64) = db
        .query_row(
            "SELECT count(*),COALESCE(sum(size),0) FROM client_file_stages",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| e.to_string())?;
    if count >= STAGE_COUNT_LIMIT || total + size as i64 > STAGE_TOTAL_LIMIT {
        return Err("Save or remove the selected files before adding more files.".into());
    }
    let id = crate::platform::new_identity()?;
    db.execute(
        "INSERT INTO client_file_stages VALUES(?1,?2,?3,?4)",
        params![id, name, mime, size as i64],
    )
    .map_err(|e| e.to_string())?;
    Ok(json!({"nativeHandle":id,"name":name,"type":mime,"size":size}))
}
fn stage_row(db: &Connection, id: &str) -> Result<(String, String, i64), String> {
    db.query_row(
        "SELECT name,mime,size FROM client_file_stages WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
    .map_err(|e| e.to_string())?
    .ok_or_else(|| {
        "The selected file is unavailable. Check saved uploads before selecting it again."
            .to_owned()
    })
}
/// Stores one part at its byte offset. A retry of a stored part changes nothing.
fn append(db: &Connection, p: &Value) -> Result<Value, String> {
    let id = text(p, "nativeHandle")?;
    let (_, _, size) = stage_row(db, id)?;
    let position = p["offset"]
        .as_u64()
        .filter(|n| *n <= size as u64)
        .ok_or("Invalid file offset.")? as i64;
    let encoded = p["base64"]
        .as_str()
        .filter(|v| v.len() <= PART_LIMIT.div_ceil(3) * 4)
        .ok_or("The file part is too large.")?;
    let bytes = STANDARD.decode(encoded).map_err(|_| "Invalid file data.")?;
    if bytes.is_empty() || bytes.len() > PART_LIMIT {
        return Err("File parts contain 1 byte to 1 MiB.".into());
    }
    let saved: Option<i64> = db
        .query_row(
            "SELECT length(bytes) FROM client_file_parts WHERE id=?1 AND position=?2",
            params![id, position],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if let Some(length) = saved {
        return if length == bytes.len() as i64 {
            Ok(Value::Null)
        } else {
            Err("The file part conflicts with saved bytes.".into())
        };
    }
    let received: i64 = db
        .query_row(
            "SELECT COALESCE(sum(length(bytes)),0) FROM client_file_parts WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if position != received {
        return Err("The file part is out of order.".into());
    }
    if received + bytes.len() as i64 > size {
        return Err("The file is larger than its declared size.".into());
    }
    db.execute(
        "INSERT INTO client_file_parts VALUES(?1,?2,?3)",
        params![id, position, bytes],
    )
    .map_err(|e| e.to_string())?;
    Ok(Value::Null)
}
/// Joins the parts of a staged file after checking that they cover its declared size.
fn assemble(db: &Connection, id: &str) -> Result<(String, String, Vec<u8>), String> {
    let (name, mime, size) = stage_row(db, id)?;
    let incomplete = || "The selected file is incomplete. Select it again.".to_owned();
    let mut statement = db
        .prepare("SELECT position,bytes FROM client_file_parts WHERE id=?1 ORDER BY position")
        .map_err(|e| e.to_string())?;
    let parts = statement
        .query_map([id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::with_capacity(size as usize);
    for part in parts {
        let (position, chunk) = part.map_err(|e| e.to_string())?;
        if position != bytes.len() as i64 {
            return Err(incomplete());
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.len() as i64 != size {
        return Err(incomplete());
    }
    Ok((name, mime, bytes))
}
fn discard(db: &Connection, id: &str) -> Result<(), String> {
    db.execute("DELETE FROM client_file_parts WHERE id=?1", [id])
        .and_then(|_| db.execute("DELETE FROM client_file_stages WHERE id=?1", [id]))
        .map(|_| ())
        .map_err(|e| e.to_string())
}
fn insert_upload(
    db: &Connection,
    id: &str,
    workspace: &str,
    state_dir: &str,
    agent: &str,
    document: &Value,
    bytes: &[u8],
) -> Result<(), String> {
    db.execute(
        "INSERT INTO client_uploads VALUES(?1,?2,?3,?4,?5,?6)",
        params![id, workspace, state_dir, agent, document.to_string(), bytes],
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}
fn apply(db: &Connection, request: &Request) -> Result<Value, String> {
    let p = &request.params;
    match request.method.as_str() {
        "files.begin" => {
            let name = file_name(p)?;
            let mime = file_mime(p);
            let size = p["size"]
                .as_u64()
                .ok_or("Files must contain 1 byte to 20 MiB.")? as usize;
            begin(db, &name, &mime, size)
        }
        "files.append" => append(db, p),
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
        // One-shot staging for picked files, which Rust already holds in memory.
        "files.stage" | "files.text" => {
            let name = file_name(p)?;
            let mime = file_mime(p);
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
            let staged = begin(db, &name, &mime, bytes.len())?;
            let id = staged["nativeHandle"].as_str().unwrap_or_default();
            db.execute(
                "INSERT INTO client_file_parts VALUES(?1,0,?2)",
                params![id, bytes],
            )
            .map_err(|e| e.to_string())?;
            Ok(staged)
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
                let (name, mime, bytes) = assemble(db, source)?;
                let id = crate::platform::new_identity()?;
                let document = json!({"id":id,"workspace":workspace,"stateDir":state_dir,"agent":agent,"name":name,"mime":mime,"size":bytes.len(),"created":now_millis()? + index as u64});
                insert_upload(db, &id, workspace, state_dir, agent, &document, &bytes)?;
                discard(db, source)?;
                result.push(document);
            }
            Ok(json!(result))
        }
        // Moves a file the previous browser store still held. Settled or existing rows win.
        "uploads.import" => {
            let id = text(p, "id")?;
            let source = text(p, "nativeHandle")?;
            if status(db, id)?["settled"] == true || !row(db, id)?.is_null() {
                discard(db, source)?;
                return Ok(Value::Null);
            }
            let workspace = text(p, "workspace")?;
            if !crate::projections::valid_workspace(workspace) {
                return Err("Invalid workspace identity.".into());
            }
            let state_dir = text(p, "stateDir")?;
            let agent = text(p, "agent")?;
            let created = p["created"]
                .as_u64()
                .ok_or("Invalid upload creation time.")?;
            let (name, mime, bytes) = assemble(db, source)?;
            let mut document = json!({"id":id,"workspace":workspace,"stateDir":state_dir,"agent":agent,"name":name,"mime":mime,"size":bytes.len(),"created":created});
            if p["asset"].is_object() {
                document["asset"] = p["asset"].clone();
            }
            insert_upload(db, id, workspace, state_dir, agent, &document, &bytes)?;
            discard(db, source)?;
            Ok(document)
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
                result.push(
                    serde_json::from_str::<Value>(&row.map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?,
                );
            }
            Ok(json!(result))
        }
        "uploads.status" => status(db, text(p, "id")?),
        "uploads.get" => row(db, text(p, "id")?),
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
            let mut current = row(db, id)?;
            if current.is_null() || status(db, id)?["settled"] == true {
                return Ok(Value::Null);
            }
            current["asset"] = p["asset"].clone();
            db.execute(
                "UPDATE client_uploads SET document=?2 WHERE id=?1",
                params![id, current.to_string()],
            )
            .map_err(|e| e.to_string())?;
            Ok(current["asset"].clone())
        }
        _ => Err("Unknown upload command.".into()),
    }
}
pub fn stored(db: &mut Connection, request: &Request) -> Response {
    let result = (|| -> Result<Response, String> {
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        // Only small requests are receipts: a retried queue returns its first rows.
        let durable = matches!(request.method.as_str(), "files.begin" | "uploads.queue");
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
            sender.send((Request{id:request.id.clone(),method:"uploads.queue".into(),params},reply)).await.map_err(|e|e.to_string())?;
            let response=receive.await.map_err(|e|e.to_string())?;
            return response.result.ok_or_else(||response.error.map(|e|e.message).unwrap_or("Missing file state response.".into()));
        }
        let expected=&p["row"];let id=text(expected,"id")?;
        let lock=upload_lock(id)?;let _guard=lock.lock().await;
        let current=state(sender,"uploads.get",json!({"id":id})).await?;
        if current.is_null(){return Ok(Value::Null);}
        for key in ["workspace","agent","stateDir"] {if current[key]!=expected[key]{return Err("The saved upload belongs to another workspace or chat.".into());}}
        let workspace=text(&current,"workspace")?;
        let identity=crate::sync::identity(backend).await.map_err(|e|e.message)?;
        if identity["workspaceId"]!=workspace{return Err("The server workspace changed. Saved files remain on this device.".into());}
        if current["asset"].is_object(){return Ok(state(sender,"uploads.get",json!({"id":id})).await?["asset"].clone());}
        let bytes=state(sender,"uploads.content",json!({"id":id})).await?;
        if bytes.is_null(){return Ok(Value::Null);}
        let response=backend.dispatch(Request{id:request.id.clone(),method:"api.request".into(),params:json!({"path":"/api/assets","workspaceId":workspace,"timeoutMs":60000,"body":{"id":id,"agent":current["agent"],"name":current["name"],"base64":bytes}})}).await;
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
                while let Some((request, reply)) = receive.blocking_recv() {
                    drop(reply.send(store.dispatch(request)));
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
    fn stage(store: &mut Store, id: &str, name: &str, size: usize) -> Value {
        store
            .dispatch(command(
                id,
                "files.begin",
                json!({"name":name,"mime":"text/plain","size":size}),
            ))
            .result
            .unwrap()
    }
    fn append(store: &mut Store, id: &str, handle: &str, offset: usize, bytes: &[u8]) -> Response {
        store.dispatch(command(
            id,
            "files.append",
            json!({"nativeHandle":handle,"offset":offset,"base64":STANDARD.encode(bytes)}),
        ))
    }
    fn queue_one(store: &mut Store, id: &str, handle: &str) -> Response {
        store.dispatch(command(
            id,
            "uploads.queue",
            json!({"workspace":"a".repeat(32),"stateDir":"/state","agent":"lead","files":[{"nativeHandle":handle}]}),
        ))
    }
    #[test]
    fn staged_parts_arrive_in_order_and_retries_change_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("client.sqlite3");
        let mut store = Store::open(&path).unwrap();
        let file = stage(&mut store, "begin", "message.txt", 10);
        let handle = file["nativeHandle"].as_str().unwrap().to_owned();
        // A retried begin replays the same identity.
        assert_eq!(stage(&mut store, "begin", "message.txt", 10), file);
        assert!(
            append(&mut store, "gap", &handle, 4, b"456")
                .error
                .is_some()
        );
        assert!(
            append(&mut store, "too-big", &handle, 0, b"0123456789a")
                .error
                .is_some()
        );
        assert!(queue_one(&mut store, "early", &handle).error.is_some());
        assert!(
            append(&mut store, "first", &handle, 0, b"0123")
                .error
                .is_none()
        );
        assert!(
            append(&mut store, "retry", &handle, 0, b"0123")
                .error
                .is_none()
        );
        assert!(
            append(&mut store, "conflict", &handle, 0, b"01")
                .error
                .is_some()
        );
        assert!(
            append(&mut store, "second", &handle, 4, b"456789")
                .error
                .is_none()
        );
        drop(store);
        // Parts survive a restart, and the queue sees the complete declared size.
        let mut store = Store::open(&path).unwrap();
        let rows = queue_one(&mut store, "queue", &handle).result.unwrap();
        assert_eq!(rows[0]["size"], 10);
        let bytes = store
            .dispatch(command(
                "bytes",
                "uploads.content",
                json!({"id":rows[0]["id"]}),
            ))
            .result
            .unwrap();
        assert_eq!(
            STANDARD.decode(bytes.as_str().unwrap()).unwrap(),
            b"0123456789"
        );
    }
    #[test]
    fn large_uploads_stage_in_bounded_parts_outside_the_request_log() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&temp.path().join("client.sqlite3")).unwrap();
        let data: Vec<u8> = (0..3 * PART_LIMIT + 17).map(|i| (i % 251) as u8).collect();
        let file = stage(&mut store, "begin", "large.bin", data.len());
        let handle = file["nativeHandle"].as_str().unwrap().to_owned();
        assert!(
            append(&mut store, "oversized", &handle, 0, &data[..PART_LIMIT + 1])
                .error
                .is_some()
        );
        for (index, part) in data.chunks(PART_LIMIT).enumerate() {
            let offset = index * PART_LIMIT;
            let name = format!("part-{index}");
            assert!(
                append(&mut store, &name, &handle, offset, part)
                    .error
                    .is_none()
            );
        }
        // Parts never enter the receipt log, so the bytes are stored once.
        let logged: i64 = store
            .connection
            .query_row(
                "SELECT count(*) FROM client_receipts WHERE method='files.append'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(logged, 0);
        let rows = queue_one(&mut store, "queue", &handle).result.unwrap();
        let bytes = store
            .dispatch(command(
                "bytes",
                "uploads.content",
                json!({"id":rows[0]["id"]}),
            ))
            .result
            .unwrap();
        assert_eq!(STANDARD.decode(bytes.as_str().unwrap()).unwrap(), data);
    }
    #[test]
    fn legacy_whole_file_staging_moves_into_parts_on_open() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("client.sqlite3");
        {
            let _ = Store::open(&path).unwrap();
            let db = Connection::open(&path).unwrap();
            db.execute_batch("DROP TABLE client_file_stages; DROP TABLE client_file_parts; CREATE TABLE client_file_sources(id TEXT PRIMARY KEY,name TEXT NOT NULL,mime TEXT NOT NULL,bytes BLOB NOT NULL); INSERT INTO client_file_sources VALUES('old','old.txt','text/plain',CAST('legacy' AS BLOB));").unwrap();
        }
        let mut store = Store::open(&path).unwrap();
        let rows = queue_one(&mut store, "queue", "old").result.unwrap();
        let bytes = store
            .dispatch(command(
                "bytes",
                "uploads.content",
                json!({"id":rows[0]["id"]}),
            ))
            .result
            .unwrap();
        assert_eq!(STANDARD.decode(bytes.as_str().unwrap()).unwrap(), b"legacy");
    }
    #[test]
    fn imported_uploads_keep_identity_and_never_return_settled_rows() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&temp.path().join("client.sqlite3")).unwrap();
        let file = stage(&mut store, "begin", "legacy.txt", 5);
        let handle = file["nativeHandle"].as_str().unwrap().to_owned();
        assert!(
            append(&mut store, "part", &handle, 0, b"hello")
                .error
                .is_none()
        );
        let import = |handle: &str| {
            command(
                "import",
                "uploads.import",
                json!({"id":"legacy-1","workspace":"a".repeat(32),"stateDir":"/state","agent":"lead","nativeHandle":handle,"created":7}),
            )
        };
        let row = store.dispatch(import(&handle)).result.unwrap();
        assert_eq!(row["id"], "legacy-1");
        assert_eq!(row["created"], 7);
        // The page checks identity before staging. A repeated import changes nothing.
        assert_eq!(store.dispatch(import("consumed")).result, Some(Value::Null));
        assert_eq!(
            store
                .dispatch(command("get", "uploads.get", json!({"id":"legacy-1"})))
                .result,
            Some(row)
        );
        // A cancelled upload never comes back through an import.
        assert!(
            store
                .dispatch(command(
                    "cancel",
                    "uploads.cancel",
                    json!({"id":"legacy-1"})
                ))
                .error
                .is_none()
        );
        assert_eq!(store.dispatch(import("consumed")).result, Some(Value::Null));
        assert_eq!(
            store
                .dispatch(command("get", "uploads.get", json!({"id":"legacy-1"})))
                .result,
            Some(Value::Null)
        );
    }
}
