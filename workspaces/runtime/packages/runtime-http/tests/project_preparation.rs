//! Project preparation must survive the follow-up inserted by an async answer.
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_http::{HttpState, router};
use studio_runtime_store::{AgentRecord, AgentRepository, QuestionsWriteRepository, RuntimeStore};
use tower::ServiceExt;
async fn call(app: &axum::Router, path: &str, body: Value, token: &str) -> (u16, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "127.0.0.1:49171")
        .header("content-type", "application/json")
        .header("x-canvas-token", token)
        .header(
            studio_runtime_http::API_SCHEMA_HASH_HEADER,
            studio_runtime_http::schema::api_schema_hash(),
        )
        .extension(ConnectInfo(
            "127.0.0.1:12345"
                .parse::<std::net::SocketAddr>()
                .expect("test fixture"),
        ))
        .body(Body::from(body.to_string()))
        .expect("test fixture");
    let response = app.clone().oneshot(request).await.expect("test fixture");
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("test fixture");
    (
        status,
        serde_json::from_slice(&bytes).expect("test fixture"),
    )
}

#[tokio::test]
async fn async_question_answer_wakes_idle_lead_once() {
    let root = tempfile::tempdir().expect("test fixture");
    let store = Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("test fixture"),
    );
    let agent_id = "lead";
    let question_id = "idle-lead-question";
    let question = json!({
        "id":question_id, "agent":agent_id, "epoch":0, "turnId":"completed-question-turn",
        "method":"agent/asyncQuestion", "status":"pending", "createdAt":10.0,
        "params":{"questions":[{"id":"scope", "question":"Which scope?"}]}
    });
    let cwd = root.path().to_string_lossy().into_owned();
    store
        .transaction(move |tx| {
            AgentRepository::put(
                tx,
                &AgentRecord::from_value(json!({
                    "id":agent_id,"rootId":agent_id,"epoch":0,"status":"idle",
                    "cwd":cwd,"isLead":true,"autoWake":true,
                }))?,
            )?;
            QuestionsWriteRepository::put_request(tx, question_id, &question.to_string())
        })
        .await
        .expect("test fixture");
    let app = router(HttpState::new(
        "token".into(),
        "fixture".into(),
        root.path().to_string_lossy().into(),
        49171,
        None,
        Some(store.clone()),
        None,
    ))
    .expect("test fixture");
    let body = json!({"id":question_id,"answers":{"scope":{"answers":["One file"]}}});
    let (status, value) = call(&app, "/api/answer", body.clone(), "token").await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["status"], "answered");

    let (status, value) = call(&app, "/api/answer", body, "token").await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["replayed"], true);
    let db = store.test_reader().expect("test fixture");
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM runtime_events WHERE id=?1",
            [format!("{question_id}:answer")],
            |row| row.get(0),
        )
        .expect("test fixture");
    assert_eq!(count, 1);
    let agent = AgentRepository::get(&db, &agent_id.into())
        .expect("test fixture")
        .expect("test fixture");
    assert_eq!(agent.status.as_str(), "queued");
    let request: String = db
        .query_row(
            "SELECT record FROM runtime_requests WHERE id=?1",
            [question_id],
            |row| row.get(0),
        )
        .expect("request");
    let request: Value = serde_json::from_str(&request).expect("request JSON");
    assert_eq!(request["status"], "answered");
}
