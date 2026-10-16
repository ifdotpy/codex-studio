#[path = "../../runtime-services/tests/wave7_p15_support/fixtures.rs"]
mod fixtures;
#[path = "../../runtime-services/tests/support/prepared_python.rs"]
mod prepared_python_support;
#[path = "../../runtime-services/tests/wave7_p15_support/mod.rs"]
mod support;
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

    let connection = store.read().expect("database snapshot connection");
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
    fixtures::seed(&store, &case);
    let port = Arc::new(support::Port::default());
    let state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        None,
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
        status, 400,
        "unknown input result must be surfaced: {result}"
    );
    assert_eq!(port.submitted.lock().expect("submitted").len(), 1);
    let after_unknown = database_snapshot(&store);
    let (status, retry) = call(&app, "/api/monitor/input", body).await;
    assert_eq!(
        status, 400,
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
        &store.read().expect("read"),
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
    fixtures::seed(&store, &case);
    let port = Arc::new(support::Port::default());
    let state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        None,
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
    assert_eq!(entities.len(), 1);
    assert_eq!(entities[0]["id"], format!("entity:monitor:{id}"));
    assert!(response["_syncEntitiesAfter"].is_number());
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
    drop(submitted);

    let saved = MonitorsRepository::get(&store.read().expect("read monitor"), id)
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
        fixtures::seed(&store, &case);
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
            None,
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
        let request = Table::<RequestRecord>::new(&store.read().expect("read request"))
            .get(&request_id)
            .expect("get approval request")
            .expect("approval request");
        assert_eq!(request.value()["status"], "answered");
        assert_eq!(request.value()["answerDecision"], decision);
        let monitor = MonitorsRepository::get(&store.read().expect("read monitor"), id)
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
