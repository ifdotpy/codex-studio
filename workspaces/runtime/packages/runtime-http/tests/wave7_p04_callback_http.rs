//! Actual HTTP middleware and generated DTOs with an injected retained reply port.
#![allow(clippy::unwrap_used)]
#[path = "../../runtime-services/tests/wave7_p04_callback/fixture.rs"]
mod fixture;
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
    let (root, store, context) = fixture();
    let (id, _) = pending(&store, &context);
    let state = HttpState::new(
        "token".into(),
        "fixture".into(),
        None,
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
    assert_eq!(checkpoint(&store, &context, 1), 1);
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
        let (root, store, context) = fixture();
        let (id, _) = pending(&store, &context);
        let port = Arc::new(Port::default());
        *port.next.lock().unwrap() = Some(outcome);
        let state = HttpState::new(
            "token".into(),
            "fixture".into(),
            None,
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
async fn enabled_native_backend_serves_success_and_null_refusal_over_tcp() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (root, store, _) = fixture();
    let port = Arc::new(Port::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    assert_ne!(address.port(), 4620);
    let state = HttpState::new(
        "token".into(),
        "fixture".into(),
        None,
        root.path().to_string_lossy().into(),
        address.port(),
        None,
        Some(store.clone()),
        None,
    )
    .with_callback_runtime(Arc::new(CallbackRuntime::new(store.clone(), port.clone())));
    let app = router(state).unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            stopped.await.unwrap();
        })
        .await
        .unwrap();
    });
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
        let id = format!("tcp-native-{index}");
        value["id"] = json!(id);
        value["rpcId"] = json!(id);
        value["method"] = json!(method);
        card(&store, &value);
        let before = physical_snapshot(&store);
        for (decision, expected) in [(Value::Null, 400), (json!("accept"), 200)] {
            let body = if method == "item/tool/requestUserInput" {
                json!({"id":id,"answers":if expected==400 {Value::Null} else {json!({})}})
            } else {
                json!({"id":id,"decision":decision})
            }
            .to_string();
            let request = format!(
                "POST /api/answer HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nX-Canvas-Token: token\r\nX-Studio-API-Schema: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                studio_runtime_http::schema::api_schema_hash(),
                body.len()
            );
            let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).await.unwrap();
            let response = String::from_utf8(bytes).unwrap();
            assert!(
                response.starts_with(&format!("HTTP/1.1 {expected}")),
                "{response}"
            );
            let body: Value =
                serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
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
    stop.send(()).unwrap();
    server.await.unwrap();
}
