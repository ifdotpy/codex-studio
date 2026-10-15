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
    let mut params = json!({"path":path,"workspaceId":workspace,"timeoutMs":15000});
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
        let retryable = error
            .status
            .is_none_or(|status| status >= 500 || status == 408 || status == 429);
        state(
            sender,
            "outbox.failure",
            json!({"workspace":workspace,"id":id,"error":error.message,"retryable":retryable}),
        )
        .await
    }
}

pub async fn dispatch(backend: &Backend, sender: &StoreSender, request: Request) -> Response {
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
                response.error.as_mut().unwrap().status = Some(400);
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
            let _ = deliver(&backend, &sender, &workspace, &id).await;
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
                let _ = reply.send(store.dispatch(request));
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
}
