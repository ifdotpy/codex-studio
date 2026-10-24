use crate::monitor_runtime_fixtures as fixtures;
use crate::monitor_runtime_support as support;
use axum::{
    Extension,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_http::{HttpState, router};
use studio_runtime_services::monitor_runtime_wiring::MonitorRuntime;
use studio_runtime_store::{MonitorsRepository, RequestRecord, Table};
use tower::ServiceExt;

fn database_snapshot(store: &studio_runtime_store::RuntimeStore) -> Value {
    use rusqlite::types::ValueRef;

    let connection = store.test_reader().expect("database snapshot connection");
    let schema = connection
        .prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name")
        .expect("database schema")
        .query_map([], |row| {
            Ok(json!([
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ]))
        })
        .expect("schema rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("schema list");
    let tables = connection
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND (name NOT LIKE 'sqlite_%' OR name='sqlite_sequence') ORDER BY name")
        .expect("database tables")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("table names")
        .collect::<Result<Vec<_>, _>>()
        .expect("table list");
    let mut snapshot = serde_json::Map::new();
    snapshot.insert("sqlite_master".into(), Value::Array(schema));
    for table in tables {
        let escaped = table.replace('"', "\"\"");
        let mut statement = connection
            .prepare(&format!("SELECT * FROM \"{escaped}\""))
            .expect("table contents");
        let columns = statement.column_count();
        let mut query = statement.query([]).expect("table query");
        let mut rows = Vec::new();
        while let Some(row) = query.next().expect("table row") {
            rows.push(
                (0..columns)
                    .map(|index| match row.get_ref(index).expect("cell") {
                        ValueRef::Null => Value::Null,
                        ValueRef::Integer(value) => json!(value),
                        ValueRef::Real(value) => json!(value),
                        ValueRef::Text(value) => {
                            json!(String::from_utf8_lossy(value).into_owned())
                        }
                        ValueRef::Blob(value) => json!(
                            value
                                .iter()
                                .map(|byte| format!("{byte:02x}"))
                                .collect::<String>()
                        ),
                    })
                    .collect::<Vec<_>>(),
            );
        }
        rows.sort_by_key(|row| Value::Array(row.clone()).to_string());
        snapshot.insert(
            table,
            Value::Array(rows.into_iter().map(Value::Array).collect()),
        );
    }
    Value::Object(snapshot)
}

async fn call(app: &axum::Router, path: &str, body: Value) -> (u16, Value) {
    call_with_identity(app, "POST", path, body, Some("http-input-fixture")).await
}

async fn call_with_identity(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Value,
    identity: Option<&str>,
) -> (u16, Value) {
    let mut request = Request::builder()
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
            "127.0.0.1:12345"
                .parse::<std::net::SocketAddr>()
                .expect("address"),
        ));
    if let Some(identity) = identity {
        request = request.header("x-studio-request-id", identity);
    }
    let request = request.body(Body::from(body.to_string())).expect("request");
    let response = app.clone().oneshot(request).await.expect("HTTP");
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("response");
    (status, serde_json::from_slice(&bytes).expect("JSON"))
}

#[tokio::test]
async fn installed_runtime_drives_input_and_cancel_through_http() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let mut case = cases
        .iter()
        .find(|case| case["name"] == "finish-0-None-False")
        .expect("case")
        .clone();
    case["before"]["interactive"] = json!(true);
    let store = Arc::new(fixtures::store(root.path(), "http").await);
    fixtures::seed(&store, &case).await;
    let port = Arc::new(support::Port::default());
    let state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        store.root().to_string_lossy().into(),
        49171,
        None,
        Some(store.clone()),
        None,
    );
    let runtime = Arc::new(MonitorRuntime::new(
        store.clone(),
        port.clone(),
        state.event_wake_handle(),
    ));
    let app = router(state).expect("router").layer(Extension(runtime));
    let body = json!({"id":case["before"]["id"],"text":"hello"});
    *port.unknown.lock().expect("unknown") = true;
    let (status, result) = call(&app, "/api/monitor/input", body.clone()).await;
    assert_eq!(
        status, 503,
        "unknown input result must be surfaced: {result}"
    );
    assert_eq!(port.submitted.lock().expect("submitted").len(), 1);
    let after_unknown = database_snapshot(&store);
    let (status, retry) = call(&app, "/api/monitor/input", body).await;
    assert_eq!(
        status, 503,
        "unknown input retry must stay refused: {retry}"
    );
    assert_eq!(
        port.submitted.lock().expect("submitted").len(),
        1,
        "a repeated HTTP request must not resend an Unknown native input"
    );
    assert_eq!(
        database_snapshot(&store),
        after_unknown,
        "refusing a repeat must leave the durable Unknown latch unchanged"
    );
    let agent = case["actor"]["id"].as_str().expect("actor id");
    let epoch = case["actor"]["epoch"].as_i64().expect("actor epoch");
    let path = format!(
        "/api/processes/action?requestId=http-input-fixture&agent={agent}&accountKey=default&epoch={epoch}"
    );
    let (status, receipt) = call_with_identity(&app, "GET", &path, Value::Null, None).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["found"], true);
    assert_eq!(receipt["result"]["status"], "uncertain");
    assert_eq!(receipt["result"]["response"]["status"], "unknown");
    *port.unknown.lock().expect("unknown") = false;
    for attempt in 0..2 {
        let (status, response) = call(
            &app,
            "/api/monitor/cancel",
            json!({"id":case["before"]["id"]}),
        )
        .await;
        assert_eq!(status, 200, "{response}");
        assert_eq!(response["cancelRequested"], true);
        if attempt == 0 {
            let entities = response["_syncEntities"].as_array().expect("sync deltas");
            let entity_id = format!(
                "entity:monitor:{}",
                case["before"]["id"].as_str().expect("monitor id")
            );
            assert!(entities.iter().any(|entity| entity["id"] == entity_id));
            assert!(response["_syncEntitiesAfter"].is_number());
        }
    }
    assert_eq!(port.submitted.lock().expect("submitted").len(), 2);
    let saved = MonitorsRepository::get(
        &store.test_reader().expect("read"),
        case["before"]["id"].as_str().expect("id"),
    )
    .expect("monitor")
    .expect("monitor")
    .value()
    .clone();
    assert_eq!(saved["cancelDispatch"]["status"], "written");
    assert_eq!(fixtures::events(&store), json!([]));
}

#[tokio::test]
async fn acknowledged_monitor_input_is_persisted_through_http() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let mut case = cases
        .iter()
        .find(|case| case["name"] == "finish-0-None-False")
        .expect("running monitor case")
        .clone();
    case["before"]["interactive"] = json!(true);
    let store = Arc::new(fixtures::store(root.path(), "http-input-ack").await);
    fixtures::seed(&store, &case).await;
    let port = Arc::new(support::Port::default());
    let state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        store.root().to_string_lossy().into(),
        49171,
        None,
        Some(store.clone()),
        None,
    );
    let runtime = Arc::new(MonitorRuntime::new(
        store.clone(),
        port.clone(),
        state.event_wake_handle(),
    ));
    let app = router(state).expect("router").layer(Extension(runtime));
    let id = case["before"]["id"].as_str().expect("monitor id");
    let (status, response) = call(
        &app,
        "/api/monitor/input",
        json!({"id":id,"text":"fixture input"}),
    )
    .await;

    assert_eq!(status, 200, "acknowledged input should succeed: {response}");
    let entities = response["_syncEntities"].as_array().expect("sync entities");
    assert_eq!(
        entities.len(),
        2,
        "monitor input also publishes owner progress"
    );
    let ids = entities
        .iter()
        .map(|entity| entity["id"].as_str().expect("entity id"))
        .collect::<std::collections::BTreeSet<_>>();
    let owner = case["before"]["agent"].as_str().expect("monitor owner");
    assert_eq!(
        ids,
        [
            format!("entity:monitor:{id}"),
            format!("entity:agent:{owner}")
        ]
        .iter()
        .map(String::as_str)
        .collect()
    );
    assert!(response["_syncEntitiesAfter"].is_number());
    {
        let submitted = port.submitted.lock().expect("submitted effects");
        assert_eq!(
            submitted.len(),
            1,
            "one HTTP input submits one native effect"
        );
        let studio_runtime_services::monitor_policy::Effect::Native {
            identity,
            method,
            params,
        } = &submitted[0]
        else {
            panic!("input must submit a native effect");
        };
        assert!(identity.starts_with(&format!("monitor-input:{id}:")));
        assert_eq!(*method, "command/exec/write");
        assert_eq!(
            params,
            &json!({
                "processId": id,
                "deltaBase64": "Zml4dHVyZSBpbnB1dA==",
                "closeStdin": false
            })
        );
    }

    let saved = MonitorsRepository::get(&store.test_reader().expect("read monitor"), id)
        .expect("read monitor record")
        .expect("monitor record")
        .value()
        .clone();
    assert_eq!(saved["lastInputAttempt"]["status"], "acknowledged");
    assert_eq!(saved["lastInputAttempt"]["method"], "command/exec/write");
    assert_eq!(
        saved["lastInputAttempt"]["params"]["deltaBase64"],
        "Zml4dHVyZSBpbnB1dA=="
    );
    assert!(saved.get("inputAttempt").is_none());

    // Discarding the first successful HTTP response must not repeat its stdin.
    let (status, replay) = call(
        &app,
        "/api/monitor/input",
        json!({"id":id,"text":"fixture input"}),
    )
    .await;
    assert_eq!(status, 200, "{replay}");
    assert_eq!(replay["requestId"], "http-input-fixture");
    assert_eq!(replay["status"], "acknowledged");
    assert_eq!(port.submitted.lock().expect("one input").len(), 1);
    let (status, _) = call(
        &app,
        "/api/monitor/input",
        json!({"id":id,"text":"changed input"}),
    )
    .await;
    assert_eq!(status, 400, "same identity cannot change content");
    let (status, _) = call_with_identity(
        &app,
        "POST",
        "/api/monitor/input",
        json!({"id":id,"text":"no identity"}),
        None,
    )
    .await;
    assert_eq!(status, 400, "old clients must fail before a native write");
    let (status, _) = call_with_identity(
        &app,
        "POST",
        "/api/monitor/input",
        json!({"id":id,"text":"bad identity"}),
        Some("not a safe identity"),
    )
    .await;
    assert_eq!(status, 400, "invalid identities fail before a native write");
    assert_eq!(port.submitted.lock().expect("no second input").len(), 1);

    let epoch = case["actor"]["epoch"].as_i64().expect("actor epoch");
    let account = case["actor"]["accountKey"].as_str().unwrap_or("default");
    for (agent, account, epoch, found) in [
        (owner, account, epoch, true),
        ("foreign-owner", account, epoch, false),
        (owner, "foreign-account", epoch, false),
        (owner, account, epoch + 1, false),
    ] {
        let path = format!(
            "/api/processes/action?requestId=http-input-fixture&agent={agent}&accountKey={account}&epoch={epoch}"
        );
        let (status, receipt) = call_with_identity(&app, "GET", &path, Value::Null, None).await;
        assert_eq!(status, 200, "{receipt}");
        assert_eq!(receipt["found"], found, "{receipt}");
        if found {
            assert_eq!(
                receipt["result"]["response"],
                json!({"kind":"monitor_input","monitorId":id,"status":"acknowledged"})
            );
            assert!(!receipt.to_string().contains("Zml4dHVyZSBpbnB1dA=="));
            assert!(!receipt.to_string().contains("fixture input"));
        }
    }
    let operation =
        serde_json::from_value(case["before"]["operation"].clone()).expect("command operation");
    let finished_id = id.to_owned();
    store
        .transaction_with(move |tx| {
            studio_runtime_services::monitor_runtime::finish_in_transaction(
                tx,
                &finished_id,
                Some(&operation),
                Some(0),
                None,
                studio_runtime_services::event_write::unix_time()?,
            )
        })
        .await
        .expect("command exits immediately after input acknowledgement");
    let path = format!(
        "/api/processes/action?requestId=http-input-fixture&agent={owner}&accountKey={account}&epoch={epoch}"
    );
    let (status, receipt) = call_with_identity(&app, "GET", &path, Value::Null, None).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(
        receipt["found"], true,
        "a process exit does not erase the durable input outcome"
    );
    assert_eq!(receipt["result"]["response"]["status"], "acknowledged");
    assert_eq!(
        port.submitted
            .lock()
            .expect("receipt read has no effect")
            .len(),
        1
    );
}

#[tokio::test]
async fn monitor_approval_routes_commit_decision_then_launch_only_accepts() {
    for decision in ["accept", "decline"] {
        let root = fixtures::tempdir();
        let cases = fixtures::corpus(root.path());
        let mut case = cases
            .iter()
            .find(|case| case["name"] == "finish-0-None-False")
            .expect("running monitor case")
            .clone();
        case["before"]["status"] = json!("approval");
        case["before"]
            .as_object_mut()
            .expect("monitor record")
            .remove("operation");
        let store = Arc::new(fixtures::store(root.path(), decision).await);
        studio_runtime_services::monitor_runtime::recover_on_startup(&store, 999.0)
            .await
            .expect("server startup recovery");
        fixtures::seed(&store, &case).await;
        let id = case["before"]["id"].as_str().expect("monitor id");
        let request_id = format!("monitor-approval:{id}");
        let request = RequestRecord::from_value(json!({
            "id":request_id,
            "method":"monitor/approve",
            "agent":"agent",
            "params":{"monitorId":id,"command":case["before"]["command"],"cwd":case["before"]["cwd"]},
            "status":"pending",
            "createdAt":1000.0
        }))
        .expect("approval request");
        store
            .transaction(move |tx| Table::<RequestRecord>::new(tx).put(&request))
            .await
            .expect("seed monitor approval request");
        let port = Arc::new(support::Port::default());
        let state = HttpState::new(
            "fixture-token".into(),
            "fixture".into(),
            store.root().to_string_lossy().into(),
            49171,
            None,
            Some(store.clone()),
            None,
        );
        let runtime = Arc::new(MonitorRuntime::new(
            store.clone(),
            port.clone(),
            state.event_wake_handle(),
        ));
        let app = router(state)
            .expect("router")
            .layer(Extension(runtime.clone()));
        let (status, response) = call(
            &app,
            "/api/answer",
            json!({"id":request_id,"decision":decision}),
        )
        .await;
        assert_eq!(status, 200, "{response}");
        assert_eq!(response["status"], "answered");
        let request = Table::<RequestRecord>::new(&store.test_reader().expect("read request"))
            .get(&request_id)
            .expect("get approval request")
            .expect("approval request");
        assert_eq!(request.value()["status"], "answered");
        assert_eq!(request.value()["answerDecision"], decision);
        let monitor = MonitorsRepository::get(&store.test_reader().expect("read monitor"), id)
            .expect("get monitor")
            .expect("monitor")
            .value()
            .clone();
        if decision == "accept" {
            assert_eq!(monitor["status"], "starting");
            runtime
                .tick(1001.0)
                .await
                .expect("dispatch committed start intent");
            assert_eq!(port.starts.lock().expect("starts").len(), 1);
            assert_eq!(port.starts.lock().expect("starts")[0].0, id);
        } else {
            assert_eq!(monitor["status"], "cancelled");
            runtime
                .tick(1001.0)
                .await
                .expect("no start dispatch for decline");
            assert!(port.starts.lock().expect("starts").is_empty());
        }
    }
}
