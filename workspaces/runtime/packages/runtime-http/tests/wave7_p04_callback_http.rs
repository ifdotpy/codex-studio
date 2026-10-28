//! Actual HTTP middleware and generated DTOs with an injected retained reply port.
#![allow(clippy::unwrap_used)]
use crate::callback_fixtures as fixture;
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use fixture::*;
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_http::{HttpState, router};
use studio_runtime_services::callback_runtime::*;
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
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn complaint_http_preserves_validation_locations_and_default_submit() {
    let (root, store, context) = fixture().await;
    let app = router(HttpState::new(
        "token".into(),
        "fixture".into(),
        root.path().to_string_lossy().into(),
        49171,
        None,
        Some(store),
        None,
    ))
    .unwrap();
    for (body, location) in [
        (
            json!({"id":"submit", "text":"Review this"}),
            json!(["body.lead"]),
        ),
        (
            json!({"id":"submit", "text":"Review this", "lead":null}),
            json!(["body.lead"]),
        ),
        (
            json!({"id":"submit", "text":"Review this", "lead":"  "}),
            json!(["body.lead"]),
        ),
        (
            json!({"id":"reply", "text":"Reviewed", "action":"respond"}),
            json!(["body"]),
        ),
        (
            json!({"id":"reply", "text":"Reviewed", "action":"respond", "complaint_id":"c", "status":null, "version":1}),
            json!(["body"]),
        ),
        (
            json!({"id":"reply", "text":"Reviewed", "action":"respond", "complaint_id":"c", "status":"resolved", "version":null}),
            json!(["body"]),
        ),
        (
            json!({"id":"reply", "text":"Reviewed", "action":"unknown"}),
            json!(["body", "action"]),
        ),
        (
            json!({"id":"reply", "text":"Reviewed", "status":"open"}),
            json!(["body", "status"]),
        ),
        (
            json!({"id":"reply", "text":"Reviewed", "surprise":true}),
            json!(["body", "surprise"]),
        ),
    ] {
        let (status, error) = call(&app, "/api/complaints", body.clone(), "token").await;
        assert_eq!(status, 400, "{body}: {error}");
        assert_eq!(error["location"], location, "{body}: {error}");
    }
    let body = json!({"id":"default-submit", "lead":context.agent, "text":"  Review this  "});
    let (status, first) = call(&app, "/api/complaints", body.clone(), "token").await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["status"], "open");
    assert_eq!(first["text"], "Review this");
    let (status, replay) = call(&app, "/api/complaints", body, "token").await;
    assert_eq!(status, 200, "{replay}");
    assert_eq!(first, replay);
    let (status, explicit) = call(
        &app,
        "/api/complaints",
        json!({"id":"default-submit", "lead":context.agent, "text":"Review this",
            "action":"submit", "complaint_id":"unused", "status":"resolved", "version":0}),
        "token",
    )
    .await;
    assert_eq!(status, 200, "{explicit}");
    assert_eq!(
        first, explicit,
        "known fields remain accepted on either action"
    );
    let (status, changed) = call(
        &app,
        "/api/complaints",
        json!({
            "id":"default-submit", "lead":context.agent, "text":"Different content"
        }),
        "token",
    )
    .await;
    assert_eq!(status, 400, "{changed}");
    assert_eq!(changed["error"], "This complaint id has different content");
}

#[tokio::test]
async fn complaint_http_replays_original_signature_before_version_and_recipient_checks() {
    let (root, store, context) = fixture().await;
    let reporter = template()["agents"][1]["id"].as_str().unwrap().to_owned();
    let lead = context.agent.clone();
    store
        .transaction_with::<_, studio_runtime_store::StoreError>(move |tx| {
            studio_runtime_store::QuestionsWriteRepository::put_complaint(
                tx,
                "complaint",
                &json!({
                    "id":"complaint", "leadId":lead, "author":reporter,
                    "text":"Please review", "recipient":"user", "status":"open",
                    "created":1, "version":1, "responses":[]
                })
                .to_string(),
            )
        })
        .await
        .unwrap();
    let app = router(HttpState::new(
        "token".into(),
        "fixture".into(),
        root.path().to_string_lossy().into(),
        49171,
        None,
        Some(store.clone()),
        None,
    ))
    .unwrap();
    let request_root = studio_runtime_schema::persistence::RequestIdentity::issue_root();
    let body = json!({"id":request_root, "action":"respond", "complaint_id":"complaint",
        "text":"  Reviewed  ", "status":"in_progress", "version":1, "lead":null});
    assert_eq!(
        call(&app, "/api/complaints", body.clone(), "wrong").await.0,
        403
    );
    let (status, first) = call(&app, "/api/complaints", body.clone(), "token").await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["version"], 2);
    assert_eq!(first["responses"][0]["text"], "Reviewed");
    // This is the pre-refactor DTO's exact serialization, including explicit null.
    let mut signature_body = body.clone();
    signature_body["operation"] = json!("complaint_response");
    let signature = studio_runtime_services::event_write::request_signature(&signature_body);
    store.transaction_with::<_, studio_runtime_store::StoreError>(move |tx| {
        let identity = studio_runtime_schema::persistence::RequestIdentity::new("complaint_response", &["complaint","user"], &request_root, &[]).unwrap();
        assert!(studio_runtime_services::event_write::replay_request::<studio_runtime_store::ComplaintRecord>(tx, identity.id(), &signature, (studio_runtime_services::event_write::unix_time().unwrap()*1000.0) as i64)?.is_some());
        // A lost reply must replay despite later version/recipient changes.
        tx.execute("UPDATE runtime_complaints SET record=json_set(record,'$.recipient','lead','$.version',9)", [])?;
        Ok(())
    }).await.unwrap();
    let (status, replay) = call(&app, "/api/complaints", body.clone(), "token").await;
    assert_eq!(status, 200, "{replay}");
    assert_eq!(replay, first);
    for field in ["text", "lead"] {
        let mut changed = body.clone();
        if field == "lead" {
            assert!(changed.as_object_mut().unwrap().remove(field).is_some());
        } else {
            changed[field] = json!("Changed reply");
        }
        let (status, error) = call(&app, "/api/complaints", changed, "token").await;
        assert_eq!(status, 409, "{error}");
        assert_eq!(error["error"], "This request ID has different content");
    }
    let mut fresh = body.clone();
    fresh["id"] = json!(studio_runtime_schema::persistence::RequestIdentity::issue_root());
    let (status, error) = call(&app, "/api/complaints", fresh.clone(), "token").await;
    assert_eq!(status, 400, "{error}");
    assert_eq!(
        error["error"],
        "This complaint requires a response from its orchestrator"
    );
    store
        .transaction_with::<_, studio_runtime_store::StoreError>(move |tx| {
            tx.execute(
                "UPDATE runtime_complaints SET record=json_set(record,'$.recipient','user')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (status, error) = call(&app, "/api/complaints", fresh, "token").await;
    assert_eq!(status, 409, "{error}");
    assert_eq!(
        error["error"],
        "This complaint changed. Review the latest response before replying"
    );
    let events: i64 = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM runtime_events WHERE kind='complaint_response'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(events, 1, "retries never enqueue another response");

    // A user can close a lead-directed complaint once its orchestrator answered.
    let lead = context.agent;
    store
        .transaction_with::<_, studio_runtime_store::StoreError>(move |tx| {
            let raw = studio_runtime_store::QuestionsWriteRepository::complaint(tx, "complaint")?
                .unwrap();
            let mut complaint: Value = serde_json::from_str(&raw).unwrap();
            complaint["recipient"] = json!("lead");
            complaint["responses"].as_array_mut().unwrap().push(json!({
                "id":"lead-response", "author":lead, "status":"resolved", "text":"Handled", "at":2
            }));
            studio_runtime_store::QuestionsWriteRepository::put_complaint(
                tx,
                "complaint",
                &complaint.to_string(),
            )
        })
        .await
        .unwrap();
    for (id, version, status) in [
        ("resolved-reply", 9, "resolved"),
        ("declined-reply", 10, "declined"),
    ] {
        let mut closing = body.clone();
        closing["id"] = json!(id);
        closing["version"] = json!(version);
        closing["status"] = json!(status);
        let (code, record) = call(&app, "/api/complaints", closing, "token").await;
        assert_eq!(code, 200, "{record}");
        assert_eq!(record["status"], status);
        assert_eq!(record["version"], version + 1);
    }
}

#[tokio::test]
async fn human_http_roundtrip_requires_consumer_and_authenticated_user() {
    let (root, store, context) = fixture().await;
    let (id, _) = pending(&store, &context).await;
    let state = HttpState::new(
        "token".into(),
        "fixture".into(),
        root.path().to_string_lossy().into(),
        49171,
        None,
        Some(store.clone()),
        None,
    );
    let unwired = router(state.clone()).unwrap();
    let body = json!({"id":id,"answers":{"secret":{"answers":["private"]}}});
    let before = stored(&store, &id);
    assert_eq!(
        call(&unwired, "/api/answer", body.clone(), "token").await.0,
        501
    );
    assert_eq!(stored(&store, &id), before);
    assert_eq!(
        call(&unwired, "/api/questions/delete", json!({"id":id}), "token")
            .await
            .0,
        501,
        "delete cannot bypass CallbackRuntime when its reply port is absent"
    );
    assert_eq!(stored(&store, &id), before);
    let port = Arc::new(Port::default());
    let app = router(
        state.with_callback_runtime(Arc::new(CallbackRuntime::new(store.clone(), port.clone()))),
    )
    .unwrap();
    assert_eq!(
        call(&app, "/api/answer", body.clone(), "wrong").await.0,
        403
    );
    assert_eq!(stored(&store, &id), before);
    let (status, value) = call(&app, "/api/answer", body.clone(), "token").await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["status"], "answered");
    assert_eq!(port.calls.lock().unwrap().len(), 1);
    assert_eq!(checkpoint(&store, &context, 1).await, 1);
    let (status, value) = call(&app, "/api/answer", body, "token").await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["replayed"], true);
    assert_eq!(port.calls.lock().unwrap().len(), 1);
    assert!(!stored(&store, &id).to_string().contains("private"));
    let (status, value) = call(&app, "/api/questions/delete", json!({"id":id}), "token").await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["status"], "deleted");
}
#[tokio::test]
async fn pending_native_delete_sends_only_cancellation_and_unknown_is_held() {
    for outcome in [ReplyOutcome::Written, ReplyOutcome::Unknown] {
        let (root, store, context) = fixture().await;
        let (id, _) = pending(&store, &context).await;
        let port = Arc::new(Port::default());
        *port.next.lock().unwrap() = Some(outcome);
        let state = HttpState::new(
            "token".into(),
            "fixture".into(),
            root.path().to_string_lossy().into(),
            49171,
            None,
            Some(store.clone()),
            None,
        )
        .with_callback_runtime(Arc::new(CallbackRuntime::new(store.clone(), port.clone())));
        let app = router(state).unwrap();
        let (status, value) = call(&app, "/api/questions/delete", json!({"id":id}), "token").await;
        assert_eq!(
            status,
            if outcome == ReplyOutcome::Written {
                200
            } else {
                400
            },
            "{value}"
        );
        {
            let calls = port.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(
                calls[0].body,
                studio_runtime_providers::ResponseBody::Result(json!({"answers":{}}))
            );
        }
        let (status, _) = call(&app, "/api/questions/delete", json!({"id":id}), "token").await;
        assert_eq!(
            status,
            if outcome == ReplyOutcome::Written {
                200
            } else {
                400
            }
        );
        assert_eq!(port.calls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn enabled_native_backend_serves_success_and_null_refusal() {
    let (root, store, _) = fixture().await;
    let port = Arc::new(Port::default());
    let state = HttpState::new(
        "token".into(),
        "fixture".into(),
        root.path().to_string_lossy().into(),
        49171,
        None,
        Some(store.clone()),
        None,
    )
    .with_callback_runtime(Arc::new(CallbackRuntime::new(store.clone(), port.clone())));
    let app = router(state).unwrap();
    for (index, method) in [
        "item/commandExecution/requestApproval",
        "item/fileChange/requestApproval",
        "mcpServer/elicitation/request",
        "item/tool/requestUserInput",
    ]
    .into_iter()
    .enumerate()
    {
        let mut value = template()["legacyCard"].clone();
        let id = format!("native-{index}");
        value["id"] = json!(id);
        value["rpcId"] = json!(id);
        value["method"] = json!(method);
        card(&store, &value).await;
        seed_admission(&store, &value).await;
        let before = physical_snapshot(&store);
        for (decision, expected) in [(Value::Null, 400), (json!("accept"), 200)] {
            let body = if method == "item/tool/requestUserInput" {
                json!({"id":id,"answers":if expected==400 {Value::Null} else {json!({})}})
            } else {
                json!({"id":id,"decision":decision})
            };
            let (status, body) = call(&app, "/api/answer", body, "token").await;
            assert_eq!(status, expected, "{body}");
            if expected == 400 {
                assert_eq!(
                    body["error"],
                    if method == "item/tool/requestUserInput" {
                        "Supply question answers"
                    } else {
                        "Choose accept, decline or cancel"
                    }
                );
                assert_eq!(physical_snapshot(&store), before);
                assert_eq!(stored(&store, &id), value);
                assert_eq!(port.calls.lock().unwrap().len(), index);
            } else {
                assert_eq!(body["status"], "answered");
                assert_eq!(port.calls.lock().unwrap().len(), index + 1);
            }
        }
    }
}

async fn permission_app(
    root: &std::path::Path,
    store: Arc<studio_runtime_store::RuntimeStore>,
    port: Arc<Port>,
) -> axum::Router {
    let registry = root.join("permission-accounts.json");
    tokio::fs::write(&registry, json!({"version":studio_runtime_schema::shared_values::ACCOUNT_REGISTRY_FORMAT_VERSION,"defaultAccountKey":"default","accounts":{"default":{"id":"default","home":root,"label":"Fixture","source":"Fixture","provider":"codex","status":"signedOut"}},"logins":{},"deleteReceipts":{}}).to_string()).await.unwrap();
    router(
        HttpState::new(
            "token".into(),
            "fixture".into(),
            root.to_string_lossy().into(),
            49171,
            None,
            Some(store.clone()),
            None,
        )
        .with_account_driver(Arc::new(
            studio_runtime_services::account_driver::AccountDriver::open(&registry).unwrap(),
        ))
        .with_callback_runtime(Arc::new(CallbackRuntime::new(store, port))),
    )
    .unwrap()
}

async fn pending_codex_approval(
    store: &Arc<studio_runtime_store::RuntimeStore>,
    context: &studio_runtime_services::provider_callbacks::CallbackContext,
) -> (String, studio_runtime_providers::Frame) {
    let actor = context.agent.clone();
    store.transaction(move |tx| {
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.yoloMode',json('false'),'$.inFlight',json('true')) WHERE id=?1", [&actor])?;
        Ok(())
    }).await.unwrap();
    let frame = studio_runtime_providers::Frame::Request(studio_runtime_providers::RequestFrame {
        id: studio_runtime_providers::RequestId::Number(87),
        method: studio_runtime_providers::Method::ItemCommandExecutionRequestApproval,
        params: Some(json!({"threadId":context.thread,"turnId":context.turn,"command":"fixture"})),
        fields: Default::default(),
    });
    let studio_runtime_services::provider_callbacks::NativeAdmission::Card(id) =
        studio_runtime_services::provider_callbacks::admit_native(
            store.clone(),
            context,
            &frame,
            10.0,
        )
        .await
        .unwrap()
    else {
        panic!("pending approval")
    };
    (id, frame)
}

#[tokio::test]
async fn live_permissions_codex_switch_settles_pending_once_and_ask_prompts_again() {
    use studio_runtime_services::provider_callbacks::*;
    let (root, store, context) = fixture().await;
    let (id, frame) = pending_codex_approval(&store, &context).await;
    let port = Arc::new(Port::default());
    let app = permission_app(root.path(), store.clone(), port.clone()).await;
    let body = json!({"id":context.agent,"yolo_mode":true,"request_id":"live-full","expected_account_key":"default"});
    assert_eq!(
        call(&app, "/api/conversation", body.clone(), "wrong")
            .await
            .0,
        403
    );
    let (status, value) = call(&app, "/api/conversation", body.clone(), "token").await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["yoloMode"], true);
    assert_eq!(stored(&store, &id)["status"], "answered");
    assert_eq!(port.calls.lock().unwrap().len(), 1);
    assert_eq!(
        call(&app, "/api/conversation", body.clone(), "token")
            .await
            .0,
        200
    );
    assert_eq!(port.calls.lock().unwrap().len(), 1);
    let mut later = frame.clone();
    if let studio_runtime_providers::Frame::Request(ref mut request) = later {
        request.id = studio_runtime_providers::RequestId::Number(88);
    }
    assert!(matches!(
        admit_native(store.clone(), &context, &later, 11.0)
            .await
            .unwrap(),
        NativeAdmission::Answer(_)
    ));
    let ask = json!({"id":context.agent,"yolo_mode":false,"request_id":"live-ask","expected_account_key":"default"});
    assert_eq!(call(&app, "/api/conversation", ask, "token").await.0, 200);
    // Replaying the old Full save must not override the later Ask save.
    assert_eq!(
        call(&app, "/api/conversation", body, "token").await.1["yoloMode"],
        false
    );
    if let studio_runtime_providers::Frame::Request(ref mut request) = later {
        request.id = studio_runtime_providers::RequestId::Number(89);
    }
    assert!(matches!(
        admit_native(store.clone(), &context, &later, 12.0)
            .await
            .unwrap(),
        NativeAdmission::Card(_)
    ));
    assert_eq!(port.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn live_permissions_archived_previous_uses_new_chat_defaults() {
    let (root, store, context) = fixture().await;
    let actor = context.agent.clone();
    store.transaction(move |tx| {
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.agentArchive',json('{\"at\":10,\"epoch\":3}'),'$.deletedAt',10,'$.yoloMode',json('false'),'$.approvalPolicy','on-request') WHERE id=?1", [&actor])?;
        Ok(())
    }).await.unwrap();
    let app = permission_app(root.path(), store, Arc::new(Port::default())).await;
    let (status, value) = call(
        &app,
        "/api/leads",
        json!({"previous":context.agent,"cwd":root.path(),"reuse_empty":false}),
        "token",
    )
    .await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["yoloMode"], true);
    assert_ne!(value["approvalPolicy"], "on-request");
}

#[tokio::test]
async fn live_permissions_live_previous_still_donates_settings() {
    let (root, store, context) = fixture().await;
    let actor = context.agent.clone();
    store.transaction(move |tx| {
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.yoloMode',json('false')) WHERE id=?1", [&actor])?;
        Ok(())
    }).await.unwrap();
    let app = permission_app(root.path(), store, Arc::new(Port::default())).await;
    let (status, value) = call(
        &app,
        "/api/leads",
        json!({"previous":context.agent,"cwd":root.path(),"reuse_empty":false}),
        "token",
    )
    .await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["yoloMode"], false);
}

#[tokio::test]
async fn live_permissions_failed_auto_answer_stays_pending_for_the_user() {
    let (root, store, context) = fixture().await;
    let (id, _) = pending_codex_approval(&store, &context).await;
    let port = Arc::new(Port::default());
    *port.next.lock().unwrap() = Some(ReplyOutcome::NotSent);
    let app = permission_app(root.path(), store.clone(), port.clone()).await;
    let full = json!({"id":context.agent,"yolo_mode":true,"request_id":"pending-full","expected_account_key":"default"});
    let (status, value) = call(&app, "/api/conversation", full, "token").await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["yoloMode"], true);
    assert_eq!(stored(&store, &id)["status"], "pending");
    assert_eq!(port.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn live_permissions_receipt_distinguishes_omitted_effort_from_null() {
    let (root, store, context) = fixture().await;
    let actor = context.agent.clone();
    store
        .transaction(move |tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.effort','high') WHERE id=?1",
                [&actor],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let app = permission_app(root.path(), store, Arc::new(Port::default())).await;
    let mut body = json!({"id":context.agent,"yolo_mode":true,"request_id":"presence"});
    let (status, value) = call(&app, "/api/conversation", body.clone(), "token").await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["effort"], "high");
    body["effort"] = Value::Null;
    let (status, value) = call(&app, "/api/conversation", body, "token").await;
    assert_eq!(status, 400, "changed content must conflict: {value}");
    assert_eq!(value["error"], "This request id has different content");
}
