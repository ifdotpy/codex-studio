//! Native transport fixtures exercise the real HTTP action and receipt boundary.

#![allow(clippy::unwrap_used)]
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use studio_runtime_http::{HttpState, router};
use studio_runtime_services::account_session_runtime::SessionOutcome as ReceiptOutcome;
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
    interrupts: Mutex<usize>,
    pages: Mutex<VecDeque<(Option<&'static str>, Value)>>,
}
impl AccountSessionPort for Port {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        if self.behavior == "claude_held_scope_changed" && !self.calls.lock().unwrap().is_empty() {
            let mut scope = self.scope.clone();
            scope.generation += 1;
            return Some(scope);
        }
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
            let method = request.method.as_str();
            if method != "thread/read" {
                assert_eq!(count, 1, "persist uncertainty before any native effect");
            }
            self.calls.lock().unwrap().push(method.into());
            let mut certainty = RequestCertainty::Written;
            let mut rejected = false;
            let value = match method {
                "thread/read" => {
                    assert_eq!(
                        request.params,
                        json!({"threadId":"thread","includeTurns":false})
                    );
                    if self.behavior == "claude_held_unknown" {
                        certainty = RequestCertainty::Unknown;
                    }
                    if self.behavior == "claude_held_changed" {
                        self.store.transaction_with(|tx| {
                            tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.epoch',2) WHERE id='owner'",[])?;
                            Ok::<_,StoreError>(())
                        }).await.unwrap();
                    }
                    json!({"thread":{"id":if self.behavior == "claude_held_wrong_thread" {"other"} else {"thread"},"status":{"type":if self.behavior == "claude_held_idle" {"idle"} else {"active"}}}})
                }
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
                    if let Some((cursor, page)) = self.pages.lock().unwrap().pop_front() {
                        assert_eq!(request.params["cursor"].as_str(), cursor);
                        return Ok(SessionReceipt {
                            outcome: {
                                let ticket = studio_runtime_providers::RequestTicket {
                                    generation: request
                                        .expected_scope
                                        .as_ref()
                                        .map_or(1, |scope| scope.generation),
                                    id: studio_runtime_providers::RequestId::String(
                                        request.operation.clone(),
                                    ),
                                    method: request.method,
                                };
                                match certainty {
                                    RequestCertainty::Written => ReceiptOutcome::Written {
                                        ticket,
                                        response: ProviderResponse::Reply(page),
                                    },
                                    RequestCertainty::Unknown => ReceiptOutcome::Unknown { ticket },
                                    RequestCertainty::NotSent => {
                                        ReceiptOutcome::NoLiveAccountWorker
                                    }
                                }
                            },
                            scope: self.scope.clone(),
                            operation: request.operation,
                            replayed: false,
                        });
                    }
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
                    if self.behavior == "claude_held_replacement" {
                        // The native adapter rejects the old exact turn before effects.
                        rejected = true;
                    } else {
                        *self.interrupts.lock().unwrap() += 1;
                    }
                    if self.behavior == "claude_held_lost" {
                        certainty = RequestCertainty::Unknown;
                    }
                    json!({})
                }
                _ => panic!("unexpected native method: {method}"),
            };
            Ok(SessionReceipt {
                outcome: {
                    let ticket = studio_runtime_providers::RequestTicket {
                        generation: request
                            .expected_scope
                            .as_ref()
                            .map_or(1, |scope| scope.generation),
                        id: studio_runtime_providers::RequestId::String(request.operation.clone()),
                        method: request.method,
                    };
                    match certainty {
                        RequestCertainty::Written => ReceiptOutcome::Written {
                            ticket,
                            response: if rejected {
                                ProviderResponse::Rejected(json!({"message":"cannot move yet"}))
                            } else {
                                ProviderResponse::Reply(value)
                            },
                        },
                        RequestCertainty::Unknown => ReceiptOutcome::Unknown { ticket },
                        RequestCertainty::NotSent => ReceiptOutcome::NoLiveAccountWorker,
                    }
                },
                scope: if self.behavior == "claude_held_wrong_receipt" {
                    let mut scope = self.scope.clone();
                    scope.generation += 1;
                    scope
                } else {
                    self.scope.clone()
                },
                operation: request.operation,
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
    port.store.transaction(move |tx| {
        use studio_runtime_schema::persistence::{RequestIdentity, OutboxKind, OutboxStatus, OwnerKind};
        use studio_runtime_schema::provider_call_contract::{ProviderCallIntent, RetainedCall, TransportRequest, RequestActor};
        use studio_runtime_services::event_write::{begin_request, CallerAdmission, RequestKey};
        tx.execute("DELETE FROM runtime_tasks", [])?;
        let operation = format!("monitor:{}", receipt_monitor["id"].as_str().unwrap());
        let root = RequestIdentity::issue_root();
        let call = ProviderCallIntent {
            root_request_id: root.clone(), admitted_request: None,
            scope: ConnectionScope { account:"default".into(), connection:"actual-uuid".into(), generation:7 },
            operation: operation.clone(), actor: Some(RequestActor { agent:"owner".into(), epoch:1 }),
            transfer: None, deleted_actor_session: None, method:studio_runtime_providers::Method::CommandExec,
            params:json!({"processId":receipt_monitor["id"],"cwd":receipt_monitor["cwd"],"command":["true"]}),
        };
        let signature = call.fingerprint("default", call.method.as_str());
        let admitted = match begin_request::<()>(tx, RequestKey::new("provider-call", &["default","owner","1"], &root).operation(&[&operation]), &signature, (studio_runtime_services::event_write::unix_time().unwrap()*1000.0) as i64)? {
            CallerAdmission::Fresh(admitted) => admitted,
            CallerAdmission::Replay(_) => panic!("fresh command"),
        };
        let key = studio_runtime_schema::provider_call_contract::transport_key("default", &operation);
        let request = TransportRequest { id:studio_runtime_providers::RequestId::String(key.clone()), generation:7, method:call.method, operation:key.clone(), thread:None,lightweight:true,deadline:60000,unknown:false,remote_id:None };
        let retained = RetainedCall { call, request:Some(request), admitted:admitted.clone(),written:true,outcome:None };
        studio_runtime_store::outbox::Outbox::put(tx,&studio_runtime_store::outbox::OutboxRow {
            id:key,kind:OutboxKind::ProviderCall,owner_kind:OwnerKind::Agent,owner_id:"owner".into(),request_id:Some(admitted.id().into()),signature,
            status:OutboxStatus::Submitted,account:Some("default".into()),stream_id:Some("default".into()),generation:Some(7),ordinal:None,next_at:None,
            payload:json!({"family":"retained","payload":retained}),created:admitted.created(),
        })?;
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
                "missing_written"=>{tx.execute("UPDATE runtime_outbox SET payload=json_set(payload,'$.payload.written',json('false')) WHERE id=?1",[&key])?;},
                "wrong_account"=>{tx.execute("UPDATE runtime_outbox SET payload=json_set(payload,'$.payload.call.scope.account','other') WHERE id=?1",[&key])?;},
                "read_receipt"=>{tx.execute("UPDATE runtime_outbox SET payload=json_set(payload,'$.payload.call.method','thread/read') WHERE id=?1",[&key])?;},
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
            "x-studio-request-id",
            studio_runtime_store::sha256_hex(body.to_string().as_bytes()),
        )
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
        let mut owner=json!({"id":"owner","rootId":"owner","epoch":1,"status":"running",
            "accountKey":"default","provider":if behavior.starts_with("claude") {"claude"} else {"codex"},
            "threadId":"thread","turnId":"turn","inFlight":true,"autoWake":true});
        if behavior.starts_with("claude_held") {
            owner["status"] = json!("interrupted");
            owner["inFlight"] = json!(false);
            owner["autoWake"] = json!(false);
            owner["nativeResponseTurn"] = json!("turn");
            owner["restartRecovery"] = json!({"stage":"held","epoch":1,"accountKey":"default","threadId":"thread","turnId":null});
            owner["disconnectRecovery"] = json!({"source":"restart","epoch":1,"accountKey":"default","threadId":"thread"});
            owner["connectionCheck"] = json!({"nativeState":"active","epoch":1,"accountKey":"default","threadId":"thread","turnId":"turn"});
            if behavior == "claude_held_no_check" {
                owner.as_object_mut().unwrap().remove("connectionCheck");
            }
        }
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
        interrupts: Mutex::new(0),
        pages: Mutex::new(VecDeque::new()),
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
async fn held_claude_response_stop_is_exact_and_replays_without_another_read() {
    for (behavior, expected, effects) in [
        ("claude_held", "requested", 1),
        ("claude_held_no_check", "requested", 1),
        ("claude_held_lost", "uncertain", 1),
        ("claude_held_replacement", "uncertain", 0),
    ] {
        let (_root, app, port) = fixture(behavior).await;
        let before: String = port
            .store
            .test_reader()
            .unwrap()
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='owner'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let (status, list) = call(&app, "GET", "/api/processes?agent=owner", Value::Null).await;
        assert_eq!(status, 200, "{list}");
        assert!(!list["responseTarget"].is_null(), "{behavior}: {list}");
        let body = json!({"action":"stop_response","requestId":"held-stop","agent":"owner","accountKey":"default","epoch":1,"responseTarget":list["responseTarget"]});
        let mut background = body.clone();
        background["action"] = json!("background_command");
        background["requestId"] = json!("held-background");
        background["itemId"] = json!("toolu-1");
        assert_eq!(
            call(&app, "POST", "/api/processes/action", background)
                .await
                .0,
            400
        );
        let (status, result) = call(&app, "POST", "/api/processes/action", body.clone()).await;
        assert_eq!(status, 200, "{result}");
        assert_eq!(result["response"]["status"], expected, "{result}");
        let calls = port.calls.lock().unwrap().clone();
        assert_eq!(calls, ["thread/read", "thread/read", "turn/interrupt"]);
        assert_eq!(*port.interrupts.lock().unwrap(), effects);
        assert_eq!(
            call(&app, "POST", "/api/processes/action", body.clone())
                .await
                .1,
            result
        );
        assert_eq!(*port.calls.lock().unwrap(), calls);
        let mut changed = body;
        changed["responseTarget"]["turnId"] = json!("other");
        assert_eq!(
            call(&app, "POST", "/api/processes/action", changed).await.0,
            400
        );
        assert_eq!(*port.calls.lock().unwrap(), calls);
        let after: String = port
            .store
            .test_reader()
            .unwrap()
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='owner'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            before, after,
            "Stop must not reactivate or settle the actor"
        );
    }
}

#[tokio::test]
async fn held_claude_response_stop_requires_fresh_current_proof() {
    for behavior in [
        "claude_held_idle",
        "claude_held_unknown",
        "claude_held_changed",
        "claude_held_scope_changed",
        "claude_held_wrong_thread",
        "claude_held_wrong_receipt",
    ] {
        let (_root, app, port) = fixture(behavior).await;
        let (status, list) = call(&app, "GET", "/api/processes?agent=owner", Value::Null).await;
        assert_eq!(status, 200, "{list}");
        assert!(list["responseTarget"].is_null(), "{behavior}: {list}");
        assert_eq!(*port.interrupts.lock().unwrap(), 0);
        let (_root, app, port) = fixture(behavior).await;
        let body = json!({"action":"stop_response","requestId":"held-stop","agent":"owner","accountKey":"default","epoch":1,"responseTarget":{"agent":"owner","accountKey":"default","epoch":1,"connectionId":"actual-uuid","threadId":"thread","turnId":"turn"}});
        assert_eq!(
            call(&app, "POST", "/api/processes/action", body).await.0,
            400
        );
        assert_eq!(*port.interrupts.lock().unwrap(), 0);
    }
}

#[tokio::test]
async fn held_claude_response_stop_refuses_foreign_or_ineligible_owners_without_native_reads() {
    for patch in [
        json!({"deleted":true}),
        json!({"accountTransferId":"transfer"}),
        json!({"executionMove":{"id":"move"}}),
        json!({"executionMode":"vm"}),
        json!({"provider":"codex"}),
        json!({"status":"completed"}),
        json!({"autoWake":true}),
        json!({"nativeResponseTurn":"other"}),
        json!({"turnId":""}),
        json!({"threadId":""}),
        json!({"restartRecovery":{"stage":"pending"}}),
        json!({"restartRecovery":{"epoch":2}}),
        json!({"disconnectRecovery":{"accountKey":"other"}}),
        json!({"disconnectRecovery":{"threadId":"other"}}),
        json!({"disconnectRecovery":{"source":"disconnect"}}),
    ] {
        let (_root, app, port) = fixture("claude_held").await;
        let diagnostic = patch.clone();
        port.store
            .transaction_with(move |tx| {
                let raw: String = tx.query_row(
                    "SELECT record FROM runtime_agents WHERE id='owner'",
                    [],
                    |r| r.get(0),
                )?;
                let mut owner: Value = serde_json::from_str(&raw)?;
                for (key, value) in patch.as_object().unwrap() {
                    if let Some(fields) = value.as_object() {
                        for (field, value) in fields {
                            owner[key][field] = value.clone();
                        }
                    } else {
                        owner[key] = value.clone();
                    }
                }
                tx.execute(
                    "UPDATE runtime_agents SET record=? WHERE id='owner'",
                    [owner.to_string()],
                )?;
                Ok::<_, StoreError>(())
            })
            .await
            .unwrap();
        let (status, list) = call(&app, "GET", "/api/processes?agent=owner", Value::Null).await;
        assert_eq!(status, 200, "{diagnostic}: {list}");
        assert!(list["responseTarget"].is_null(), "{diagnostic}: {list}");
        let body = json!({"action":"stop_response","requestId":"held-stop","agent":"owner","accountKey":"default","epoch":1,"responseTarget":{"agent":"owner","accountKey":"default","epoch":1,"connectionId":"actual-uuid","threadId":"thread","turnId":"turn"}});
        assert_eq!(
            call(&app, "POST", "/api/processes/action", body).await.0,
            400,
            "{diagnostic}"
        );
        assert!(port.calls.lock().unwrap().is_empty(), "{diagnostic}");
    }
}
#[tokio::test]
async fn process_stop_pagination_requires_valid_cursors_before_certifying_absence() {
    let terminal = json!({"data":[{"itemId":"command","processId":"42"}]});
    let mut cases = vec![
        (
            "missing",
            vec![(None, json!({"data":[]}))],
            "already_finished",
            "completed",
        ),
        (
            "null",
            vec![(None, json!({"data":[],"nextCursor":null}))],
            "already_finished",
            "completed",
        ),
        (
            "continuation",
            vec![
                (None, json!({"data":[],"nextCursor":"next"})),
                (Some("next"), terminal),
            ],
            "stopped",
            "completed",
        ),
        (
            "repeated",
            vec![
                (None, json!({"data":[],"nextCursor":"next"})),
                (Some("next"), json!({"data":[],"nextCursor":"next"})),
            ],
            "uncertain",
            "uncertain",
        ),
    ];
    for (name, cursor) in [
        ("number", json!(42)),
        ("object", json!({"cursor":"next"})),
        ("empty", json!("")),
        ("bool", json!(false)),
        ("array", json!(["next"])),
    ] {
        cases.push((
            name,
            vec![(None, json!({"data":[],"nextCursor":cursor}))],
            "uncertain",
            "uncertain",
        ));
    }
    for (name, pages, expected, phase) in cases {
        let (_root, app, port) = fixture("success").await;
        let page_count = pages.len();
        port.pages.lock().unwrap().extend(pages);
        let (_, list) = call(&app, "GET", "/api/processes?agent=owner", Value::Null).await;
        let p = &list["processes"][0];
        let body = json!({"action":"stop_selected","requestId":"stop","agent":"owner","accountKey":"default","epoch":1,
            "process":{"key":p["key"],"generation":p["generation"],"agent":"owner","accountKey":"default","epoch":1}});
        let (status, result) = call(&app, "POST", "/api/processes/action", body.clone()).await;
        assert_eq!(status, 200, "{name}: {result}");
        assert_eq!(
            result["processes"][0]["status"], expected,
            "{name}: {result}"
        );
        assert_eq!(result["status"], phase, "{name}: {result}");
        let before = port.calls.lock().unwrap().clone();
        assert_eq!(
            before
                .iter()
                .filter(|m| m.as_str() == "thread/backgroundTerminals/list")
                .count(),
            page_count,
            "{name}: {before:?}"
        );
        assert_eq!(
            before
                .iter()
                .filter(|m| m.as_str() == "thread/backgroundTerminals/terminate")
                .count(),
            usize::from(expected == "stopped"),
            "{name}: {before:?}"
        );
        let (status, replay) = call(&app, "POST", "/api/processes/action", body).await;
        assert_eq!(status, 200, "{name}: {replay}");
        assert_eq!(replay, result, "{name}");
        assert_eq!(*port.calls.lock().unwrap(), before, "{name}");
        let (_, receipt) = call(
            &app,
            "GET",
            "/api/processes/action?requestId=stop&agent=owner&accountKey=default&epoch=1",
            Value::Null,
        )
        .await;
        assert_eq!(receipt, json!({"found":true,"result":result}), "{name}");
    }
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

#[tokio::test]
async fn process_receipt_cannot_certify_a_missing_result() {
    let (_root, app, port) = fixture("claude").await;
    let (_, list) = call(&app, "GET", "/api/processes?agent=owner", Value::Null).await;
    let body = json!({"action":"background_command","requestId":"move","agent":"owner","accountKey":"default","epoch":1,"responseTarget":list["responseTarget"],"itemId":"toolu-1"});
    assert_eq!(
        call(&app, "POST", "/api/processes/action", body.clone())
            .await
            .0,
        200
    );
    port.store.transaction_with(|tx| {
        tx.execute("UPDATE runtime_process_actions SET record=json_set(record,'$.result',null) WHERE id='move'", [])?;
        Ok::<_, StoreError>(())
    }).await.unwrap();
    assert_eq!(
        call(
            &app,
            "GET",
            "/api/processes/action?requestId=move&agent=owner&accountKey=default&epoch=1",
            Value::Null
        )
        .await
        .0,
        500
    );
    assert_eq!(
        call(&app, "POST", "/api/processes/action", body).await.0,
        500
    );
    assert_eq!(port.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn process_command_legacy_nulls_keep_exact_replay_signature() {
    let (_root, app, port) = fixture("claude").await;
    let (_, list) = call(&app, "GET", "/api/processes?agent=owner", Value::Null).await;
    let body = json!({"action":"background_command","requestId":"move","agent":"owner","accountKey":"default","epoch":1,"responseTarget":list["responseTarget"],"itemId":"toolu-1","process":null,"processes":[],"includeDescendants":false});
    let (status, result) = call(&app, "POST", "/api/processes/action", body.clone()).await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(
        call(&app, "POST", "/api/processes/action", body.clone())
            .await
            .1,
        result
    );
    let mut changed = body;
    changed.as_object_mut().unwrap().remove("process");
    assert_eq!(
        call(&app, "POST", "/api/processes/action", changed).await.0,
        400
    );
    assert_eq!(port.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn process_identity_numeric_spelling_is_not_normalized_into_authority() {
    let (_root, app, port) = fixture("success").await;
    let (_, list) = call(&app, "GET", "/api/processes?agent=owner", Value::Null).await;
    let p = &list["processes"][0];
    let body = json!({"action":"stop_selected","requestId":"stop","agent":"owner","accountKey":"default","epoch":1,
        "process":{"key":p["key"],"generation":p["generation"],"agent":"owner","accountKey":"default","epoch":1.0}});
    let (status, result) = call(&app, "POST", "/api/processes/action", body.clone()).await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["status"], "partial");
    assert_eq!(result["processes"][0]["status"], "stale");
    assert!(port.calls.lock().unwrap().is_empty());
    assert_eq!(
        call(&app, "POST", "/api/processes/action", body).await.1,
        result
    );
}

#[tokio::test]
async fn process_receipt_and_replay_preserve_retained_result_payload() {
    for (explicit_null, omit_processes) in [(false, false), (true, false), (false, true)] {
        let (_root, app, port) = fixture("success").await;
        let (_, list) = call(&app, "GET", "/api/processes?agent=owner", Value::Null).await;
        let p = &list["processes"][0];
        let body = json!({"action":"stop_selected","requestId":"stop","agent":"owner","accountKey":"default","epoch":1,
            "process":{"key":p["key"],"generation":p["generation"],"agent":"owner","accountKey":"default","epoch":1}});
        let (status, mut retained) =
            call(&app, "POST", "/api/processes/action", body.clone()).await;
        assert_eq!(status, 200, "{retained}");
        if explicit_null {
            retained["response"] = Value::Null;
            retained["processes"][0]["generation"] = Value::Null;
        } else {
            retained["processes"][0]
                .as_object_mut()
                .unwrap()
                .remove("generation");
        }
        retained["legacyExtension"] = json!({"opaque":null,"version":1});
        retained["processes"][0]["nativeExtension"] = json!([null, "retained"]);
        if omit_processes {
            retained.as_object_mut().unwrap().remove("processes");
        }
        let stored = retained.clone();
        port.store.transaction_with(move |tx| {
            tx.execute("UPDATE runtime_process_actions SET record=json_set(record,'$.result',json(?)) WHERE id='stop'", [stored.to_string()])?;
            Ok::<_, StoreError>(())
        }).await.unwrap();
        let before = port.calls.lock().unwrap().clone();
        let (_, receipt) = call(
            &app,
            "GET",
            "/api/processes/action?requestId=stop&agent=owner&accountKey=default&epoch=1",
            Value::Null,
        )
        .await;
        assert_eq!(receipt, json!({"found":true,"result":retained}));
        let (status, replay) = call(&app, "POST", "/api/processes/action", body).await;
        assert_eq!(status, 200);
        assert_eq!(replay, retained);
        assert_eq!(*port.calls.lock().unwrap(), before);
    }
}
