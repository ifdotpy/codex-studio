//! A manual pause or resume replaces a pause that the rule tick made while
//! the owner could not run, so the tick does not resume the rule by itself.
#![allow(clippy::unwrap_used)]
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_http::{HttpState, router};
use studio_runtime_store::{
    AgentRecord, AgentRepository, RuleRecord, RulesRepository, RuntimeStore,
};
use tower::ServiceExt;

async fn post(app: &axum::Router, body: Value) -> (u16, Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/api/rules")
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
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn owner_held_rule() -> Value {
    json!({"id":"rule","agent":"owner","rootId":"owner","epoch":0,"name":"Watch",
        "kind":"interval","intervalSeconds":240,"nextAt":1_800_000_000.0,"at":1_800_000_000.0,
        "path":null,"event":"worker_completed","command":"check.sh","stallTimeoutSeconds":0,
        "livenessCommand":"","fileActivityAt":1_800_000_000.0,"fileGeneration":0,
        "stallWakeGeneration":-1,"text":"Continue","status":"paused",
        "created":1_800_000_000.0,"inFlight":false,"checks":3,"wakes":0,"fingerprint":null,
        "ownerHold":"failed_input_held",
        "error":"Paused: the agent failed and its input waits, so it cannot act on a wake. The rule resumes when the agent can run again."})
}

#[tokio::test]
async fn a_manual_pause_or_resume_replaces_the_owner_hold_pause() {
    for (action, status) in [("pause", "paused"), ("resume", "active")] {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
        let path = root.path().to_path_buf();
        store
            .transaction(move |tx| {
                AgentRepository::put(
                    tx,
                    &AgentRecord::from_value(json!({"id":"owner","rootId":"owner","epoch":0,
                        "status":"waiting","autoWake":true,"cwd":path.to_string_lossy(),
                        "accountKey":"default","provider":"codex","name":"Fixture lead"}))?,
                )?;
                RulesRepository::put(tx, &RuleRecord::from_value(owner_held_rule())?)?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .await
            .unwrap();
        let state = HttpState::new(
            "fixture-token".into(),
            "fixture".into(),
            store.root().to_string_lossy().into(),
            49171,
            None,
            Some(store.clone()),
            None,
        );
        let app = router(state).unwrap();
        let (code, body) = post(&app, json!({"action":action,"id":"rule","agent":"owner"})).await;
        assert_eq!(code, 200, "{action}: {body}");
        let stored = RulesRepository::get(&store.test_reader().unwrap(), "rule")
            .unwrap()
            .unwrap()
            .value()
            .clone();
        assert_eq!(stored["status"], status, "{action}: {stored}");
        assert!(
            stored.get("ownerHold").is_none_or(Value::is_null),
            "{action}: the owner decided: {stored}"
        );
        assert!(stored["error"].is_null(), "{action}: {stored}");
    }
}
