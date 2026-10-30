//! Draft pull and push preserve the master document used for each attempt.
use crate::{
    backend::Backend,
    delivery::{StoreSender, state},
    draft_store::{DraftRow, PendingDrafts},
    projections::Document,
    protocol::Request,
    sync,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use studio_client_domain::json_object::optional_field;

// DraftDocumentInput uses `deleted`; local projections use `_deleted`.
#[derive(serde::Serialize)]
struct WireDocument<'a> {
    id: &'a str,
    payload: &'a str,
    seq: u64,
    deleted: bool,
}
impl<'a> From<&'a Document> for WireDocument<'a> {
    fn from(document: &'a Document) -> Self {
        Self {
            id: &document.id,
            payload: &document.payload,
            seq: document.seq,
            deleted: document.deleted,
        }
    }
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct WireRow<'a> {
    new_document_state: WireDocument<'a>,
    assumed_master_state: Option<WireDocument<'a>>,
}
fn wire_rows(rows: &[DraftRow]) -> Vec<WireRow<'_>> {
    rows.iter()
        .map(|row| WireRow {
            new_document_state: (&row.new_document_state).into(),
            assumed_master_state: row.assumed_master_state.as_ref().map(Into::into),
        })
        .collect()
}

// The backend's generic response can be partial. Keep each check separate so
// workspace, checkpoint, and document errors retain their existing order.
#[derive(Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullPage {
    #[serde(default, deserialize_with = "optional_field")]
    workspace_id: Option<String>,
    #[serde(default, deserialize_with = "optional_field")]
    checkpoint: Option<studio_client_domain::json_object::JsonObject<PullCheckpoint>>,
    #[serde(default, deserialize_with = "optional_field")]
    documents: Option<Vec<Value>>,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceIdentity {
    workspace_id: Option<String>,
}
fn workspace_id(value: &Value) -> Option<String> {
    studio_client_domain::json_object::JsonObject::<WorkspaceIdentity>::deserialize(value)
        .ok()?
        .0
        .workspace_id
}
#[derive(serde::Deserialize)]
struct PendingCheckpoint {
    checkpoint: u64,
}
#[derive(serde::Deserialize)]
struct PullCheckpoint {
    #[serde(default, deserialize_with = "optional_field")]
    seq: Option<u64>,
}
impl PullPage {
    fn checked(self, workspace: &str, after: u64) -> Result<(u64, Vec<Value>), String> {
        if self.workspace_id.as_deref() != Some(workspace) {
            return Err("The server workspace changed during draft synchronization.".into());
        }
        let checkpoint = self
            .checkpoint
            .and_then(|value| value.0.seq)
            .filter(|v| *v >= after && *v <= 9_007_199_254_740_991)
            .ok_or("Invalid server draft checkpoint.")?;
        let documents = self
            .documents
            .filter(|v| v.len() <= 100)
            .ok_or("Invalid server draft batch.")?;
        if !documents.is_empty() && checkpoint <= after {
            return Err("The draft checkpoint did not advance.".into());
        }
        Ok((checkpoint, documents))
    }
}

#[derive(serde::Deserialize)]
struct Conflict {
    id: String,
    #[serde(flatten)]
    fields: serde_json::Map<String, Value>,
}
impl Conflict {
    fn from_value(value: Value) -> Result<Self, String> {
        studio_client_domain::json_object::JsonObject::<Self>::deserialize(value)
            .map(|object| object.0)
            .map_err(|_| "Missing draft conflict identity.".into())
    }
    fn into_value(self) -> Value {
        let mut fields = self.fields;
        fields.insert("id".into(), Value::String(self.id));
        Value::Object(fields)
    }
}

async fn replicate(backend: &Backend, store: &StoreSender, workspace: &str) -> Result<(), String> {
    let identity = sync::identity(backend).await.map_err(|e| e.message)?;
    if workspace_id(&identity).as_deref() != Some(workspace) {
        return Err("The server workspace changed. Saved drafts remain on this device.".into());
    }
    loop {
        let pending = state(store, "drafts.pending", json!({"workspace":workspace})).await?;
        let after =
            studio_client_domain::json_object::JsonObject::<PendingCheckpoint>::deserialize(
                pending,
            )
            .map_err(|_| "Invalid local draft checkpoint.")?
            .0
            .checkpoint;
        let result = sync::http(
            backend,
            &format!("/api/sync/pull?scope=drafts&after={after}&limit=100"),
        )
        .await
        .map_err(|e| e.message)?;
        let result = studio_client_domain::json_object::JsonObject::<PullPage>::deserialize(result)
            .map(|value| value.0)
            .unwrap_or_default();
        let (checkpoint, documents) = result.checked(workspace, after)?;
        if !documents.is_empty() {
            state(
                store,
                "drafts.receive",
                json!({"workspace":workspace,"documents":&documents,"checkpoint":checkpoint}),
            )
            .await?;
        }
        if documents.len() < 100 {
            break;
        }
    }
    let pending = state(store, "drafts.pending", json!({"workspace":workspace})).await?;
    let rows: PendingDrafts =
        PendingDrafts::deserialize(pending).map_err(|_| "Invalid pending draft batch.")?;
    let rows = rows.rows;
    if rows.is_empty() {
        return Ok(());
    }
    let response=backend.dispatch(Request{id:"core-draft-push".into(),method:"api.request".into(),params:json!({
        "path":"/api/sync/drafts","body":{"rows":wire_rows(&rows)},"workspaceId":workspace,"timeoutMs":15000
    })}).await;
    let value = response.result.ok_or_else(|| {
        response
            .error
            .map(|e| e.message)
            .unwrap_or_else(|| "Missing draft response.".into())
    })?;
    let conflicts = value.as_array().ok_or("Invalid draft conflict response.")?;
    let mut conflicted = BTreeSet::new();
    let mut parsed = Vec::with_capacity(conflicts.len());
    for value in conflicts {
        let conflict = Conflict::from_value(value.clone())?;
        if !rows
            .iter()
            .any(|row| row.new_document_state.id == conflict.id)
            || !conflicted.insert(conflict.id.clone())
        {
            return Err("The server returned an unrelated draft conflict.".into());
        }
        parsed.push(conflict);
    }
    let accepted = rows
        .iter()
        .map(|row| &row.new_document_state)
        .filter(|doc| !conflicted.contains(&doc.id))
        .collect::<Vec<_>>();
    let conflicts = parsed
        .into_iter()
        .map(Conflict::into_value)
        .collect::<Vec<_>>();
    if !accepted.is_empty() {
        state(
            store,
            "drafts.acknowledge",
            json!({"workspace":workspace,"documents":accepted}),
        )
        .await?;
    }
    if !conflicts.is_empty() {
        state(
            store,
            "drafts.receive",
            json!({"workspace":workspace,"documents":conflicts}),
        )
        .await?;
    }
    Ok(())
}

pub async fn refresh(backend: &Backend, store: &StoreSender) {
    let cached = state(store, "identity.get", Value::Null)
        .await
        .unwrap_or(Value::Null);
    let Some(workspace) = workspace_id(&cached) else {
        return;
    };
    let result = replicate(backend, store, &workspace).await;
    drop(
        state(
            store,
            "drafts.sync_status",
            json!({"workspace":workspace,"error":result.err().unwrap_or_default()}),
        )
        .await,
    );
}

/// Refresh only the opened workspace.
pub async fn dispatch(
    backend: &Backend,
    store: &StoreSender,
    request: Request,
) -> crate::protocol::Response {
    let result = async {
        let identity = state(store, "identity.get", Value::Null).await?;
        let workspace = request.params["workspace"]
            .as_str()
            .filter(|value| crate::projections::valid_workspace(value))
            .ok_or("Invalid draft workspace.")?;
        if workspace_id(&identity).as_deref() != Some(workspace) {
            return Err("The draft workspace is not the opened workspace.".to_owned());
        }
        refresh(backend, store).await;
        state(store, "drafts.view", json!({"workspace":workspace})).await
    }
    .await;
    match result {
        Ok(value) => crate::protocol::Response::success(request.id, value),
        Err(error) => crate::protocol::Response::failure(request.id, "draft_sync", error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn edit(sender: &StoreSender, request_id: &str, workspace: &str, text: &str) {
        let (reply, receive) = tokio::sync::oneshot::channel();
        sender
            .send(crate::delivery::StoreCommand::Request(
                Request {
                    id: request_id.into(),
                    method: "drafts.edit".into(),
                    params: json!({"workspace":workspace,"changes":{"room":text}}),
                },
                reply,
            ))
            .await
            .unwrap();
        assert!(receive.await.unwrap().error.is_none());
    }

    #[test]
    fn pull_and_conflict_projections_preserve_partial_validation_order() {
        let workspace = "a".repeat(32);
        let old = |value: &Value| -> Result<(u64, Vec<Value>), String> {
            if value["workspaceId"] != workspace {
                return Err("The server workspace changed during draft synchronization.".into());
            }
            let checkpoint = value["checkpoint"]["seq"]
                .as_u64()
                .filter(|v| *v >= 1 && *v <= 9_007_199_254_740_991)
                .ok_or("Invalid server draft checkpoint.")?;
            let documents = value["documents"]
                .as_array()
                .filter(|v| v.len() <= 100)
                .ok_or("Invalid server draft batch.")?;
            if !documents.is_empty() && checkpoint <= 1 {
                return Err("The draft checkpoint did not advance.".into());
            }
            Ok((checkpoint, documents.clone()))
        };
        for value in [
            json!(null),
            json!([]),
            json!({}),
            json!({"workspaceId":workspace,"checkpoint":null}),
            json!({"workspaceId":workspace,"checkpoint":[1],"documents":[]}),
            json!({"workspaceId":workspace,"checkpoint":{"seq":"1"},"documents":[]}),
            json!({"workspaceId":workspace,"checkpoint":{"seq":1},"documents":true}),
            json!({"workspaceId":workspace,"checkpoint":{"seq":1.0},"documents":[]}),
            json!({"workspaceId":workspace,"checkpoint":{"seq":1},"documents":[null]}),
            json!({"workspaceId":workspace,"checkpoint":{"seq":2},"documents":[null,{"id":"x","future":[1]}]}),
        ] {
            let page = serde_json::from_value::<
                studio_client_domain::json_object::JsonObject<PullPage>,
            >(value.clone())
            .map(|v| v.0)
            .unwrap_or_default();
            assert_eq!(page.checked(&workspace, 1), old(&value), "{value}");
        }
        for value in [
            json!(null),
            json!([]),
            json!({}),
            json!({"id":null}),
            json!({"id":42}),
            json!({"id":"","partial":[null,{"x":1}]}),
            json!({"id":"a","unknown":1.0}),
        ] {
            let parsed = Conflict::from_value(value.clone());
            assert_eq!(parsed.is_ok(), value["id"].as_str().is_some(), "{value}");
            if let Ok(parsed) = parsed {
                assert_eq!(parsed.into_value(), value);
            }
        }
    }

    #[tokio::test]
    async fn actual_partial_conflict_acknowledges_other_rows_before_receive_fails() {
        use crate::backend::tests::{read_http, reply};
        let workspace = "a".repeat(32);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server_workspace = workspace.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (headers, body) = read_http(&mut stream).await;
                if headers.starts_with("GET /api/session ") {
                    reply(&mut stream, r#"{"token":"test"}"#).await;
                } else if headers.starts_with("GET /api/sync/identity ") {
                    reply(
                        &mut stream,
                        &json!({"workspaceId":server_workspace}).to_string(),
                    )
                    .await;
                } else if headers.starts_with("GET /api/sync/pull?") {
                    reply(&mut stream, &json!({"workspaceId":server_workspace,"checkpoint":{"seq":0},"documents":[]}).to_string()).await;
                } else {
                    assert!(headers.starts_with("POST /api/sync/drafts "));
                    assert_eq!(body["rows"].as_array().unwrap().len(), 2);
                    reply(
                        &mut stream,
                        &json!([{"id":body["rows"][0]["newDocumentState"]["id"],"future":true}])
                            .to_string(),
                    )
                    .await;
                    break;
                }
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let (sender, mut receiver) =
            tokio::sync::mpsc::channel::<crate::delivery::StoreCommand>(64);
        let worker = std::thread::spawn(move || {
            let mut store = crate::store::Store::open(&path).unwrap();
            while let Some(command) = receiver.blocking_recv() {
                command.execute(&mut store);
            }
        });
        state(
            &sender,
            "drafts.edit",
            json!({"workspace":workspace,"changes":{"one":"1","two":"2"}}),
        )
        .await
        .unwrap();
        assert!(
            replicate(&Backend::new(&origin).unwrap(), &sender, &workspace)
                .await
                .is_err()
        );
        let pending = state(&sender, "drafts.pending", json!({"workspace":workspace}))
            .await
            .unwrap();
        assert_eq!(pending["rows"].as_array().unwrap().len(), 1);
        drop(sender);
        worker.join().unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn actual_pull_refuses_partial_pages_in_original_order_without_acknowledging() {
        use crate::backend::tests::{read_http, reply};
        let workspace = "a".repeat(32);
        for (page, expected) in [
            (
                json!({"workspaceId":"foreign","checkpoint":null,"documents":null}),
                "The server workspace changed during draft synchronization.",
            ),
            (
                json!({"workspaceId":workspace,"checkpoint":null,"documents":null}),
                "Invalid server draft checkpoint.",
            ),
            (
                json!({"workspaceId":workspace,"checkpoint":{"seq":0},"documents":null}),
                "Invalid server draft batch.",
            ),
            (
                json!({"workspaceId":workspace,"checkpoint":{"seq":0},"documents":[null]}),
                "The draft checkpoint did not advance.",
            ),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let server_workspace = workspace.clone();
            let server = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let (headers, _) = read_http(&mut stream).await;
                    if headers.starts_with("GET /api/session ") {
                        reply(&mut stream, r#"{"token":"test"}"#).await;
                    } else if headers.starts_with("GET /api/sync/identity ") {
                        reply(
                            &mut stream,
                            &json!({"workspaceId":server_workspace}).to_string(),
                        )
                        .await;
                    } else {
                        assert!(headers.starts_with("GET /api/sync/pull?"));
                        reply(&mut stream, &page.to_string()).await;
                        break;
                    }
                }
            });
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("client.sqlite3");
            let (sender, mut receiver) =
                tokio::sync::mpsc::channel::<crate::delivery::StoreCommand>(64);
            let worker = std::thread::spawn(move || {
                let mut store = crate::store::Store::open(&path).unwrap();
                while let Some(command) = receiver.blocking_recv() {
                    command.execute(&mut store);
                }
            });
            state(
                &sender,
                "drafts.edit",
                json!({"workspace":workspace,"changes":{"room":"saved"}}),
            )
            .await
            .unwrap();
            let error = replicate(&Backend::new(&origin).unwrap(), &sender, &workspace)
                .await
                .unwrap_err();
            assert_eq!(error, expected);
            let pending = state(&sender, "drafts.pending", json!({"workspace":workspace}))
                .await
                .unwrap();
            assert_eq!(pending["rows"].as_array().unwrap().len(), 1);
            assert_eq!(pending["checkpoint"], 0);
            drop(sender);
            worker.join().unwrap();
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn actual_push_ack_cannot_erase_an_edit_made_during_request() {
        use crate::backend::tests::{read_http, reply};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let workspace = "a".repeat(32);
        let server_workspace = workspace.clone();
        let (posted, reached) = tokio::sync::oneshot::channel();
        let (release, wait) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (headers, body) = read_http(&mut stream).await;
                if headers.starts_with("GET /api/session ") {
                    reply(&mut stream, r#"{"token":"test"}"#).await;
                } else if headers.starts_with("GET /api/sync/identity ") {
                    reply(
                        &mut stream,
                        &json!({"workspaceId":server_workspace}).to_string(),
                    )
                    .await;
                } else if headers.starts_with("GET /api/sync/pull?") {
                    reply(&mut stream, &json!({"workspaceId":server_workspace,"checkpoint":{"seq":0},"documents":[]}).to_string()).await;
                } else {
                    assert!(headers.starts_with("POST /api/sync/drafts "));
                    posted.send(body).unwrap();
                    wait.await.unwrap();
                    reply(&mut stream, "[]").await;
                    break;
                }
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let (sender, mut receiver) =
            tokio::sync::mpsc::channel::<crate::delivery::StoreCommand>(64);
        let worker_path = path.clone();
        let worker = std::thread::spawn(move || {
            let mut store = crate::store::Store::open(&worker_path).unwrap();
            while let Some(command) = receiver.blocking_recv() {
                command.execute(&mut store);
            }
        });
        edit(&sender, "old-edit", &workspace, "old").await;
        let backend = Backend::new(&origin).unwrap();
        let replicate_sender = sender.clone();
        let replicate_workspace = workspace.clone();
        let task = tokio::spawn(async move {
            replicate(&backend, &replicate_sender, &replicate_workspace).await
        });
        let body = tokio::time::timeout(std::time::Duration::from_secs(3), reached)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(body["rows"].as_array().unwrap().len(), 1);
        edit(&sender, "new-edit", &workspace, "new").await;
        release.send(()).unwrap();
        task.await.unwrap().unwrap();
        let pending = state(&sender, "drafts.pending", json!({"workspace":workspace}))
            .await
            .unwrap();
        assert_eq!(pending["rows"].as_array().unwrap().len(), 1);
        drop(sender);
        worker.join().unwrap();
        let mut store = crate::store::Store::open(&path).unwrap();
        let view = store
            .dispatch(Request {
                id: "after-restart".into(),
                method: "drafts.view".into(),
                params: json!({"workspace":workspace}),
            })
            .result
            .unwrap();
        assert_eq!(view["drafts"]["room"], "new");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn actual_replication_keeps_dirty_rows_on_lost_and_foreign_conflicts_after_restart() {
        use crate::backend::tests::{read_http, reply};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let workspace = "a".repeat(32);
        let server_workspace = workspace.clone();
        let server = tokio::spawn(async move {
            let mut pushes = Vec::new();
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (headers, body) = read_http(&mut stream).await;
                if headers.starts_with("GET /api/session ") {
                    reply(&mut stream, r#"{"token":"test"}"#).await;
                } else if headers.starts_with("GET /api/sync/identity ") {
                    reply(
                        &mut stream,
                        &json!({"workspaceId":server_workspace}).to_string(),
                    )
                    .await;
                } else if headers.starts_with("GET /api/sync/pull?") {
                    reply(&mut stream, &json!({"workspaceId":server_workspace,"checkpoint":{"seq":0},"documents":[]}).to_string()).await;
                } else {
                    assert!(headers.starts_with("POST /api/sync/drafts "), "{headers}");
                    pushes.push(body);
                    match pushes.len() {
                        1 => drop(stream),
                        2 => reply(&mut stream, r#"[{"id":"foreign"}]"#).await,
                        _ => {
                            reply(&mut stream, "[]").await;
                            assert_eq!(pushes[0], pushes[1]);
                            assert_eq!(pushes[1], pushes[2]);
                            break;
                        }
                    }
                }
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let run_store = |path: std::path::PathBuf| {
            let (sender, mut receiver) =
                tokio::sync::mpsc::channel::<crate::delivery::StoreCommand>(64);
            let worker = std::thread::spawn(move || {
                let mut store = crate::store::Store::open(&path).unwrap();
                while let Some(command) = receiver.blocking_recv() {
                    command.execute(&mut store);
                }
            });
            (sender, worker)
        };
        let backend = Backend::new(&origin).unwrap();
        for attempt in 0..3 {
            let (sender, worker) = run_store(path.clone());
            if attempt == 0 {
                state(&sender, "identity.set", json!({"workspaceId":workspace}))
                    .await
                    .unwrap();
                state(
                    &sender,
                    "drafts.edit",
                    json!({"workspace":workspace,"changes":{"room":"saved draft"}}),
                )
                .await
                .unwrap();
            }
            let outcome = replicate(&backend, &sender, &workspace).await;
            let pending = state(&sender, "drafts.pending", json!({"workspace":workspace}))
                .await
                .unwrap();
            if attempt < 2 {
                assert!(outcome.is_err());
                assert_eq!(pending["rows"].as_array().unwrap().len(), 1);
            } else {
                outcome.unwrap();
                assert_eq!(pending["rows"].as_array().unwrap().len(), 0);
            }
            drop(sender);
            worker.join().unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn draft_wire_documents_translate_tombstones_without_changing_acknowledgements() {
        let rows: Vec<DraftRow> = serde_json::from_str(r#"[
            {"newDocumentState":{"id":"d:r","payload":"{}","seq":2,"_deleted":true},"assumedMasterState":{"id":"d:r","payload":"{}","seq":1,"_deleted":false}},
            {"newDocumentState":{"id":"d:s","payload":"{}","seq":0,"_deleted":false},"assumedMasterState":null}
        ]"#).unwrap();
        let wire = serde_json::to_value(wire_rows(&rows)).unwrap();
        assert_eq!(wire[0]["newDocumentState"]["deleted"], true);
        assert_eq!(wire[0]["assumedMasterState"]["deleted"], false);
        assert!(wire[0]["newDocumentState"].get("_deleted").is_none());
        assert!(wire[0]["assumedMasterState"].get("_deleted").is_none());
        assert_eq!(wire[1]["assumedMasterState"], Value::Null);
        assert!(rows[0].new_document_state.deleted);
        assert_eq!(wire[0]["assumedMasterState"]["seq"], 1);
    }
}
