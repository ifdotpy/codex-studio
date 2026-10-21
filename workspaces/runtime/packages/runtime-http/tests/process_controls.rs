//! Native transport fixtures exercise the real HTTP action and receipt boundary.
#![allow(clippy::unwrap_used)]
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use studio_runtime_http::{HttpState, router};
use studio_runtime_services::account_session_runtime::{
    AccountSessionFuture, AccountSessionPort, ConnectionScope, ProviderRequest, ProviderResponse,
    RequestCertainty, SessionReceipt,
};
use studio_runtime_store::{RuntimeStore, StoreError};
use tower::ServiceExt;

struct Port {
    store: Arc<RuntimeStore>,
    scope: ConnectionScope,
    behavior: &'static str,
    calls: Mutex<Vec<String>>,
}
impl AccountSessionPort for Port {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        (account == self.scope.account).then(|| self.scope.clone())
    }
    fn provider_request(
        &self,
        request: ProviderRequest,
    ) -> AccountSessionFuture<'_, SessionReceipt> {
        Box::pin(async move {
            assert_eq!(request.expected_scope.as_ref(), Some(&self.scope));
            assert_eq!(request.actor.as_ref().unwrap().agent, "owner");
            let count: i64 = self
                .store
                .test_reader()
                .unwrap()
                .query_row("SELECT count(*) FROM runtime_process_actions", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(count, 1, "persist uncertainty before any native effect");
            let method = request.method.as_str();
            self.calls.lock().unwrap().push(method.into());
            let mut certainty = RequestCertainty::Written;
            let mut rejected = false;
            let value = match method {
                "thread/commandExecution/background" => {
                    assert_eq!(
                        request.params,
                        json!({"threadId":"thread","turnId":"turn","itemId":"toolu-1"})
                    );
                    match self.behavior {
                        "claude_lost" => certainty = RequestCertainty::Unknown,
                        "claude_rejected" => rejected = true,
                        _ => {}
                    }
                    json!({"status":"backgrounded"})
                }
                "thread/backgroundTerminals/list" => {
                    if self.behavior == "owner_changed" {
                        self.store.transaction_with(|tx| {
                            tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.epoch',2) WHERE id='owner'",[])?;
                            Ok::<_,StoreError>(())
                        }).await.unwrap();
                    }
                    json!({"data":[{"itemId":if self.behavior=="wrong_pair" {"other"} else {"command"},"processId":"42"}],"nextCursor":null})
                }
                "thread/backgroundTerminals/terminate" => {
                    assert_eq!(
                        request.params,
                        json!({"threadId":"thread","processId":"42"})
                    );
                    if self.behavior == "lost_reply" {
                        certainty = RequestCertainty::Unknown;
                    }
                    json!({"terminated":true})
                }
                "turn/interrupt" => {
                    assert_eq!(request.params["turnId"], "turn");
                    if self.behavior.starts_with("claude") {
                        assert_eq!(request.params["stopBackgroundTasks"], false);
                    } else {
                        assert!(request.params.get("stopBackgroundTasks").is_none());
                    }
                    json!({})
                }
                _ => panic!("unexpected native method: {method}"),
            };
            Ok(SessionReceipt {
                scope: self.scope.clone(),
                operation: request.operation,
                ticket: None,
                certainty,
                result: Some(if rejected {
                    ProviderResponse::Rejected(json!({"message":"cannot move yet"}))
                } else {
                    ProviderResponse::Reply(value)
                }),
                not_sent_reason: None,
                replayed: false,
            })
        })
    }
}
async fn call(app: &axum::Router, method: &str, path: &str, body: Value) -> (u16, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:49171")
        .header("content-type", "application/json")
        .header("x-canvas-token", "fixture-token")
        .header(
            studio_runtime_http::API_SCHEMA_HASH_HEADER,
            studio_runtime_http::api_schema_hash(),
        )
        .extension(ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(if method == "GET" {
            Body::empty()
        } else {
            Body::from(body.to_string())
        })
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn fixture(behavior: &'static str) -> (tempfile::TempDir, axum::Router, Arc<Port>) {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    store.transaction_with(move |tx| {
        let owner=json!({"id":"owner","rootId":"owner","epoch":1,"status":"running",
            "accountKey":"default","provider":if behavior.starts_with("claude") {"claude"} else {"codex"},
            "threadId":"thread","turnId":"turn","inFlight":true,"autoWake":true});
        let task=json!({"id":"owner:command","agent":"owner","kind":"command","itemId":"command",
            "processId":"42","status":"running","ownerEpoch":1,"sourceAccountKey":"default",
            "sourceConnectionId":"actual-uuid","sourceThreadId":"thread","turnId":"turn","created":1});
        tx.execute("INSERT INTO runtime_agents(id,record) VALUES('owner',?)",[owner.to_string()])?;
        tx.execute("INSERT INTO runtime_tasks(id,record) VALUES('owner:command',?)",[task.to_string()])?;
        Ok::<_,StoreError>(())
    }).await.unwrap();
    let port = Arc::new(Port {
        store: store.clone(),
        scope: ConnectionScope {
            account: "default".into(),
            connection: "actual-uuid".into(),
            generation: 7,
        },
        behavior,
        calls: Mutex::new(vec![]),
    });
    let mut state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        store.root().to_string_lossy().into(),
        49171,
        None,
        Some(store),
        None,
    );
    state.account_session = Some(port.clone());
    (root, router(state).unwrap(), port)
}
#[tokio::test]
async fn exact_process_stop_and_lost_reply_never_repeat_the_native_effect() {
    for (behavior, expected, stops) in [
        ("success", "stopped", 1),
        ("lost_reply", "uncertain", 1),
        ("wrong_pair", "already_finished", 0),
        ("owner_changed", "uncertain", 0),
    ] {
        let (_root, app, port) = fixture(behavior).await;
        let (status, list) = call(&app, "GET", "/api/processes?agent=owner", Value::Null).await;
        assert_eq!(status, 200, "{list}");
        let p = &list["processes"][0];
        let body = json!({"action":"stop_selected","requestId":"stop","agent":"owner","accountKey":"default","epoch":1,
            "process":{"key":p["key"],"generation":p["generation"],"agent":"owner","accountKey":"default","epoch":1}});
        let (status, result) = call(&app, "POST", "/api/processes/action", body.clone()).await;
        assert_eq!(status, 200, "{result}");
        assert_eq!(
            result["processes"][0]["status"], expected,
            "{behavior}: {result}"
        );
        let before = port.calls.lock().unwrap().clone();
        let (status, replay) = call(&app, "POST", "/api/processes/action", body).await;
        assert_eq!(status, 200, "{replay}");
        assert_eq!(result, replay);
        assert_eq!(*port.calls.lock().unwrap(), before);
        assert_eq!(
            before
                .iter()
                .filter(|m| m.as_str() == "thread/backgroundTerminals/terminate")
                .count(),
            stops
        );
    }
}
#[tokio::test]
async fn response_stop_preserves_background_tasks_and_rejects_old_turns() {
    for provider in ["codex", "claude"] {
        let (_root, app, port) = fixture(provider).await;
        let (_, list) = call(&app, "GET", "/api/processes?agent=owner", Value::Null).await;
        let mut body = json!({"action":"stop_response","requestId":"stop","agent":"owner","accountKey":"default","epoch":1,"responseTarget":list["responseTarget"]});
        body["responseTarget"]["turnId"] = json!("old-turn");
        assert_eq!(
            call(&app, "POST", "/api/processes/action", body.clone())
                .await
                .0,
            400
        );
        assert!(port.calls.lock().unwrap().is_empty());
        body["responseTarget"]["turnId"] = json!("turn");
        let (status, result) = call(&app, "POST", "/api/processes/action", body).await;
        assert_eq!(status, 200, "{result}");
        assert_eq!(result["response"]["status"], "requested", "{result}");
        assert_eq!(*port.calls.lock().unwrap(), vec!["turn/interrupt"]);
    }
}
#[tokio::test]
async fn background_move_targets_one_item_once_and_keeps_uncertainty() {
    for (behavior, expected, phase, sent) in [
        ("claude", "backgrounded", "completed", 1),
        ("claude_lost", "uncertain", "uncertain", 1),
        ("claude_rejected", "failed", "completed", 1),
        ("codex", "failed", "completed", 0),
    ] {
        let (_root, app, port) = fixture(behavior).await;
        let (_, list) = call(&app, "GET", "/api/processes?agent=owner", Value::Null).await;
        let body = json!({"action":"background_command","requestId":"move","agent":"owner","accountKey":"default","epoch":1,"responseTarget":list["responseTarget"],"itemId":"toolu-1"});
        let (status, result) = call(&app, "POST", "/api/processes/action", body.clone()).await;
        assert_eq!(status, 200, "{behavior}: {result}");
        assert_eq!(result["status"], phase, "{behavior}: {result}");
        assert_eq!(
            result["response"]["status"], expected,
            "{behavior}: {result}"
        );
        assert_eq!(result["response"]["itemId"], "toolu-1");
        let (status, replay) = call(&app, "POST", "/api/processes/action", body).await;
        assert_eq!(status, 200, "{replay}");
        assert_eq!(result, replay);
        let calls = port.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), sent, "{behavior}: {calls:?}");
        let (_, saved) = call(
            &app,
            "GET",
            "/api/processes/action?requestId=move&agent=owner&accountKey=default&epoch=1",
            Value::Null,
        )
        .await;
        assert_eq!(saved, json!({"found":true,"result":result}));
    }
}
#[tokio::test]
async fn background_move_requires_the_current_turn_and_one_item() {
    let (_root, app, port) = fixture("claude").await;
    let (_, list) = call(&app, "GET", "/api/processes?agent=owner", Value::Null).await;
    let mut body = json!({"action":"background_command","requestId":"move","agent":"owner","accountKey":"default","epoch":1,"responseTarget":list["responseTarget"],"itemId":"toolu-1"});
    body["responseTarget"]["turnId"] = json!("old-turn");
    assert_eq!(
        call(&app, "POST", "/api/processes/action", body.clone())
            .await
            .0,
        400
    );
    body["responseTarget"]["turnId"] = json!("turn");
    let mut missing = body.clone();
    missing.as_object_mut().unwrap().remove("itemId");
    assert_eq!(
        call(&app, "POST", "/api/processes/action", missing).await.0,
        400
    );
    let mut stop = body.clone();
    stop["action"] = json!("stop_response");
    assert_eq!(
        call(&app, "POST", "/api/processes/action", stop).await.0,
        400
    );
    assert!(port.calls.lock().unwrap().is_empty());
}
