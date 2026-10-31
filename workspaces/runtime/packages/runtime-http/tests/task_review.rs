//! Task review reads use the same refusal identity as writes and expose the team default.
#![allow(clippy::unwrap_used)]
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use futures_util::StreamExt;
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
    studio_runtime_services::entity_publication::install(&store)
        .await
        .unwrap();
    store.transaction(|tx| {
        for agent in [
            json!({"compactions":0,"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,"id":"lead","rootId":"lead","parentId":null,"epoch":1,"status":"running","autoWake":true,"isLead":true,"name":"lead"}),
            json!({"compactions":0,"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,"id":"worker","rootId":"lead","parentId":"lead","epoch":1,"status":"running","autoWake":true,"isLead":false,"name":"worker"}),
            json!({"compactions":0,"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,"id":"other","rootId":"other","parentId":null,"epoch":1,"status":"running","autoWake":true,"isLead":true,"name":"other"}),
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
        json!({"action":"set_default_gate","agent":"lead","id":uuid::Uuid::now_v7().to_string(),"gate":default}),
    )
    .await;
    let created = write(&app, json!({"action":"create","agent":"lead","id":uuid::Uuid::now_v7().to_string(),"title":"Review me","owner":"worker","result_schema":{"type":"object","properties":{"count":{"type":"integer"}},"required":["count"]}})).await;
    let task = created["id"].as_str().unwrap();
    write(
        &app,
        json!({"action":"create","agent":"other","id":uuid::Uuid::now_v7().to_string(),"title":"Other team"}),
    )
    .await;
    let (status, listed) = call(&app, "GET", None).await;
    assert_eq!(status, 200, "{listed}");
    assert_eq!(listed["taskDefaultGate"], default);
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    assert_eq!(listed["items"][0]["gate"], default);
    assert_eq!(listed["items"][0]["submissionRefusal"]["failedSubmits"], 0);
    assert_eq!(
        projected(&store, "agentDetail", "lead")["taskDefaultGate"],
        default
    );
    assert_eq!(
        projected(&store, "workDetail", task)["version"],
        created["version"]
    );

    let limit = studio_runtime_schema::task_results::MAX_FAILED_RESULT_SUBMITS_PER_ATTEMPT;
    for index in 0..limit {
        let submit = json!({"action":"submit","agent":"worker","id":uuid::Uuid::now_v7().to_string(),"task_id":task,"result":{"count":"bad"},"checks":"checked","revision":"abc","files":[]});
        let (status, refusal) = call(&app, "POST", Some(submit.clone())).await;
        assert_eq!(status, 400, "{refusal}");
        let (status, _) = call(&app, "POST", Some(submit)).await;
        assert_eq!(status, 400, "the same identity must not count twice");
        let (status, listed) = call(&app, "GET", None).await;
        assert_eq!(status, 200, "{listed}");
        let view = &listed["items"][0];
        assert_eq!(view["version"], created["version"]);
        assert_eq!(view["status"], created["status"]);
        assert_eq!(
            view["submissionRefusal"]["failedSubmits"],
            index + 1,
            "{refusal}"
        );
        assert_eq!(view["submissionRefusal"]["limit"], limit);
        assert_eq!(view["submissionRefusal"]["retryAllowed"], index + 1 < limit);
        assert_eq!(view["submissionRefusal"]["errors"][0]["path"], "/count");
        assert_eq!(listed["items"], listed["tasks"]);
        assert_eq!(
            projected(&store, "workDetail", task)["submissionRefusal"],
            view["submissionRefusal"],
            "refused submissions publish current typed evidence without changing task version"
        );
    }
    let (status, _) = call(&app, "POST", Some(json!({"action":"submit","agent":"worker","id":uuid::Uuid::now_v7().to_string(),"task_id":task,"result":{"count":1},"checks":"checked","revision":"abc","files":[]}))).await;
    assert_eq!(status, 400);
    let (_, capped) = call(&app, "GET", None).await;
    assert_eq!(
        capped["items"][0]["submissionRefusal"]["errors"][0]["path"], "/count",
        "keep the last validation paths after the cap"
    );
    write(&app, json!({"action":"update","agent":"lead","id":uuid::Uuid::now_v7().to_string(),"task_id":task,"version":created["version"],"description":"Try again"})).await;
    let (_, reset) = call(&app, "GET", None).await;
    assert_eq!(reset["items"][0]["submissionRefusal"]["failedSubmits"], 0);
    assert_eq!(reset["items"][0]["submissionRefusal"]["errors"], json!([]));

    let (status, _) = call(&app, "POST", Some(json!({"action":"submit","agent":"worker","id":uuid::Uuid::now_v7().to_string(),"task_id":task,"result":{"count":"bad"},"checks":"checked","revision":"abc","files":[]}))).await;
    assert_eq!(status, 400);
    store
        .transaction(|tx| {
            let mut worker = AgentRepository::get(tx, &"worker".into())?.unwrap();
            worker.epoch += 1;
            AgentRepository::put(tx, &worker)?;
            tx.execute("UPDATE runtime_work SET record=json_set(record,'$.title','New attempt title') WHERE json_extract(record,'$.owner')='worker'", [])?;
            Ok(())
        })
        .await
        .unwrap();
    let (_, new_epoch) = call(&app, "GET", None).await;
    assert_eq!(
        new_epoch["items"][0]["submissionRefusal"]["failedSubmits"],
        0
    );
    assert_eq!(
        projected(&store, "workDetail", task)["submissionRefusal"]["failedSubmits"],
        0
    );
    write(
        &app,
        json!({"action":"set_default_gate","agent":"lead","id":uuid::Uuid::now_v7().to_string(),"gate":null}),
    )
    .await;
    let (_, cleared) = call(&app, "GET", None).await;
    assert!(cleared["taskDefaultGate"].is_null());
    assert!(projected(&store, "agentDetail", "lead")["taskDefaultGate"].is_null());
    assert_eq!(
        cleared["items"][0]["gate"], default,
        "existing task keeps its gate"
    );
}

fn projected(store: &RuntimeStore, collection: &str, id: &str) -> Value {
    store
        .test_read_transaction(|tx| {
            let raw: String = tx.query_row(
                "SELECT payload FROM sync_entities WHERE collection=?1 AND id=?2 AND deleted=0",
                [collection, id],
                |row| row.get(0),
            )?;
            Ok(serde_json::from_str::<Value>(&raw)?["value"].clone())
        })
        .unwrap()
}

async fn archive_fixture(
    owner: Option<&str>,
    vm: bool,
) -> (tempfile::TempDir, Arc<RuntimeStore>, axum::Router) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
    let owner = owner.map(str::to_owned);
    store.transaction(move |tx| {
        for agent in [
            json!({"compactions":0,"accountKey":"default","provider":"codex","id":"lead","rootId":"lead","epoch":1,"status":"running","autoWake":true,"isLead":true,"executionMode":if vm { "vm" } else { "host" }}),
            json!({"compactions":0,"accountKey":"default","provider":"codex","id":"worker","rootId":"lead","parentId":"lead","epoch":2,"status":"failed","autoWake":true,"executionMode":"vm"}),
        ] { AgentRepository::put(tx, &AgentRecord::from_value(agent)?)?; }
        let mut work = json!({"id":"archive-task","rootId":"lead","owner":owner,"status":"review","title":"archive-task","description":"","created":1.0,"updated":1.0,"version":2,"dependencies":[],"results":[{"id":"result-1","agent":"lead","text":"done","checks":"checked","revision":"state","files":[]}],"decisions":[],"createdBy":"lead"});
        if vm { work["vmAccept"] = json!({"status":"completed","requestId":"layr-accept:result-1","taskId":"archive-task","owner":"worker","revision":"state","resultId":"result-1","created":1.0}); }
        studio_runtime_store::WorkItemsRepository::put_work(tx, "archive-task", &work)?;
        if vm { tx.execute_batch("CREATE TRIGGER fail_archive BEFORE UPDATE ON runtime_agents WHEN NEW.id='worker' AND json_type(NEW.record,'$.agentArchive')='object' BEGIN SELECT RAISE(ABORT, 'archive failure'); END;")?; }
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
    (directory, store, app)
}

async fn accept_bytes(app: &axum::Router, id: &str) -> (u16, Vec<u8>) {
    let body = json!({"action":"accept","agent":"lead","id":id,"task_id":"archive-task","version":2,"result":"verified"});
    let request = Request::builder()
        .method("POST")
        .uri("/api/work")
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
    (
        status,
        to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
}

#[tokio::test]
async fn archive_http_skipped_first_reply_matches_replay_bytes() {
    for owner in [Some("lead"), None] {
        let (_directory, _store, app) = archive_fixture(owner, false).await;
        let id = studio_runtime_schema::persistence::RequestIdentity::issue_root();
        let (status, first) = accept_bytes(&app, &id).await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&first));
        let (status, replay) = accept_bytes(&app, &id).await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&replay));
        assert_eq!(first, replay, "owner={owner:?}");
        assert_eq!(
            serde_json::from_slice::<Value>(&first).unwrap()["archive"]["status"],
            "skipped"
        );
    }
}

#[tokio::test]
async fn archive_http_unknown_preserves_admission_and_pending_reply() {
    let (_directory, store, app) = archive_fixture(Some("worker"), true).await;
    let id = studio_runtime_schema::persistence::RequestIdentity::issue_root();
    let (status, first) = accept_bytes(&app, &id).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&first));
    let first: Value = serde_json::from_slice(&first).unwrap();
    assert!(
        first["archive"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.starts_with("The archive check failed:")),
        "{first}"
    );
    assert_eq!(first["archivePending"], true);
    let work = store
        .test_read_transaction(|tx| {
            Ok(
                studio_runtime_store::WorkItemsRepository::work_record(tx, "archive-task")?
                    .unwrap()
                    .into_value(),
            )
        })
        .unwrap();
    assert_eq!(work["archiveIntent"]["status"], "unknown");
    assert!(work["archiveIntent"]["publicAdmission"].is_object());
    let (status, replay) = accept_bytes(&app, &id).await;
    assert_eq!(status, 400, "{}", String::from_utf8_lossy(&replay));
    assert_eq!(
        serde_json::from_slice::<Value>(&replay).unwrap()["error"],
        "The accepted archive outcome is pending"
    );
    let after = store
        .test_read_transaction(|tx| {
            Ok(
                studio_runtime_store::WorkItemsRepository::work_record(tx, "archive-task")?
                    .unwrap()
                    .into_value(),
            )
        })
        .unwrap();
    assert_eq!(
        after, work,
        "retry must not dispatch or change the reservation"
    );
}

#[tokio::test]
async fn legacy_work_is_available_in_the_first_sse_baseline_without_an_entity_pull() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
    store.transaction(|tx| {
        AgentRepository::put(tx,&AgentRecord::from_value(json!({"compactions":0,"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,"id":"lead","rootId":"lead","epoch":1,"status":"completed","isLead":true,"taskDefaultGate":{"command":"upgrade-check","timeout_ms":1234}}))?)?;
        tx.execute("INSERT INTO runtime_work(id,record) VALUES('old-work',?1)",[json!({"id":"old-work","rootId":"lead","title":"Legacy assignment","status":"review","created":1,"updated":1,"version":1,"results":[{"id":"old-result","agent":"lead","text":"Legacy evidence","checks":"Checked","revision":"abc","files":[],"created":1}]}).to_string()])?;
        tx.execute("DELETE FROM sync_entity_meta WHERE key=?1",[studio_runtime_store::WORK_DETAILS_SEED_MARKER])?;
        Ok(())
    }).await.unwrap();
    studio_runtime_services::entity_publication::install(&store)
        .await
        .unwrap();
    let workspace = store
        .test_read_transaction(|tx| studio_runtime_store::SyncDocumentsRepository::workspace_id(tx))
        .unwrap();
    let app = router(HttpState::new(
        "fixture-token".into(),
        workspace,
        store.root().to_string_lossy().into(),
        49171,
        None,
        Some(store),
        None,
    ))
    .unwrap();
    let resources =
        json!([{"kind":"work","agentId":"lead"},{"kind":"agentDetail","agentId":"lead"}])
            .to_string();
    let query = form_urlencoded::Serializer::new(String::from("protocol=4&"))
        .append_pair("resources", &resources)
        .finish();
    let request = Request::builder()
        .uri(format!("/api/sync/stream?{query}"))
        .header("host", "127.0.0.1:49171")
        .header("x-canvas-token", "fixture-token")
        .header(
            studio_runtime_http::API_SCHEMA_HASH_HEADER,
            studio_runtime_http::api_schema_hash(),
        )
        .extension(ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200, "first work subscription is handled");
    let mut stream = response.into_body().into_data_stream();
    let mut received = String::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(chunk) = stream.next().await {
            received.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
            if received.contains("Legacy evidence") && received.contains("upgrade-check") {
                break;
            }
        }
    })
    .await
    .expect("first SSE carries the upgraded work views");
    assert!(received.contains("workDetail"), "{received}");
    assert!(received.contains("workResult"), "{received}");
    assert!(received.contains("Legacy assignment"), "{received}");
}

#[tokio::test]
async fn task_http_description_and_reason_keep_their_transport_bounds() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
    store.transaction(|tx| {
        AgentRepository::put(tx, &AgentRecord::from_value(json!({"compactions":0,"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,"id":"lead","rootId":"lead","parentId":null,"epoch":1,"status":"running","autoWake":true,"isLead":true,"name":"lead"}))?)?;
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
    for (index, token) in ["x", "🙂"].into_iter().enumerate() {
        let (status, created) = call(&app, "POST", Some(json!({"action":"create","agent":"lead","id":studio_runtime_schema::persistence::RequestIdentity::issue_root(),"title":"Task","description":token.repeat(64000)}))).await;
        assert_eq!(status, 200, "{created}");
        let task = &created["id"];
        for action in ["create", "update"] {
            let (status, _) = call(&app, "POST", Some(json!({"action":action,"agent":"lead","id":format!("too-long-{index}-{action}"),"task_id":task,"title":"Task","description":token.repeat(64001)}))).await;
            assert_eq!(status, 400);
        }
        let (status, result) = call(&app, "POST", Some(json!({"action":"update","agent":"lead","id":studio_runtime_schema::persistence::RequestIdentity::issue_root(),"task_id":task,"description":token.repeat(64000)}))).await;
        assert_eq!(status, 200, "{result}");
        let (status, _) = call(&app, "POST", Some(json!({"action":"cancel","agent":"lead","id":format!("reason-long-{index}"),"task_id":task,"reason":token.repeat(12001)}))).await;
        assert_eq!(status, 400);
        let (status, result) = call(&app, "POST", Some(json!({"action":"cancel","agent":"lead","id":studio_runtime_schema::persistence::RequestIdentity::issue_root(),"task_id":task,"reason":token.repeat(12000)}))).await;
        assert_eq!(status, 200, "{result}");
    }
}
