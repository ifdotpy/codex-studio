#[path = "../support/prepared_python.rs"]
mod python;
pub(super) mod support;

#[tokio::test]
async fn http_accepts_unrelated_operation_started_after_preparation() {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;
    let fixture = support::Fixture::new("http", python::prepared_python);
    let state = crate::router::HttpState::new(
        "token".into(),
        "workspace".into(),
        None,
        "isolated".into(),
        4620,
        None,
        Some(fixture.store.clone()),
        None,
    );
    let app = crate::router::router(state).expect("router");
    let (ready, release) = support::arm();
    let store = fixture.store.clone();
    let operation =
        std::thread::spawn(move || support::start_late_operation(&store, ready, release));
    let request = Request::builder().method("POST").uri("/api/messages")
        .header("x-canvas-token", "token").header("host", "127.0.0.1:4620").header("content-type", "application/json")
        .header(crate::schema::API_SCHEMA_HASH_HEADER, crate::schema::api_schema_hash())
        .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 12345))))
        .body(Body::from(serde_json::json!({"room":"late-operation-recipient", "id":"00000000-0000-4000-8000-000000000001", "text":"hello"}).to_string())).expect("request");
    let response = app.oneshot(request).await.expect("HTTP response");
    operation.join().expect("operation writer joins");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let receipt: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.expect("body"))
            .expect("receipt");
    assert_eq!(receipt["status"], "queued");
    assert!(
        receipt.get("waitingFor").is_none(),
        "unrelated operation is not a blocker"
    );
}

#[tokio::test]
async fn http_accepts_uploaded_asset_id_with_empty_message_text() {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;

    let fixture = support::Fixture::new("pending-asset", python::prepared_python);
    let state = crate::router::HttpState::new(
        "token".into(),
        "workspace".into(),
        None,
        "isolated".into(),
        4620,
        None,
        Some(fixture.store.clone()),
        None,
    );
    let app = crate::router::router(state).expect("router");
    let asset_id = "00000000-0000-4000-8000-000000000003";
    let asset = studio_runtime_services::payload_store::upload(
        &fixture.store,
        "late-operation-recipient",
        "pending.txt",
        Some(asset_id),
        "YWJj",
    )
    .expect("upload through the shared payload owner");
    let request = Request::builder()
        .method("POST")
        .uri("/api/messages")
        .header("x-canvas-token", "token")
        .header("host", "127.0.0.1:4620")
        .header("content-type", "application/json")
        .header(
            crate::schema::API_SCHEMA_HASH_HEADER,
            crate::schema::api_schema_hash(),
        )
        .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            12345,
        ))))
        .body(Body::from(
            serde_json::json!({
                "room":"late-operation-recipient",
                "id":"00000000-0000-4000-8000-000000000002",
                "text":"",
                "assets":[asset["id"].clone()]
            })
            .to_string(),
        ))
        .expect("request");
    let response = app.oneshot(request).await.expect("HTTP response");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let receipt: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.expect("body"))
            .expect("receipt");
    assert_eq!(receipt["status"], "queued");
    let connection = fixture.store.read().expect("message store reader");
    let raw: String = connection
        .query_row(
            "SELECT record FROM runtime_event_meta WHERE id=?1",
            ["00000000-0000-4000-8000-000000000002"],
            |row| row.get(0),
        )
        .expect("saved attachment metadata");
    let metadata: serde_json::Value = serde_json::from_str(&raw).expect("metadata JSON");
    assert_eq!(metadata["assets"], serde_json::json!([asset["id"]]));
}

#[tokio::test]
async fn http_queues_remote_anchor_message_as_paired_event_intent() {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use studio_runtime_store::{AgentId, AgentRepository};
    use tower::ServiceExt;

    let fixture = support::Fixture::new("remote-anchor-message", python::prepared_python);
    fixture
        .store
        .transaction(|tx| {
            let mut agent = AgentRepository::get(
                tx,
                &AgentId::from("late-operation-recipient"),
            )?
            .expect("message recipient");
            agent.extra.insert(
                "remoteAnchor".into(),
                serde_json::json!({"home":"remote-server","link":"remote-link"}),
            );
            AgentRepository::put(tx, &agent)?;
            tx.execute(
                "INSERT INTO runtime_server_links(id,record) VALUES(?1,?2)",
                (
                    "remote-link",
                    serde_json::json!({"id":"remote-link","side":"local","home":"remote-server","parentEpoch":4}).to_string(),
                ),
            )?;
            tx.execute(
                "INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2)",
                (
                    "remote-server",
                    serde_json::json!({"id":"remote-server","clientId":"client","kind":"server","label":"Remote","created":1,"publicKey":"public","status":"paired"}).to_string(),
                ),
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .expect("seed paired remote anchor");
    let state = crate::router::HttpState::new(
        "token".into(),
        "workspace".into(),
        None,
        "isolated".into(),
        4620,
        None,
        Some(fixture.store.clone()),
        None,
    );
    let app = crate::router::router(state).expect("router");
    let request = Request::builder()
        .method("POST")
        .uri("/api/messages")
        .header("x-canvas-token", "token")
        .header("host", "127.0.0.1:4620")
        .header("content-type", "application/json")
        .header(
            crate::schema::API_SCHEMA_HASH_HEADER,
            crate::schema::api_schema_hash(),
        )
        .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            12345,
        ))))
        .body(Body::from(
            serde_json::json!({
                "room":"late-operation-recipient",
                "id":"00000000-0000-4000-8000-000000000004",
                "text":"Continue on the paired server"
            })
            .to_string(),
        ))
        .expect("request");
    let response = app.oneshot(request).await.expect("HTTP response");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let receipt: serde_json::Value = serde_json::from_slice(
        &to_bytes(response.into_body(), 65536)
            .await
            .expect("response body"),
    )
    .expect("message receipt");
    assert_eq!(receipt["status"], "queued");
    let connection = fixture.store.read().expect("outbox reader");
    let (server, raw): (String, String) = connection
        .query_row(
            "SELECT server,body FROM runtime_server_outbox WHERE state='queued'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("paired remote event intent");
    let envelope: serde_json::Value = serde_json::from_str(&raw).expect("outbox envelope");
    assert_eq!(server, "remote-server");
    assert_eq!(envelope["action"], "event");
    assert_eq!(envelope["payload"]["link"], "remote-link");
    assert_eq!(envelope["payload"]["kind"], "user");
    assert_eq!(envelope["payload"]["text"], "Continue on the paired server");
}
