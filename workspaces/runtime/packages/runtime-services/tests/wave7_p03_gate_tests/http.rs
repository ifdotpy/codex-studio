pub(super) mod support;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_accepts_unrelated_operation_started_after_preparation() {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;
    let fixture = support::Fixture::new("http").await;
    let state = crate::router::HttpState::new(
        "token".into(),
        "workspace".into(),
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
        tokio::spawn(async move { support::start_late_operation(store, ready, release).await });
    let request = Request::builder().method("POST").uri("/api/messages")
        .header("x-canvas-token", "token").header("host", "127.0.0.1:4620").header("content-type", "application/json")
        .header(crate::schema::API_SCHEMA_HASH_HEADER, crate::schema::api_schema_hash())
        .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 12345))))
        .body(Body::from(serde_json::json!({"room":"late-operation-recipient", "id":"00000000-0000-4000-8000-000000000001", "text":"hello"}).to_string())).expect("request");
    let response = app.oneshot(request).await.expect("HTTP response");
    operation.await.expect("operation writer joins");
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

    let fixture = support::Fixture::new("pending-asset").await;
    let state = crate::router::HttpState::new(
        "token".into(),
        "workspace".into(),
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
    .await
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
    let raw = support::read(fixture.store.clone(), |connection| {
        connection
            .query_row(
                "SELECT record FROM runtime_event_meta WHERE id=?1",
                ["00000000-0000-4000-8000-000000000002"],
                |row| row.get::<_, String>(0),
            )
            .expect("saved attachment metadata")
    })
    .await;
    let metadata: serde_json::Value = serde_json::from_str(&raw).expect("metadata JSON");
    assert_eq!(metadata["assets"], serde_json::json!([asset["id"]]));
}

#[tokio::test]
async fn http_queues_remote_anchor_message_as_paired_event_intent() {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use std::sync::Arc;
    use studio_runtime_services::multi_server_auth::{AccessError, refused};
    use studio_runtime_services::multi_server_orchestration::{
        ExchangeOutcome, OrchestrationDriver, OrchestrationEffects, PairedTransport,
        RuntimeOrchestrationEffects,
    };
    use studio_runtime_store::{AgentId, AgentRepository, RuntimeStore};
    use tower::ServiceExt;

    struct NoTransport;
    impl PairedTransport for NoTransport {
        fn exchange<'a>(
            &'a self,
            _: &'a str,
            _: &'a serde_json::Value,
            _: std::time::Duration,
        ) -> futures_util::future::BoxFuture<'a, Result<serde_json::Value, AccessError>> {
            Box::pin(async {
                Err(refused(
                    503,
                    "unused_transport",
                    "The receiver test does not send a second request",
                ))
            })
        }
    }

    struct UnknownEffects;
    impl OrchestrationEffects for UnknownEffects {
        fn supports(&self, action: &str) -> bool {
            action == "input"
        }

        fn execute<'a>(
            &'a self,
            _: &'a str,
            _: &'a serde_json::Value,
        ) -> futures_util::future::BoxFuture<'a, Result<ExchangeOutcome, AccessError>> {
            Box::pin(async {
                Err(refused(
                    503,
                    "unknown_effect",
                    "The test effect leaves its receipt unknown",
                ))
            })
        }
    }

    let fixture = support::Fixture::new("remote-anchor-message").await;
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
                    serde_json::json!({"id":"remote-link","side":"remote","home":"remote-server","parent":"late-operation-recipient","parentEpoch":0,"workers":[]}).to_string(),
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
        .await
        .expect("seed paired remote anchor");
    let state = crate::router::HttpState::new(
        "token".into(),
        "workspace".into(),
        "isolated".into(),
        4620,
        None,
        Some(fixture.store.clone()),
        None,
    );
    let app = crate::router::router(state).expect("router");
    let request = |id: &str, text: &str, assets: &[String]| {
        Request::builder()
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
                    "id":id,
                    "text":text,
                    "assets":assets
                })
                .to_string(),
            ))
            .expect("request")
    };
    let response = app
        .clone()
        .oneshot(request(
            "00000000-0000-4000-8000-000000000004",
            "Continue on the paired server",
            &[],
        ))
        .await
        .expect("HTTP response");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let receipt: serde_json::Value = serde_json::from_slice(
        &to_bytes(response.into_body(), 65536)
            .await
            .expect("response body"),
    )
    .expect("message receipt");
    assert_eq!(receipt["status"], "queued");
    let duplicate = app
        .clone()
        .oneshot(request(
            "00000000-0000-4000-8000-000000000004",
            "Continue on the paired server",
            &[],
        ))
        .await
        .expect("duplicate HTTP response");
    assert_eq!(duplicate.status(), axum::http::StatusCode::OK);
    let duplicate_receipt: serde_json::Value = serde_json::from_slice(
        &to_bytes(duplicate.into_body(), 65536)
            .await
            .expect("duplicate response body"),
    )
    .expect("duplicate message receipt");
    assert_eq!(duplicate_receipt["id"], receipt["id"]);
    assert_eq!(duplicate_receipt["status"], receipt["status"]);
    let outbox_count: i64 = support::read(fixture.store.clone(), |connection| {
        connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_server_outbox WHERE state='queued'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("queued paired intent count")
    })
    .await;
    assert_eq!(outbox_count, 1, "exact retry must not duplicate the intent");
    let receipts_request = Request::builder()
        .method("GET")
        .uri("/api/messages/receipts?agent=late-operation-recipient&ids=%5B%2200000000-0000-4000-8000-000000000004%22%5D")
        .header("x-canvas-token", "token")
        .header("host", "127.0.0.1:4620")
        .header(crate::schema::API_SCHEMA_HASH_HEADER, crate::schema::api_schema_hash())
        .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            12345,
        ))))
        .body(Body::empty())
        .expect("receipts request");
    let receipts_response = app
        .clone()
        .oneshot(receipts_request)
        .await
        .expect("receipts HTTP response");
    assert_eq!(receipts_response.status(), axum::http::StatusCode::OK);
    let receipts: serde_json::Value = serde_json::from_slice(
        &to_bytes(receipts_response.into_body(), 65536)
            .await
            .expect("receipts response body"),
    )
    .expect("message receipts");
    assert_eq!(receipts["items"][0]["id"], receipt["id"]);
    assert_eq!(receipts["items"][0]["status"], "stored_only");
    let (server, raw): (String, String) = support::read(fixture.store.clone(), |connection| {
        connection
            .query_row(
                "SELECT server,body FROM runtime_server_outbox WHERE state='queued'",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .expect("paired remote event intent")
    })
    .await;
    let envelope: serde_json::Value = serde_json::from_str(&raw).expect("outbox envelope");
    assert_eq!(server, "remote-server");
    assert_eq!(envelope["action"], "input");
    assert_eq!(envelope["payload"]["anchor"], true);
    assert_eq!(envelope["payload"]["link"], "remote-link");
    assert_eq!(envelope["payload"]["kind"], "user");
    assert_eq!(envelope["payload"]["text"], "Continue on the paired server");
    let receiver = support::Fixture::new("remote-anchor-home").await;
    receiver
        .store
        .transaction(|tx| {
            tx.execute(
                "INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2)",
                (
                    "source-server",
                    serde_json::json!({"id":"source-server","clientId":"source-client","kind":"server","label":"Source","created":1,"publicKey":"public","status":"paired"}).to_string(),
                ),
            )?;
            tx.execute(
                "INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2)",
                (
                    "attacker-server",
                    serde_json::json!({"id":"attacker-server","clientId":"attacker-client","kind":"server","label":"Attacker","created":1,"publicKey":"public","status":"paired"}).to_string(),
                ),
            )?;
            tx.execute(
                "INSERT INTO runtime_server_links(id,record) VALUES(?1,?2)",
                (
                    "remote-link",
                    serde_json::json!({"id":"remote-link","side":"home","server":"source-server","parent":"late-operation-recipient","parentEpoch":0,"workers":[]}).to_string(),
                ),
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("seed paired home link");
    let receiver_store: Arc<RuntimeStore> = receiver.store.clone();
    let effects = RuntimeOrchestrationEffects::new(receiver_store.clone());
    let driver = OrchestrationDriver::new(
        receiver_store.clone(),
        Arc::new(NoTransport),
        Arc::new(effects),
    );
    let delivered = driver
        .receive("source-server", &envelope, 100)
        .await
        .expect("paired input applies to the home parent");
    assert!(matches!(delivered, ExchangeOutcome::Applied(_)));
    let replay = driver
        .receive("source-server", &envelope, 101)
        .await
        .expect("paired input receipt replays");
    assert_eq!(replay, delivered);
    let remote_event_id = envelope["requestId"]
        .as_str()
        .expect("request identity")
        .to_owned();
    let event_id_for_reset = remote_event_id.clone();
    receiver
        .store
        .transaction(move |tx| {
            tx.execute(
                "UPDATE runtime_server_inbox SET state='running',result=NULL WHERE id=?1",
                [event_id_for_reset.as_str()],
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("simulate response loss after committed input effect");
    let recovered = driver
        .receive("source-server", &envelope, 102)
        .await
        .expect("running input receipt safely replays its exact effect");
    let (ExchangeOutcome::Applied(first_receipt), ExchangeOutcome::Applied(recovered_receipt)) =
        (&delivered, &recovered)
    else {
        panic!("paired input and its recovered retry must both be applied");
    };
    assert_eq!(recovered, delivered, "recovery returns the saved receipt");
    assert_eq!(first_receipt["id"], recovered_receipt["id"]);
    let read_event_id = remote_event_id.clone();
    let (event_count, event_status, inbox_state, inbox_result) = support::read(
        receiver.store.clone(),
        move |connection| {
            let (event_count, event_status): (i64, String) = connection
                .query_row(
            "SELECT COUNT(*),MAX(status) FROM runtime_events WHERE id=?1 AND agent=?2 AND kind='user'",
            (read_event_id.as_str(), "late-operation-recipient"),
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
                .expect("durable home user event");
            let (inbox_state, inbox_result): (String, String) = connection
                .query_row(
                    "SELECT state,result FROM runtime_server_inbox WHERE id=?1",
                    [read_event_id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("recovered inbox receipt");
            (event_count, event_status, inbox_state, inbox_result)
        },
    )
    .await;
    assert_eq!(event_count, 1);
    assert_eq!(event_status, "pending");
    assert_eq!(inbox_state, "complete");
    let inbox_result: serde_json::Value =
        serde_json::from_str(&inbox_result).expect("completed inbox result");
    assert_eq!(inbox_result["requestId"], remote_event_id);
    assert_eq!(inbox_result["outcome"], "applied");
    assert_eq!(inbox_result["value"], first_receipt.clone());

    let mut uncertain_envelope = envelope.clone();
    uncertain_envelope["requestId"] = serde_json::json!("00000000-0000-4000-8000-000000000008");
    let uncertain_driver = OrchestrationDriver::new(
        receiver_store.clone(),
        Arc::new(NoTransport),
        Arc::new(UnknownEffects),
    );
    assert_eq!(
        uncertain_driver
            .receive("source-server", &uncertain_envelope, 103)
            .await
            .expect("first uncertain input"),
        ExchangeOutcome::Unknown
    );
    assert_eq!(
        driver
            .receive("source-server", &uncertain_envelope, 104)
            .await
            .expect("uncertain retry remains unknown"),
        ExchangeOutcome::Unknown
    );
    let uncertain_request_id = uncertain_envelope["requestId"]
        .as_str()
        .expect("request ID")
        .to_owned();
    let uncertain_event_count: i64 = support::read(receiver.store.clone(), move |connection| {
        connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_events WHERE id=?1",
                [uncertain_request_id.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .expect("uncertain retry has no event")
    })
    .await;
    assert_eq!(uncertain_event_count, 0);

    let unicode_text = "🙂".repeat(10_000);
    let unicode_response = app
        .clone()
        .oneshot(request(
            "00000000-0000-4000-8000-000000000007",
            &unicode_text,
            &[],
        ))
        .await
        .expect("Unicode HTTP response");
    assert_eq!(unicode_response.status(), axum::http::StatusCode::OK);
    let unicode_for_query = unicode_text.clone();
    let raw = support::read(fixture.store.clone(), move |connection| {
        connection
            .query_row(
            "SELECT body FROM runtime_server_outbox WHERE server='remote-server' AND json_extract(body,'$.payload.text')=?1",
            [&unicode_for_query],
            |row| row.get::<_, String>(0),
        )
        .expect("queued Unicode paired input")
    })
    .await;
    let unicode_envelope: serde_json::Value =
        serde_json::from_str(&raw).expect("Unicode paired envelope");
    assert_eq!(unicode_envelope["payload"]["text"], unicode_text);
    let unicode_delivery = driver
        .receive("source-server", &unicode_envelope, 104)
        .await
        .expect("10,000 Unicode characters pass paired validation");
    assert!(matches!(unicode_delivery, ExchangeOutcome::Applied(_)));
    let unicode_event_id = unicode_envelope["requestId"]
        .as_str()
        .expect("Unicode input request identity")
        .to_owned();
    let read_unicode_event_id = unicode_event_id.clone();
    let (unicode_count, stored_text) = support::read(receiver.store.clone(), move |connection| {
        connection
            .query_row(
            "SELECT COUNT(*),MAX(text) FROM runtime_events WHERE id=?1 AND agent=?2 AND kind='user'",
            (read_unicode_event_id.as_str(), "late-operation-recipient"),
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
            .expect("durable Unicode home user event")
    })
    .await;
    assert_eq!(unicode_count, 1);
    assert_eq!(stored_text, unicode_text);

    let mut changed_content = envelope.clone();
    changed_content["payload"]["text"] = serde_json::json!("Changed paired content");
    assert_eq!(
        driver
            .receive("source-server", &changed_content, 103)
            .await
            .expect_err("same identity cannot change content")
            .status(),
        409
    );
    let mut followup = envelope.clone();
    followup["requestId"] = serde_json::json!("unsupported-anchor-followup");
    followup["payload"]["kind"] = serde_json::json!("followup");
    let refused_followup = driver
        .receive("source-server", &followup, 105)
        .await
        .expect("model-authored followup is a definite refusal");
    assert!(matches!(refused_followup, ExchangeOutcome::NotApplied(_)));
    let effects = RuntimeOrchestrationEffects::new(receiver_store.clone());
    assert_eq!(
        effects
            .execute("attacker-server", &envelope)
            .await
            .expect_err("wrong paired principal is refused")
            .status(),
        403
    );
    let mut wrong_link = envelope.clone();
    wrong_link["requestId"] = serde_json::json!("different-request-wrong-link");
    wrong_link["payload"]["link"] = serde_json::json!("missing-link");
    assert_eq!(
        effects
            .execute("source-server", &wrong_link)
            .await
            .expect_err("unknown link is refused")
            .status(),
        404
    );
    let mut wrong_epoch = envelope.clone();
    wrong_epoch["requestId"] = serde_json::json!("different-request-wrong-epoch");
    wrong_epoch["payload"]["parentEpoch"] = serde_json::json!(1);
    assert_eq!(
        effects
            .execute("source-server", &wrong_epoch)
            .await
            .expect_err("wrong link epoch is refused")
            .status(),
        403
    );
    let event_id_for_epoch_reset = remote_event_id.clone();
    receiver
        .store
        .transaction(move |tx| {
            tx.execute(
                "UPDATE runtime_server_links SET record=json_set(record,'$.parentEpoch',1) WHERE id='remote-link'",
                [],
            )?;
            tx.execute(
                "UPDATE runtime_server_inbox SET state='running',result=NULL WHERE id=?1",
                [event_id_for_epoch_reset.as_str()],
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("simulate link epoch change after effect and before inbox completion");
    let epoch_changed_retry = driver
        .receive("source-server", &envelope, 106)
        .await
        .expect("a committed-effect retry stays uncertain after link epoch change");
    assert_eq!(epoch_changed_retry, ExchangeOutcome::Unknown);
    let event_id_after_epoch = remote_event_id.clone();
    let event_count: i64 = support::read(receiver.store.clone(), move |connection| {
        connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_events WHERE id=?1 AND agent=?2 AND kind='user'",
                (event_id_after_epoch.as_str(), "late-operation-recipient"),
                |row| row.get::<_, i64>(0),
            )
            .expect("home event remains durable after epoch change")
    })
    .await;
    assert_eq!(event_count, 1);

    let asset = studio_runtime_services::payload_store::upload(
        &fixture.store,
        "late-operation-recipient",
        "remote-anchor.txt",
        Some("00000000-0000-4000-8000-000000000005"),
        "YWJj",
    )
    .await
    .expect("upload for remote attachment refusal");
    let asset_id = asset["id"].as_str().expect("uploaded asset ID").to_owned();
    let attachment = app
        .clone()
        .oneshot(request(
            "00000000-0000-4000-8000-000000000006",
            "Attachment must be refused",
            &[asset_id],
        ))
        .await
        .expect("attachment HTTP response");
    assert_eq!(attachment.status(), axum::http::StatusCode::BAD_REQUEST);

    let changed = app
        .oneshot(request(
            "00000000-0000-4000-8000-000000000004",
            "Changed content must conflict",
            &[],
        ))
        .await
        .expect("changed-content HTTP response");
    assert_eq!(changed.status(), axum::http::StatusCode::BAD_REQUEST);
    let outbox_count: i64 = support::read(fixture.store.clone(), |connection| {
        connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_server_outbox WHERE state='queued'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("queued paired intent count after conflict")
    })
    .await;
    assert_eq!(outbox_count, 2, "refused inputs must not add an intent");
}
