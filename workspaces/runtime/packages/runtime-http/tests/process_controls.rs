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
        if self.behavior.starts_with("transferred") && account == "target" {
            return Some(ConnectionScope {
                account: account.into(),
                connection: "target-connection".into(),
                generation: 11,
            });
        }
        if self.behavior == "transferred_missing_source" && account == self.scope.account {
            return None;
        }
        if self.behavior == "transferred_wrong_generation" && account == self.scope.account {
            let mut scope = self.scope.clone();
            scope.generation += 1;
            return Some(scope);
        }
        if matches!(
            self.behavior,
            "transferred_reattached" | "transferred_wrong_connection"
        ) && account == self.scope.account
        {
            let mut scope = self.scope.clone();
            scope.connection = "reattached-source".into();
            return Some(scope);
        }
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

impl studio_runtime_services::monitor_driver::MonitorNative for Port {
    fn submit<'a>(
        &'a self,
        operation: &'a studio_runtime_services::monitor_policy::Operation,
        effect: &'a studio_runtime_services::monitor_policy::Effect,
    ) -> futures_util::future::BoxFuture<'a, studio_runtime_services::monitor_driver::Submission>
    {
        Box::pin(async move {
            assert_eq!(operation.agent, "owner");
            assert_eq!(operation.epoch, 1);
            assert_eq!(operation.account_key, self.scope.account);
            assert_eq!(operation.connection_id, self.scope.connection);
            let studio_runtime_services::monitor_policy::Effect::Native { method, params, .. } =
                effect
            else {
                panic!("only existing monitor controls are permitted")
            };
            assert!(matches!(
                *method,
                "command/exec/terminate" | "command/exec/write" | "command/exec/resize"
            ));
            assert_eq!(
                params["processId"],
                crate::monitor_fixtures::corpus(self.store.root())[0]["before"]["id"]
            );
            self.calls.lock().unwrap().push((*method).into());
            if self.behavior == "transferred_unknown_stop" && *method == "command/exec/terminate" {
                return studio_runtime_services::monitor_driver::Submission::Unknown;
            }
            studio_runtime_services::monitor_driver::Submission::Written
        })
    }
}
impl studio_runtime_services::rules_tick::RuleMonitor for Port {
    fn available(&self) -> bool {
        false
    }
    fn launch<'a>(
        &'a self,
        _: &'a studio_runtime_services::rules_engine::RuleCheck,
    ) -> futures_util::future::BoxFuture<'a, studio_runtime_services::rules_tick::CheckSubmission>
    {
        Box::pin(async { panic!("no command launch") })
    }
    fn cancel<'a>(
        &'a self,
        _: &'a str,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<(), studio_runtime_services::rules_tick::RulesTickError>,
    > {
        Box::pin(async { panic!("no rule effect") })
    }
}
impl studio_runtime_services::monitor_runtime_wiring::MonitorPort for Port {
    fn next(
        &self,
    ) -> futures_util::future::BoxFuture<
        '_,
        Result<
            Option<studio_runtime_services::monitor_runtime_wiring::Delivery>,
            studio_runtime_services::monitor_policy::MonitorError,
        >,
    > {
        Box::pin(async { Ok(None) })
    }
    fn acknowledge<'a>(
        &'a self,
        _: &'a studio_runtime_services::monitor_runtime_wiring::Delivery,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<(), studio_runtime_services::monitor_policy::MonitorError>,
    > {
        Box::pin(async { panic!("no queued monitor event") })
    }
    fn reattach<'a>(
        &'a self,
        _: &'a studio_runtime_services::monitor_policy::MonitorSnapshot,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<
            studio_runtime_services::monitor_policy::ReattachOutcome,
            studio_runtime_services::monitor_policy::MonitorError,
        >,
    > {
        Box::pin(async { panic!("no process reattach") })
    }
    fn probe<'a>(
        &'a self,
        _: &'a str,
        _: i64,
        _: &'a str,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<(), studio_runtime_services::monitor_policy::MonitorError>,
    > {
        Box::pin(async { panic!("no process probe") })
    }
    fn input_reply<'a>(
        &'a self,
        _: &'a studio_runtime_services::monitor_policy::Operation,
        _: &'a str,
    ) -> futures_util::future::BoxFuture<
        'a,
        studio_runtime_services::monitor_runtime_wiring::InputReply,
    > {
        Box::pin(async {
            studio_runtime_services::monitor_runtime_wiring::InputReply::Acknowledged(json!({}))
        })
    }
}

async fn transferred_monitor_fixture(
    behavior: &'static str,
) -> (tempfile::TempDir, axum::Router, Arc<Port>, String) {
    use axum::Extension;
    use studio_runtime_services::monitor_runtime_wiring::MonitorRuntime;
    let (root, app, port) = fixture(behavior).await;
    let mut case = crate::monitor_fixtures::corpus(port.store.root()).remove(0);
    case["actor"] = json!({"id":"owner","rootId":"owner","epoch":1,"status":"idle","accountKey":"target","provider":"codex","threadId":"target-thread","autoWake":true});
    case["before"]["agent"] = json!("owner");
    case["before"]["epoch"] = json!(1);
    case["before"]["interactive"] = json!(true);
    if behavior == "transferred_reattached" {
        case["before"]["reattachedConnectionId"] = json!("reattached-source");
    }
    case["before"]["operation"] =
        json!({"agent":"owner","epoch":1,"accountKey":"default","connectionId":"actual-uuid"});
    crate::monitor_fixtures::seed(&port.store, &case).await;
    let monitor = case["before"].clone();
    let id = monitor["id"].as_str().unwrap().to_owned();
    let receipt_monitor = monitor.clone();
    port.store.transaction(move|tx|{
        tx.execute("DELETE FROM runtime_tasks",[])?;
        let operation=format!("monitor:{}",receipt_monitor["id"].as_str().unwrap());
        let key=format!("retained:default:{}",studio_runtime_store::sha256_hex(operation.as_bytes()));
        let request=json!({"id":key,"generation":7,"method":"command/exec","operation":key,"thread":null,"lightweight":true,"deadline":60000,"remoteId":null});
        let submitted=json!({"call":{"scope":{"account":"default","connection":"actual-uuid","generation":7},"operation":operation,"actor":{"agent":"owner","epoch":1},"transfer":null,"deleted_actor_session":null,"method":"command/exec","params":{"processId":receipt_monitor["id"],"cwd":receipt_monitor["cwd"],"command":["true"]}},"request":request});
        for (phase,value) in [("submitted",submitted),("written",request)] {
            tx.execute("INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'fixture',?2)",(format!("account-worker:{key}:{phase}"),value.to_string()))?;
        }
        Ok(())
    }).await.unwrap();
    let runtime = Arc::new(MonitorRuntime::new(
        port.store.clone(),
        port.clone(),
        Arc::new(tokio::sync::Notify::new()),
    ));
    let app = app.layer(Extension(runtime));
    (root, app, port, id)
}

fn action_payload(mut value: Value) -> Value {
    // The first mutation can include fresh entity updates. The durable action
    // receipt excludes that transport delta and must replay exactly.
    let object = value.as_object_mut().unwrap();
    object.remove("_syncEntities");
    object.remove("_syncEntitiesAfter");
    value
}

#[tokio::test]
async fn transferred_monitor_panel_keeps_source_controls_and_guarded_stop() {
    for behavior in ["transferred", "transferred_reattached"] {
        let (_root, app, port, id) = transferred_monitor_fixture(behavior).await;
        let (status, list) = call(&app, "GET", "/api/processes?agent=owner", Value::Null).await;
        assert_eq!(status, 200, "{list}");
        let process = &list["processes"][0];
        assert_eq!(process["accountKey"], "target");
        assert_eq!(process["capabilities"]["sendInput"], true, "{list}");
        assert_eq!(process["capabilities"]["resize"], true, "{list}");
        assert_eq!(process["capabilities"]["stop"], true, "{list}");
        for input in [
            json!({"id":id,"text":"hello"}),
            json!({"id":id,"rows":30,"cols":100}),
        ] {
            let (status, result) = call(&app, "POST", "/api/monitor/input", input).await;
            assert_eq!(status, 200, "{result}");
        }
        let body = json!({"action":"stop_selected","requestId":"stop-transferred","agent":"owner","accountKey":"target","epoch":1,
        "process":{"key":process["key"],"generation":process["generation"],"agent":"owner","accountKey":"target","epoch":1}});
        let (status, result) = call(&app, "POST", "/api/processes/action", body.clone()).await;
        assert_eq!(status, 200, "{result}");
        assert_eq!(result["processes"][0]["status"], "requested", "{result}");
        let calls = port.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                "command/exec/write",
                "command/exec/resize",
                "command/exec/terminate"
            ]
        );
        assert_eq!(
            action_payload(call(&app, "POST", "/api/processes/action", body).await.1),
            action_payload(result)
        );
        assert_eq!(
            *port.calls.lock().unwrap(),
            calls,
            "the stop receipt never repeats a native effect"
        );
    }
}

#[tokio::test]
async fn transferred_monitor_panel_refuses_unproven_or_changed_source_ownership() {
    for case in [
        "missing_written",
        "wrong_account",
        "wrong_generation",
        "wrong_connection",
        "read_receipt",
        "wrong_epoch",
        "missing_source",
        "vm_owner",
    ] {
        let behavior = match case {
            "wrong_generation" => "transferred_wrong_generation",
            "wrong_connection" => "transferred_wrong_connection",
            "missing_source" => "transferred_missing_source",
            _ => "transferred",
        };
        let (_root, app, port, id) = transferred_monitor_fixture(behavior).await;
        port.store.transaction(move|tx|{
            let key=format!("retained:default:{}",studio_runtime_store::sha256_hex(format!("monitor:{id}").as_bytes()));
            match case {
                "missing_written"=>{tx.execute("DELETE FROM runtime_operation_receipts WHERE id=?1",[format!("account-worker:{key}:written")])?;},
                "wrong_account"=>{tx.execute("UPDATE runtime_operation_receipts SET result=json_set(result,'$.call.scope.account','other') WHERE id=?1",[format!("account-worker:{key}:submitted")])?;},
                "read_receipt"=>{tx.execute("UPDATE runtime_operation_receipts SET result=json_set(result,'$.call.method','thread/read') WHERE id=?1",[format!("account-worker:{key}:submitted")])?;},
                "wrong_epoch"=>{tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.epoch',2) WHERE id='owner'",[])?;},
                "vm_owner"=>{tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.executionMode','vm') WHERE id='owner'",[])?;},
                _=>{}
            }
            Ok(())
        }).await.unwrap();
        let (status, list) = call(&app, "GET", "/api/processes?agent=owner", Value::Null).await;
        assert_eq!(status, 200, "{case}: {list}");
        let process = &list["processes"][0];
        for capability in ["sendInput", "resize", "stop"] {
            assert_eq!(process["capabilities"][capability], false, "{case}: {list}");
        }
        let body = json!({"action":"stop_selected","requestId":"refused-stop","agent":"owner","accountKey":"target","epoch":process["epoch"],
            "process":{"key":process["key"],"generation":process["generation"],"agent":"owner","accountKey":"target","epoch":process["epoch"]}});
        let (status, result) = call(&app, "POST", "/api/processes/action", body).await;
        assert_eq!(status, 200, "{case}: {result}");
        assert_eq!(
            result["processes"][0]["status"], "stale",
            "{case}: {result}"
        );
        assert!(port.calls.lock().unwrap().is_empty(), "{case}");
    }
}

#[tokio::test]
async fn transferred_monitor_panel_authenticates_target_and_keeps_an_unknown_stop() {
    let (_root, app, port, _) = transferred_monitor_fixture("transferred_unknown_stop").await;
    let (_, list) = call(&app, "GET", "/api/processes?agent=owner", Value::Null).await;
    let process = &list["processes"][0];
    let mut body = json!({"action":"stop_selected","requestId":"unknown-stop","agent":"owner","accountKey":"target","epoch":1,
        "process":{"key":process["key"],"generation":process["generation"],"agent":"owner","accountKey":"target","epoch":1}});
    body["accountKey"] = json!("default");
    assert_eq!(
        call(&app, "POST", "/api/processes/action", body.clone())
            .await
            .0,
        400
    );
    assert!(port.calls.lock().unwrap().is_empty());
    body["accountKey"] = json!("target");
    let (status, result) = call(&app, "POST", "/api/processes/action", body.clone()).await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["processes"][0]["status"], "uncertain", "{result}");
    let (status, replayed) = call(&app, "POST", "/api/processes/action", body).await;
    assert_eq!(status, 200, "{replayed}");
    assert_eq!(action_payload(replayed), action_payload(result));
    assert_eq!(*port.calls.lock().unwrap(), vec!["command/exec/terminate"]);
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
