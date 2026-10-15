//! Durable file bytes and upload identities. Cancellation wins over late replies.
use crate::{
    backend::Backend,
    delivery::{StoreSender, state},
    protocol::{Request, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
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
                let document = json!({"id":id,"workspace":workspace,"stateDir":state_dir,"agent":agent,"name":name,"mime":mime,"size":bytes.len(),"created":created});
                db.execute(
                    "INSERT INTO client_uploads VALUES(?1,?2,?3,?4,?5,?6)",
                    params![id, workspace, state_dir, agent, document.to_string(), bytes],
                )
                .map_err(|e| e.to_string())?;
                db.execute("DELETE FROM client_file_sources WHERE id=?1", [source])
                    .map_err(|e| e.to_string())?;
                result.push(document);
            }
            Ok(json!(result))
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
        let durable = matches!(
            request.method.as_str(),
            "files.stage_many" | "files.stage" | "files.text" | "uploads.queue"
        );
        if durable {
            if let Some(reply) = crate::store::replay(&tx, request).map_err(|e| e.to_string())? {
                return Ok(reply);
            }
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
                    let _ = reply.send(store.dispatch(request));
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
