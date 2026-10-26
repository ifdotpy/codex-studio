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
    tokio::fs::write(&registry, json!({"version":1,"defaultAccountKey":"default","accounts":{"default":{"id":"default","home":root,"label":"Fixture","source":"Fixture","provider":"codex","status":"signedOut"}},"logins":{},"deleteReceipts":{}}).to_string()).await.unwrap();
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
            1,
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
        admit_native(store.clone(), &context, &later, 2, 11.0)
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
        admit_native(store.clone(), &context, &later, 3, 12.0)
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
