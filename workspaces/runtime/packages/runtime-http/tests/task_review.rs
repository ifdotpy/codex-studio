//! Task review reads use the same refusal identity as writes and expose the team default.
#![allow(clippy::unwrap_used)]
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_http::{HttpState, router};
use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore};
use tower::ServiceExt;

async fn call(app: &axum::Router, method: &str, body: Option<Value>) -> (u16, Value) {
    let request = Request::builder()
        .method(method)
        .uri(if method == "GET" {
            "/api/work?agent=worker"
        } else {
            "/api/work"
        })
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
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn write(app: &axum::Router, body: Value) -> Value {
    let (status, result) = call(app, "POST", Some(body)).await;
    assert_eq!(status, 200, "{result}");
    result
}

#[tokio::test]
async fn task_review_projects_default_and_current_refusals_through_http() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
    store.transaction(|tx| {
        for agent in [
            json!({"id":"lead","rootId":"lead","parentId":null,"epoch":1,"status":"running","autoWake":true,"isLead":true,"name":"lead"}),
            json!({"id":"worker","rootId":"lead","parentId":"lead","epoch":1,"status":"running","autoWake":true,"isLead":false,"name":"worker"}),
            json!({"id":"other","rootId":"other","parentId":null,"epoch":1,"status":"running","autoWake":true,"isLead":true,"name":"other"}),
        ] { AgentRepository::put(tx, &AgentRecord::from_value(agent)?)?; }
        Ok(())
    }).await.unwrap();
    let app = router(HttpState::new(
        "fixture-token".into(),
        "lead".into(),
        store.root().to_string_lossy().into(),
        49171,
        None,
        Some(store.clone()),
        None,
    ))
    .unwrap();
    let (status, initial) = call(&app, "GET", None).await;
    assert_eq!(status, 200, "{initial}");
    assert_eq!(initial["rootId"], "lead");
    assert!(initial["taskDefaultGate"].is_null());
    let default = json!({"command":"check","timeout_ms":1234});
    write(
        &app,
        json!({"action":"set_default_gate","agent":"lead","id":"default","gate":default}),
    )
    .await;
    let created = write(&app, json!({"action":"create","agent":"lead","id":"create","title":"Review me","owner":"worker","result_schema":{"type":"object","properties":{"count":{"type":"integer"}},"required":["count"]}})).await;
    let task = created["id"].as_str().unwrap();
    write(
        &app,
        json!({"action":"create","agent":"other","id":"private","title":"Other team"}),
    )
    .await;
    let (status, listed) = call(&app, "GET", None).await;
    assert_eq!(status, 200, "{listed}");
    assert_eq!(listed["taskDefaultGate"], default);
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    assert_eq!(listed["items"][0]["gate"], default);
    assert_eq!(listed["items"][0]["submissionRefusal"]["failedSubmits"], 0);

    let limit = studio_runtime_schema::task_results::MAX_FAILED_RESULT_SUBMITS_PER_ATTEMPT;
    for index in 0..limit {
        let submit = json!({"action":"submit","agent":"worker","id":format!("invalid-{index}"),"task_id":task,"result":{"count":"bad"},"checks":"checked","revision":"abc","files":[]});
        let (status, refusal) = call(&app, "POST", Some(submit.clone())).await;
        assert_eq!(status, 400, "{refusal}");
        let (status, _) = call(&app, "POST", Some(submit)).await;
        assert_eq!(status, 400, "the same identity must not count twice");
        let (status, listed) = call(&app, "GET", None).await;
        assert_eq!(status, 200, "{listed}");
        let view = &listed["items"][0];
        assert_eq!(view["version"], created["version"]);
        assert_eq!(view["status"], created["status"]);
        assert_eq!(view["submissionRefusal"]["failedSubmits"], index + 1);
        assert_eq!(view["submissionRefusal"]["limit"], limit);
        assert_eq!(view["submissionRefusal"]["retryAllowed"], index + 1 < limit);
        assert_eq!(view["submissionRefusal"]["errors"][0]["path"], "/count");
        assert_eq!(listed["items"], listed["tasks"]);
    }
    let (status, _) = call(&app, "POST", Some(json!({"action":"submit","agent":"worker","id":"over-cap","task_id":task,"result":{"count":1},"checks":"checked","revision":"abc","files":[]}))).await;
    assert_eq!(status, 400);
    let (_, capped) = call(&app, "GET", None).await;
    assert_eq!(
        capped["items"][0]["submissionRefusal"]["errors"][0]["path"], "/count",
        "keep the last validation paths after the cap"
    );
    // Existing installations have count-only counter receipts. Their saved refusal is readable.
    store.transaction(|tx| {
        tx.execute("UPDATE runtime_operation_receipts SET result=json_remove(result, '$.errors') WHERE signature='work-result-failures-v1'", [])?;
        Ok(())
    }).await.unwrap();
    let (_, legacy) = call(&app, "GET", None).await;
    assert_eq!(
        legacy["items"][0]["submissionRefusal"]["errors"][0]["path"],
        "/count"
    );
    write(&app, json!({"action":"update","agent":"lead","id":"reset","task_id":task,"version":created["version"],"description":"Try again"})).await;
    let (_, reset) = call(&app, "GET", None).await;
    assert_eq!(reset["items"][0]["submissionRefusal"]["failedSubmits"], 0);
    assert_eq!(reset["items"][0]["submissionRefusal"]["errors"], json!([]));

    let (status, _) = call(&app, "POST", Some(json!({"action":"submit","agent":"worker","id":"after-reset","task_id":task,"result":{"count":"bad"},"checks":"checked","revision":"abc","files":[]}))).await;
    assert_eq!(status, 400);
    store
        .transaction(|tx| {
            let mut worker = AgentRepository::get(tx, &"worker".into())?.unwrap();
            worker.epoch += 1;
            AgentRepository::put(tx, &worker)?;
            Ok(())
        })
        .await
        .unwrap();
    let (_, new_epoch) = call(&app, "GET", None).await;
    assert_eq!(
        new_epoch["items"][0]["submissionRefusal"]["failedSubmits"],
        0
    );
    write(
        &app,
        json!({"action":"set_default_gate","agent":"lead","id":"clear-default","gate":null}),
    )
    .await;
    let (_, cleared) = call(&app, "GET", None).await;
    assert!(cleared["taskDefaultGate"].is_null());
    assert_eq!(
        cleared["items"][0]["gate"], default,
        "existing task keeps its gate"
    );
}
