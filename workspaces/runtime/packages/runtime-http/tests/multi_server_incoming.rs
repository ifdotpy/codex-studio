#[path = "../../runtime-services/tests/multi_server/common.rs"]
mod common;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::sync::Arc;
use studio_runtime_http::{HttpState, router};
use tower::ServiceExt;

fn incoming(case: &Value) -> Request<Body> {
    let raw = case["raw"].as_str().expect("raw");
    let mut request = Request::builder()
        .method(case["method"].as_str().expect("method"))
        .uri(case["target"].as_str().expect("target"))
        .header("host", "127.0.0.1:4000")
        .header("x-forwarded-host", "local.fixture.ts.net")
        .header("x-forwarded-proto", "https")
        .header("x-forwarded-for", "100.64.0.2")
        .header("content-type", "application/json")
        .header("content-length", raw.len().to_string());
    for pair in case["headers"].as_array().expect("headers") {
        request = request.header(
            pair[0].as_str().expect("name"),
            pair[1].as_str().expect("value"),
        );
    }
    let mut request = request.body(Body::from(raw.to_owned())).expect("request");
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:4321"
            .parse::<std::net::SocketAddr>()
            .expect("address"),
    ));
    request
}
#[tokio::test]
async fn signed_requests_cross_the_http_boundary_and_missing_owner_is_no_write() {
    let (_root, store) = common::store();
    let data = &common::fixture().data;
    let case = &data["auth"][0];
    let state = HttpState::new(
        "token".into(),
        "workspace".into(),
        None,
        "fixture".into(),
        4000,
        Some("https://local.fixture.ts.net".into()),
        Some(store.clone()),
        None,
    );
    let before = common::rows(&store);
    let absent = router(state.clone())
        .expect("router")
        .oneshot(incoming(case))
        .await
        .expect("response");
    assert_eq!(absent.status(), StatusCode::NOT_IMPLEMENTED);
    assert_eq!(common::rows(&store), before);
    let app =
        router(state.with_multi_server_boundary(Arc::new(common::boundary()))).expect("router");
    let mut identity_case = case.clone();
    identity_case["method"] = serde_json::json!("GET");
    identity_case["target"] = serde_json::json!("/api/multi-server/v1/identity");
    identity_case["raw"] = serde_json::json!("");
    identity_case["headers"] = serde_json::json!([]);
    let response = app
        .clone()
        .oneshot(incoming(&identity_case))
        .await
        .expect("identity response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
    let actual: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), 4096)
            .await
            .expect("identity body"),
    )
    .expect("identity JSON");
    assert_eq!(actual, data["discoveryIdentity"]);
    let mut preflight = incoming(&identity_case);
    *preflight.method_mut() = axum::http::Method::OPTIONS;
    preflight.headers_mut().insert(
        "access-control-request-headers",
        "x-studio-signature".parse().expect("header"),
    );
    assert_eq!(
        app.clone()
            .oneshot(preflight)
            .await
            .expect("preflight")
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(common::rows(&store), before);
    let mut spoof = incoming(case);
    spoof.headers_mut().insert(
        "x-forwarded-host",
        "foreign.fixture.ts.net".parse().expect("header"),
    );
    assert_eq!(
        app.clone().oneshot(spoof).await.expect("response").status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(common::rows(&store), before);
    let mut forged = incoming(case);
    forged
        .headers_mut()
        .insert("x-studio-signature", "invalid".parse().expect("header"));
    assert_eq!(
        app.clone()
            .oneshot(forged)
            .await
            .expect("response")
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(common::rows(&store), before);
    let response = app.clone().oneshot(incoming(case)).await.expect("response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4096).await.expect("body");
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let value: Value = serde_json::from_slice(&bytes).expect("JSON");
    assert_eq!(value["clientId"], "new-client");
    assert_eq!(value["paired"], true);
    assert_eq!(
        app.oneshot(incoming(case))
            .await
            .expect("response")
            .status(),
        StatusCode::UNAUTHORIZED
    );
    // Keep the shared helpers exercised in this binary as well.
    assert!(common::request(case).is_ok());
    assert_eq!(common::owner().login(), "owner@example.test");
}
