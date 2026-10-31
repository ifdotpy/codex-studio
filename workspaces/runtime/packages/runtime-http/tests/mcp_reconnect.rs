//! Reconnect authorization, session identity and durable HTTP replay witnesses.

#![allow(clippy::unwrap_used, clippy::disallowed_methods)]
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use studio_runtime_http::{HttpState, router};
use studio_runtime_services::account_session_runtime::SessionOutcome as ReceiptOutcome;
use studio_runtime_services::{account_policy::AccountPolicyState, account_session_runtime::*};
use studio_runtime_store::RuntimeStore;
use tower::ServiceExt;

struct Session {
    calls: Mutex<Vec<Value>>,
    unknown: bool,
    reconnect_gate: Mutex<Option<(Arc<tokio::sync::Notify>, tokio::sync::oneshot::Receiver<()>)>>,
}
fn scope() -> ConnectionScope {
    ConnectionScope {
        account: "account".into(),
        connection: "connection".into(),
        generation: 1,
    }
}
impl AccountSessionPort for Session {
    fn current_scope(&self, _: &str) -> Option<ConnectionScope> {
        Some(scope())
    }
    fn ensure_session<'a>(&'a self, _: &'a str) -> AccountSessionFuture<'a, ConnectionScope> {
        Box::pin(async { Ok(scope()) })
    }
    fn provider_request<'a>(
        &'a self,
        request: ProviderRequest,
    ) -> AccountSessionFuture<'a, SessionReceipt> {
        Box::pin(async move {
            assert_eq!(request.expected_scope, Some(scope()));
            self.calls.lock().unwrap().push(json!({"method":request.method.as_str(),"params":request.params,"actor":request.actor,"operation":request.operation}));
            let gate = self.reconnect_gate.lock().unwrap().take();
            if let Some((entered, release)) = gate {
                entered.notify_one();
                release.await.unwrap();
            }
            let result = match request.method.as_str() {
                "mcpServer/reconnect" => {
                    assert_eq!(request.actor.unwrap().agent, "agent");
                    assert_eq!(request.params, json!({"threadId":"thread","name":"server"}));
                    json!({})
                }
                "skills/list" => json!({"data":[]}),
                "mcpServerStatus/list" => {
                    if request.params["cursor"] == "next" {
                        json!({"data":[{"name":"second","status":"connected"}],"nextCursor":null})
                    } else {
                        json!({"data":[{"name":"server","status":"failed","error":"offline"}],"nextCursor":"next"})
                    }
                }
                _ => panic!("unexpected method"),
            };
            Ok(SessionReceipt {
                outcome: {
                    let ticket = studio_runtime_providers::RequestTicket {
                        generation: scope().generation,
                        id: studio_runtime_providers::RequestId::String(request.operation.clone()),
                        method: request.method,
                    };
                    match (
                        if self.unknown {
                            RequestCertainty::Unknown
                        } else {
                            RequestCertainty::Written
                        },
                        if self.unknown {
                            None
                        } else {
                            Some(ProviderResponse::Reply(result))
                        },
                    ) {
                        (RequestCertainty::Written, Some(response)) => {
                            ReceiptOutcome::Written { ticket, response }
                        }
                        (RequestCertainty::Written, None) | (RequestCertainty::Unknown, _) => {
                            ReceiptOutcome::Unknown {
                                ticket: studio_runtime_providers::RequestTicket {
                                    generation: scope().generation,
                                    id: studio_runtime_providers::RequestId::String(
                                        request.operation.clone(),
                                    ),
                                    method: request.method,
                                },
                            }
                        }
                        (RequestCertainty::NotSent, _) => ReceiptOutcome::NoLiveAccountWorker,
                    }
                },
                scope: scope(),
                operation: request.operation.clone(),
                replayed: false,
            })
        })
    }
}
async fn fixture(
    provider: &str,
    unknown: bool,
) -> (
    tempfile::TempDir,
    Arc<RuntimeStore>,
    axum::Router,
    Arc<Session>,
) {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    store
        .transaction(|tx| {
            studio_runtime_store::receipt_schema::create(tx)?;
            Ok(())
        })
        .await
        .unwrap();
    let agent = json!({"compactions":0,"id":"agent","rootId":"agent","epoch":2,"cwd":root.path(),"model":"model","role":"orchestrator","provider":provider,"accountKey":"account","threadId":"thread","status":"paused","autoWake":false});
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_agents(id,record) VALUES('agent',?1)",
                [agent.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let registry = json!({"version":studio_runtime_schema::shared_values::ACCOUNT_REGISTRY_FORMAT_VERSION,"defaultAccountKey":"account","accounts":{"account":{"id":"account","provider":provider,"home":"/fixture","label":"Fixture","source":"Fixture","status":"ready"}},"logins":{}}).to_string();
    let policy = AccountPolicyState::from_startup(Some(registry.as_bytes()), []).unwrap();
    let session = Arc::new(Session {
        calls: Mutex::new(vec![]),
        unknown,
        reconnect_gate: Mutex::new(None),
    });
    let mut state = HttpState::new(
        "token".into(),
        "fixture".into(),
        root.path().display().to_string(),
        49171,
        None,
        Some(store.clone()),
        Some(Arc::new(policy)),
    );
    state.account_session = Some(session.clone());
    state = state.with_tool_catalog(studio_runtime_services::thread_prepare::CatalogSnapshot {
        source: studio_runtime_services::thread_prepare::CatalogSource::Current,
        definitions: vec![],
        saved_thread_id: None,
        saved_digest: None,
    });
    (root, store, router(state).unwrap(), session)
}
thread_local! {
    static REQUEST_ID: String = studio_runtime_schema::persistence::RequestIdentity::issue_root();
}
fn body() -> Value {
    let request_id = REQUEST_ID.with(Clone::clone);
    json!({"id":"agent","name":"server","request_id":request_id,"session":{"account":"account","thread":"thread","epoch":2,"connection":"connection","generation":1}})
}
async fn request(app: &axum::Router, path: &str, body: Option<Value>, token: bool) -> (u16, Value) {
    request_from(app, path, body, token, "http://127.0.0.1:49171").await
}
async fn request_from(
    app: &axum::Router,
    path: &str,
    body: Option<Value>,
    token: bool,
    origin: &str,
) -> (u16, Value) {
    let mut request = Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(path)
        .header("host", "127.0.0.1:49171")
        .header("content-type", "application/json")
        .header("origin", origin)
        .header(
            studio_runtime_http::API_SCHEMA_HASH_HEADER,
            studio_runtime_http::schema::api_schema_hash(),
        )
        .extension(ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ));
    if token {
        request = request.header("x-canvas-token", "token");
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(body.map_or(Body::empty(), |body| Body::from(body.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    (
        status,
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap(),
    )
}
#[tokio::test]
async fn mcp_reconnect_requires_user_authority_and_exact_session_identity() {
    let (_root, _store, app, session) = fixture("claude", false).await;
    assert_eq!(
        request(&app, "/api/mcp/reconnect", Some(body()), false)
            .await
            .0,
        403
    );
    assert_eq!(
        request_from(
            &app,
            "/api/mcp/reconnect",
            Some(body()),
            true,
            "https://untrusted.example"
        )
        .await
        .0,
        403
    );
    for (field, value) in [
        ("thread", json!("other")),
        ("epoch", json!(3)),
        ("account", json!("other")),
        ("connection", json!("other")),
        ("generation", json!(2)),
    ] {
        let mut invalid = body();
        invalid["session"][field] = value;
        assert!(
            request(&app, "/api/mcp/reconnect", Some(invalid), true)
                .await
                .0
                >= 400,
            "{field}"
        );
    }
    assert!(session.calls.lock().unwrap().is_empty());
}
#[tokio::test]
async fn mcp_reconnect_replays_lost_success_without_second_provider_call() {
    let (_root, store, app, session) = fixture("claude", false).await;
    let first = request(&app, "/api/mcp/reconnect", Some(body()), true).await;
    assert_eq!(first.0, 200, "{first:?}");
    assert_eq!(first.1["status"], "completed");
    let root = body()["request_id"].as_str().unwrap().to_owned();
    let identity = studio_runtime_schema::persistence::RequestIdentity::new(
        "mcp-reconnect",
        &["agent"],
        &root,
        &["reconnect"],
    )
    .unwrap();
    let db = store.test_reader().unwrap();
    let signature = serde_json::to_value(
        serde_json::from_value::<studio_runtime_http::generated::McpReconnectRequest>(body())
            .unwrap(),
    )
    .unwrap()
    .to_string();
    let saved_signature: String = db
        .query_row(
            "SELECT signature FROM request_results WHERE id=?1",
            [identity.id()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        saved_signature, signature,
        "canonical signature bytes remain exact"
    );
    drop(db);
    assert_eq!(
        request(&app, "/api/mcp/reconnect", Some(body()), true).await,
        first
    );
    assert_eq!(
        request(&app, "/api/mcp/reconnect", Some(body()), false)
            .await
            .0,
        403
    );
    let mut different = body();
    different["name"] = json!("another");
    assert_eq!(
        request(&app, "/api/mcp/reconnect", Some(different), true)
            .await
            .0,
        409
    );
    assert_eq!(session.calls.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn mcp_reconnect_replay_refuses_a_changed_or_deleted_agent() {
    let (_root, store, app, session) = fixture("claude", false).await;
    assert_eq!(
        request(&app, "/api/mcp/reconnect", Some(body()), true)
            .await
            .1["status"],
        "completed"
    );
    store
        .transaction(|tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.epoch',3) WHERE id='agent'",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        request(&app, "/api/mcp/reconnect", Some(body()), true)
            .await
            .0,
        409
    );
    store.transaction(|tx| {
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.epoch',2,'$.deletedAt',1) WHERE id='agent'", [])?;
        Ok(())
    }).await.unwrap();
    assert_eq!(
        request(&app, "/api/mcp/reconnect", Some(body()), true)
            .await
            .0,
        409
    );
    assert_eq!(session.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn mcp_reconnect_validates_the_mutation_contract_before_admission() {
    let (_root, _store, app, session) = fixture("claude", false).await;
    for invalid in [
        {
            let mut value = body();
            value["request_id"] = json!("invalid");
            value
        },
        {
            let mut value = body();
            value["name"] = json!("");
            value
        },
        {
            let mut value = body();
            value["all"] = json!(true);
            value
        },
        {
            let mut value = body();
            value["session"]["thread"] = json!("");
            value
        },
    ] {
        let response = request(&app, "/api/mcp/reconnect", Some(invalid), true).await;
        assert_eq!(response.0, 400, "{response:?}");
        assert_eq!(response.1["error"], "Invalid request");
    }
    assert!(session.calls.lock().unwrap().is_empty());
}
#[tokio::test]
async fn mcp_reconnect_unknown_is_never_resent_and_codex_is_explicitly_unsupported() {
    let (_root, _store, app, session) = fixture("claude", true).await;
    let first = request(&app, "/api/mcp/reconnect", Some(body()), true).await;
    assert_eq!(first.1["status"], "unknown", "{first:?}");
    assert_eq!(
        request(&app, "/api/mcp/reconnect", Some(body()), true).await,
        first
    );
    assert_eq!(session.calls.lock().unwrap().len(), 1);
    let (_root, _store, app, session) = fixture("codex", false).await;
    let response = request(&app, "/api/mcp/reconnect", Some(body()), true).await;
    assert_eq!(response.0, 400);
    assert!(response.1.to_string().contains("not supported for Codex"));
    assert!(session.calls.lock().unwrap().is_empty());
}
#[tokio::test]
async fn mcp_reconnect_missing_account_is_not_classified_as_codex() {
    let (_root, _store, app, session) = fixture("claude", false).await;
    let mut missing = body();
    missing["session"]["account"] = json!("missing");
    let response = request(&app, "/api/mcp/reconnect", Some(missing), true).await;
    assert_eq!(response.0, 400);
    assert_eq!(
        response.1["error"],
        "MCP server reconnect requires a supported account provider."
    );
    assert!(session.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn mcp_inventory_follows_cursor_and_returns_session_binding() {
    let (_root, _store, app, session) = fixture("claude", false).await;
    let first = request(&app, "/api/capabilities?agent=agent", None, true).await;
    assert_eq!(first.0, 200, "{first:?}");
    assert_eq!(first.1["serversCursor"], "next");
    assert_eq!(first.1["mcpSession"], body()["session"]);
    let next = request(
        &app,
        "/api/capabilities?agent=agent&serversCursor=next",
        None,
        true,
    )
    .await;
    assert_eq!(next.0, 200, "{next:?}");
    assert_eq!(next.1["mcp"][0]["name"], "second");
    assert!(next.1["serversCursor"].is_null());
    assert_eq!(
        session
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call["method"] == "mcpServerStatus/list")
            .count(),
        2
    );
}

#[tokio::test]
async fn mcp_reconnect_concurrent_replay_and_crash_receipt_never_resend() {
    let (_root, store, app, session) = fixture("claude", false).await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let (release, held) = tokio::sync::oneshot::channel();
    *session.reconnect_gate.lock().unwrap() = Some((entered.clone(), held));
    let first = request(&app, "/api/mcp/reconnect", Some(body()), true);
    tokio::pin!(first);
    tokio::select! {
        _ = entered.notified() => {},
        result = &mut first => panic!("provider result must remain held: {result:?}"),
    }
    let retry = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        request(&app, "/api/mcp/reconnect", Some(body()), true),
    )
    .await
    .unwrap();
    assert_eq!(retry.0, 200, "{retry:?}");
    assert_eq!(retry.1["status"], "unknown");
    assert_eq!(session.calls.lock().unwrap().len(), 1);
    release.send(()).unwrap();
    let completed = first.await;
    assert_eq!(completed.1["status"], "completed");
    assert_eq!(
        request(&app, "/api/mcp/reconnect", Some(body()), true).await,
        completed
    );
    let mut pending = body();
    let pending_root = studio_runtime_schema::persistence::RequestIdentity::issue_root();
    pending["request_id"] = json!(pending_root);
    let canonical: studio_runtime_http::generated::McpReconnectRequest =
        serde_json::from_value(pending.clone()).unwrap();
    let signature = serde_json::to_value(canonical).unwrap().to_string();
    store.transaction_with_error(move |tx| -> Result<(), studio_runtime_store::StoreError> {
        use studio_runtime_services::event_write::{begin_request, CallerAdmission, RequestKey};
        let request = match begin_request::<studio_runtime_http::generated::McpReconnectResponse>(tx, RequestKey::new("mcp-reconnect", &["agent"], &pending_root).operation(&["reconnect"]), &signature, (studio_runtime_services::event_write::unix_time().unwrap() * 1000.0) as i64)? {
            CallerAdmission::Fresh(request) => request,
            CallerAdmission::Replay(_) => panic!("fresh request"),
        };
        tx.execute("INSERT INTO runtime_outbox(id,kind,owner_kind,owner_id,request_id,signature,status,next_at,payload,created) VALUES(?1,'mcp_reconnect','agent','agent',?2,?3,'unknown',NULL,?4,?5)", rusqlite::params![format!("mcp-reconnect:{pending_root}"),request.id(),signature,json!({"request":request}).to_string(),request.created()])?;
        Ok(())
    }).await.unwrap();
    let replay = request(&app, "/api/mcp/reconnect", Some(pending), true).await;
    assert_eq!(replay.0, 200, "{replay:?}");
    assert_eq!(replay.1["status"], "unknown");
    assert_eq!(session.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn mcp_reconnect_v4_miss_returns_expired_without_provider_send() {
    let (_root, _store, app, session) = fixture("claude", false).await;
    let mut expired = body();
    expired["request_id"] = json!("f43c1885-f4fc-485b-bf99-8b4f31e7e7c4");
    let result = request(&app, "/api/mcp/reconnect", Some(expired), true).await;
    assert_eq!(result.0, 410, "{result:?}");
    assert!(result.1["error"].as_str().unwrap().contains("expired"));
    assert!(session.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn mcp_reconnect_migrated_raw_v4_key_replays_after_authorization() {
    let (_root, store, app, session) = fixture("claude", false).await;
    let mut original = body();
    original["request_id"] = json!("f43c1885-f4fc-485b-bf99-8b4f31e7e7c4");
    let typed: studio_runtime_http::generated::McpReconnectRequest =
        serde_json::from_value(original.clone()).unwrap();
    let signature = serde_json::to_value(typed).unwrap().to_string();
    let reply = json!({"status":"completed","message":"Saved original reconnect reply."});
    let envelope = json!({"actor":"agent","reply":reply,"references":[]}).to_string();
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO request_results(id,signature,result,created) VALUES(?1,?2,?3,?4)",
                rusqlite::params![
                    "mcp-reconnect:f43c1885-f4fc-485b-bf99-8b4f31e7e7c4",
                    signature,
                    envelope,
                    (studio_runtime_services::event_write::unix_time().unwrap() * 1000.0) as i64
                ],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        request(&app, "/api/mcp/reconnect", Some(original.clone()), false)
            .await
            .0,
        403
    );
    assert_eq!(
        request(&app, "/api/mcp/reconnect", Some(original.clone()), true).await,
        (200, reply)
    );
    original["name"] = json!("different");
    assert_eq!(
        request(&app, "/api/mcp/reconnect", Some(original), true)
            .await
            .0,
        409
    );
    assert!(session.calls.lock().unwrap().is_empty());
}
