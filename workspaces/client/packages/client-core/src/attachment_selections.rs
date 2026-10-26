//! Host file selections retain their original scope and request identities.

use crate::protocol::{Request, Response};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub fn contract() -> &'static Value {
    static CONTRACT: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    CONTRACT.get_or_init(|| {
        serde_json::from_str(include_str!("../../../contracts/attachment-selection.json"))
            .expect("attachment selection contract")
    })
}

pub(crate) fn initialize(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS client_attachment_selections(
        id TEXT PRIMARY KEY,scope TEXT NOT NULL,kind TEXT NOT NULL,enqueue_request_id TEXT NOT NULL UNIQUE,document TEXT NOT NULL,
        text_sha256 TEXT,stage_request_id TEXT UNIQUE,content_sha256 TEXT NOT NULL,acknowledged INTEGER NOT NULL DEFAULT 0);
        CREATE UNIQUE INDEX IF NOT EXISTS client_attachment_pending_owner
        ON client_attachment_selections(scope,kind) WHERE acknowledged=0;")
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttachmentScope {
    pub server_id: String,
    pub workspace_id: String,
    pub view_id: String,
    pub source_identity: String,
}
fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn scope(db: &Connection, value: &Value) -> Result<String, String> {
    let owner: AttachmentScope =
        serde_json::from_value(value.clone()).map_err(|_| "Invalid native attachment owner.")?;
    if owner.server_id.is_empty()
        || owner.view_id.is_empty()
        || owner.source_identity.len() != 64
        || !owner
            .source_identity
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || !crate::projections::valid_workspace(&owner.workspace_id)
    {
        return Err("Invalid native attachment owner.".into());
    }
    let workspace: Option<String> = db
        .query_row(
            "SELECT json_extract(value,'$.workspaceId') FROM client_identity WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if workspace.as_deref() != Some(owner.workspace_id.as_str()) {
        return Err("The native attachment workspace changed.".into());
    }
    serde_json::to_string(&owner).map_err(|error| error.to_string())
}
fn target(value: &Value, workspace: &Value) -> Result<Value, String> {
    let object = value
        .as_object()
        .ok_or("The attachment target is invalid.")?;
    let fields = contract()["targetFields"]
        .as_array()
        .expect("attachment target fields");
    if object.len() != fields.len()
        || fields.iter().any(|key| {
            !value[key.as_str().expect("target field")]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        })
        || value["workspace"] != *workspace
    {
        return Err("The attachment target belongs to another workspace or is invalid.".into());
    }
    Ok(value.clone())
}
struct Selection {
    scope: String,
    kind: String,
    document: Value,
    text_hash: Option<String>,
    stage_id: Option<String>,
    content_hashes: Value,
    acknowledged: bool,
}
fn read(db: &Connection, id: &str) -> Result<Selection, String> {
    let result = db.query_row("SELECT scope,kind,document,text_sha256,stage_request_id,content_sha256,acknowledged FROM client_attachment_selections WHERE id=?1", [id], |row| {
        Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,Option<String>>(3)?,row.get::<_,Option<String>>(4)?,row.get::<_,String>(5)?,row.get::<_,bool>(6)?))
    }).map_err(|_| "The native attachment selection is unavailable.")?;
    Ok(Selection {
        scope: result.0,
        kind: result.1,
        document: serde_json::from_str(&result.2).map_err(|error| error.to_string())?,
        text_hash: result.3,
        stage_id: result.4,
        content_hashes: serde_json::from_str(&result.5).map_err(|error| error.to_string())?,
        acknowledged: result.6,
    })
}
fn owned(db: &Connection, id: &str, owner: &str) -> Result<Selection, String> {
    let saved = read(db, id)?;
    if saved.scope != owner {
        return Err("The native attachment selection belongs to another owner.".into());
    }
    Ok(saved)
}
fn save(db: &Connection, saved: &Selection) -> Result<(), String> {
    db.execute(
        "UPDATE client_attachment_selections SET document=?2,content_sha256=?3 WHERE id=?1",
        params![
            saved.document["id"]
                .as_str()
                .ok_or("Invalid selection identity.")?,
            saved.document.to_string(),
            saved.content_hashes.to_string()
        ],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}
fn pending_id(db: &Connection, owner: &str, kind: &str) -> Result<Option<String>, String> {
    db.query_row(
        "SELECT id FROM client_attachment_selections WHERE scope=?1 AND kind=?2 AND acknowledged=0",
        params![owner, kind],
        |row| row.get(0),
    )
    .optional()
    .map_err(|error| error.to_string())
}
fn id() -> Result<String, String> {
    crate::server_keys::random_id()
}
fn insert(
    db: &Connection,
    owner: &str,
    kind: &str,
    document: Value,
    text_hash: Option<&str>,
    stage_id: Option<&str>,
    content_hashes: Value,
) -> Result<Value, String> {
    db.execute("INSERT INTO client_attachment_selections(id,scope,kind,enqueue_request_id,document,text_sha256,stage_request_id,content_sha256) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![document["id"].as_str(),owner,kind,document["enqueueRequestId"].as_str(),document.to_string(),text_hash,stage_id,content_hashes.to_string()]).map_err(|error|error.to_string())?;
    Ok(document)
}
fn normalized(request: &Request) -> Result<Request, String> {
    let mut result = Request {
        id: request.id.clone(),
        method: request.method.clone(),
        params: request.params.clone(),
    };
    if request.method == contract()["privateMethods"]["capture"] {
        let files = result.params["files"]
            .as_array_mut()
            .ok_or("Invalid native attachment files.")?;
        for file in files {
            let encoded = file["base64"]
                .as_str()
                .filter(|v| v.len() <= crate::uploads::LIMIT.div_ceil(3) * 4)
                .ok_or("Invalid native attachment bytes.")?;
            let bytes = STANDARD
                .decode(encoded)
                .map_err(|_| "Invalid native attachment bytes.")?;
            file.as_object_mut()
                .ok_or("Invalid native attachment file.")?
                .remove("base64");
            file["sha256"] = json!(hash(&bytes));
        }
    } else if request.method == contract()["methods"]["textIntent"] {
        let text = result.params["text"]
            .as_str()
            .ok_or("Attachment text is required.")?;
        if text.is_empty() || text.len() > crate::uploads::LIMIT {
            return Err("The attachment text exceeds the file byte limit.".into());
        }
        let digest = hash(text.as_bytes());
        result
            .params
            .as_object_mut()
            .ok_or("Invalid text attachment intent.")?
            .remove("text");
        result.params["textSha256"] = json!(digest);
    }
    Ok(result)
}
fn receipt(db: &Connection, id: &str) -> Result<Option<(String, Value, Response)>, String> {
    let row: Option<(String, String, String)> = db
        .query_row(
            "SELECT method,params,response FROM client_receipts WHERE request_id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    row.map(|(method, params, response)| {
        Ok((
            method,
            serde_json::from_str(&params).map_err(|error| error.to_string())?,
            serde_json::from_str(&response).map_err(|error| error.to_string())?,
        ))
    })
    .transpose()
}
fn hydrate_text(db: &Connection, saved: &mut Selection) -> Result<(), String> {
    if saved.kind != "text"
        || saved.document["files"]
            .as_array()
            .is_some_and(|files| !files.is_empty())
    {
        return Ok(());
    }
    let stage = saved
        .stage_id
        .as_deref()
        .ok_or("The native text attachment has no stage identity.")?;
    let (method, params, response) =
        receipt(db, stage)?.ok_or("The text attachment has not confirmed its saved file.")?;
    let text = params["text"]
        .as_str()
        .ok_or("The text attachment receipt has invalid contents.")?;
    let field = contract()["requestScopeField"]
        .as_str()
        .expect("attachment request scope field");
    if scope(db, &params[field])? != saved.scope {
        return Err("The text attachment receipt belongs to another native view.".into());
    }
    if method != contract()["textFile"]["stageMethod"]
        || response.id != stage
        || response.error.is_some()
        || params["name"] != contract()["textFile"]["name"]
        || params["mime"] != contract()["textFile"]["mime"]
        || saved.text_hash.as_deref() != Some(hash(text.as_bytes()).as_str())
    {
        return Err("The saved text attachment receipt has another identity or contents.".into());
    }
    let file = response
        .result
        .ok_or("The text attachment has no saved handle.")?;
    if file["name"] != params["name"]
        || file["type"] != params["mime"]
        || file["size"].as_u64() != Some(text.len() as u64)
        || !file["nativeHandle"].as_str().is_some_and(|s| !s.is_empty())
    {
        return Err("The text attachment receipt has invalid file metadata.".into());
    }
    saved.document["files"] = json!([file]);
    saved.content_hashes = json!([saved.text_hash]);
    save(db, saved)
}
fn queue_params(document: &Value, owner: &str) -> Result<Value, String> {
    let mut params = document["target"]
        .as_object()
        .ok_or("Bind the original attachment target before enqueue.")?
        .clone();
    params.insert("files".into(), document["files"].clone());
    params.insert(
        contract()["requestScopeField"]
            .as_str()
            .expect("attachment request scope field")
            .into(),
        serde_json::from_str(owner).map_err(|error| error.to_string())?,
    );
    Ok(Value::Object(params))
}

/// This runs inside the existing upload transaction, before source consumption.
/// Only a journal-owned enqueue ID gains the additional exact selection fence.
pub(crate) fn validate_enqueue(db: &Connection, request: &Request) -> Result<(), String> {
    let selected: Option<String> = db
        .query_row(
            "SELECT id FROM client_attachment_selections WHERE enqueue_request_id=?1",
            [&request.id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let Some(selected) = selected else {
        return Ok(());
    };
    let mut saved = read(db, &selected)?;
    let field = contract()["requestScopeField"]
        .as_str()
        .expect("attachment request scope field");
    if scope(db, &request.params[field])? != saved.scope {
        return Err("The native attachment enqueue belongs to another caller view.".into());
    }
    let fields = contract()["batchFields"]
        .as_array()
        .expect("attachment batch fields");
    if saved.document.as_object().is_none_or(|object| {
        object.len() != fields.len()
            || fields
                .iter()
                .any(|field| !object.contains_key(field.as_str().expect("attachment batch field")))
    }) {
        return Err("The native attachment selection has an invalid saved schema.".into());
    }
    let owner: Value = serde_json::from_str(&saved.scope).map_err(|error| error.to_string())?;
    scope(db, &owner)?;
    hydrate_text(db, &mut saved)?;
    if queue_params(&saved.document, &saved.scope)? != request.params {
        return Err(
            "The original native attachment target or handles changed before enqueue.".into(),
        );
    }
    for (index, file) in saved.document["files"]
        .as_array()
        .ok_or("Invalid saved attachment handles.")?
        .iter()
        .enumerate()
    {
        let (name, mime, bytes): (String, String, Vec<u8>) = db
            .query_row(
                "SELECT name,mime,bytes FROM client_file_sources WHERE id=?1",
                [file["nativeHandle"]
                    .as_str()
                    .ok_or("Invalid attachment handle.")?],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|_| "The original attachment bytes are unavailable.")?;
        if file["name"] != name
            || file["type"] != mime
            || file["size"].as_u64() != Some(bytes.len() as u64)
            || saved.content_hashes[index].as_str() != Some(hash(&bytes).as_str())
        {
            return Err("The original native attachment bytes or metadata changed.".into());
        }
    }
    Ok(())
}

/// A host-bound text intent cannot be poisoned through another native view.
pub(crate) fn validate_text_stage(db: &Connection, request: &Request) -> Result<(), String> {
    let selected: Option<String> = db
        .query_row(
            "SELECT id FROM client_attachment_selections WHERE stage_request_id=?1",
            [&request.id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let Some(selected) = selected else {
        return Ok(());
    };
    let saved = read(db, &selected)?;
    let field = contract()["requestScopeField"]
        .as_str()
        .expect("attachment request scope field");
    let p = &request.params;
    if scope(db, &p[field])? != saved.scope
        || saved.kind != "text"
        || saved.acknowledged
        || p["name"] != contract()["textFile"]["name"]
        || p["mime"] != contract()["textFile"]["mime"]
        || p["text"].as_str().map(|text| hash(text.as_bytes())) != saved.text_hash
    {
        return Err(
            "The original text attachment scope or contents changed before staging.".into(),
        );
    }
    Ok(())
}

fn apply(
    db: &Connection,
    request: &Request,
    canonical: &Request,
    owner: &str,
) -> Result<Value, String> {
    let p = &request.params;
    let methods = &contract()["methods"];
    if request.method == methods["pending"] {
        let kind = p["kind"].as_str();
        if kind.is_some_and(|kind| {
            !contract()["kinds"]
                .as_array()
                .expect("attachment kinds")
                .iter()
                .any(|allowed| allowed == kind)
        }) {
            return Err("Invalid native attachment kind.".into());
        }
        let mut statement = db.prepare("SELECT document FROM client_attachment_selections WHERE scope=?1 AND acknowledged=0 AND (?2 IS NULL OR kind=?2) ORDER BY rowid").map_err(|error|error.to_string())?;
        let rows = statement
            .query_map(params![owner, kind], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?;
        let mut result = vec![];
        for row in rows {
            result.push(
                serde_json::from_str::<Value>(&row.map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?,
            );
        }
        return Ok(json!(result));
    }
    if request.method == contract()["privateMethods"]["captureStatus"] {
        let capture = p["captureId"]
            .as_str()
            .ok_or("Missing native capture identity.")?;
        let Some((method, params, response)) = receipt(db, capture)? else {
            return Ok(Value::Null);
        };
        if method != contract()["privateMethods"]["capture"]
            || scope(db, &params["scope"])? != owner
            || params["kind"] != p["kind"]
            || params["target"] != p["target"]
            || response.id != capture
            || response.error.is_some()
        {
            return Err("The original native capture has another scope or intent.".into());
        }
        return response
            .result
            .ok_or("The native capture has no saved result.".into());
    }
    if request.method == contract()["privateMethods"]["capture"] {
        let kind = p["kind"]
            .as_str()
            .filter(|kind| {
                *kind != "text"
                    && contract()["kinds"]
                        .as_array()
                        .expect("attachment kinds")
                        .iter()
                        .any(|allowed| allowed == *kind)
            })
            .ok_or("Invalid native attachment gesture.")?;
        if pending_id(db, owner, kind)?.is_some() {
            return Err(
                "Resolve the original native attachment selection before selecting more files."
                    .into(),
            );
        }
        let selected = p["files"]
            .as_array()
            .filter(|files| {
                !files.is_empty()
                    && files.len() <= studio_runtime_schema::shared_values::MAX_TURN_ATTACHMENTS
            })
            .ok_or("The selected file count exceeds the attachment limit.")?;
        let files = crate::uploads::stage_selection_files(db, selected)?;
        let destination = if p["target"].is_null() {
            Value::Null
        } else {
            target(&p["target"], &p["scope"]["workspaceId"])?
        };
        let hashes: Vec<Value> = canonical.params["files"]
            .as_array()
            .ok_or("Invalid native attachment hashes.")?
            .iter()
            .map(|file| file["sha256"].clone())
            .collect();
        return insert(
            db,
            owner,
            kind,
            json!({"id":id()?,"enqueueRequestId":id()?,"serverId":p["scope"]["serverId"],"workspaceId":p["scope"]["workspaceId"],"files":files,"target":destination}),
            None,
            None,
            json!(hashes),
        );
    }
    if request.method == methods["textIntent"] {
        let destination = target(&p["target"], &p["scope"]["workspaceId"])?;
        let digest = canonical.params["textSha256"]
            .as_str()
            .ok_or("Missing text attachment digest.")?;
        if let Some(selected) = pending_id(db, owner, "text")? {
            let saved = owned(db, &selected, owner)?;
            if saved.document["target"] != destination || saved.text_hash.as_deref() != Some(digest)
            {
                return Err(
                    "Resolve the original text attachment before changing its contents or target."
                        .into(),
                );
            }
            let mut document = saved.document;
            document["stageRequestId"] = json!(saved.stage_id);
            return Ok(document);
        }
        let stage = id()?;
        let mut document = insert(
            db,
            owner,
            "text",
            json!({"id":id()?,"enqueueRequestId":id()?,"serverId":p["scope"]["serverId"],"workspaceId":p["scope"]["workspaceId"],"files":[],"target":destination}),
            Some(digest),
            Some(&stage),
            json!([]),
        )?;
        document["stageRequestId"] = json!(stage);
        return Ok(document);
    }
    if request.method == methods["bindTarget"] {
        let selected = p["selectionId"]
            .as_str()
            .ok_or("Missing attachment selection identity.")?;
        let mut saved = owned(db, selected, owner)?;
        if saved.acknowledged {
            return Err("The native attachment selection is already acknowledged.".into());
        }
        let destination = target(&p["target"], &p["scope"]["workspaceId"])?;
        if !saved.document["target"].is_null() && saved.document["target"] != destination {
            return Err("The original attachment target cannot change during a retry.".into());
        }
        saved.document["target"] = destination;
        if saved.kind == "text" {
            hydrate_text(db, &mut saved)?;
        }
        save(db, &saved)?;
        return Ok(saved.document);
    }
    if request.method == methods["settle"] {
        let selected = p["batch"]["id"]
            .as_str()
            .ok_or("Missing attachment selection identity.")?;
        let mut saved = owned(db, selected, owner)?;
        hydrate_text(db, &mut saved)?;
        if saved.document != p["batch"] {
            return Err(
                "The native attachment acknowledgement has another selection or contents.".into(),
            );
        }
        let enqueue = saved.document["enqueueRequestId"]
            .as_str()
            .ok_or("Missing original enqueue identity.")?;
        let (method, params, response) =
            receipt(db, enqueue)?.ok_or("The native attachment enqueue has no durable receipt.")?;
        if method != "uploads.queue"
            || response.id != enqueue
            || response.error.is_some()
            || response
                .result
                .as_ref()
                .and_then(Value::as_array)
                .map(Vec::len)
                != saved.document["files"].as_array().map(Vec::len)
            || params != queue_params(&saved.document, &saved.scope)?
        {
            return Err(
                "The native attachment enqueue receipt does not prove this selection.".into(),
            );
        }
        db.execute(
            "UPDATE client_attachment_selections SET acknowledged=1 WHERE id=?1",
            [selected],
        )
        .map_err(|error| error.to_string())?;
        return Ok(json!({"selectionId":selected,"enqueueRequestId":enqueue,"acknowledged":true}));
    }
    Err("Unknown native attachment selection command.".into())
}

pub(crate) fn stored(db: &mut Connection, request: &Request) -> Response {
    let result = (|| -> Result<Response, String> {
        let fields = contract()["parameterFields"][&request.method]
            .as_array()
            .ok_or("Unknown native attachment selection command.")?;
        let params = request
            .params
            .as_object()
            .ok_or("Invalid native attachment parameters.")?;
        if params.keys().any(|key| {
            !fields
                .iter()
                .any(|field| field.as_str() == Some(key.as_str()))
        }) {
            return Err("The native attachment parameters contain an unknown field.".into());
        }
        if request.method == contract()["privateMethods"]["capture"] {
            let fields = contract()["captureFileFields"]
                .as_array()
                .expect("native capture file fields");
            let files = request.params["files"]
                .as_array()
                .ok_or("Invalid native captured files.")?;
            for file in files {
                let object = file.as_object().ok_or("Invalid native captured file.")?;
                if object.len() != fields.len()
                    || object.keys().any(|key| {
                        !fields
                            .iter()
                            .any(|field| field.as_str() == Some(key.as_str()))
                    })
                {
                    return Err("The native captured file parameters are invalid.".into());
                }
            }
        }
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let owner = scope(&tx, &request.params["scope"])?;
        let canonical = normalized(request)?;
        let durable = request.method != contract()["methods"]["pending"]
            && request.method != contract()["privateMethods"]["captureStatus"];
        if durable
            && let Some(response) =
                crate::store::replay(&tx, &canonical).map_err(|error| error.to_string())?
        {
            return Ok(response);
        }
        let value = apply(&tx, request, &canonical, &owner)?;
        let response = Response::success(request.id.clone(), value);
        if durable {
            crate::store::record(&tx, &canonical, &response).map_err(|error| error.to_string())?;
        }
        tx.commit().map_err(|error| error.to_string())?;
        Ok(response)
    })();
    result.unwrap_or_else(|error| {
        Response::failure(request.id.clone(), "attachment_selection", error)
    })
}

#[cfg(test)]
mod tests {
    use crate::{protocol::Request, store::Store};
    use serde_json::{Value, json};

    fn scope() -> Value {
        json!({"serverId":"local","workspaceId":"a".repeat(32),"viewId":"main:local","sourceIdentity":"c".repeat(64)})
    }
    fn target() -> Value {
        json!({"stateDir":"project","agent":"original-agent","workspace":"a".repeat(32)})
    }
    fn call(id: &str, method: &str, params: Value) -> Request {
        Request {
            id: id.into(),
            method: method.into(),
            params,
        }
    }
    fn capture(id: &str) -> Request {
        call(
            id,
            "attachments.capture",
            json!({"scope":scope(),"kind":"paste","target":target(),
          "files":[{"name":"original.txt","mime":"text/plain","base64":"Ym9keQ=="}]}),
        )
    }
    fn value(store: &mut Store, request: Request) -> Value {
        let response = store.dispatch(request);
        assert!(response.error.is_none(), "{:?}", response.error);
        response.result.expect("successful selection result")
    }
    fn fixture(path: &std::path::Path) -> Store {
        let mut store = Store::open(path).expect("selection fixture store");
        value(
            &mut store,
            call(
                "identity",
                "identity.set",
                json!({"workspaceId":"a".repeat(32)}),
            ),
        );
        store
    }
    fn enqueue(batch: &Value) -> Request {
        let mut params = batch["target"].clone();
        params["files"] = batch["files"].clone();
        params["_attachmentScope"] = scope();
        call(
            batch["enqueueRequestId"]
                .as_str()
                .expect("original enqueue ID"),
            "uploads.queue",
            params,
        )
    }
    #[tokio::test]
    async fn native_attachment_selection_picker_restores_original_capture_before_any_native_action()
    {
        let directory = tempfile::tempdir().unwrap();
        let mut store = fixture(&directory.path().join("client.sqlite"));
        let mut original = capture("picker-original");
        original.params["kind"] = json!("pick");
        let batch = value(&mut store, original);
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<(
            Request,
            tokio::sync::oneshot::Sender<crate::protocol::Response>,
        )>(8);
        let worker = tokio::spawn(async move {
            while let Some((request, reply)) = receiver.recv().await {
                let _ = reply.send(store.dispatch(request));
            }
            store
        });
        let picker = |id: &str| {
            call(
                id,
                "desktop.pickAttachmentSelection",
                json!({"scope":scope(),"target":target()}),
            )
        };
        let response =
            crate::desktop::dispatch(picker("picker-original"), Some(&sender), None, None).await;
        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(response.result, Some(batch.clone()));
        let pending =
            crate::desktop::dispatch(picker("picker-after-reload"), Some(&sender), None, None)
                .await;
        assert_eq!(pending.result, Some(batch.clone()));
        let mut wrong = picker("picker-original");
        wrong.params["target"]["agent"] = json!("other-agent");
        assert!(
            crate::desktop::dispatch(wrong, Some(&sender), None, None)
                .await
                .error
                .is_some()
        );
        drop(sender);
        let store = worker.await.unwrap();
        let counts:(i64,i64)=store.connection.query_row("SELECT (SELECT count(*) FROM client_file_sources),(SELECT count(*) FROM client_uploads)",[],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();
        assert_eq!(
            counts,
            (1, 0),
            "receipt reads must not repeat capture or enqueue"
        );
    }
    #[test]
    fn native_attachment_selection_first_binding_is_durable_and_cannot_move_to_another_target() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite");
        let mut store = fixture(&path);
        let mut unbound = capture("unbound");
        unbound.params.as_object_mut().unwrap().remove("target");
        let batch = value(&mut store, unbound);
        assert!(store.dispatch(enqueue(&json!({"target":target(),"files":batch["files"],"enqueueRequestId":batch["enqueueRequestId"]}))).error.is_some());
        let bind = || {
            call(
                "bind-original",
                "attachments.bindTarget",
                json!({"scope":scope(),"selectionId":batch["id"],"target":target()}),
            )
        };
        let bound = value(&mut store, bind());
        drop(store);
        let mut store = Store::open(&path).unwrap();
        assert_eq!(value(&mut store, bind()), bound);
        let mut changed = bind();
        changed.id = "other-bind".into();
        changed.params["target"]["agent"] = json!("other-agent");
        assert!(store.dispatch(changed).error.is_some());
        assert_eq!(
            value(
                &mut store,
                call(
                    "pending-bound",
                    "attachments.pending",
                    json!({"scope":scope()})
                )
            ),
            json!([bound])
        );
    }
    #[test]
    fn native_attachment_selection_reuses_text_intent_and_the_existing_text_stage_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite");
        let mut store = fixture(&path);
        let intent = || {
            call(
                "text-intent",
                "attachments.textIntent",
                json!({"scope":scope(),"target":target(),"text":"original body"}),
            )
        };
        let first = value(&mut store, intent());
        drop(store);
        let mut store = Store::open(&path).unwrap();
        let mut again = intent();
        again.id = "text-intent-after-reload".into();
        assert_eq!(value(&mut store, again), first);
        let mut changed = intent();
        changed.id = "changed-text".into();
        changed.params["text"] = json!("changed body");
        assert!(store.dispatch(changed).error.is_some());
        let stage = || {
            call(
                first["stageRequestId"].as_str().unwrap(),
                "files.text",
                json!({"name":"message.txt","mime":"text/plain","text":"original body","_attachmentScope":scope()}),
            )
        };
        let mut wrong_view = stage();
        wrong_view.params["_attachmentScope"]["viewId"] = json!("other-view");
        assert!(store.dispatch(wrong_view).error.is_some());
        let mut wrong_text = stage();
        wrong_text.params["text"] = json!("changed body");
        assert!(store.dispatch(wrong_text).error.is_some());
        let mut missing_scope = stage();
        missing_scope
            .params
            .as_object_mut()
            .unwrap()
            .remove("_attachmentScope");
        assert!(store.dispatch(missing_scope).error.is_some());
        let before: i64 = store
            .connection
            .query_row("SELECT count(*) FROM client_file_sources", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            before, 0,
            "an unproven text stage must not write file bytes"
        );
        let file = value(&mut store, stage());
        assert_eq!(value(&mut store, stage()), file);
        let mut batch = first.clone();
        batch.as_object_mut().unwrap().remove("stageRequestId");
        batch["files"] = json!([file]);
        let rows = value(&mut store, enqueue(&batch));
        assert_eq!(rows[0]["size"], 13);
        assert_eq!(value(&mut store, enqueue(&batch)), rows);
        value(
            &mut store,
            call(
                "text-settle",
                "attachments.settle",
                json!({"scope":scope(),"batch":batch}),
            ),
        );
        let raw: String = store
            .connection
            .query_row(
                "SELECT params FROM client_receipts WHERE request_id='text-intent'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !raw.contains("original body"),
            "selection receipts retain the text digest, not another text copy"
        );
    }
    #[test]
    fn native_attachment_selection_refuses_changed_source_bytes_before_any_enqueue() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = fixture(&directory.path().join("client.sqlite"));
        let batch = value(&mut store, capture("captured"));
        store
            .connection
            .execute(
                "UPDATE client_file_sources SET bytes=?2 WHERE id=?1",
                rusqlite::params![
                    batch["files"][0]["nativeHandle"].as_str().unwrap(),
                    b"evil".as_slice()
                ],
            )
            .unwrap();
        assert!(store.dispatch(enqueue(&batch)).error.is_some());
        let count: i64 = store
            .connection
            .query_row("SELECT count(*) FROM client_uploads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(
            value(
                &mut store,
                call(
                    "pending-changed",
                    "attachments.pending",
                    json!({"scope":scope()})
                )
            ),
            json!([batch])
        );
    }
    #[test]
    fn native_attachment_selection_reopens_and_replays_exact_queue_and_settle_receipts() {
        let directory = tempfile::tempdir().expect("selection fixture directory");
        let path = directory.path().join("client.sqlite");
        let mut store = fixture(&path);
        let batch = value(&mut store, capture("native-paste"));
        assert_eq!(batch["target"], target());
        assert_eq!(batch["files"][0]["name"], "original.txt");
        let handle = batch["files"][0]["nativeHandle"]
            .as_str()
            .expect("opaque handle");
        assert!(!handle.is_empty());
        drop(store);
        let mut store = Store::open(&path).expect("reopened selection store");
        assert_eq!(
            value(
                &mut store,
                call("pending", "attachments.pending", json!({"scope":scope()}))
            ),
            json!([batch])
        );
        let original = value(&mut store, enqueue(&batch));
        drop(store); // The first queue result does not reach the renderer.
        let mut store = Store::open(&path).expect("reopened queue store");
        assert_eq!(value(&mut store, enqueue(&batch)), original);
        let count: i64 = store
            .connection
            .query_row("SELECT count(*) FROM client_uploads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "a lost success must not enqueue another upload");
        let settle = || {
            call(
                "settle-original",
                "attachments.settle",
                json!({"scope":scope(),"batch":batch}),
            )
        };
        let receipt = value(&mut store, settle());
        drop(store); // The acknowledgement can also lose its reply.
        let mut store = Store::open(&path).expect("reopened settled store");
        assert_eq!(value(&mut store, settle()), receipt);
        assert_eq!(
            value(
                &mut store,
                call(
                    "pending-final",
                    "attachments.pending",
                    json!({"scope":scope()})
                )
            ),
            json!([])
        );
        assert_eq!(value(&mut store, enqueue(&batch)), original);
    }
    #[test]
    fn native_attachment_selection_refuses_scope_target_handle_and_unproven_settlement() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = fixture(&directory.path().join("client.sqlite"));
        let batch = value(&mut store, capture("native-drop"));
        for (field, changed) in [
            ("serverId", "other"),
            ("workspaceId", "b"),
            ("viewId", "other-view"),
            ("sourceIdentity", "other-profile"),
        ] {
            let mut scope = scope();
            scope[field] = json!(changed);
            let result = store.dispatch(call(
                "wrong-scope",
                "attachments.bindTarget",
                json!({"scope":scope,"selectionId":batch["id"],"target":target()}),
            ));
            assert!(result.error.is_some(), "wrong {field} must refuse");
            let mut queue = enqueue(&batch);
            queue.params["_attachmentScope"][field] = json!(changed);
            assert!(
                store.dispatch(queue).error.is_some(),
                "wrong queue {field} must refuse before consuming sources"
            );
        }
        let mut changed = enqueue(&batch);
        changed.params["agent"] = json!("other-agent");
        assert!(
            store.dispatch(changed).error.is_some(),
            "first queue cannot change the bound target"
        );
        let mut changed = enqueue(&batch);
        changed.params["files"][0]["nativeHandle"] = json!("other-handle");
        assert!(
            store.dispatch(changed).error.is_some(),
            "first queue cannot replace a captured handle"
        );
        assert!(
            store
                .dispatch(call(
                    "unproven-settle",
                    "attachments.settle",
                    json!({"scope":scope(),"batch":batch})
                ))
                .error
                .is_some()
        );
        let count: i64 = store
            .connection
            .query_row("SELECT count(*) FROM client_uploads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
        let original = value(&mut store, enqueue(&batch));
        let mut different = batch.clone();
        different["files"][0]["size"] = json!(999);
        assert!(
            store
                .dispatch(call(
                    "changed-settle",
                    "attachments.settle",
                    json!({"scope":scope(),"batch":different})
                ))
                .error
                .is_some()
        );
        assert_eq!(value(&mut store, enqueue(&batch)), original);
    }
    #[test]
    fn native_attachment_selection_stages_all_files_atomically_and_retains_original_pending_batch()
    {
        let directory = tempfile::tempdir().unwrap();
        let mut store = fixture(&directory.path().join("client.sqlite"));
        let mut partial = capture("partial");
        partial.params["files"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"empty.txt","mime":"text/plain","base64":""}));
        assert!(store.dispatch(partial).error.is_some());
        let count: i64 = store
            .connection
            .query_row("SELECT count(*) FROM client_file_sources", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            count, 0,
            "one invalid file must roll back the whole capture"
        );
        let first = value(&mut store, capture("capture-first"));
        assert_eq!(value(&mut store, capture("capture-first")), first);
        let mut second = capture("capture-second");
        second.params["files"][0]["base64"] = json!("b3RoZXI=");
        assert!(
            store.dispatch(second).error.is_some(),
            "an unresolved capture cannot be replaced"
        );
        assert_eq!(
            value(
                &mut store,
                call("pending", "attachments.pending", json!({"scope":scope()}))
            ),
            json!([first])
        );
    }
}
