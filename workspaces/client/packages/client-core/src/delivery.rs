//! Native queue delivery. The server owns deduplication of each message ID.
use crate::{
    backend::Backend,
    protocol::{Request, Response},
};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

pub type StoreCommand = (Request, oneshot::Sender<Response>);
pub type StoreSender = mpsc::Sender<StoreCommand>;

pub async fn state(sender: &StoreSender, method: &str, params: Value) -> Result<Value, String> {
    let (reply, receive) = oneshot::channel();
    sender
        .send((
            Request {
                id: "core-delivery".into(),
                method: method.into(),
                params,
            },
            reply,
        ))
        .await
        .map_err(|_| "The state worker stopped.")?;
    let response = receive.await.map_err(|_| "The state worker stopped.")?;
    response.result.ok_or_else(|| {
        response
            .error
            .map(|error| error.message)
            .unwrap_or_else(|| "Missing state response.".into())
    })
}

async fn http(backend: &Backend, path: &str, body: Option<Value>, workspace: &str) -> Response {
    let mut params = json!({"path":path,"timeoutMs":15000});
    // The identity read precedes any workspace; an empty header is refused.
    if !workspace.is_empty() {
        params["workspaceId"] = json!(workspace);
    }
    if let Some(body) = body {
        params["body"] = body;
    }
    backend
        .dispatch(Request {
            id: "core-delivery".into(),
            method: "api.request".into(),
            params,
        })
        .await
}

pub async fn deliver(
    backend: &Backend,
    sender: &StoreSender,
    workspace: &str,
    id: &str,
) -> Result<Value, String> {
    let identity = http(backend, "/api/sync/identity", None, workspace).await;
    let current = identity
        .result
        .as_ref()
        .and_then(|value| value.get("workspaceId"))
        .and_then(Value::as_str);
    if current != Some(workspace) {
        return Err(identity
            .error
            .map(|e| e.message)
            .unwrap_or_else(|| "The server workspace changed. Reload before sending.".into()));
    }
    backend.prepare_session().await.map_err(|e| e.message)?;
    let claim = state(
        sender,
        "outbox.claim",
        json!({"workspace":workspace,"id":id}),
    )
    .await?;
    if claim.get("claimed").and_then(Value::as_bool) != Some(true) {
        return Ok(claim["entry"].clone());
    }
    let response = http(
        backend,
        "/api/messages",
        Some(claim["entry"]["body"].clone()),
        workspace,
    )
    .await;
    if let Some(result) = response.result {
        match state(
            sender,
            "outbox.receipt",
            json!({"workspace":workspace,"id":id,"receipt":result}),
        )
        .await
        {
            Ok(entry) => Ok(entry),
            Err(error) => {
                state(
                    sender,
                    "outbox.failure",
                    json!({"workspace":workspace,"id":id,"error":error,"retryable":true}),
                )
                .await
            }
        }
    } else {
        let error = response.error.ok_or("Missing delivery response.")?;
        // 426: the API schema changed. The message waits for the updated client.
        let retryable = error
            .status
            .is_none_or(|status| status >= 500 || status == 408 || status == 426 || status == 429);
        state(
            sender,
            "outbox.failure",
            json!({"workspace":workspace,"id":id,"error":error.message,"retryable":retryable}),
        )
        .await
    }
}

/// Read the server receipts of acknowledged messages that this device still
/// shows as pending (`recoverAcknowledgedOutbox` in the web client).
pub async fn recover(backend: &Backend, sender: &StoreSender) -> Result<Value, String> {
    let identity = http(backend, "/api/sync/identity", None, "").await;
    let workspace = identity
        .result
        .as_ref()
        .and_then(|value| value.get("workspaceId"))
        .and_then(Value::as_str)
        .ok_or("The server workspace is unavailable.")?
        .to_owned();
    let entries = state(sender, "outbox.list", json!({"workspace":workspace})).await?;
    let mut rooms = std::collections::BTreeMap::<String, Vec<String>>::new();
    for entry in entries.as_array().into_iter().flatten() {
        let (Some(id), Some(room)) = (
            entry.get("id").and_then(Value::as_str),
            entry["body"].get("room").and_then(Value::as_str),
        ) else {
            continue;
        };
        if entry["displayPending"] != false
            && ["accepted", "uncertain"].contains(&entry["status"].as_str().unwrap_or(""))
        {
            rooms
                .entry(room.to_owned())
                .or_default()
                .push(id.to_owned());
        }
    }
    let encode = |value: &str| {
        percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
    };
    let (mut changed, mut failure) = (0, None);
    for (room, ids) in rooms {
        for batch in ids.chunks(100) {
            let path = format!(
                "/api/messages/receipts?agent={}&ids={}",
                encode(&room),
                encode(&json!(batch).to_string())
            );
            let response = http(backend, &path, None, &workspace).await;
            let Some(value) = response.result else {
                failure = response.error.map(|error| error.message);
                continue;
            };
            // `checkedMessageReceipts` in the web client.
            let items = value["items"].as_array();
            let ids = items
                .into_iter()
                .flatten()
                .filter_map(|item| item.get("id").and_then(Value::as_str))
                .collect::<std::collections::BTreeSet<_>>();
            if value["workspaceId"] != workspace.as_str()
                || value["agent"] != room.as_str()
                || items.is_none_or(|items| items.len() != ids.len())
                || !ids.iter().all(|id| batch.iter().any(|other| other == id))
            {
                failure = Some("The delivery receipts do not match the requested messages.".into());
                continue;
            }
            changed += state(
                sender,
                "outbox.reconcile",
                json!({"workspace":workspace,"room":room,"receipts":value["items"]}),
            )
            .await?["changed"]
                .as_u64()
                .unwrap_or(0);
        }
    }
    failure.map_or(Ok(json!({"changed":changed})), Err)
}

pub async fn dispatch(backend: &Backend, sender: &StoreSender, request: Request) -> Response {
    if request.method == "outbox.recover" {
        return match recover(backend, sender).await {
            Ok(value) => Response::success(request.id, value),
            Err(error) => Response::failure(request.id, "delivery_error", error),
        };
    }
    let result = match (
        request
            .params
            .get("workspace")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty()),
        request
            .params
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty()),
    ) {
        (Some(workspace), Some(id)) => deliver(backend, sender, workspace, id).await,
        _ => Err("Workspace and message identity are required.".into()),
    };
    match result {
        Ok(value) => {
            if value["status"] == "failed" {
                let mut response = Response::failure(
                    request.id,
                    "message_failed",
                    value["error"].as_str().unwrap_or("Message failed."),
                );
                // Response::failure always initializes its error member.
                response
                    .error
                    .as_mut()
                    .expect("failure has an error")
                    .status = Some(400);
                response
            } else {
                let result = value.get("receipt").cloned().unwrap_or_else(|| {
                    json!({
                        "queued":value["status"]=="queued" || value["status"]=="paused",
                        "status":value["status"],"error":value["error"]
                    })
                });
                Response::success(request.id, result)
            }
        }
        Err(error) => Response::failure(request.id, "delivery_error", error),
    }
}

pub async fn drain(backend: &Backend, sender: &StoreSender) {
    let identity = http(backend, "/api/sync/identity", None, "").await;
    let Some(workspace) = identity
        .result
        .as_ref()
        .and_then(|value| value.get("workspaceId"))
        .and_then(Value::as_str)
    else {
        return;
    };
    let Ok(entries) = state(sender, "outbox.list", json!({"workspace":workspace})).await else {
        return;
    };
    let Some(entries) = entries.as_array() else {
        return;
    };
    let mut rooms = std::collections::BTreeSet::new();
    let mut pending = tokio::task::JoinSet::new();
    // One oldest queued message per room, with at most four concurrent HTTP waits.
    for entry in entries {
        if entry.get("status").and_then(Value::as_str) != Some("queued") {
            continue;
        }
        let (Some(id), Some(room)) = (
            entry.get("id").and_then(Value::as_str),
            entry["body"].get("room").and_then(Value::as_str),
        ) else {
            continue;
        };
        if !rooms.insert(room.to_owned()) {
            continue;
        }
        let (sender, backend, workspace, id) = (
            sender.clone(),
            backend.clone(),
            workspace.to_owned(),
            id.to_owned(),
        );
        pending.spawn(async move {
            drop(deliver(&backend, &sender, &workspace, &id).await);
        });
        if pending.len() >= 4 {
            let _ = pending.join_next().await;
        }
    }
    while pending.join_next().await.is_some() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        backend::tests::{read_http, reply},
        store::Store,
    };
    use tokio::net::TcpListener;

    fn store(path: std::path::PathBuf) -> (StoreSender, std::thread::JoinHandle<()>) {
        let (sender, mut receiver) = mpsc::channel::<StoreCommand>(64);
        let worker = std::thread::spawn(move || {
            let mut store = Store::open(&path).unwrap();
            while let Some((request, reply)) = receiver.blocking_recv() {
                drop(reply.send(store.dispatch(request)));
            }
        });
        (sender, worker)
    }

    async fn user(sender: &StoreSender, id: &str, method: &str, params: Value) -> Response {
        let (reply, receive) = oneshot::channel();
        sender
            .send((
                Request {
                    id: id.into(),
                    method: method.into(),
                    params,
                },
                reply,
            ))
            .await
            .unwrap();
        receive.await.unwrap()
    }

    #[tokio::test]
    async fn delivery_retry_after_restart_keeps_the_original_message_identity() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (headers, body) = read_http(&mut stream).await;
                if headers.starts_with("GET /api/sync/identity ") {
                    reply(&mut stream, r#"{"workspaceId":"w"}"#).await;
                } else if headers.starts_with("GET /api/session ") {
                    reply(&mut stream, r#"{"token":"private-test-token"}"#).await;
                } else {
                    assert!(headers.starts_with("POST /api/messages "));
                    bodies.push(body);
                    if bodies.len() == 1 {
                        drop(stream);
                        continue;
                    }
                    reply(&mut stream, r#"{"id":"message-one","status":"sent"}"#).await;
                    assert_eq!(bodies.len(), 2);
                    assert_eq!(bodies[0], bodies[1]);
                    assert_eq!(bodies[0]["id"], "message-one");
                    break;
                }
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let (sender, worker) = store(path.clone());
        assert!(
            user(
                &sender,
                "enqueue",
                "outbox.enqueue",
                json!({"workspace":"w","body":{"id":"message-one","room":"room","text":"Привет"}})
            )
            .await
            .error
            .is_none()
        );
        let backend = Backend::new(&origin).unwrap();
        let first = deliver(&backend, &sender, "w", "message-one")
            .await
            .unwrap();
        assert_eq!(first["status"], "queued");
        assert_eq!(first["attempted"], true);
        drop(sender);
        worker.join().unwrap();
        let (sender, worker) = store(path);
        let cancelled = user(
            &sender,
            "cancel",
            "outbox.change",
            json!({"workspace":"w","id":"message-one","action":"cancel"}),
        )
        .await;
        assert!(
            cancelled
                .error
                .unwrap()
                .message
                .contains("may have started")
        );
        let sent = deliver(&Backend::new(&origin).unwrap(), &sender, "w", "message-one")
            .await
            .unwrap();
        assert_eq!(sent["status"], "accepted");
        assert_eq!(sent["receipt"]["status"], "sent");
        drop(sender);
        worker.join().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn recovery_reads_receipts_of_acknowledged_messages_only() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (headers, _) = read_http(&mut stream).await;
                if headers.starts_with("GET /api/sync/identity ") {
                    reply(&mut stream, r#"{"workspaceId":"w"}"#).await;
                    continue;
                }
                // Only the accepted message is asked for; the queued one keeps
                // the idempotent send path.
                assert!(
                    headers.starts_with(
                        "GET /api/messages/receipts?agent=room&ids=%5B%22accepted%22%5D "
                    ),
                    "{headers}"
                );
                reply(
                    &mut stream,
                    r#"{"workspaceId":"w","agent":"room","items":[{"id":"accepted","agent":"room","status":"delivered","materialized":true}]}"#,
                )
                .await;
                break;
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let (sender, worker) = store(directory.path().join("client.sqlite3"));
        for id in ["accepted", "queued"] {
            let body = json!({"id":id,"room":"room","text":id});
            user(
                &sender,
                id,
                "outbox.enqueue",
                json!({"workspace":"w","body":body}),
            )
            .await;
        }
        state(
            &sender,
            "outbox.claim",
            json!({"workspace":"w","id":"accepted"}),
        )
        .await
        .unwrap();
        state(
            &sender,
            "outbox.receipt",
            json!({"workspace":"w","id":"accepted","receipt":{"id":"accepted","status":"queued"}}),
        )
        .await
        .unwrap();
        let backend = Backend::new(&origin).unwrap();
        assert_eq!(recover(&backend, &sender).await.unwrap()["changed"], 1);
        let entries = state(&sender, "outbox.list", json!({"workspace":"w"}))
            .await
            .unwrap();
        let accepted = &entries.as_array().unwrap()[0];
        assert_eq!(accepted["receipt"]["status"], "delivered");
        assert_eq!(accepted["displayPending"], false);
        drop(sender);
        worker.join().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
    }
}
