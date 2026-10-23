#[allow(dead_code, unused_imports)]
use crate::monitor_runtime_fixtures as fixtures;

use axum::{
    Extension,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use studio_runtime_http::{HttpState, router};
use studio_runtime_providers::{Method, RequestId, RequestTicket};
use studio_runtime_services::{
    account_session_runtime::{
        AccountSessionFuture, AccountSessionPort, ConnectionScope, ProviderRequest,
        ProviderResponse, RequestCertainty, SessionReceipt,
    },
    agent_lifecycle_runtime::LifecyclePorts,
    lifecycle_effects::ProductionLifecyclePorts,
};
use studio_runtime_store::{
    AgentRecord, AgentRepository, ExecutionHistoryRepository, RuntimeStore,
};
use tokio::sync::Notify;
use tower::ServiceExt;

struct InterruptBroker {
    store: Arc<RuntimeStore>,
    scope: ConnectionScope,
    calls: Mutex<Vec<ProviderRequest>>,
}

impl AccountSessionPort for InterruptBroker {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        (account == self.scope.account).then(|| self.scope.clone())
    }

    fn provider_request(
        &self,
        request: ProviderRequest,
    ) -> AccountSessionFuture<'_, SessionReceipt> {
        assert_eq!(request.expected_scope.as_ref(), Some(&self.scope));
        let reader = self.store.test_reader().expect("committed store read");
        let raw: String = reader
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='agent'",
                [],
                |row| row.get(0),
            )
            .expect("stopped agent exists");
        let agent: Value = serde_json::from_str(&raw).expect("agent record");
        assert_eq!(agent["status"], "paused");
        let interrupt = &agent["startAttempt"]["interrupt"];
        if request.method == Method::TurnInterrupt {
            assert_eq!(interrupt["outcome"], "unknown");
            assert_eq!(interrupt["requestTicket"], request.operation);
        } else {
            // The command cleanup follows the settled interrupt.
            assert_eq!(request.method, Method::ThreadBackgroundTerminalsClean);
            assert_eq!(interrupt["outcome"], "accepted");
            assert_eq!(request.params, json!({"threadId":"fixture-thread"}));
        }
        self.calls.lock().expect("calls lock").push(request.clone());
        let scope = self.scope.clone();
        Box::pin(async move {
            Ok(SessionReceipt {
                scope: scope.clone(),
                operation: request.operation.clone(),
                ticket: Some(RequestTicket {
                    generation: scope.generation,
                    id: RequestId::String(request.operation),
                    method: request.method,
                }),
                certainty: RequestCertainty::Written,
                result: Some(ProviderResponse::Reply(json!({"ok":true}))),
                not_sent_reason: None,
                replayed: false,
            })
        })
    }
}

async fn call(app: &axum::Router, path: &str, body: Value) -> (u16, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "127.0.0.1:49171")
        .header("content-type", "application/json")
        .header("x-canvas-token", "fixture-token")
        .header(
            studio_runtime_http::API_SCHEMA_HASH_HEADER,
            studio_runtime_http::api_schema_hash(),
        )
        .extension(ConnectInfo(
            "127.0.0.1:12345"
                .parse::<std::net::SocketAddr>()
                .expect("address"),
        ))
        .body(Body::from(body.to_string()))
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("HTTP");
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("response");
    (status, serde_json::from_slice(&bytes).expect("JSON"))
}

#[tokio::test]
async fn production_lifecycle_bundle_interrupts_active_turn_through_http_route() {
    let root = fixtures::tempdir();
    let _ = fixtures::corpus(root.path());
    let store = Arc::new(fixtures::store(root.path(), "lifecycle-http").await);
    store
        .transaction(|tx| {
            let agent = AgentRecord::from_value(json!({
                "id":"agent", "rootId":"agent", "epoch":4,
                "status":"running", "isLead":true, "name":"Lead",
                "cwd":"/fixture", "autoWake":true, "inFlight":true,
                "turnId":"active-turn", "threadId":"fixture-thread",
                "accountKey":"fixture-account", "connectionId":"fixture-connection",
                "connectionGeneration":7,
                "startAttempt":{
                    "id":"attempt", "action":"turn", "submitted":true,
                    "epoch":4, "events":["input-event"],
                    "turnId":"active-turn", "threadId":"fixture-thread",
                    "accountKey":"fixture-account", "connectionId":"fixture-connection",
                    "connectionGeneration":7
                }
            }))?;
            AgentRepository::put(tx, &agent)?;
            ExecutionHistoryRepository::record(tx, &agent)
        })
        .await
        .expect("seed active agent");
    let broker = Arc::new(InterruptBroker {
        store: store.clone(),
        scope: ConnectionScope {
            account: "fixture-account".into(),
            connection: "fixture-connection".into(),
            generation: 7,
        },
        calls: Mutex::new(Vec::new()),
    });
    let ports: Arc<dyn LifecyclePorts> = Arc::new(ProductionLifecyclePorts::new(
        store.clone(),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        broker.clone(),
        None,
    ));
    let state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        store.root().to_string_lossy().into(),
        49171,
        None,
        Some(store.clone()),
        None,
    );
    let app = router(state).expect("router").layer(Extension(ports));

    let (status, response) = call(
        &app,
        "/api/stop",
        json!({"id":"agent", "descendants":false}),
    )
    .await;

    let calls = broker.calls.lock().expect("calls lock");
    let state = store.test_reader().expect("diagnostic store read");
    let raw: Option<String> = state
        .query_row(
            "SELECT record FROM runtime_agents WHERE id='agent'",
            [],
            |row| row.get(0),
        )
        .ok();
    assert_eq!(
        status,
        200,
        "response={response}; broker_calls={}; agent={raw:?}",
        calls.len()
    );
    drop(state);
    assert_eq!(response["stopped"], json!(["agent"]));
    assert_eq!(
        calls.iter().map(|call| call.method).collect::<Vec<_>>(),
        [
            Method::TurnInterrupt,
            Method::ThreadBackgroundTerminalsClean
        ]
    );
    for call in calls.iter() {
        assert_eq!(
            call.actor.as_ref().map(|actor| actor.agent.as_str()),
            Some("agent")
        );
        assert_eq!(call.expected_scope.as_ref(), Some(&broker.scope));
    }
    let reader = store.test_reader().expect("final state");
    let raw: String = reader
        .query_row(
            "SELECT record FROM runtime_agents WHERE id='agent'",
            [],
            |row| row.get(0),
        )
        .expect("final agent row");
    let agent: Value = serde_json::from_str(&raw).expect("final agent");
    assert_eq!(agent["startAttempt"]["interrupt"]["outcome"], "accepted");
}

#[tokio::test]
async fn committed_delete_route_succeeds_with_unrelated_failing_cleanup() {
    struct NoBroker;
    impl AccountSessionPort for NoBroker {}
    let root = fixtures::tempdir();
    let _ = fixtures::corpus(root.path());
    let store = Arc::new(fixtures::store(root.path(), "delete-cleanup-failure-http").await);
    let cwd = root.path().to_path_buf();
    store.transaction(move |tx| {
        for record in [
            json!({"id":"agent", "rootId":"agent", "epoch":1, "status":"idle", "isLead":true, "autoWake":false}),
            json!({"id":"a-unrelated", "rootId":"a-unrelated", "epoch":1, "status":"paused", "deletedAt":1,
                "cwd":cwd, "worktreeReady":true, "agentArchive":{"at":1,"epoch":1,"cleanupPending":true},
                "workspaceCleanupIntent":studio_runtime_services::workspace_cleanup_runtime::queued_cleanup_intent("a-unrelated", 1, studio_runtime_services::workspace_cleanup_runtime::CleanupAction::Archive)})
        ] {
            AgentRepository::put(tx, &AgentRecord::from_value(record)?)?;
        }
        Ok(())
    }).await.expect("seed agents");
    let ports: Arc<dyn LifecyclePorts> = Arc::new(ProductionLifecyclePorts::new(
        store.clone(),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(NoBroker),
        None,
    ));
    let state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        store.root().to_string_lossy().into(),
        49171,
        None,
        Some(store.clone()),
        None,
    );
    let app = router(state).expect("router").layer(Extension(ports));
    let (status, response) = call(&app, "/api/conversation/delete", json!({"id":"agent"})).await;
    assert_eq!(status, 200, "committed delete response: {response}");
    assert_eq!(response["deleted"], json!(["agent"]));
    let reader = store.test_reader().expect("reader");
    let tombstone: bool = reader.query_row("SELECT json_type(record,'$.deletedAt') IN ('real','integer') AND json_type(record,'$.workspaceCleanupIntent') IS NULL FROM runtime_agents WHERE id='agent'", [], |row| row.get(0)).expect("deleted agent");
    assert!(tombstone, "delete commits and its own cleanup drains");
    let queued: String = reader.query_row("SELECT json_extract(record,'$.workspaceCleanupIntent.phase') FROM runtime_agents WHERE id='a-unrelated'", [], |row| row.get(0)).expect("unrelated intent");
    assert_eq!(queued, "queued");
}

#[tokio::test]
async fn production_delete_route_interrupts_and_settles_active_turn() {
    let root = fixtures::tempdir();
    let _ = fixtures::corpus(root.path());
    let store = Arc::new(fixtures::store(root.path(), "lifecycle-delete-http").await);
    store
        .transaction(|tx| {
            let agent = AgentRecord::from_value(json!({
                "id":"agent", "rootId":"agent", "epoch":4,
                "status":"running", "isLead":true, "name":"Lead",
                "cwd":"/fixture", "autoWake":true, "inFlight":true,
                "turnId":"active-turn", "threadId":"fixture-thread",
                "accountKey":"fixture-account", "connectionId":"fixture-connection",
                "connectionGeneration":7,
                "startAttempt":{
                    "id":"attempt", "action":"turn", "submitted":true,
                    "epoch":4, "events":["input-event"],
                    "turnId":"active-turn", "threadId":"fixture-thread",
                    "accountKey":"fixture-account", "connectionId":"fixture-connection",
                    "connectionGeneration":7
                }
            }))?;
            AgentRepository::put(tx, &agent)?;
            ExecutionHistoryRepository::record(tx, &agent)
        })
        .await
        .expect("seed active agent");
    let broker = Arc::new(InterruptBroker {
        store: store.clone(),
        scope: ConnectionScope {
            account: "fixture-account".into(),
            connection: "fixture-connection".into(),
            generation: 7,
        },
        calls: Mutex::new(Vec::new()),
    });
    let ports: Arc<dyn LifecyclePorts> = Arc::new(ProductionLifecyclePorts::new(
        store.clone(),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        broker.clone(),
        None,
    ));
    let state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        store.root().to_string_lossy().into(),
        49171,
        None,
        Some(store.clone()),
        None,
    );
    let app = router(state).expect("router").layer(Extension(ports));

    let (status, response) = call(&app, "/api/conversation/delete", json!({"id":"agent"})).await;

    let calls = broker.calls.lock().expect("calls lock");
    let reader = store.test_reader().expect("diagnostic store read");
    let raw: String = reader
        .query_row(
            "SELECT record FROM runtime_agents WHERE id='agent'",
            [],
            |row| row.get(0),
        )
        .expect("tombstoned agent row");
    let agent: Value = serde_json::from_str(&raw).expect("agent record");
    assert_eq!(status, 200, "response={response}; agent={raw}");
    assert_eq!(response["deleted"], json!(["agent"]));
    assert_eq!(
        calls.iter().map(|call| call.method).collect::<Vec<_>>(),
        [
            Method::TurnInterrupt,
            Method::ThreadBackgroundTerminalsClean
        ]
    );
    assert!(agent["deletedAt"].is_number() || agent["deletedAt"] == true);
    assert_eq!(agent["status"], "paused");
    assert_eq!(agent["inFlight"], false);
    assert!(agent["turnId"].is_null());
    assert_eq!(agent["startAttempt"]["interrupt"]["outcome"], "accepted");
}
