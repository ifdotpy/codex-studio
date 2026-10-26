//! Workspace verification and projection pulls run in Rust.
use crate::{
    backend::Backend,
    delivery::{StoreSender, state},
    projections::Document,
    protocol::{CommandError, Request, Response},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};

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
    if let Some(removed) = incoming["removed"].as_array() {
        for id in removed.iter().filter_map(Value::as_str) {
            items.remove(id);
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
    // Legacy persisted snapshots may still carry the retired wire map.
    for key in ["delta", "removed", "order", "itemRevisions"] {
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

async fn pull_entities(
    backend: &Backend,
    sender: &StoreSender,
    workspace: &str,
) -> Result<Value, CommandError> {
    // Read the local revision before the cursor so a concurrent application
    // makes this HTTP interval stale rather than allowing a late reset to win.
    let previous = state(
        sender,
        "projection.get",
        json!({"workspace":workspace,"scope":"state"}),
    )
    .await
    .map_err(|e| error("storage_error", e))?;
    let cached = state(
        sender,
        "projection.get",
        json!({"workspace":workspace,"scope":"state:entities:checkpoint"}),
    )
    .await
    .map_err(|e| error("storage_error", e))?;
    let ready_marker = state(
        sender,
        "projection.get",
        json!({"workspace":workspace,"scope":"state:entities:ready"}),
    )
    .await
    .map_err(|e| error("storage_error", e))?;
    let ready = ready_marker["payload"]
        .as_str()
        .and_then(|payload| serde_json::from_str::<Value>(payload).ok())
        == Some(Value::String("ready".into()))
        && ready_marker["_deleted"] != true;
    let mut replace = !ready;
    let mut after = if ready {
        cached["seq"].as_u64().unwrap_or(0)
    } else {
        0
    };
    let mut initial_high = (!ready).then_some(0);
    let mut changes = Vec::new();
    loop {
        let mut query = vec![
            ("scope", "state:entities:v1".to_owned()),
            ("after", after.to_string()),
            ("limit", "500".to_owned()),
            ("reset", "1".to_owned()),
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
        if result["reset"] == true {
            if initial_high.is_some() {
                return Err(error(
                    "invalid_projection",
                    "The server reset a fresh entity pull.",
                ));
            }
            // Keep the old cache intact until every replacement page is fetched.
            // Cancellation or a failed page leaves both entities and cursor intact.
            replace = true;
            after = 0;
            initial_high = Some(0);
            changes.clear();
            continue;
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
            initial_high = Some(result["initialHigh"].as_u64().ok_or_else(|| {
                error(
                    "invalid_projection",
                    "The server returned no initial entity bound.",
                )
            })?);
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
        after = next;
        let max = result["maxSeq"].as_u64().ok_or_else(|| {
            error(
                "invalid_projection",
                "The server returned no entity sequence bound.",
            )
        })?;
        changes.extend(rows.iter().cloned());
        if rows.len() < 500 || after >= max {
            break;
        }
    }
    state(sender, "projection.applyEntities", json!({"workspace":workspace,"changes":changes,"replace":replace,"checkpoint":after,"expectedRevision":previous["seq"].as_u64().unwrap_or(0)}))
        .await.map_err(|e| error("storage_error", e))
}

/// The UI receives a projection payload as an escaped JSON string in one
/// message. Refuse a payload that cannot fit, with the envelope reserve, so the
/// UI gets one actionable error instead of a transport response_size error.
pub fn projection_error(payload: &str) -> Option<CommandError> {
    // Bytes reserved for the response envelope around the escaped projection payload.
    const PROJECTION_ENVELOPE_RESERVE_BYTES: usize = 4096;
    projection_error_with_limit(
        payload,
        crate::protocol::MAX_MESSAGE.saturating_sub(PROJECTION_ENVELOPE_RESERVE_BYTES),
    )
}

fn projection_error_with_limit(payload: &str, limit: usize) -> Option<CommandError> {
    let encoded = serde_json::to_string(payload).map_or(usize::MAX, |text| text.len());
    (encoded > limit).then(|| {
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
    state(
        sender,
        "projection.applyEntities",
        json!({"workspace":workspace,"changes":values}),
    )
    .await
    .map_err(|e| error("storage_error", e))
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
    fn transcript_delta_merges_legacy_cache_without_retaining_item_revisions() {
        let base = json!({"payload":json!({"items":[{"id":"a","v":1},{"id":"b","v":2}],"itemRevisions":{"a":1,"b":1},"title":"old"}).to_string()});
        let delta = json!({"items":[{"id":"a","v":3},{"id":"c","v":4}],"removed":["b"],"order":["c","a"],"delta":true,"title":"new"});
        let actual: Value =
            serde_json::from_str(&merge_transcript_delta(&base, &delta).unwrap()).unwrap();
        assert_eq!(
            actual,
            json!({"items":[{"id":"c","v":4},{"id":"a","v":3}],"title":"new"})
        );
    }

    #[test]
    fn oversized_materialized_state_returns_one_actionable_projection_error() {
        let snapshot = json!({"runtime":{"tasks":["x".repeat(44 * 1024 * 1024)]}});
        let failure = crate::projections::serialize_state(&snapshot).unwrap_err();
        assert_eq!(failure.code, "projection_too_large");
        assert!(failure.message.contains("Update the Studio server"));
    }

    #[test]
    fn projection_limit_counts_json_escaping() {
        // Quotes occupy two JSON bytes; plain text occupies one, plus delimiters.
        assert_eq!(
            projection_error_with_limit(&"\"".repeat(32), 63)
                .unwrap()
                .code,
            "projection_too_large"
        );
        assert!(projection_error_with_limit(&"x".repeat(61), 63).is_none());
        assert!(projection_error_with_limit(&"x".repeat(62), 63).is_some());
    }

    #[tokio::test]
    async fn entity_batches_with_out_of_order_sequences_remain_visible() {
        let workspace = "b".repeat(32);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server_workspace = workspace.clone();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
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
        let worker_path = path.clone();
        let worker = tokio::spawn(async move {
            let mut store = crate::store::Store::open(&worker_path).unwrap();
            while let Some((request, reply)) = receive.recv().await {
                let _ = reply.send(store.dispatch(request));
            }
        });
        let mut previous = 0;
        for (id, seq, count) in [("A", 11, 1), ("B", 10, 2)] {
            let response = dispatch(&backend, &sender, Request {
                id: format!("apply-{id}"), method: "sync.applyEntities".into(),
                params: json!({"workspace":workspace,"documents":[{"op":"upsert","collection":"task","id":id,"seq":seq,"value":{"id":id}}]}),
            }).await;
            assert_eq!(response.id, format!("apply-{id}"));
            assert!(response.error.is_none(), "{:?}", response.error);
            let doc = response.result.unwrap();
            let snapshot: Value = serde_json::from_str(doc["payload"].as_str().unwrap()).unwrap();
            assert_eq!(
                snapshot["runtime"]["tasks"].as_array().unwrap().len(),
                count
            );
            assert!(doc["seq"].as_u64().unwrap() > previous);
            previous = doc["seq"].as_u64().unwrap();
        }
        drop(sender);
        worker.await.unwrap();
        server.await.unwrap();
        let mut store = crate::store::Store::open(&path).unwrap();
        let doc = store
            .dispatch(Request {
                id: "read".into(),
                method: "projection.get".into(),
                params: json!({"workspace":workspace,"scope":"state"}),
            })
            .result
            .unwrap();
        assert_eq!(doc["seq"], previous);
        let snapshot: Value = serde_json::from_str(doc["payload"].as_str().unwrap()).unwrap();
        assert_eq!(snapshot["runtime"]["tasks"], json!([{"id":"A"},{"id":"B"}]));
    }

    #[tokio::test]
    async fn entity_pull_resets_pruned_tombstones_and_replaces_cache() {
        let workspace = "a".repeat(32);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server_workspace = workspace.clone();
        let server = tokio::spawn(async move {
            let mut reset_supported = false;
            for index in 0..6 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (headers, _) = crate::backend::tests::read_http(&mut stream).await;
                let path = headers
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap();
                let body = if matches!(index, 0 | 3) {
                    json!({"workspaceId":server_workspace,"chatState":true})
                } else {
                    let url = reqwest::Url::parse(&format!("http://localhost{path}")).unwrap();
                    reset_supported |= url.query_pairs().any(|(k, v)| k == "reset" && v == "1");
                    if matches!(index, 1 | 4) {
                        assert!(url.query_pairs().any(|(k, v)| k == "after" && v == "5"));
                        json!({"workspaceId":server_workspace,"reset":true,"floor":8,"maxSeq":12})
                    } else {
                        assert!(url.query_pairs().any(|(k, v)| k == "after" && v == "0"));
                        assert!(url.query_pairs().any(|(k, v)| k == "fresh" && v == "1"));
                        json!({"workspaceId":server_workspace,"documents":[{"op":"upsert","collection":"task","id":"current","seq":12,"value":{"id":"current"}}],"checkpoint":{"seq":12},"maxSeq":12,"initialHigh":12})
                    }
                };
                let body = body.to_string();
                let status = if index == 2 {
                    "503 Service Unavailable"
                } else {
                    "200 OK"
                };
                stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
            }
            assert!(reset_supported, "entity pulls must request reset support");
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let mut store = crate::store::Store::open(&path).unwrap();
        assert!(store.dispatch(Request { id: "seed".into(), method: "projection.putEntity".into(), params: json!({"workspace":workspace,"change":{"op":"upsert","collection":"task","id":"deleted","seq":5,"value":{"id":"deleted"}}}) }).error.is_none());
        for (scope, payload) in [
            ("state:entities:checkpoint", "{}"),
            ("state:entities:ready", "\"ready\""),
            ("transcript:kept", "{}"),
            ("state", "{}"),
        ] {
            assert!(store.dispatch(Request { id: "seed".into(), method: "projection.put".into(), params: json!({"workspace":workspace,"scope":scope,"document":{"id":scope,"seq":5,"payload":payload}}) }).error.is_none());
        }
        let (sender, mut receive) = mpsc::channel::<crate::delivery::StoreCommand>(32);
        let worker = tokio::spawn(async move {
            while let Some((request, reply)) = receive.recv().await {
                let _ = reply.send(store.dispatch(request));
            }
        });
        let failure = pull(&backend, &sender, &workspace, "state")
            .await
            .unwrap_err();
        assert_eq!(failure.status, Some(503));
        let retained = state(
            &sender,
            "projection.get",
            json!({"workspace":workspace,"scope":"entity:task:deleted"}),
        )
        .await
        .unwrap();
        assert_eq!(
            retained["seq"], 5,
            "a failed reset keeps the previous cache"
        );
        let retained = state(
            &sender,
            "projection.get",
            json!({"workspace":workspace,"scope":"state:entities:checkpoint"}),
        )
        .await
        .unwrap();
        assert_eq!(
            retained["seq"], 5,
            "a failed reset keeps the previous cursor"
        );
        let doc = pull(&backend, &sender, &workspace, "state").await.unwrap();
        let snapshot: Value = serde_json::from_str(doc["payload"].as_str().unwrap()).unwrap();
        assert_eq!(snapshot["runtime"]["tasks"], json!([{"id":"current"}]));
        assert!(doc["seq"].as_u64().unwrap() > 5);
        drop(sender);
        worker.await.unwrap();
        server.await.unwrap();
        let mut store = crate::store::Store::open(&path).unwrap();
        let get = |scope| Request {
            id: "read".into(),
            method: "projection.get".into(),
            params: json!({"workspace":workspace,"scope":scope}),
        };
        assert_eq!(
            store.dispatch(get("entity:task:deleted")).result,
            Some(Value::Null)
        );
        assert_eq!(
            store
                .dispatch(get("state:entities:checkpoint"))
                .result
                .unwrap()["seq"],
            12
        );
        assert_eq!(
            store.dispatch(get("transcript:kept")).result.unwrap()["seq"],
            5
        );
    }

    #[tokio::test]
    async fn entity_pull_rejects_interval_racing_a_newer_application() {
        let workspace = "a".repeat(32);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let (started, waiting) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let server_workspace = workspace.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            crate::backend::tests::read_http(&mut stream).await;
            let body = json!({"workspaceId":server_workspace,"chatState":true}).to_string();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
            let (mut stream, _) = listener.accept().await.unwrap();
            crate::backend::tests::read_http(&mut stream).await;
            started.send(()).unwrap();
            released.await.unwrap();
            let body = json!({"workspaceId":server_workspace,"documents":[{"op":"upsert","collection":"task","id":"A","seq":1,"value":{"id":"A"}}],"checkpoint":{"seq":1},"maxSeq":1,"initialHigh":1}).to_string();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let (sender, mut receive) = mpsc::channel::<crate::delivery::StoreCommand>(32);
        let worker = tokio::spawn(async move {
            let mut store = crate::store::Store::open(&path).unwrap();
            while let Some((request, reply)) = receive.recv().await {
                let _ = reply.send(store.dispatch(request));
            }
            drop(directory);
        });
        let pull_sender = sender.clone();
        let pull_workspace = workspace.clone();
        let pending =
            tokio::spawn(
                async move { pull(&backend, &pull_sender, &pull_workspace, "state").await },
            );
        waiting.await.unwrap();
        let deleted = state(&sender, "projection.applyEntities", json!({"workspace":workspace,"changes":[{"op":"delete","collection":"task","id":"A","seq":2}]})).await.unwrap();
        release.send(()).unwrap();
        let failure = pending.await.unwrap().unwrap_err();
        assert!(failure.message.contains("Entity state changed"));
        assert_eq!(
            state(
                &sender,
                "projection.get",
                json!({"workspace":workspace,"scope":"state"})
            )
            .await
            .unwrap(),
            deleted
        );
        assert_eq!(
            state(
                &sender,
                "projection.get",
                json!({"workspace":workspace,"scope":"entity:task:A"})
            )
            .await
            .unwrap()["op"],
            "delete"
        );
        assert!(
            state(
                &sender,
                "projection.get",
                json!({"workspace":workspace,"scope":"state:entities:checkpoint"})
            )
            .await
            .unwrap()
            .is_null()
        );
        drop(sender);
        worker.await.unwrap();
        server.await.unwrap();
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
        let commands = Arc::new(AtomicUsize::new(0));
        let observed_commands = Arc::clone(&commands);
        let worker = tokio::spawn(async move {
            let mut store = crate::store::Store::open(&path).unwrap();
            while let Some((request, reply)) = receive.recv().await {
                observed_commands.fetch_add(1, Ordering::SeqCst);
                let batch = request.method == "projection.applyEntities";
                let response = store.dispatch(request);
                if batch {
                    let revision = response
                        .result
                        .as_ref()
                        .and_then(|v| v["seq"].as_u64())
                        .unwrap_or(0) as usize;
                    observed.store(revision, Ordering::SeqCst);
                }
                let _ = reply.send(response);
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
        assert_eq!(
            commands.load(Ordering::SeqCst),
            1,
            "one storage command applies and materializes the entire batch"
        );
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
        assert_eq!(commands.load(Ordering::SeqCst), 2);
        let stale = dispatch(&backend, &sender, apply(json!([task]))).await;
        assert_eq!(stale.result, Some(removed));
        assert_eq!(commands.load(Ordering::SeqCst), 3);
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
                let batch = request.method == "projection.applyEntities";
                let response = store.dispatch(request);
                if batch {
                    let revision = response
                        .result
                        .as_ref()
                        .and_then(|v| v["seq"].as_u64())
                        .unwrap_or(0) as usize;
                    observed_state_puts.store(revision, Ordering::SeqCst);
                }
                let _ = reply.send(response);
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
        let retired = state(
            &sender,
            "projection.applyEntities",
            json!({"workspace":workspace,"changes":[tombstone]}),
        )
        .await
        .unwrap();
        let retired: Value = serde_json::from_str(retired["payload"].as_str().unwrap()).unwrap();
        assert!(
            retired["runtime"]["tasks"].as_array().unwrap().is_empty(),
            "a normal entity tombstone must remove the task from materialized state"
        );
        server.await.unwrap();
    }
}
