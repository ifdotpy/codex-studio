//! Workspace verification and projection pulls run in Rust.
use crate::{
    backend::Backend,
    delivery::{StoreSender, state},
    projections::Document,
    protocol::{CommandError, Request, Response},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    time::Duration,
};

pub fn error(code: &str, message: impl ToString) -> CommandError {
    CommandError {
        code: code.into(),
        message: message.to_string(),
        status: None,
        details: None,
    }
}

pub async fn http(backend: &Backend, path: &str) -> Result<Value, CommandError> {
    let response = backend
        .dispatch(Request {
            id: "core-sync".into(),
            method: "api.request".into(),
            params: json!({"path":path,"timeoutMs":15000}),
        })
        .await;
    response.result.ok_or_else(|| {
        response
            .error
            .unwrap_or_else(|| error("invalid_response", "Missing server response."))
    })
}

pub async fn identity(backend: &Backend) -> Result<Value, CommandError> {
    let value = http(backend, "/api/sync/identity").await?;
    if !value["workspaceId"]
        .as_str()
        .is_some_and(crate::projections::valid_workspace)
    {
        return Err(error(
            "workspace_identity",
            "Invalid server workspace identity.",
        ));
    }
    Ok(value)
}

pub async fn open(backend: &Backend, sender: &StoreSender) -> Result<Value, CommandError> {
    let cached = state(sender, "identity.get", Value::Null)
        .await
        .map_err(|e| error("storage_error", e))?;
    let timeout = if cached.is_object() {
        Duration::from_millis(500)
    } else {
        Duration::from_secs(15)
    };
    match tokio::time::timeout(timeout, identity(backend)).await {
        Ok(Ok(value)) => state(sender, "identity.set", value)
            .await
            .map_err(|e| error("storage_error", e)),
        Ok(Err(failure)) if failure.status == Some(404) => Err(error(
            "unsupported_sync",
            "This server does not support synchronization.",
        )),
        Ok(Err(failure))
            if cached.is_null()
                || failure
                    .status
                    .is_some_and(|s| s < 500 && s != 408 && s != 429) =>
        {
            Err(failure)
        }
        _ if !cached.is_null() => Ok(cached),
        Ok(Err(failure)) => Err(failure),
        Err(_) => Err(error("timeout", "The server did not respond in time.")),
    }
}

fn merge_transcript_delta(previous: &Value, incoming: &Value) -> Result<String, CommandError> {
    let base: Value = serde_json::from_str(previous["payload"].as_str().unwrap_or(""))
        .map_err(|e| error("invalid_projection", e))?;
    let base_items = base["items"].as_array().ok_or_else(|| {
        error(
            "invalid_projection",
            "Transcript projection base has no items.",
        )
    })?;
    let mut items: BTreeMap<String, Value> = base_items
        .iter()
        .filter_map(|v| v["id"].as_str().map(|id| (id.into(), v.clone())))
        .collect();
    let mut revisions = base["itemRevisions"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    if let Some(removed) = incoming["removed"].as_array() {
        for id in removed.iter().filter_map(Value::as_str) {
            items.remove(id);
            revisions.remove(id);
        }
    }
    if let Some(changed) = incoming["items"].as_array() {
        for item in changed {
            let id = item["id"].as_str().ok_or_else(|| {
                error(
                    "invalid_projection",
                    "Transcript delta item has no identity.",
                )
            })?;
            items.insert(id.into(), item.clone());
        }
    }
    if let Some(changed) = incoming["itemRevisions"].as_object() {
        revisions.extend(changed.clone());
    }
    if let Some(removed) = incoming["removed"].as_array() {
        for id in removed.iter().filter_map(Value::as_str) {
            revisions.remove(id);
        }
    }
    let order: Vec<String> = incoming["order"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_else(|| {
            base_items
                .iter()
                .filter_map(|v| v["id"].as_str().map(str::to_owned))
                .collect()
        });
    let mut merged = base.as_object().cloned().ok_or_else(|| {
        error(
            "invalid_projection",
            "Transcript projection base is invalid.",
        )
    })?;
    if let Some(fields) = incoming.as_object() {
        merged.extend(fields.clone());
    }
    for key in ["delta", "removed", "order"] {
        merged.remove(key);
    }
    merged.insert(
        "items".into(),
        Value::Array(
            order
                .iter()
                .filter_map(|id| items.get(id).cloned())
                .collect(),
        ),
    );
    merged.insert("itemRevisions".into(), Value::Object(revisions));
    serde_json::to_string(&Value::Object(merged)).map_err(|e| error("invalid_projection", e))
}

pub async fn pull(
    backend: &Backend,
    sender: &StoreSender,
    workspace: &str,
    scope: &str,
) -> Result<Value, CommandError> {
    let current = identity(backend).await?;
    if current["workspaceId"] != workspace {
        return Err(error(
            "workspace_changed",
            "The server workspace changed. Reload to synchronize.",
        ));
    }
    if scope == "state" {
        return pull_entities(backend, sender, workspace).await;
    }
    pull_document(backend, sender, workspace, scope).await
}

async fn pull_document(
    backend: &Backend,
    sender: &StoreSender,
    workspace: &str,
    scope: &str,
) -> Result<Value, CommandError> {
    let current = identity(backend).await?;
    if current["workspaceId"] != workspace {
        return Err(error(
            "workspace_changed",
            "The server workspace changed. Reload to synchronize.",
        ));
    }
    let previous = state(
        sender,
        "projection.get",
        json!({"workspace":workspace,"scope":scope}),
    )
    .await
    .map_err(|e| error("storage_error", e))?;
    let after = previous["seq"].as_u64().unwrap_or(0);
    let url = reqwest::Url::parse_with_params(
        "http://127.0.0.1/api/sync/pull",
        &[
            ("scope", scope.to_owned()),
            ("after", after.to_string()),
            ("limit", "100".into()),
        ],
    )
    .map_err(|e| error("invalid_scope", e))?;
    let result = http(backend, &format!("/api/sync/pull?{}", url.query().unwrap())).await?;
    if result["workspaceId"] != workspace {
        return Err(error(
            "workspace_changed",
            "The server workspace changed during synchronization.",
        ));
    }
    let docs = result["documents"]
        .as_array()
        .ok_or_else(|| error("invalid_projection", "Missing projection documents."))?;
    for raw in docs {
        let mut doc: Document =
            serde_json::from_value(raw.clone()).map_err(|e| error("invalid_projection", e))?;
        if doc.id != scope {
            return Err(error(
                "invalid_projection",
                "The server returned a different projection scope.",
            ));
        }
        if scope.starts_with("transcript:") && !doc.deleted {
            let incoming: Value =
                serde_json::from_str(&doc.payload).map_err(|e| error("invalid_projection", e))?;
            if incoming["delta"] == true {
                if previous.is_null() || previous["_deleted"] == true {
                    return Err(error(
                        "invalid_projection",
                        "Transcript projection delta has no valid base.",
                    ));
                }
                if doc.seq <= previous["seq"].as_u64().unwrap_or(0) {
                    continue;
                }
                doc.payload = merge_transcript_delta(&previous, &incoming)?;
            }
        }
        state(
            sender,
            "projection.put",
            json!({"workspace":workspace,"scope":scope,"document":doc}),
        )
        .await
        .map_err(|e| error("storage_error", e))?;
    }
    state(
        sender,
        "projection.get",
        json!({"workspace":workspace,"scope":scope}),
    )
    .await
    .map_err(|e| error("storage_error", e))
}

async fn put_entity_change(
    sender: &StoreSender,
    workspace: &str,
    change: &Value,
) -> Result<bool, CommandError> {
    state(
        sender,
        "projection.putEntity",
        json!({"workspace":workspace,"change":change}),
    )
    .await
    .map(|changed| changed == true)
    .map_err(|e| error("invalid_projection", e))
}

async fn pull_entities(
    backend: &Backend,
    sender: &StoreSender,
    workspace: &str,
) -> Result<Value, CommandError> {
    let checkpoint_id = "state:entities:checkpoint";
    let cached = state(
        sender,
        "projection.get",
        json!({"workspace":workspace,"scope":checkpoint_id}),
    )
    .await
    .map_err(|e| error("storage_error", e))?;
    let mut after = cached["seq"].as_u64().unwrap_or(0);
    let ready_marker_id = "state:entities:ready";
    let ready_marker = state(
        sender,
        "projection.get",
        json!({"workspace":workspace,"scope":ready_marker_id}),
    )
    .await
    .map_err(|e| error("storage_error", e))?;
    let initial_marker_id = "state:entities:initial";
    let initial_marker = state(
        sender,
        "projection.get",
        json!({"workspace":workspace,"scope":initial_marker_id}),
    )
    .await
    .map_err(|e| error("storage_error", e))?;
    let ready = ready_marker["payload"]
        .as_str()
        .and_then(|payload| serde_json::from_str::<Value>(payload).ok())
        == Some(Value::String("ready".into()))
        && ready_marker["_deleted"] != true;
    let mut initial_high = if !ready && initial_marker["_deleted"] != true {
        Some(initial_marker["seq"].as_u64().unwrap_or(0))
    } else {
        None
    };
    let started_initial_sync = initial_high.is_some();
    let mut stored_entity = false;
    loop {
        let mut query = vec![
            ("scope", "state:entities:v1".to_owned()),
            ("after", after.to_string()),
            ("limit", "500".to_owned()),
        ];
        if let Some(high) = initial_high {
            query.push(("fresh", "1".into()));
            query.push(("initialHigh", high.to_string()));
        }
        let url = reqwest::Url::parse_with_params("http://127.0.0.1/api/sync/pull", &query)
            .map_err(|e| error("invalid_scope", e))?;
        let result = http(backend, &format!("/api/sync/pull?{}", url.query().unwrap())).await?;
        if result["workspaceId"] != workspace {
            return Err(error(
                "workspace_changed",
                "The server workspace changed during synchronization.",
            ));
        }
        let rows = result["documents"]
            .as_array()
            .ok_or_else(|| error("invalid_projection", "Missing entity projection documents."))?;
        if rows.len() > 500 {
            return Err(error(
                "invalid_projection",
                "The server returned too many entity documents.",
            ));
        }
        if initial_high.is_some() {
            let high = result["initialHigh"].as_u64().ok_or_else(|| {
                error(
                    "invalid_projection",
                    "The server returned no initial entity bound.",
                )
            })?;
            initial_high = Some(high);
            let marker = Document {
                id: initial_marker_id.into(),
                payload: "{}".into(),
                seq: high,
                deleted: false,
            };
            state(
                sender,
                "projection.put",
                json!({"workspace":workspace,"scope":initial_marker_id,"document":marker}),
            )
            .await
            .map_err(|e| error("storage_error", e))?;
        }
        for change in rows {
            stored_entity |= put_entity_change(sender, workspace, change).await?;
        }
        let next = result["checkpoint"]["seq"].as_u64().ok_or_else(|| {
            error(
                "invalid_projection",
                "The server returned an invalid entity checkpoint.",
            )
        })?;
        if next < after || (!rows.is_empty() && next == after) {
            return Err(error(
                "invalid_projection",
                "The entity checkpoint did not advance.",
            ));
        }
        let cp = Document {
            id: checkpoint_id.into(),
            payload: json!({"initialHigh":initial_high}).to_string(),
            seq: next,
            deleted: false,
        };
        state(
            sender,
            "projection.put",
            json!({"workspace":workspace,"scope":checkpoint_id,"document":cp}),
        )
        .await
        .map_err(|e| error("storage_error", e))?;
        after = next;
        let max = result["maxSeq"].as_u64().ok_or_else(|| {
            error(
                "invalid_projection",
                "The server returned no entity sequence bound.",
            )
        })?;
        if rows.len() < 500 || after >= max {
            break;
        }
    }
    let state_doc = state(
        sender,
        "projection.get",
        json!({"workspace":workspace,"scope":"state"}),
    )
    .await
    .map_err(|e| error("storage_error", e))?;
    if stored_entity || state_doc.is_null() || state_doc["seq"].as_u64().unwrap_or(0) < after {
        materialize_state(sender, workspace).await?;
    }
    if started_initial_sync {
        let marker = Document {
            id: initial_marker_id.into(),
            payload: "{}".into(),
            seq: initial_high
                .unwrap_or_else(|| initial_marker["seq"].as_u64().unwrap_or(0))
                .saturating_add(1),
            deleted: true,
        };
        state(
            sender,
            "projection.put",
            json!({"workspace":workspace,"scope":initial_marker_id,"document":marker}),
        )
        .await
        .map_err(|e| error("storage_error", e))?;
    }
    if !ready {
        let marker = Document {
            id: ready_marker_id.into(),
            payload: json!("ready").to_string(),
            seq: after,
            deleted: false,
        };
        state(
            sender,
            "projection.put",
            json!({"workspace":workspace,"scope":ready_marker_id,"document":marker}),
        )
        .await
        .map_err(|e| error("storage_error", e))?;
    }
    state(
        sender,
        "projection.get",
        json!({"workspace":workspace,"scope":"state"}),
    )
    .await
    .map_err(|e| error("storage_error", e))
}

async fn entity_snapshot(
    sender: &StoreSender,
    workspace: &str,
) -> Result<(Value, u64), CommandError> {
    let rows = state(
        sender,
        "projection.entities",
        json!({"workspace":workspace,"scope":"entity:*"}),
    )
    .await
    .map_err(|e| error("storage_error", e))?;
    let mut collections: HashMap<String, BTreeMap<String, Value>> = HashMap::new();
    let mut seq = 0;
    for doc in rows.as_array().into_iter().flatten() {
        seq = seq.max(doc["seq"].as_u64().unwrap_or(0));
        if doc["op"] == "delete" {
            continue;
        }
        if let (Some(collection), Some(id), Some(value)) = (
            doc["collection"].as_str(),
            doc["id"].as_str(),
            doc.get("value"),
        ) {
            collections
                .entry(collection.into())
                .or_default()
                .insert(id.into(), value.clone());
        }
    }
    let list = |name: &str| {
        Value::Array(
            collections
                .get(name)
                .into_iter()
                .flat_map(|v| v.values().cloned())
                .collect(),
        )
    };
    let agents = list("agent");
    let chats = list("chat");
    let mut nodes = agents.as_array().cloned().unwrap_or_default();
    nodes.extend(chats.as_array().cloned().unwrap_or_default());
    let mut runtime = json!({"agents":agents,"rooms":list("room"),"tasks":list("task"),"monitors":list("monitor"),"complaints":list("complaint"),"requests":list("request"),"rules":list("rule"),"projects":list("project"),"peerTeams":list("peerTeam"),"events":list("event"),"work":list("work")});
    let workspace_entity = collections
        .get("workspace")
        .and_then(|v| v.get("current"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    if let (Some(target), Some(extra)) = (runtime.as_object_mut(), workspace_entity.as_object()) {
        target.extend(extra.clone());
    }
    let edges = list("edge");
    let limits = collections.remove("rateLimit").unwrap_or_default();
    runtime["rateLimits"] = limits.get("default").cloned().unwrap_or(Value::Null);
    runtime["rateLimitsByAccount"] = Value::Object(limits.into_iter().collect());
    let state_dir = workspace_entity
        .get("stateDir")
        .cloned()
        .unwrap_or_else(|| json!(""));
    Ok((
        json!({"token":"","stateDir":state_dir,"threads":agents,"chats":chats,"nodes":nodes,"edges":edges,"runtime":runtime}),
        seq,
    ))
}

async fn materialize_state(sender: &StoreSender, workspace: &str) -> Result<Value, CommandError> {
    let (snapshot, max_entity) = entity_snapshot(sender, workspace).await?;
    let payload = serialize_state(&snapshot)?;
    let checkpoint = state(
        sender,
        "projection.get",
        json!({"workspace":workspace,"scope":"state:entities:checkpoint"}),
    )
    .await
    .map_err(|e| error("storage_error", e))?;
    let doc = Document {
        id: "state".into(),
        payload,
        seq: max_entity.max(checkpoint["seq"].as_u64().unwrap_or(0)),
        deleted: false,
    };
    state(
        sender,
        "projection.put",
        json!({"workspace":workspace,"scope":"state","document":doc}),
    )
    .await
    .map_err(|e| error("storage_error", e))
}

fn serialize_state(snapshot: &Value) -> Result<String, CommandError> {
    let payload = snapshot.to_string();
    match projection_error(&payload) {
        Some(failure) => Err(failure),
        None => Ok(payload),
    }
}

/// The UI receives a projection payload as an escaped JSON string in one
/// message. Refuse a payload that cannot fit, with the envelope reserve, so the
/// UI gets one actionable error instead of a transport response_size error.
pub fn projection_error(payload: &str) -> Option<CommandError> {
    let encoded = serde_json::to_string(payload).map_or(usize::MAX, |text| text.len());
    (encoded > crate::protocol::MAX_MESSAGE.saturating_sub(4096)).then(|| {
        error(
            "projection_too_large",
            "The workspace state is too large to open. Update the Studio server; it removes old background tasks from sync.",
        )
    })
}

async fn apply_entities(
    backend: &Backend,
    sender: &StoreSender,
    workspace: &str,
    values: &[Value],
) -> Result<Value, CommandError> {
    let current = identity(backend).await?;
    let workspace = if workspace.is_empty() {
        current["workspaceId"].as_str().unwrap_or("")
    } else {
        if current["workspaceId"] != workspace {
            return Err(error(
                "workspace_changed",
                "The server workspace changed during synchronization.",
            ));
        }
        workspace
    };
    if !crate::projections::valid_workspace(workspace) {
        return Err(error("invalid_workspace", "Invalid workspace identity."));
    }
    let mut stored_entity = false;
    for change in values {
        stored_entity |= put_entity_change(sender, workspace, change).await?;
    }
    if stored_entity {
        materialize_state(sender, workspace).await
    } else {
        state(
            sender,
            "projection.get",
            json!({"workspace":workspace,"scope":"state"}),
        )
        .await
        .map_err(|e| error("storage_error", e))
    }
}

pub async fn dispatch(backend: &Backend, sender: &StoreSender, request: Request) -> Response {
    let result = match request.method.as_str() {
        "sync.open" => open(backend, sender).await,
        "sync.pull" => match (
            request.params["workspace"].as_str(),
            request.params["scope"].as_str(),
        ) {
            (Some(w), Some(s)) => pull(backend, sender, w, s).await,
            _ => Err(error(
                "invalid_params",
                "Workspace and projection scope are required.",
            )),
        },
        "sync.applyEntities" => match (
            request.params["workspace"].as_str().unwrap_or(""),
            request.params["documents"].as_array(),
        ) {
            (w, Some(d)) => apply_entities(backend, sender, w, d).await,
            (_, None) => Err(error(
                "invalid_params",
                "Workspace and entity documents are required.",
            )),
        },
        _ => Err(error("unknown_method", "Unknown synchronization command.")),
    };
    match result {
        Ok(value) => Response::success(request.id, value),
        Err(error) => Response {
            id: request.id,
            result: None,
            error: Some(error),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::{io::AsyncWriteExt, net::TcpListener, sync::mpsc};
    #[test]
    fn transcript_delta_merges_items_revisions_metadata_and_order() {
        let base = json!({"payload":json!({"items":[{"id":"a","v":1},{"id":"b","v":2}],"itemRevisions":{"a":1,"b":1},"title":"old"}).to_string()});
        let delta = json!({"items":[{"id":"a","v":3},{"id":"c","v":4}],"itemRevisions":{"a":2,"c":1},"removed":["b"],"order":["c","a"],"delta":true,"title":"new"});
        let actual: Value =
            serde_json::from_str(&merge_transcript_delta(&base, &delta).unwrap()).unwrap();
        assert_eq!(
            actual,
            json!({"items":[{"id":"c","v":4},{"id":"a","v":3}],"itemRevisions":{"a":2,"c":1},"title":"new"})
        );
    }

    #[test]
    fn oversized_materialized_state_returns_one_actionable_projection_error() {
        let snapshot = json!({"runtime":{"tasks":["x".repeat(44 * 1024 * 1024)]}});
        let failure = serialize_state(&snapshot).unwrap_err();
        assert_eq!(failure.code, "projection_too_large");
        assert!(failure.message.contains("Update the Studio server"));
    }

    #[test]
    fn projection_limit_counts_json_escaping() {
        // 17 MiB of quotes becomes 34 MiB after escaping in the UI message.
        let payload = "\"".repeat(17 * 1024 * 1024);
        assert_eq!(
            projection_error(&payload).unwrap().code,
            "projection_too_large"
        );
        assert!(projection_error(&"x".repeat(17 * 1024 * 1024)).is_none());
    }

    #[tokio::test]
    async fn entity_v4_batch_applies_structured_upserts_and_deletes_once() {
        let workspace = "b".repeat(32);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server_workspace = workspace.clone();
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (headers, _) = crate::backend::tests::read_http(&mut stream).await;
                assert!(headers.starts_with("GET /api/sync/identity"));
                let body = json!({"workspaceId":server_workspace,"chatState":true}).to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let (sender, mut receive) = mpsc::channel::<crate::delivery::StoreCommand>(32);
        let state_puts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&state_puts);
        let worker = tokio::spawn(async move {
            let mut store = crate::store::Store::open(&path).unwrap();
            while let Some((request, reply)) = receive.recv().await {
                if request.method == "projection.put" && request.params["scope"] == "state" {
                    observed.fetch_add(1, Ordering::SeqCst);
                }
                let _ = reply.send(store.dispatch(request));
            }
            drop(directory);
        });
        let apply = |documents: Value| Request {
            id: "apply".into(),
            method: "sync.applyEntities".into(),
            params: json!({"workspace":workspace,"documents":documents}),
        };
        let task = json!({"op":"upsert","collection":"task","id":"one","seq":10,"value":{"id":"one","status":"running"}});
        let other = json!({"op":"upsert","collection":"task","id":"two","seq":11,"value":{"id":"two","status":"running"}});
        let first = dispatch(&backend, &sender, apply(json!([task, other]))).await;
        assert!(first.error.is_none(), "{:?}", first.error);
        let snapshot: Value =
            serde_json::from_str(first.result.unwrap()["payload"].as_str().unwrap()).unwrap();
        assert_eq!(snapshot["runtime"]["tasks"].as_array().unwrap().len(), 2);
        assert_eq!(state_puts.load(Ordering::SeqCst), 1);
        let removed = dispatch(
            &backend,
            &sender,
            apply(json!([
                {"op":"delete","collection":"task","id":"one","seq":12},
                {"op":"delete","collection":"task","id":"two","seq":13}
            ])),
        )
        .await;
        assert!(removed.error.is_none(), "{:?}", removed.error);
        let removed = removed.result.unwrap();
        let snapshot: Value = serde_json::from_str(removed["payload"].as_str().unwrap()).unwrap();
        assert!(snapshot["runtime"]["tasks"].as_array().unwrap().is_empty());
        assert_eq!(state_puts.load(Ordering::SeqCst), 2);
        let stale = dispatch(&backend, &sender, apply(json!([task]))).await;
        assert_eq!(stale.result, Some(removed));
        assert_eq!(state_puts.load(Ordering::SeqCst), 2);
        let checkpoint = state(
            &sender,
            "projection.get",
            json!({"workspace":workspace,"scope":"state:entities:checkpoint"}),
        )
        .await
        .unwrap();
        assert!(
            checkpoint.is_null(),
            "notifications cannot advance the pull cursor"
        );
        drop(sender);
        worker.await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn entity_pull_paginates_and_materializes_the_browser_snapshot() {
        let workspace = "a".repeat(32);
        let mut entities = Vec::new();
        for index in 0..700 {
            let id = format!("agent-{index:03}");
            entities.push(json!({"collection":"agent","id":id,"value":{"id":id,"name":id}}));
        }
        entities.extend([
            json!({"collection":"chat","id":"chat-1","value":{"id":"chat-1","name":"Chat"}}),
            json!({"collection":"edge","id":"edge-1","value":{"id":"edge-1","source":"agent-000","target":"chat-1"}}),
            json!({"collection":"workspace","id":"current","value":{"stateDir":"isolated-state"}}),
            json!({"collection":"task","id":"task-1","value":{"id":"task-1","status":"running"}}),
            json!({"collection":"rateLimit","id":"default","value":{"accountKey":"default","at":100.0,"data":{"accountId":"fixture"}}}),
        ]);
        let docs: Vec<Value> = entities.iter().enumerate().map(|(index, entity)| {
            json!({"op":"upsert","collection":entity["collection"],"id":entity["id"],"value":entity["value"],"seq":index+1})
        }).collect();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server_docs = docs.clone();
        let server_workspace = workspace.clone();
        let server = tokio::spawn(async move {
            for request_index in 0..5 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (headers, _) = crate::backend::tests::read_http(&mut stream).await;
                let path = headers
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap();
                let body = if matches!(request_index, 0 | 3) {
                    assert!(path.starts_with("/api/sync/identity"));
                    json!({"workspaceId":server_workspace,"chatState":true})
                } else {
                    let url = reqwest::Url::parse(&format!("http://localhost{path}")).unwrap();
                    assert_eq!(
                        url.query_pairs().find(|(key, _)| key == "scope").unwrap().1,
                        "state:entities:v1"
                    );
                    let after: usize = url
                        .query_pairs()
                        .find(|(key, _)| key == "after")
                        .unwrap()
                        .1
                        .parse()
                        .unwrap();
                    let fresh = url
                        .query_pairs()
                        .any(|(key, value)| key == "fresh" && value == "1");
                    if after < 705 {
                        assert!(
                            fresh,
                            "every first-sync page must retain the fresh high-water mark"
                        );
                        assert_eq!(
                            url.query_pairs().find(|(key, _)| key == "limit").unwrap().1,
                            "500"
                        );
                    } else {
                        assert!(!fresh, "incremental entity pulls must use the checkpoint");
                    }
                    let rows = server_docs
                        .iter()
                        .filter(|doc| doc["seq"].as_u64().unwrap() as usize > after)
                        .take(500)
                        .cloned()
                        .collect::<Vec<_>>();
                    let checkpoint = rows
                        .last()
                        .map(|doc| doc["seq"].as_u64().unwrap())
                        .unwrap_or(after as u64);
                    json!({"workspaceId":server_workspace,"documents":rows,"checkpoint":{"seq":checkpoint},"maxSeq":server_docs.len(),"initialHigh":if fresh { server_docs.len() } else { 0 }})
                };
                let encoded = body.to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", encoded.len(), encoded).as_bytes()).await.unwrap();
            }
        });

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let (sender, mut receive) = mpsc::channel::<crate::delivery::StoreCommand>(32);
        let state_puts = Arc::new(AtomicUsize::new(0));
        let observed_state_puts = Arc::clone(&state_puts);
        tokio::spawn(async move {
            let mut store = crate::store::Store::open(&path).unwrap();
            while let Some((request, reply)) = receive.recv().await {
                if request.method == "projection.put" && request.params["scope"] == "state" {
                    observed_state_puts.fetch_add(1, Ordering::SeqCst);
                }
                let _ = reply.send(store.dispatch(request));
            }
            drop(directory);
        });
        let backend = Backend::new(&origin).unwrap();
        let document = pull(&backend, &sender, &workspace, "state").await.unwrap();
        let snapshot: Value = serde_json::from_str(document["payload"].as_str().unwrap()).unwrap();
        assert_eq!(snapshot["threads"].as_array().unwrap().len(), 700);
        assert_eq!(snapshot["chats"][0]["id"], "chat-1");
        assert_eq!(snapshot["nodes"].as_array().unwrap().len(), 701);
        assert_eq!(snapshot["edges"][0]["id"], "edge-1");
        assert_eq!(snapshot["runtime"]["tasks"][0]["id"], "task-1");
        assert_eq!(snapshot["runtime"]["stateDir"], "isolated-state");
        assert_eq!(
            snapshot["runtime"]["rateLimitsByAccount"]["default"]["at"],
            100.0
        );
        assert_eq!(
            snapshot["runtime"]["rateLimits"],
            snapshot["runtime"]["rateLimitsByAccount"]["default"]
        );
        assert_eq!(state_puts.load(Ordering::SeqCst), 1);
        let unchanged = pull(&backend, &sender, &workspace, "state").await.unwrap();
        assert_eq!(unchanged, document);
        assert_eq!(
            state_puts.load(Ordering::SeqCst),
            1,
            "an unchanged entity refresh must not rewrite the full state document"
        );
        let checkpoint = state(
            &sender,
            "projection.get",
            json!({"workspace":workspace,"scope":"state:entities:checkpoint"}),
        )
        .await
        .unwrap();
        assert_eq!(checkpoint["seq"], 705);
        let tombstone = json!({"op":"delete","collection":"task","id":"task-1","seq":706});
        assert!(
            put_entity_change(&sender, &workspace, &tombstone)
                .await
                .unwrap()
        );
        let retired = materialize_state(&sender, &workspace).await.unwrap();
        let retired: Value = serde_json::from_str(retired["payload"].as_str().unwrap()).unwrap();
        assert!(
            retired["runtime"]["tasks"].as_array().unwrap().is_empty(),
            "a normal entity tombstone must remove the task from materialized state"
        );
        server.await.unwrap();
    }
}
