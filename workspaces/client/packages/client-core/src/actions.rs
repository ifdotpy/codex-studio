//! Durable identities for Compact and Review, independent of the UI lifetime.
use crate::{
    backend::Backend,
    delivery::StoreSender,
    protocol::{Request, Response},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value[key]
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 4096)
        .ok_or_else(|| format!("{key} is required."))
}
fn key(params: &Value) -> Result<String, String> {
    Ok(format!(
        "studio-native-action:{}:{}",
        text(params, "stateDir")?,
        text(params, "agentId")?
    ))
}
fn read(db: &Connection, key: &str, agent: &str) -> Result<Value, String> {
    let raw: Option<String> = db
        .query_row(
            "SELECT value FROM client_preferences WHERE key=?1",
            [key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let Some(raw) = raw else {
        return Ok(Value::Null);
    };
    let value: Value = serde_json::from_str(&raw).map_err(
        |_| "The saved action request is invalid. Check its receipt before another action.",
    )?;
    if value["id"].as_str() != Some(agent)
        || !matches!(value["action"].as_str(), Some("review" | "compact"))
        || value["request_id"]
            .as_str()
            .is_none_or(|s| s.is_empty() || s.len() > 128)
        || value["context"]["accountKey"].as_str().is_none()
    {
        return Err(
            "The saved action request is invalid. Check its receipt before another action.".into(),
        );
    }
    Ok(value)
}
fn apply(db: &Connection, request: &Request) -> Result<Value, String> {
    let p = &request.params;
    let key = key(p)?;
    let agent = text(p, "agentId")?;
    let existing = read(db, &key, agent)?;
    match request.method.as_str() {
        "actions.pending" => Ok(existing),
        "actions.acknowledge" => {
            let id = text(p, "requestId")?;
            if existing["request_id"].as_str() == Some(id) {
                db.execute(
                    "DELETE FROM client_preferences WHERE key=?1 OR key=?2",
                    params![key, format!("{key}:workspace")],
                )
                .map_err(|e| e.to_string())?;
            }
            Ok(Value::Null)
        }
        "actions.prepare" => {
            let action = text(p, "action")?;
            if !matches!(action, "review" | "compact") {
                return Err("Unknown native action.".into());
            }
            let workspace = text(p, "workspaceId")?;
            if workspace.len() != 32
                || !workspace
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            {
                return Err("Invalid workspace identity.".into());
            }
            if !existing.is_null() {
                let previous: Option<String> = db
                    .query_row(
                        "SELECT value FROM client_preferences WHERE key=?1",
                        [format!("{key}:workspace")],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(|e| e.to_string())?;
                if previous.as_deref() != Some(workspace) {
                    return Err("The saved action belongs to another or an unknown workspace. Check its receipt before another action.".into());
                }
                if existing["action"].as_str() != Some(action) {
                    return Err("Check the saved action request before another action.".into());
                }
                return Ok(existing);
            }
            let uuid = crate::platform::dispatch(&Request {
                id: "action-identity".into(),
                method: "system.uuid".into(),
                params: Value::Null,
            });
            let id = uuid
                .result
                .ok_or("The request identity could not be created.")?;
            let mut context = json!({"accountKey":p["accountKey"].as_str().filter(|s|!s.is_empty()).unwrap_or("default")});
            if let Some(thread) = p["threadId"].as_str().filter(|s| !s.is_empty()) {
                context["threadId"] = json!(thread);
            }
            if let Some(epoch) = p["epoch"].as_u64() {
                context["epoch"] = json!(epoch);
            }
            let value = json!({"id":agent,"action":action,"request_id":id,"context":context});
            db.execute(
                "INSERT INTO client_preferences VALUES(?1,?2)",
                params![key, value.to_string()],
            )
            .map_err(|e| e.to_string())?;
            db.execute("INSERT INTO client_preferences VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![format!("{key}:workspace"),workspace]).map_err(|e|e.to_string())?;
            Ok(value)
        }
        _ => Err("Unknown action state command.".into()),
    }
}
pub fn stored(db: &mut Connection, request: &Request) -> Response {
    let result = (|| -> Result<Response, String> {
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let durable = request.method == "actions.prepare";
        if durable
            && let Some(replay) = crate::store::replay(&tx, request).map_err(|e| e.to_string())?
        {
            return Ok(replay);
        }
        let value = apply(&tx, request)?;
        let reply = Response::success(request.id.clone(), value);
        if durable {
            crate::store::record(&tx, request, &reply).map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(reply)
    })();
    result.unwrap_or_else(|e| Response::failure(request.id.clone(), "action_state", e))
}
async fn stored_request(sender: &StoreSender, request: Request) -> Response {
    let id = request.id.clone();
    let (reply, receive) = tokio::sync::oneshot::channel();
    if sender.send((request, reply)).await.is_err() {
        return Response::failure(id, "core_failed", "The state worker stopped.");
    }
    receive
        .await
        .unwrap_or_else(|e| Response::failure(id, "core_failed", e))
}
fn notice(value: &Value) -> Value {
    for path in [
        value.get("error"),
        value.get("outcome").and_then(|v| v.get("error")),
        value.get("result").and_then(|v| v.get("error")),
    ] {
        if let Some(value) = path.filter(|v| {
            !v.is_null() && **v != Value::Bool(false) && **v != Value::String(String::new())
        }) {
            return value.clone();
        }
    }
    if value["replayed"].as_bool() == Some(true) {
        json!("This action request was already saved. No new action starts.")
    } else {
        Value::Null
    }
}
pub async fn submit(backend: &Backend, sender: &StoreSender, request: Request) -> Response {
    if request.id.len() > 100 {
        return Response::failure(
            request.id,
            "invalid_request",
            "The action identity is too long.",
        );
    }
    let Some(workspace) = request.params["workspaceId"]
        .as_str()
        .filter(|s| !s.is_empty())
    else {
        return Response::failure(
            request.id,
            "action_scope",
            "Wait for the workspace to load.",
        );
    };
    let identity = backend
        .dispatch(Request {
            id: request.id.clone(),
            method: "api.request".into(),
            params: json!({"path":"/api/sync/identity","timeoutMs":15000}),
        })
        .await;
    if identity.error.is_some() {
        return identity;
    }
    if identity
        .result
        .as_ref()
        .and_then(|v| v["workspaceId"].as_str())
        != Some(workspace)
    {
        return Response::failure(
            request.id,
            "action_scope",
            "The server workspace changed. The saved action has not been sent.",
        );
    }
    let prepared = stored_request(
        sender,
        Request {
            id: format!("{}:prepare", request.id),
            method: "actions.prepare".into(),
            params: request.params.clone(),
        },
    )
    .await;
    let Some(body) = prepared.result else {
        return Response {
            id: request.id,
            result: None,
            error: prepared.error,
        };
    };
    let response=backend.dispatch(Request {id:request.id.clone(),method:"api.request".into(),params:json!({"path":"/api/action","body":body,"workspaceId":workspace,"timeoutMs":15000})}).await;
    let acknowledged = response
        .result
        .as_ref()
        .is_some_and(|value| value["receipt"]["requestId"] == body["request_id"]);
    let not_applied = response
        .error
        .as_ref()
        .and_then(|e| e.details.as_ref())
        .is_some_and(|d| d["outcome"] == "not_applied");
    if acknowledged || not_applied {
        let cleared=crate::delivery::state(sender,"actions.acknowledge",json!({"stateDir":request.params["stateDir"],"agentId":request.params["agentId"],"requestId":body["request_id"]})).await;
        if let Err(error) = cleared {
            return Response::failure(request.id, "action_state", error);
        }
    }
    if let Some(value) = response.result {
        if !acknowledged {
            return Response::failure(
                request.id,
                "action_uncertain",
                "The action reply has no matching receipt. Check the saved request.",
            );
        }
        Response::success(
            request.id,
            json!({"response":value,"notice":notice(&value)}),
        )
    } else {
        response
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
    fn inputs() -> Value {
        json!({"stateDir":"/private/state","workspaceId":"a".repeat(32),"agentId":"lead","action":"compact","accountKey":"one","threadId":"thread-one","epoch":3})
    }
    #[test]
    fn retry_after_restart_preserves_context_and_old_ack_cannot_clear_a_new_action() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("client.sqlite3");
        let first = {
            let mut store = Store::open(&path).unwrap();
            store
                .dispatch(command("first", "actions.prepare", inputs()))
                .result
                .unwrap()
        };
        let mut store = Store::open(&path).unwrap();
        let mut changed = inputs();
        changed["accountKey"] = json!("two");
        changed["epoch"] = json!(4);
        assert_eq!(
            store
                .dispatch(command("retry", "actions.prepare", changed))
                .result,
            Some(first.clone())
        );
        let ack =
            json!({"stateDir":"/private/state","agentId":"lead","requestId":first["request_id"]});
        assert!(
            store
                .dispatch(command("ack", "actions.acknowledge", ack.clone()))
                .error
                .is_none()
        );
        let second = store
            .dispatch(command("second", "actions.prepare", inputs()))
            .result
            .unwrap();
        assert_ne!(second["request_id"], first["request_id"]);
        assert_eq!(
            store
                .dispatch(command("first", "actions.prepare", inputs()))
                .result,
            Some(first)
        );
        assert!(
            store
                .dispatch(command("late-ack", "actions.acknowledge", ack))
                .error
                .is_none()
        );
        assert_eq!(
            store
                .dispatch(command("read", "actions.pending", inputs()))
                .result,
            Some(second)
        );
    }
    #[test]
    fn changed_identity_workspace_and_action_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&temp.path().join("client.sqlite3")).unwrap();
        assert!(
            store
                .dispatch(command("first", "actions.prepare", inputs()))
                .error
                .is_none()
        );
        let mut changed = inputs();
        changed["action"] = json!("review");
        assert_eq!(
            store
                .dispatch(command("first", "actions.prepare", changed.clone()))
                .error
                .unwrap()
                .code,
            "identity_conflict"
        );
        assert!(
            store
                .dispatch(command("another", "actions.prepare", changed))
                .error
                .is_some()
        );
        let mut changed = inputs();
        changed["workspaceId"] = json!("b".repeat(32));
        assert!(
            store
                .dispatch(command("scope", "actions.prepare", changed))
                .error
                .is_some()
        );
    }
    #[tokio::test]
    async fn lost_http_reply_and_unmatched_receipt_keep_the_same_request_after_restart() {
        use crate::backend::tests::{read_http, reply};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
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
                    reply(&mut stream, r#"{"token":"test-token"}"#).await;
                } else {
                    assert!(headers.starts_with("POST /api/action "));
                    assert!(
                        headers
                            .to_ascii_lowercase()
                            .contains(&format!("x-canvas-workspace: {}", "a".repeat(32)))
                    );
                    bodies.push(body);
                    if bodies.len() == 1 {
                        drop(stream);
                        continue;
                    }
                    if bodies.len() == 2 {
                        reply(&mut stream, r#"{"receipt":{"requestId":"wrong"}}"#).await;
                        continue;
                    }
                    reply(
                        &mut stream,
                        &json!({"receipt":{"requestId":bodies[0]["request_id"]},"replayed":true})
                            .to_string(),
                    )
                    .await;
                    assert_eq!(bodies[0], bodies[1]);
                    assert_eq!(bodies[0], bodies[2]);
                    break;
                }
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let run_store = |path: std::path::PathBuf| {
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
        let backend = Backend::new(&origin).unwrap();
        for attempt in 0..3 {
            let (sender, thread) = run_store(path.clone());
            let mut params = inputs();
            params["accountKey"] = json!(format!("account-{attempt}"));
            let response = submit(
                &backend,
                &sender,
                command(&format!("rpc-{attempt}"), "actions.submit", params),
            )
            .await;
            let pending = crate::delivery::state(&sender, "actions.pending", inputs())
                .await
                .unwrap();
            if attempt < 2 {
                assert!(response.error.is_some());
                assert!(!pending.is_null());
                assert_eq!(pending["context"]["accountKey"], "account-0");
            } else {
                assert!(response.error.is_none(), "{response:?}");
                assert!(pending.is_null());
                assert_eq!(response.result.unwrap()["response"]["replayed"], true);
            }
            drop(sender);
            thread.join().unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
    }
}
