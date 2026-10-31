//! Project preparation must survive the follow-up inserted by an async answer.
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_http::{HttpState, router};
use studio_runtime_schema::approvals::{
    AgentId, Answer, ApprovalId, ApprovalState, Detail, ProviderObject, Question, QuestionAnswer,
    Request as ApprovalRequest, Timestamp, TurnId,
};
use studio_runtime_store::approvals::{self, NewApproval};
use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore, StoreError};
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
    let question = NewApproval {
        id: ApprovalId(question_id.to_owned()),
        agent: AgentId(agent_id.to_owned()),
        turn: Some(TurnId("completed-question-turn".to_owned())),
        epoch: Some(0),
        request: ApprovalRequest::AgentQuestion {
            questions: vec![Question {
                id: "scope".to_owned(),
                question: "Which scope?".to_owned(),
                header: None,
                options: vec![],
                is_secret: false,
                multi_select: None,
                is_other: None,
            }],
        },
        delivery: None,
        detail: Detail {
            params: ProviderObject(Default::default()),
            preview: None,
            answer_error: None,
            host_close_reason: None,
        },
        fire_at: None,
    };
    let cwd = root.path().to_string_lossy().into_owned();
    store
        .transaction(move |tx| {
            AgentRepository::put(
                tx,
                &AgentRecord::from_value(
                    json!({"compactions":0,"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,
                        "id":agent_id,"rootId":agent_id,"epoch":0,"status":"idle",
                        "cwd":cwd,"isLead":true,"autoWake":true,
                    }),
                )?,
            )?;
            approvals::create(tx, question, Timestamp(10.0)).map_err(StoreError::from)?;
            Ok(())
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
    let request = approvals::get(&db, &ApprovalId(question_id.to_owned()))
        .expect("request")
        .expect("request row");
    let ApprovalState::Answered { answer, .. } = request.state else {
        panic!("the question is answered");
    };
    assert_eq!(
        answer,
        Answer::Answers {
            answers: vec![QuestionAnswer {
                id: "scope".to_owned(),
                answer: vec!["One file".to_owned()],
                digest: None,
            }]
        }
    );
}
