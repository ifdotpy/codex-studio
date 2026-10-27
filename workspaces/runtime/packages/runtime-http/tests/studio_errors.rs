//! The owner's Studio error list reads Studio-caused endings and hides a
//! dismissed row.
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
use studio_runtime_store::{AgentRecord, AgentRepository, MonitorRecord, RuntimeStore, Table};
use tower::ServiceExt;

async fn incident_stream(app: &axum::Router) -> axum::body::BodyDataStream {
    let request = Request::builder()
        .method("GET")
        .uri("/api/sync/stream?protocol=4&resources=%5B%7B%22kind%22%3A%22state%22%7D%5D")
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
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    response.into_body().into_data_stream()
}

async fn streamed_incident(stream: &mut axum::body::BodyDataStream, id: &str, op: &str) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut pending = String::new();
        loop {
            pending.push_str(std::str::from_utf8(&stream.next().await.unwrap().unwrap()).unwrap());
            while let Some(end) = pending.find("\n\n") {
                let frame = pending[..end].to_owned();
                pending.drain(..end + 2);
                for line in frame.lines().filter_map(|line| line.strip_prefix("data: ")) {
                    let event: Value = serde_json::from_str(line).unwrap();
                    for version in event["resourceVersions"].as_array().into_iter().flatten() {
                        for row in version["entityChanges"]["documents"]
                            .as_array()
                            .into_iter()
                            .flatten()
                        {
                            if row["collection"] == "studioError"
                                && row["id"] == id
                                && row["op"] == op
                            {
                                return row.clone();
                            }
                        }
                    }
                }
            }
        }
    })
    .await
    .expect("incident arrives through SSE without a resource GET")
}

async fn call(app: &axum::Router, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    call_with_id(app, method, path, body, None).await
}

async fn call_with_id(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    request_id: Option<&str>,
) -> (u16, Value) {
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
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .unwrap();
    let mut request = request;
    if let Some(id) = request_id {
        request
            .headers_mut()
            .insert("x-studio-request-id", id.parse().unwrap());
    }
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn monitor(
    root: &std::path::Path,
    id: &str,
    status: &str,
    exit: Value,
    error: &str,
    now: f64,
) -> Value {
    json!({"id":id,"agent":"owner","epoch":0,"turnId":null,"requestId":id,
        "command":"cargo test","cwd":root.to_string_lossy(),
        "timeout_ms":60000,"stallTimeoutSeconds":1800,"livenessCommand":"",
        "activityAt":now,"activityGeneration":0,"stallWakeGeneration":-1,"wakeOn":"exit",
        "successExitCodes":[0],"outputPatterns":[],"outputEventSequence":0,
        "outputEventCount":0,"outputEventIds":[],"status":status,"created":now,
        "finished":now,"exitCode":exit,"error":error,"tail":"","bytes":0,"interactive":false,
        "ruleId":null,"log":root.join(format!("{id}.log")).to_string_lossy(),"cancelRequested":false})
}

#[tokio::test]
async fn studio_errors_list_studio_caused_endings_and_hide_a_dismissed_row() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    let now = studio_runtime_services::event_write::unix_time().unwrap();
    let path = root.path().to_path_buf();
    store
        .transaction(move |tx| {
            AgentRepository::put(
                tx,
                &AgentRecord::from_value(json!({"id":"owner","rootId":"owner","epoch":0,
                    "status":"waiting","autoWake":true,"cwd":path.to_string_lossy(),
                    "accountKey":"default","provider":"codex","name":"Fixture worker"}))?,
            )?;
            for record in [
                monitor(
                    &path,
                    "lost-watch",
                    "lost",
                    Value::Null,
                    "Command outcome unknown after losing the provider result; not rerun.",
                    now,
                ),
                monitor(&path, "own-exit", "failed", json!(1), "", now),
                monitor(
                    &path,
                    "stopped",
                    "failed",
                    Value::Null,
                    "The agent turn was stopped",
                    now,
                ),
            ] {
                Table::new(tx).put(&MonitorRecord::from_value(record)?)?;
            }
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

    let (status, body) = call(&app, "GET", "/api/studio-errors", None).await;
    assert_eq!(status, 200, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "only the Studio-caused ending: {body}");
    assert_eq!(items[0]["id"], "monitor:lost-watch");
    assert_eq!(items[0]["kind"], "monitor_outcome_lost");
    assert_eq!(items[0]["action"], "check");
    assert_eq!(items[0]["agentName"], "Fixture worker");

    for _ in 0..2 {
        let (status, body) = call(
            &app,
            "POST",
            "/api/studio-errors/dismiss",
            Some(json!({"id":"monitor:lost-watch"})),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["items"], json!([]), "a dismissed row stays hidden");
    }
    let (_, body) = call(&app, "GET", "/api/studio-errors", None).await;
    assert_eq!(body["items"], json!([]));
}

async fn batch_fixture() -> (tempfile::TempDir, Arc<RuntimeStore>, axum::Router) {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    let path = root.path().to_path_buf();
    let now = studio_runtime_services::event_write::unix_time().unwrap();
    store
        .transaction(move |tx| {
            AgentRepository::put(
                tx,
                &AgentRecord::from_value(json!({"id":"owner","rootId":"owner","epoch":0,
            "status":"waiting","autoWake":true,"cwd":path.to_string_lossy(),
            "accountKey":"default","provider":"codex","name":"Fixture worker"}))?,
            )?;
            for id in ["one", "two", "three"] {
                Table::new(tx).put(&MonitorRecord::from_value(monitor(
                    &path,
                    id,
                    "lost",
                    Value::Null,
                    "Command outcome unknown after losing the provider result; not rerun.",
                    now,
                ))?)?;
            }
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .unwrap();
    let app = router(HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        store.root().to_string_lossy().into(),
        49171,
        None,
        Some(store.clone()),
        None,
    ))
    .unwrap();
    (root, store, app)
}

#[tokio::test]
async fn a_real_stored_data_failure_becomes_one_safe_server_incident() {
    let (_root, store, app) = batch_fixture().await;
    studio_runtime_services::entity_publication::install(&store)
        .await
        .unwrap();
    let mut stream = incident_stream(&app).await;
    store.transaction(|tx| {
        tx.execute("UPDATE runtime_monitors SET record=json_set(record,'$.log',123,'$.tail','password=secret-canary') WHERE id='one'", [])?;
        Ok(())
    }).await.unwrap();
    for _ in 0..2 {
        let (status, body) = call_with_id(
            &app,
            "GET",
            "/api/monitor/log?id=one",
            None,
            Some("read-broken-monitor"),
        )
        .await;
        assert_eq!(status, 503, "{body}");
        assert!(!body.to_string().contains("secret-canary"));
    }
    let (status, body) = call(&app, "GET", "/api/studio-errors", None).await;
    assert_eq!(status, 200, "{body}");
    let rows: Vec<_> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["kind"] == "http_failure")
        .collect();
    assert_eq!(rows.len(), 1, "one diagnostic for the same request: {body}");
    let row = rows[0];
    assert_eq!(row["action"], "details");
    assert!(row.get("agentId").is_none());
    assert_eq!(
        row["diagnostics"]["context"]["requestId"],
        "read-broken-monitor"
    );
    assert_eq!(row["diagnostics"]["context"]["status"], 503);
    assert_eq!(row["diagnostics"]["causes"][0]["kind"], "stored_record");
    assert_eq!(
        row["diagnostics"]["causes"][0]["object"],
        "runtime_monitors"
    );
    assert_eq!(row["diagnostics"]["causes"][0]["id"], "one");
    assert!(!row.to_string().contains("secret-canary"));
    let id = row["id"].as_str().unwrap().to_owned();
    let streamed = streamed_incident(&mut stream, &id, "upsert").await;
    assert_eq!(
        streamed["value"], *row,
        "HTTP and SSE consume the same typed incident"
    );
    let (status, body) = call(
        &app,
        "POST",
        "/api/studio-errors/dismiss",
        Some(json!({"ids":[id]})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["kind"] != "http_failure")
    );
    streamed_incident(&mut stream, &id, "delete").await;
}

#[tokio::test]
async fn actual_lost_monitor_settlement_preserves_identity_and_unknown_effects() {
    let (root, store, app) = batch_fixture().await;
    studio_runtime_services::entity_publication::install(&store)
        .await
        .unwrap();
    let now = studio_runtime_services::event_write::unix_time().unwrap();
    let mut record = monitor(root.path(), "recovering", "running", Value::Null, "", now);
    record["requestId"] = json!("exact-original-command");
    record["turnId"] = json!("original-turn");
    record["operation"] = json!({"agent":"owner","epoch":0,"accountKey":"default","connectionId":"original-connection"});
    record["reattachRecovery"] =
        json!({"accountKey":"default","epoch":0,"status":"running","startedAt":now});
    record["command"] = json!("password=command-canary");
    store
        .transaction(move |tx| Table::new(tx).put(&MonitorRecord::from_value(record)?))
        .await
        .unwrap();
    let mut stream = incident_stream(&app).await;
    for _ in 0..2 {
        studio_runtime_services::monitor_runtime::settle_reattach(
            &store,
            "recovering",
            "reattach_window_expired",
            now,
        )
        .await
        .unwrap();
    }
    let streamed = streamed_incident(&mut stream, "monitor:recovering", "upsert").await;
    let row = &streamed["value"];
    assert_eq!(row["diagnostics"]["certainty"], "unknown");
    assert_eq!(
        row["diagnostics"]["context"]["requestId"],
        "exact-original-command"
    );
    assert_eq!(row["diagnostics"]["context"]["turnId"], "original-turn");
    assert_eq!(row["action"], "check");
    assert_eq!(row["diagnostics"]["causes"][0]["status"], "lost");
    assert_eq!(
        row["diagnostics"]["causes"][0]["operation"]["connectionId"],
        "original-connection"
    );
    assert_eq!(
        row["diagnostics"]["causes"][0]["recovery"]["proof"]["reason"],
        "reattach_window_expired"
    );
    assert!(!row.to_string().contains("canary"));
    let (_, listed) = call(&app, "GET", "/api/studio-errors", None).await;
    assert_eq!(
        listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["id"] == "monitor:recovering")
            .count(),
        1
    );
    let events: i64 = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM runtime_events WHERE id='monitor:recovering'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        events, 1,
        "repeat settlement queues one result and cannot execute a command"
    );
}

#[tokio::test]
async fn malformed_and_unclassified_source_failures_remain_visible_without_private_text() {
    let (_root, store, app) = batch_fixture().await;
    store.transaction(|tx| {
        tx.execute("UPDATE runtime_monitors SET record=json_set(record,'$.log',123) WHERE id='one'", [])?;
        tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,error) VALUES('unknown-input','owner','user_message','private input','failed',?1,0,'unclassified password=unknown-canary')", [studio_runtime_services::event_write::unix_time().expect("time")])?;
        Ok(())
    }).await.unwrap();
    let (_, body) = call(&app, "GET", "/api/studio-errors", None).await;
    let rows = body["items"].as_array().unwrap();
    let malformed = rows.iter().find(|row| row["id"] == "monitor:one").unwrap();
    assert_eq!(malformed["kind"], "stored_record_invalid");
    assert_eq!(malformed["diagnostics"]["causes"][0]["pointer"], "/log");
    let unknown = rows
        .iter()
        .find(|row| row["id"] == "event:unknown-input")
        .expect("unknown failure must not disappear");
    assert_eq!(unknown["kind"], "unclassified_failure");
    assert_eq!(unknown["diagnostics"]["certainty"], "unknown");
    assert!(!body.to_string().contains("unknown-canary"));
}

#[tokio::test]
async fn incident_storage_failure_keeps_the_original_response_without_recursive_capture() {
    let (_root, store, app) = batch_fixture().await;
    studio_runtime_services::entity_publication::install(&store)
        .await
        .unwrap();
    store.transaction(|tx| {
        tx.execute("UPDATE runtime_monitors SET record=json_set(record,'$.log',123) WHERE id='one'",[])?;
        tx.execute_batch("CREATE TRIGGER refuse_incident BEFORE INSERT ON runtime_operation_receipts WHEN NEW.id LIKE 'studio-errors:incident:%' BEGIN SELECT RAISE(ABORT,'password=sink-canary'); END;")?;
        Ok(())
    }).await.unwrap();
    let (status, body) = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        call_with_id(
            &app,
            "GET",
            "/api/monitor/log?id=one",
            None,
            Some("sink-unavailable"),
        ),
    )
    .await
    .expect("one failed diagnostic write cannot recurse");
    assert_eq!(status, 503, "{body}");
    assert!(!body.to_string().contains("sink-canary"));
    let receipts:i64=store.test_reader().unwrap().query_row("SELECT count(*) FROM runtime_operation_receipts WHERE id LIKE 'studio-errors:incident:%'",[],|row|row.get(0)).unwrap();
    assert_eq!(
        receipts, 0,
        "failed capture neither creates receipts nor retries an operation"
    );
}

#[tokio::test]
async fn a_malformed_source_timestamp_uses_observation_time_and_still_streams() {
    let (root, store, app) = batch_fixture().await;
    studio_runtime_services::entity_publication::install(&store)
        .await
        .unwrap();
    let now = studio_runtime_services::event_write::unix_time().unwrap();
    let record = monitor(root.path(), "bad-clock", "running", Value::Null, "", now);
    store
        .transaction(move |tx| Table::new(tx).put(&MonitorRecord::from_value(record)?))
        .await
        .unwrap();
    let mut stream = incident_stream(&app).await;
    store.transaction(|tx| {tx.execute("UPDATE runtime_monitors SET record=json_set(record,'$.status','lost','$.finished',-1) WHERE id='bad-clock'",[])?;Ok(())}).await.unwrap();
    let (_, body) = call(&app, "GET", "/api/studio-errors", None).await;
    let row = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "monitor:bad-clock")
        .unwrap();
    assert!(
        row["at"].as_f64().unwrap() >= now,
        "diagnostic observation time is independent of invalid source time: {row}"
    );
    assert_eq!(row["diagnostics"]["context"]["source"], "stored");
    assert_eq!(row["diagnostics"]["causes"][0]["pointer"], "/finished");
    let streamed = streamed_incident(&mut stream, "monitor:bad-clock", "upsert").await;
    assert_eq!(streamed["value"]["kind"], "stored_record_invalid");
    assert!(streamed["value"]["at"].as_f64().unwrap() >= now);
    let retained: f64 = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT json_extract(record,'$.finished') FROM runtime_monitors WHERE id='bad-clock'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        retained, -1.0,
        "the original invalid evidence stays with its source"
    );
}

#[tokio::test]
async fn malformed_receipts_agents_and_owners_do_not_poison_startup_or_following_publication() {
    let (_root, store, app) = batch_fixture().await;
    store.transaction(|tx| {
        for (id,status) in [("bad-agent","failed"),("bad-owner","waiting")] {
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",rusqlite::params![id,json!({"id":id,"rootId":id,"epoch":"epoch-private-canary","status":status,"provider":"codex","name":"private-name-canary","error":"private-error-canary"}).to_string()])?;
        }
        tx.execute("UPDATE runtime_monitors SET record=json_set(record,'$.agent','bad-owner') WHERE id='one'",[])?;
        for (id,encoded) in [("http:bad-json","{"),("http:bad-shape","{\"private\":\"receipt-private-canary\"}"),("http:bad-identity","{\"id\":\"wrong-id\",\"kind\":\"http_failure\",\"reason\":\"private\",\"action\":\"details\",\"detail\":\"\",\"subject\":\"\",\"at\":1,\"diagnostics\":{\"context\":{\"source\":\"http\",\"route\":\"GET /api/example\",\"requestId\":null,\"status\":500,\"class\":\"store_data\"},\"certainty\":\"unknown\",\"causes\":[]}}") ] {
            tx.execute("INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'studio-error-incident-v1',?2)",rusqlite::params![format!("studio-errors:incident:{id}"),encoded])?;
        }
        Ok(())
    }).await.unwrap();
    studio_runtime_services::entity_publication::install(&store)
        .await
        .expect("malformed located sources cannot prevent startup publication");
    let mut stream = incident_stream(&app).await;
    store.transaction(|tx| {tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.name','Healthy after malformed sources') WHERE id='owner'",[])?;Ok(())}).await.expect("ordinary source transaction still commits");
    let (_, body) = call(&app, "GET", "/api/studio-errors", None).await;
    let invalid: Vec<_> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["kind"] == "stored_record_invalid")
        .collect();
    assert_eq!(
        invalid.len(),
        5,
        "two invalid owners and three bad receipts remain visible: {body}"
    );
    assert!(
        invalid
            .iter()
            .all(|row| row["action"] == "details" && row.get("agentId").is_none())
    );
    assert!(!body.to_string().contains("private-canary"));
    assert!(!body.to_string().contains("private-name-canary"));
    let streamed =
        streamed_incident(&mut stream, "stored:runtime_agents:bad-agent", "upsert").await;
    assert_eq!(
        streamed["value"]["diagnostics"]["causes"][0]["pointer"],
        "/epoch"
    );
    let raw:String=store.test_reader().unwrap().query_row("SELECT result FROM runtime_operation_receipts WHERE id='studio-errors:incident:http:bad-json'",[],|row|row.get(0)).unwrap();
    assert_eq!(
        raw, "{",
        "diagnostic interpretation never repairs the retained receipt"
    );
}

#[tokio::test]
async fn malformed_failed_monitor_fields_remain_candidates_and_stream_after_a_healthy_write() {
    let (root, store, app) = batch_fixture().await;
    studio_runtime_services::entity_publication::install(&store)
        .await
        .unwrap();
    let now = studio_runtime_services::event_write::unix_time().unwrap();
    let path = root.path().to_path_buf();
    store
        .transaction(move |tx| {
            for id in [
                "bad-exit",
                "object-exit",
                "unknown-status",
                "typed-status",
                "bad-created",
                "own-exit",
            ] {
                Table::new(tx).put(&MonitorRecord::from_value(monitor(
                    &path,
                    id,
                    "running",
                    Value::Null,
                    "",
                    now,
                ))?)?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let mut stream = incident_stream(&app).await;
    store.transaction(|tx| {
        tx.execute("UPDATE runtime_monitors SET record=json_set(record,'$.status','failed','$.exitCode','exit-private-canary') WHERE id='bad-exit'",[])?;
        tx.execute("UPDATE runtime_monitors SET record=json_set(record,'$.status','failed','$.exitCode',json('{\"private\":\"exit-private-canary\"}')) WHERE id='object-exit'",[])?;
        tx.execute("UPDATE runtime_monitors SET record=json_set(record,'$.status','status-private-canary') WHERE id='unknown-status'",[])?;
        tx.execute("UPDATE runtime_monitors SET record=json_set(record,'$.status',42) WHERE id='typed-status'",[])?;
        tx.execute("UPDATE runtime_monitors SET record=json_set(record,'$.status','lost','$.created',-1) WHERE id='bad-created'",[])?;
        tx.execute("UPDATE runtime_monitors SET record=json_set(record,'$.status','failed','$.exitCode',1) WHERE id='own-exit'",[])?;
        Ok(())
    }).await.expect("malformed monitor records do not roll back their source transaction");
    store.transaction(|tx| {
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.name','Healthy following source') WHERE id='owner'",[])?;
        Ok(())
    }).await.expect("following healthy source commits");
    let (_, body) = call(&app, "GET", "/api/studio-errors", None).await;
    for id in [
        "bad-exit",
        "object-exit",
        "unknown-status",
        "typed-status",
        "bad-created",
    ] {
        let row = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == format!("monitor:{id}"))
            .expect("malformed candidate stays visible");
        assert_eq!(row["kind"], "stored_record_invalid");
        assert_eq!(row["action"], "details");
        assert!(row["at"].as_f64().unwrap() >= now);
    }
    assert!(
        !body["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == "monitor:own-exit"),
        "integer command exits stay ordinary"
    );
    assert!(!body.to_string().contains("private-canary"));
    let row = streamed_incident(&mut stream, "monitor:bad-exit", "upsert").await;
    assert_eq!(row["value"]["kind"], "stored_record_invalid");
    assert_eq!(
        row["value"]["diagnostics"]["causes"][0]["pointer"],
        "/exitCode"
    );
    let retained: String = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT json_extract(record,'$.exitCode') FROM runtime_monitors WHERE id='bad-exit'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, "exit-private-canary");
}

#[tokio::test]
async fn a_submitted_provider_rejection_cannot_spoof_not_sent_certainty_with_error_text() {
    let (_root, store, app) = batch_fixture().await;
    studio_runtime_services::entity_publication::install(&store)
        .await
        .unwrap();
    let now = studio_runtime_services::event_write::unix_time().unwrap();
    store.transaction(move|tx| {
        AgentRepository::put(tx,&AgentRecord::from_value(json!({"id":"owner","rootId":"owner","epoch":0,"status":"starting","autoWake":true,"inFlight":true,"turnEpoch":0,"error":null,"provider":"codex","name":"Worker","accountKey":"default","threadId":"thread","startAttempt":{"id":"start","epoch":0,"submitted":true,"activeAtReservation":false,"accountKey":"default","threadId":"thread","events":["submitted-input"],"connectionGeneration":1,"requestTicket":"ticket","created":now}}))?)?;
        tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES('submitted-input','owner','user','Exact input','dispatching',?1,0)",[now])?;
        tx.execute("INSERT INTO runtime_execution_attempts(id,run,record) VALUES('start','run',?1)",[json!({"id":"start","epoch":0,"runId":"run","events":["submitted-input"],"submission":"submitted"}).to_string()])?;
        assert!(studio_runtime_store::SchedulerRepository::settle_start_rejected_provider(tx,"owner",0,"start","ticket","default",1,"thread",json!({"message":studio_runtime_schema::shared_values::PROVIDER_START_NOT_SENT_ERROR}))?);
        Ok(())
    }).await.unwrap();
    let (_, body) = call(&app, "GET", "/api/studio-errors", None).await;
    let incident = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["eventId"] == "submitted-input")
        .expect("provider reached failure stays visible");
    assert_eq!(
        incident["diagnostics"]["certainty"], "unknown",
        "error text cannot prove no effect: {incident}"
    );
    assert_eq!(incident["kind"], "unclassified_failure");
    assert!(
        !incident["reason"]
            .as_str()
            .unwrap()
            .contains("did not send")
    );
    assert_eq!(incident["action"], "check");
}

#[tokio::test]
async fn review_completed_turn_identity_survives_dismissal_and_the_next_real_settlement() {
    let (_root, store, app) = batch_fixture().await;
    studio_runtime_services::entity_publication::install(&store)
        .await
        .unwrap();
    for (attempt, turn) in [
        ("first-attempt", "turn-one"),
        ("second-attempt", "turn-two"),
    ] {
        store.transaction(move |tx| {
            AgentRepository::put(tx, &AgentRecord::from_value(json!({"id":"owner","rootId":"owner","epoch":0,"status":"running","autoWake":true,"inFlight":true,"turnId":turn,"turnEpoch":0,"error":null,"provider":"codex","name":"Worker","accountKey":"default","threadId":"thread","startAttempt":{"id":attempt,"epoch":0,"submitted":true,"turnId":turn,"observedTurnId":turn,"accountKey":"default","threadId":"thread","events":[attempt],"connectionGeneration":1,"requestTicket":attempt}}))?)?;
            tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES(?1,'owner','user','Exact turn input','dispatching',1,0)",[attempt])?;
            let error=json!({"message":"provider-private-canary"});
            assert!(studio_runtime_store::SchedulerRepository::settle_terminal(tx,"owner",0,attempt,turn,studio_runtime_store::AgentStatus::Failed,Some(error.clone()),"failed",Some(&error),&[])?);
            Ok(())
        }).await.unwrap();
        let (_, body) = call(&app, "GET", "/api/studio-errors", None).await;
        let row = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| {
                row["id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("agent:owner:"))
            })
            .expect("each actual terminal failure has its own incident");
        if turn == "turn-one" {
            let (_, dismissed) = call(
                &app,
                "POST",
                "/api/studio-errors/dismiss",
                Some(json!({"ids":[row["id"]]})),
            )
            .await;
            assert!(
                dismissed["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|item| item["id"] != row["id"])
            );
        } else {
            assert_eq!(row["id"], "agent:owner:turn-two");
            assert_eq!(row["diagnostics"]["context"]["turnId"], "turn-two");
            assert!(!body.to_string().contains("provider-private-canary"));
        }
    }
}

#[tokio::test]
async fn review_malformed_queue_event_does_not_poison_startup_or_following_source_sse() {
    let (_root, store, app) = batch_fixture().await;
    let now = studio_runtime_services::event_write::unix_time().unwrap();
    store.transaction(move |tx| {
        tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,error) VALUES('bad-input','owner','user','input-private-canary','failed',?1,'epoch-private-canary','error-private-canary')",[now])?;
        tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,error) VALUES('healthy-input','owner','user','Healthy neighbor','failed',?1,0,'unclassified')",[now])?;
        Ok(())
    }).await.unwrap();
    studio_runtime_services::entity_publication::install(&store)
        .await
        .expect("one malformed queue event cannot refuse startup publication");
    let mut stream = incident_stream(&app).await;
    store.transaction(move|tx| {
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.name','Healthy following event') WHERE id='owner'",[])?;
        tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,error) VALUES('following-input','owner','user','Following neighbor','failed',?1,0,'unclassified')",[now])?;
        tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,error) VALUES('bad-created','owner','user','input-private-canary','failed','created-private-canary',0,'error-private-canary')",[])?;
        Ok(())
    }).await.expect("unrelated source transaction commits with malformed retained event");
    let (_, body) = call(&app, "GET", "/api/studio-errors", None).await;
    for id in ["healthy-input", "following-input"] {
        assert!(
            body["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["id"] == format!("event:{id}")
                    && row["kind"] == "unclassified_failure")
        );
    }
    let row = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "event:bad-input")
        .expect("malformed input remains located");
    assert_eq!(row["kind"], "stored_record_invalid");
    assert_eq!(row["action"], "details");
    assert_eq!(row["diagnostics"]["context"]["object"], "runtime_events");
    assert_eq!(row["diagnostics"]["causes"][0]["pointer"], "/epoch");
    let malformed_time = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "event:bad-created")
        .expect("source-write shortcut uses the same checked event owner");
    assert_eq!(malformed_time["kind"], "stored_record_invalid");
    assert_eq!(
        malformed_time["diagnostics"]["causes"][0]["pointer"],
        "/created"
    );
    assert!(!body.to_string().contains("private-canary"));
    let streamed = streamed_incident(&mut stream, "event:bad-input", "upsert").await;
    assert_eq!(streamed["value"]["kind"], "stored_record_invalid");
    let retained: String = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT epoch FROM runtime_events WHERE id='bad-input'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, "epoch-private-canary");
}

#[tokio::test]
async fn studio_errors_batch_exact_ids_unknown_and_idempotent_repeat() {
    let (_root, _store, app) = batch_fixture().await;
    let request = json!({"ids":["monitor:one", "missing", "monitor:three", "monitor:one"]});
    let (status, body) = call(
        &app,
        "POST",
        "/api/studio-errors/dismiss",
        Some(request.clone()),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["dismissedIds"],
        json!(["monitor:one", "monitor:three"])
    );
    assert_eq!(body["unknownIds"], json!(["missing"]));
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["id"], "monitor:two");
    let (status, repeat) = call(&app, "POST", "/api/studio-errors/dismiss", Some(request)).await;
    assert_eq!(status, 200, "{repeat}");
    assert_eq!(repeat["dismissedIds"], json!([]));
    assert_eq!(
        repeat["unknownIds"],
        json!(["monitor:one", "missing", "monitor:three"])
    );
    assert_eq!(repeat["items"], body["items"]);
}

#[tokio::test]
async fn studio_errors_batch_rejects_oversize_without_dismissing_anything() {
    let (_root, _store, app) = batch_fixture().await;
    let ids = vec![
        "monitor:one";
        studio_runtime_schema::shared_values::STUDIO_ERRORS_DISMISS_MAX_IDS + 1
    ];
    let (status, body) = call(
        &app,
        "POST",
        "/api/studio-errors/dismiss",
        Some(json!({"ids":ids})),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let (_, body) = call(&app, "GET", "/api/studio-errors", None).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn studio_errors_batch_rolls_back_all_receipts_if_one_write_fails() {
    let (_root, store, app) = batch_fixture().await;
    store
        .transaction(|tx| {
            tx.execute_batch(
                "CREATE TRIGGER fail_second_dismissal BEFORE INSERT ON runtime_operation_receipts
            WHEN NEW.id='studio-errors:dismissed:monitor:two'
            BEGIN SELECT RAISE(ABORT, 'fixture dismissal failure'); END;",
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .unwrap();
    let (status, body) = call(
        &app,
        "POST",
        "/api/studio-errors/dismiss",
        Some(json!({"ids":["monitor:one", "monitor:two"]})),
    )
    .await;
    assert_eq!(status, 500, "{body}");
    let (_, body) = call(&app, "GET", "/api/studio-errors", None).await;
    assert_eq!(
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["monitorId"].is_string())
            .count(),
        3,
        "the first dismissal must roll back"
    );
    let failure = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["kind"] == "http_failure")
        .expect("actual database refusal is captured");
    assert_eq!(failure["diagnostics"]["certainty"], "unknown");
    assert_eq!(failure["diagnostics"]["causes"][0]["kind"], "sqlite");
    let dismissals:i64=store.test_reader().unwrap().query_row("SELECT count(*) FROM runtime_operation_receipts WHERE id LIKE 'studio-errors:dismissed:%'",[],|row|row.get(0)).unwrap();
    assert_eq!(
        dismissals, 0,
        "diagnostic capture cannot commit a rolled-back dismissal"
    );
}

#[tokio::test]
async fn startup_models_http_reports_pending_and_refuses_an_unknown_explicit_account() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    let registry = json!({"version":studio_runtime_schema::shared_values::ACCOUNT_REGISTRY_FORMAT_VERSION,"defaultAccountKey":"default","accounts":{},"logins":{}});
    let policy = studio_runtime_services::account_policy::AccountPolicyState::from_startup(
        Some(&serde_json::to_vec(&registry).unwrap()),
        [],
    )
    .unwrap();
    let state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        store.root().to_string_lossy().into(),
        49171,
        None,
        Some(store),
        Some(Arc::new(policy)),
    );
    let app = router(state).unwrap();
    let (status, value) = call(&app, "GET", "/api/models", None).await;
    assert_eq!(
        status, 503,
        "startup is pending, not an invalid request: {value}"
    );
    assert_eq!(value["catalogPending"], true);
    let (status, value) = call(&app, "GET", "/api/models?account_key=unknown", None).await;
    assert_eq!(
        status, 400,
        "explicit unknown accounts remain invalid: {value}"
    );
    assert_ne!(value["catalogPending"], true);
}

#[tokio::test]
async fn studio_errors_sync_pull_carries_upserts_and_bulk_dismissal_tombstones() {
    let (_root, store, app) = batch_fixture().await;
    studio_runtime_services::entity_publication::install(&store)
        .await
        .unwrap();
    let (status, page) = call(
        &app,
        "GET",
        "/api/sync/pull?scope=state:entities:v1&after=0&fresh=1&limit=500",
        None,
    )
    .await;
    assert_eq!(
        status, 200,
        "published errors must cross the HTTP boundary: {page}"
    );
    let rows: Vec<_> = page["documents"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["collection"] == "studioError")
        .collect();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|row| row["op"] == "upsert"));
    let after = page["checkpoint"]["seq"].as_i64().unwrap();
    let (status, result) = call(
        &app,
        "POST",
        "/api/studio-errors/dismiss",
        Some(json!({"ids":["monitor:one", "monitor:three"]})),
    )
    .await;
    assert_eq!(status, 200, "{result}");
    let (status, page) = call(
        &app,
        "GET",
        &format!("/api/sync/pull?scope=state:entities:v1&after={after}&limit=500"),
        None,
    )
    .await;
    assert_eq!(
        status, 200,
        "dismissal tombstones must cross the HTTP boundary: {page}"
    );
    let rows: Vec<_> = page["documents"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["collection"] == "studioError")
        .collect();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row["op"] == "delete"));
}
