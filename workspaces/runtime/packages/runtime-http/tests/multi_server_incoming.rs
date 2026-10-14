#[path = "../../runtime-services/tests/multi_server/common.rs"]
mod common;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::sync::{Arc, atomic::Ordering};
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
    let lookup = common::CountingOwner::new(case.clone());
    let boundary = studio_runtime_services::multi_server_pairing::MultiServerBoundary::new(
        data["identity"].clone(),
        lookup.clone(),
    )
    .expect("boundary");
    let app = router(state.with_multi_server_boundary(Arc::new(boundary))).expect("router");
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
    for name in ["bad-signature", "bad-auto-signature"] {
        let forged = data["auth"]
            .as_array()
            .expect("cases")
            .iter()
            .find(|c| c["name"] == name)
            .expect("forged case");
        let calls = lookup.calls.load(Ordering::SeqCst);
        let probes = lookup.probes.load(Ordering::SeqCst);
        let response = app
            .clone()
            .oneshot(incoming(forged))
            .await
            .expect("response");
        assert_eq!(
            response.status().as_u16(),
            forged["refusal"]["status"].as_u64().expect("status") as u16
        );
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.expect("body"))
                .expect("JSON");
        assert_eq!(
            body,
            serde_json::json!({"error":forged["refusal"]["error"],"code":forged["refusal"]["code"]})
        );
        assert_eq!(lookup.calls.load(Ordering::SeqCst), calls);
        assert_eq!(lookup.probes.load(Ordering::SeqCst), probes);
        assert_eq!(common::rows(&store), before);
    }
    let query = data["auth"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["name"] == "status-query")
        .expect("queried status");
    let response = app
        .clone()
        .oneshot(incoming(query))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.expect("body"))
            .expect("JSON");
    assert_eq!(body, data["identity"]);
    let before = common::rows(&store);
    let mut changed = query.clone();
    changed["target"] = serde_json::json!("/api/multi-server/v1/status?check=2");
    let calls = lookup.calls.load(Ordering::SeqCst);
    let response = app
        .clone()
        .oneshot(incoming(&changed))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.expect("body"))
            .expect("JSON");
    let bad = data["auth"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["name"] == "bad-signature")
        .expect("signature refusal");
    assert_eq!(
        body,
        serde_json::json!({"code":bad["refusal"]["code"],"error":bad["refusal"]["error"]})
    );
    assert_eq!(lookup.calls.load(Ordering::SeqCst), calls);
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
    let before = common::rows(&store);
    let replay = app.oneshot(incoming(case)).await.expect("response");
    assert_eq!(
        replay.status().as_u16(),
        case["replayRefusal"]["status"].as_u64().expect("status") as u16
    );
    let body: Value =
        serde_json::from_slice(&to_bytes(replay.into_body(), 4096).await.expect("body"))
            .expect("JSON");
    assert_eq!(
        body,
        serde_json::json!({"code":case["replayRefusal"]["code"],"error":case["replayRefusal"]["error"]})
    );
    assert_eq!(common::rows(&store), before);
    // Keep the shared helpers exercised in this binary as well.
    assert!(common::request(case).is_ok());
    assert_eq!(common::owner().login(), "owner@example.test");
    let _ = common::boundary();
}

#[tokio::test]
async fn browser_forward_opens_local_window_without_database_writes() {
    let (root, store) = common::store();
    let state = HttpState::new(
        "token".into(),
        "workspace".into(),
        None,
        root.path().display().to_string(),
        4000,
        None,
        Some(store.clone()),
        None,
    );
    let app = router(state).expect("router");
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("free loopback port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let body = serde_json::json!({
        "server":"paired-test-peer",
        "bridgeId":"0123456789abcdef0123456789abcdef",
        "host":"127.0.0.1",
        "port":port,
        "path":"/oauth/callback",
        "expiresAt":now + 120.0,
        "token":"0123456789abcdef"
    });
    let mut request = Request::builder()
        .method("POST")
        .uri("/api/browser/forward")
        .header("host", "127.0.0.1:4000")
        .header("content-type", "application/json")
        .header("x-canvas-token", "token")
        .header(
            studio_runtime_http::schema::API_SCHEMA_HASH_HEADER,
            studio_runtime_http::schema::api_schema_hash(),
        )
        .body(Body::from(body.to_string()))
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:4321".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let before = common::rows(&store);
    let response = app.oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&to_bytes(response.into_body(), 4096).await.expect("body"))
            .unwrap(),
        serde_json::json!({"status":"forwarding"})
    );
    assert_eq!(common::rows(&store), before);
}

#[tokio::test]
async fn signed_remote_event_crosses_http_boundary_and_persists_once() {
    use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, AgentStatus};
    let (root, store) = common::store();
    let mut parent = AgentRecord {
        id: AgentId::from("event-parent"),
        root_id: AgentId::from("event-parent"),
        epoch: 3,
        status: AgentStatus::Idle,
        extra: Default::default(),
    };
    parent
        .extra
        .insert("autoWake".into(), serde_json::json!(true));
    parent
        .extra
        .insert("moveImportPending".into(), serde_json::json!(true));
    let link = serde_json::json!({"id":"fixture-link","side":"home","server":"peer-server","home":"local-server","parent":"event-parent","parentEpoch":3,"workers":[]});
    store
        .transaction_with(|tx| {
            tx.execute(
                "INSERT INTO runtime_server_links(id,record) VALUES('fixture-link',?1)",
                [link.to_string()],
            )?;
            AgentRepository::put(tx, &parent)?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .expect("link and owner rows");
    let data = &common::fixture().data["eventRequest"];
    let raw = data["raw"].as_str().unwrap();
    let mut request = Request::builder()
        .method(data["method"].as_str().unwrap())
        .uri(data["target"].as_str().unwrap())
        .header("host", "127.0.0.1:4000")
        .header("x-forwarded-host", "local.fixture.ts.net")
        .header("x-forwarded-proto", "https")
        .header("x-forwarded-for", data["address"].as_str().unwrap())
        .header("content-type", "application/json")
        .header("content-length", raw.len().to_string());
    for pair in data["headers"].as_array().unwrap() {
        request = request.header(pair[0].as_str().unwrap(), pair[1].as_str().unwrap());
    }
    for login in data["logins"].as_array().unwrap() {
        request = request.header("tailscale-user-login", login.as_str().unwrap());
    }
    let mut request = request.body(Body::from(raw.to_owned())).unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:4321".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let state = HttpState::new(
        "token".into(),
        "workspace".into(),
        None,
        root.path().display().to_string(),
        4000,
        Some("https://local.fixture.ts.net".into()),
        Some(store.clone()),
        None,
    )
    .with_multi_server_boundary(Arc::new(common::boundary()));
    let response = router(state)
        .expect("router")
        .oneshot(request)
        .await
        .expect("signed event response");
    assert_eq!(response.status(), StatusCode::OK);
    let receipt: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), 4096)
            .await
            .expect("receipt body"),
    )
    .expect("receipt JSON");
    assert_eq!(receipt["outcome"], "applied");
    assert_eq!(receipt["value"]["eventId"], "python-event-http");
    let db = store.read().unwrap();
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM runtime_events WHERE id='python-event-http' AND agent='event-parent' AND text='signed owner event'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}
