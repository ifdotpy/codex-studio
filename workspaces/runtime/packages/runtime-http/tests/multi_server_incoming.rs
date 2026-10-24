#![allow(clippy::unwrap_used)]

use crate::multi_server_fixtures as common;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::sync::{Arc, atomic::Ordering};
use studio_runtime_http::{HttpState, router};
use studio_runtime_services::multi_server_auth::{AccessError, OwnerObservation};
use studio_runtime_services::multi_server_pairing::{MultiServerBoundary, OwnerLookup};
use studio_runtime_services::remote_transport::{
    MultiServerRequest, load_multi_server_identity, multi_server_signed_headers,
};
use tower::ServiceExt;

struct FixtureOwner;
impl OwnerLookup for FixtureOwner {
    fn observe<'a>(
        &'a self,
        _: &'a str,
        _: &'a [String],
        _: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(OwnerObservation, Option<Value>), AccessError>> {
        Box::pin(async { Ok((common::owner(), None)) })
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct FixtureCredentials {
    #[serde(rename = "X-Studio-Client")]
    client: String,
    #[serde(rename = "X-Studio-Server")]
    server: String,
    #[serde(rename = "X-Studio-Timestamp")]
    timestamp: String,
    #[serde(rename = "X-Studio-Nonce")]
    nonce: String,
    #[serde(rename = "X-Studio-Request-Id")]
    request: String,
    #[serde(rename = "X-Studio-Signature")]
    signature: String,
}

/// Refresh valid frozen signatures without repairing deliberately forged requests.
fn fresh_headers(request: &common::FixtureRequest, nonce: Option<&str>) -> Vec<(String, String)> {
    use std::collections::BTreeMap;
    use studio_runtime_services::remote_transport::{
        MultiServerSigningInput, multi_server_signing_bytes, verify_signature,
    };
    if request.headers.is_empty() {
        return request.headers.clone();
    }
    let headers: BTreeMap<_, _> = request.headers.iter().cloned().collect();
    let mut credentials: FixtureCredentials =
        serde_json::from_value(serde_json::to_value(headers).expect("credential map"))
            .expect("typed fixture credentials");
    let wire = |timestamp: &str, nonce: &str| {
        multi_server_signing_bytes(MultiServerSigningInput {
            method: &request.method,
            target: &request.target,
            server_id: &credentials.server,
            client_id: &credentials.client,
            timestamp,
            nonce,
            request_id: &credentials.request,
            body: request.raw.as_bytes(),
        })
    };
    let identity = common::signing_identity();
    let valid = verify_signature(
        identity.public_key_pem(),
        &wire(&credentials.timestamp, &credentials.nonce),
        &credentials.signature,
    )
    .is_ok();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        .to_string();
    if valid {
        credentials.signature =
            identity.sign(&wire(&timestamp, nonce.unwrap_or(&credentials.nonce)));
    }
    if let Some(nonce) = nonce {
        credentials.nonce = nonce.to_owned();
    }
    credentials.timestamp = timestamp;
    let headers: BTreeMap<String, String> =
        serde_json::from_value(serde_json::to_value(credentials).expect("credentials"))
            .expect("header map");
    headers.into_iter().collect()
}

fn incoming(case: &Value) -> Request<Body> {
    incoming_with_nonce(case, None)
}

fn incoming_with_nonce(case: &Value, nonce: Option<&str>) -> Request<Body> {
    let case: common::FixtureRequest =
        serde_json::from_value(case.clone()).expect("fixture request");
    let raw = &case.raw;
    let mut request = Request::builder()
        .method(case.method.as_str())
        .uri(case.target.as_str())
        .header("host", "127.0.0.1:4000")
        .header("x-forwarded-host", "local.fixture.ts.net")
        .header("x-forwarded-proto", "https")
        .header("x-forwarded-for", "100.64.0.2")
        .header("content-type", "application/json")
        .header("content-length", raw.len().to_string());
    for (name, value) in fresh_headers(&case, nonce) {
        request = request.header(name, value);
    }
    let mut request = request.body(Body::from(raw.to_owned())).expect("request");
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:4321"
            .parse::<std::net::SocketAddr>()
            .expect("address"),
    ));
    request
}

/// A receiver router that trusts one paired fake caller server. The caller
/// signs requests with its own state root; both roots live as long as this value.
struct PairedReceiver {
    _caller_root: tempfile::TempDir,
    _receiver_root: tempfile::TempDir,
    receiver_store: Arc<studio_runtime_store::RuntimeStore>,
    receiver_identity: Value,
    caller_signing: studio_runtime_services::remote_transport::RemoteIdentity,
    origin: &'static str,
    app: axum::Router,
}

async fn paired_receiver() -> PairedReceiver {
    let caller_root = tempfile::tempdir().expect("caller state root");
    let caller_store = Arc::new(
        studio_runtime_store::RuntimeStore::open(Some(caller_root.path()))
            .await
            .expect("caller store opens"),
    );
    let caller_credentials =
        Arc::new(studio_runtime_services::federation_credentials::Credentials::default());
    let caller_identity = caller_credentials
        .identity(
            Arc::clone(&caller_store),
            Some("https://caller.fixture.ts.net"),
        )
        .await
        .expect("caller identity exists");
    let caller_signing =
        load_multi_server_identity(caller_root.path()).expect("caller signing identity loads");

    let receiver_root = tempfile::tempdir().expect("receiver state root");
    let receiver_store = Arc::new(
        studio_runtime_store::RuntimeStore::open(Some(receiver_root.path()))
            .await
            .expect("receiver store opens"),
    );
    let receiver_credentials =
        Arc::new(studio_runtime_services::federation_credentials::Credentials::default());
    let origin = "https://receiver.fixture.ts.net";
    let receiver_identity = receiver_credentials
        .identity(Arc::clone(&receiver_store), Some(origin))
        .await
        .expect("receiver identity exists");
    let caller_identity_for_trust = caller_identity.clone();
    receiver_store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2)",
                rusqlite::params![
                    caller_identity_for_trust["serverId"]
                        .as_str()
                        .expect("caller ID"),
                    serde_json::json!({
                        "id":caller_identity_for_trust["serverId"],
                        "clientId":caller_identity_for_trust["serverId"],
                        "kind":"server",
                        "status":"paired",
                        "label":caller_identity_for_trust["label"],
                        "created":1,
                        "publicKey":caller_identity_for_trust["publicKey"],
                        "origin":"https://caller.fixture.ts.net",
                        "tailscaleUser":"owner@example.test"
                    })
                    .to_string(),
                ],
            )?;
            Ok(())
        })
        .await
        .expect("receiver trusts the paired fake server");

    let boundary = MultiServerBoundary::new(receiver_identity.clone(), Arc::new(FixtureOwner))
        .expect("receiver boundary is ready");
    let state = HttpState::new(
        "token".into(),
        "workspace".into(),
        receiver_root.path().display().to_string(),
        4000,
        Some(origin.into()),
        Some(Arc::clone(&receiver_store)),
        None,
    )
    .with_credentials(Arc::clone(&receiver_credentials))
    .with_multi_server_boundary(Arc::new(boundary))
    .with_tailscale_whoami(Arc::new(|| {
        Box::pin(async { Some(serde_json::json!({"user":"owner@example.test"})) })
    }));
    let app = router(state).expect("receiver router");
    PairedReceiver {
        _caller_root: caller_root,
        _receiver_root: receiver_root,
        receiver_store,
        receiver_identity,
        caller_signing,
        origin,
        app,
    }
}

#[tokio::test]
async fn paired_fake_servers_create_and_recover_the_same_invitation() {
    let PairedReceiver {
        _caller_root,
        _receiver_root,
        receiver_store,
        receiver_identity,
        caller_signing,
        origin,
        app,
    } = paired_receiver().await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_secs() as i64;
    let raw = serde_json::to_vec(&serde_json::json!({
        "action":"create_invite",
        "requestId":"fake-server-invite",
        "label":null
    }))
    .expect("management body serializes");
    let url = format!("{origin}/api/multi-server");

    let send = |nonce: &'static str| {
        let signed = MultiServerRequest {
            server_id: receiver_identity["serverId"].as_str().expect("receiver ID"),
            method: "POST",
            url: &url,
            request_id: "fake-server-invite",
            timestamp: now,
            nonce,
            body: &raw,
        };
        let signed_headers = multi_server_signed_headers(&caller_signing, &signed, now)
            .expect("fake caller signs request");
        let mut builder = Request::builder()
            .method("POST")
            .uri("/api/multi-server")
            .header("host", "127.0.0.1:4000")
            .header("x-forwarded-host", "receiver.fixture.ts.net")
            .header("x-forwarded-proto", "https")
            .header("x-forwarded-for", "100.64.0.2")
            .header("tailscale-user-login", "owner@example.test")
            .header("content-type", "application/json")
            .header("content-length", raw.len().to_string());
        for (name, value) in signed_headers {
            builder = builder.header(name, value);
        }
        let mut request = builder
            .body(Body::from(raw.clone()))
            .expect("signed request builds");
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:4321"
                .parse::<std::net::SocketAddr>()
                .expect("loopback peer address"),
        ));
        request
    };

    let first = app
        .clone()
        .oneshot(send("abcdefghijklmnopqrstuvwxyz012345"))
        .await
        .expect("first fake-server response");
    let first_status = first.status();
    let first_body = to_bytes(first.into_body(), 16 * 1024)
        .await
        .expect("first invitation body");
    assert_eq!(
        first_status,
        StatusCode::OK,
        "{}; receiver state: {}",
        String::from_utf8_lossy(&first_body),
        common::rows(&receiver_store)
    );
    serde_json::from_slice::<studio_runtime_http::generated::InvitationResponse>(&first_body)
        .expect("signed invitation response matches the generated contract");
    let first: Value = serde_json::from_slice(&first_body).expect("first invitation JSON");
    assert_eq!(
        first["invitation"]["serverId"],
        receiver_identity["serverId"]
    );
    assert_eq!(first["invitation"]["label"], receiver_identity["label"]);

    let retry = app
        .clone()
        .oneshot(send("0123456789abcdefghijklmnopqrstuv"))
        .await
        .expect("recovery response");
    assert_eq!(retry.status(), StatusCode::OK);
    let retry: Value = serde_json::from_slice(
        &to_bytes(retry.into_body(), 16 * 1024)
            .await
            .expect("retry invitation body"),
    )
    .expect("retry invitation JSON");
    assert_eq!(retry, first, "the same logical ID recovers the same secret");

    let name_body = serde_json::json!({"label":"Renamed receiver","requestId":"fake-server-name"});
    let name_raw = serde_json::to_vec(&name_body).expect("rename body serializes");
    let name_request_id = "fake-server-name";
    let name_url = format!("{origin}/api/multi-server/v1/name");
    let name_signed = MultiServerRequest {
        server_id: receiver_identity["serverId"].as_str().expect("receiver ID"),
        method: "POST",
        url: &name_url,
        request_id: name_request_id,
        timestamp: now,
        nonce: "fedcba9876543210fedcba9876543210",
        body: &name_raw,
    };
    let name_headers = multi_server_signed_headers(&caller_signing, &name_signed, now)
        .expect("fake caller signs rename request");
    let mut name_builder = Request::builder()
        .method("POST")
        .uri("/api/multi-server/v1/name")
        .header("host", "127.0.0.1:4000")
        .header("x-forwarded-host", "receiver.fixture.ts.net")
        .header("x-forwarded-proto", "https")
        .header("x-forwarded-for", "100.64.0.2")
        .header("tailscale-user-login", "owner@example.test")
        .header("content-type", "application/json")
        .header("content-length", name_raw.len().to_string());
    for (name, value) in name_headers {
        name_builder = name_builder.header(name, value);
    }
    let mut name_request = name_builder
        .body(Body::from(name_raw))
        .expect("signed rename request builds");
    name_request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:4321"
            .parse::<std::net::SocketAddr>()
            .expect("loopback peer address"),
    ));
    let renamed = app
        .oneshot(name_request)
        .await
        .expect("signed rename response");
    let rename_status = renamed.status();
    let rename_body = to_bytes(renamed.into_body(), 4096)
        .await
        .expect("renamed identity body");
    assert_eq!(
        rename_status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&rename_body)
    );
    let renamed: Value = serde_json::from_slice(&rename_body).expect("renamed identity JSON");
    assert_eq!(renamed["label"], "Renamed receiver");

    let invite_count = receiver_store
        .test_read_transaction(|tx| {
            tx.query_row("SELECT count(*) FROM runtime_access_invites", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(studio_runtime_store::StoreError::from)
        })
        .expect("invitation count reads");
    assert_eq!(invite_count, 1);
}
/// docs/multi-server.md management table: the owner session may call
/// `ui_invite` locally or through Serve; `discover` and `alias` are local only;
/// a signed principal may call none of them. Mirrors the Python
/// `multi-server-discovery-contract.py` phone access tests.
#[tokio::test]
async fn phone_owner_session_may_request_ui_access_and_signed_principals_may_not() {
    let PairedReceiver {
        _caller_root,
        _receiver_root,
        receiver_store,
        receiver_identity,
        caller_signing,
        origin,
        app,
    } = paired_receiver().await;
    let caller_id = caller_signing.id().to_owned();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_secs() as i64;
    let url = format!("{origin}/api/multi-server");
    let actions = [
        serde_json::json!({"action":"ui_invite","serverId":"unpaired-peer","requestId":"phone-invite"}),
        serde_json::json!({"action":"discover","requestId":"phone-discover"}),
        serde_json::json!({"action":"alias","serverId":caller_id,"alias":"PHN","requestId":"phone-alias"}),
    ];
    let request = |body: &Value, signed: Option<&'static str>, token: &str| {
        let raw = serde_json::to_vec(body).expect("management body serializes");
        let mut builder = Request::builder()
            .method("POST")
            .uri("/api/multi-server")
            .header("host", "127.0.0.1:4000")
            .header("x-forwarded-host", "receiver.fixture.ts.net")
            .header("x-forwarded-proto", "https")
            .header("x-forwarded-for", "100.64.0.2")
            .header("tailscale-user-login", "owner@example.test")
            .header("content-type", "application/json")
            .header("content-length", raw.len().to_string());
        match signed {
            Some(nonce) => {
                let signed = MultiServerRequest {
                    server_id: receiver_identity["serverId"].as_str().expect("receiver ID"),
                    method: "POST",
                    url: &url,
                    request_id: body["requestId"].as_str().expect("request ID"),
                    timestamp: now,
                    nonce,
                    body: &raw,
                };
                for (name, value) in multi_server_signed_headers(&caller_signing, &signed, now)
                    .expect("fake caller signs request")
                {
                    builder = builder.header(name, value);
                }
            }
            None => {
                builder = builder
                    .header("origin", origin)
                    .header("x-canvas-token", token);
            }
        }
        let mut request = builder.body(Body::from(raw)).expect("request builds");
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:4321"
                .parse::<std::net::SocketAddr>()
                .expect("loopback peer address"),
        ));
        request
    };
    let send = |request: Request<Body>| {
        let app = app.clone();
        async move {
            let response = app.oneshot(request).await.expect("response");
            let status = response.status();
            let body = to_bytes(response.into_body(), 16 * 1024)
                .await
                .expect("response body");
            (
                status,
                serde_json::from_slice::<Value>(&body).expect("JSON response"),
            )
        }
    };
    let audit_rows = || common::rows(&receiver_store);
    let before = audit_rows();
    let nonces = [
        "abcdefghijklmnopqrstuvwxyz012345",
        "bcdefghijklmnopqrstuvwxyz0123456",
        "cdefghijklmnopqrstuvwxyz01234567",
    ];
    for (body, nonce) in actions.iter().zip(nonces) {
        let action = body["action"].as_str().expect("action");
        let message = if action == "ui_invite" {
            "This action requires the owner server session"
        } else {
            "This action requires the local server session"
        };
        assert_eq!(
            send(request(body, Some(nonce), "")).await,
            (
                StatusCode::FORBIDDEN,
                serde_json::json!({"error":message,"code":"local_session_required"})
            ),
            "a paired server credential cannot call {action}"
        );
        let (status, refusal) = send(request(body, None, "wrong")).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{action}: {refusal}");
        assert_eq!(refusal["error"], "Local origin and session token required");
        let owner = send(request(body, None, "token")).await;
        if action == "ui_invite" {
            // The owner session through Serve reaches the invitation service,
            // which refuses only because the selected server is not paired.
            assert_eq!(
                owner,
                (
                    StatusCode::FORBIDDEN,
                    serde_json::json!({"error":"The server is not paired or is revoked"})
                )
            );
        } else {
            assert_eq!(
                owner,
                (
                    StatusCode::FORBIDDEN,
                    serde_json::json!({"error":message,"code":"local_session_required"})
                ),
                "the owner session through Serve cannot call {action}"
            );
        }
    }
    assert_eq!(audit_rows(), before, "no refused request writes state");
}

#[tokio::test]
async fn signed_requests_cross_the_http_boundary_and_missing_owner_is_no_write() {
    let (_root, store) = common::store().await;
    let data = &common::fixture().data;
    // The service corpus uses its frozen clock; this HTTP caller uses wall time.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let frozen_now = data["now"].as_u64().expect("frozen clock");
    let clock_shift = now as i64 - frozen_now as i64;
    store.transaction_with(move |tx| {
        tx.execute("UPDATE runtime_access_invites SET record=json_set(record,'$.expires',json_extract(record,'$.expires')+?1)", [clock_shift])?;
        Ok::<_, studio_runtime_store::StoreError>(())
    }).await.expect("refresh invitation clock");
    let case = &data["auth"][0];
    let state = HttpState::new(
        "token".into(),
        "workspace".into(),
        _root.path().to_string_lossy().into_owned(),
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
    let auto_case = data["auth"]
        .as_array()
        .expect("auth cases")
        .iter()
        .find(|case| case["name"] == "auto-pair")
        .expect("valid auto-pair fixture")
        .clone();
    let (auto_root, auto_store) = common::store().await;
    let auto_lookup = common::CountingOwner::new(auto_case.clone());
    let auto_boundary = studio_runtime_services::multi_server_pairing::MultiServerBoundary::new(
        data["identity"].clone(),
        auto_lookup.clone(),
    )
    .expect("auto-pair boundary");
    let auto_state = HttpState::new(
        "token".into(),
        "workspace".into(),
        auto_root.path().to_string_lossy().into_owned(),
        4000,
        Some("https://local.fixture.ts.net".into()),
        Some(auto_store.clone()),
        None,
    );
    let auto_app = router(
        auto_state
            .clone()
            .with_multi_server_boundary(Arc::new(auto_boundary)),
    )
    .expect("auto-pair router");
    let auto_response = auto_app
        .clone()
        .oneshot(incoming(&auto_case))
        .await
        .expect("signed auto-pair response");
    assert_eq!(auto_response.status(), StatusCode::OK);
    let auto_value: Value = serde_json::from_slice(
        &to_bytes(auto_response.into_body(), 4096)
            .await
            .expect("auto-pair body"),
    )
    .expect("auto-pair JSON");
    assert_eq!(auto_value["clientId"], "auto-server");
    assert_eq!(auto_value["paired"], true);
    let wrong_owner = data["auth"]
        .as_array()
        .expect("auth cases")
        .iter()
        .find(|case| case["name"] == "auto-wrong-owner")
        .expect("wrong-owner fixture");
    let wrong_owner_lookup = common::CountingOwner::new(wrong_owner.clone());
    let wrong_owner_boundary =
        studio_runtime_services::multi_server_pairing::MultiServerBoundary::new(
            data["identity"].clone(),
            wrong_owner_lookup,
        )
        .expect("wrong-owner boundary");
    let wrong_owner_app =
        router(auto_state.with_multi_server_boundary(Arc::new(wrong_owner_boundary)))
            .expect("wrong-owner auto-pair router");
    let before_owner_refusal = common::rows(&auto_store);
    let owner_response = wrong_owner_app
        .oneshot(incoming(wrong_owner))
        .await
        .expect("owner-proof refusal");
    assert_eq!(owner_response.status(), StatusCode::FORBIDDEN);
    assert_eq!(common::rows(&auto_store), before_owner_refusal);
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
    let (root, store) = common::store().await;
    let state = HttpState::new(
        "token".into(),
        "workspace".into(),
        root.path().display().to_string(),
        4000,
        None,
        Some(store.clone()),
        None,
    );
    let app = router(state).expect("router");
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("free loopback port");
    let port = listener.local_addr().expect("listener address").port();
    drop(listener);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
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
        .expect("browser request");
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
    let (root, store) = common::store().await;
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
        .transaction_with(move |tx| {
            tx.execute(
                "INSERT INTO runtime_server_links(id,record) VALUES('fixture-link',?1)",
                [link.to_string()],
            )?;
            AgentRepository::put(tx, &parent)?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("link and owner rows");
    let data = &common::fixture().data["eventRequest"];
    let state = HttpState::new(
        "token".into(),
        "workspace".into(),
        root.path().display().to_string(),
        4000,
        Some("https://local.fixture.ts.net".into()),
        Some(store.clone()),
        None,
    )
    .with_multi_server_boundary(Arc::new(common::boundary()));
    let app = router(state).expect("router");
    let mut receipt = None;
    for nonce in [
        "abcdefghijklmnopqrstuvwxyz012345",
        "bcdefghijklmnopqrstuvwxyz0123456",
    ] {
        let response = app
            .clone()
            .oneshot({
                let mut request = incoming_with_nonce(data, Some(nonce));
                for login in data["logins"].as_array().unwrap() {
                    request.headers_mut().append(
                        "tailscale-user-login",
                        login.as_str().unwrap().parse().unwrap(),
                    );
                }
                request
            })
            .await
            .expect("signed event response");
        assert_eq!(response.status(), StatusCode::OK);
        let replay: Value = serde_json::from_slice(
            &to_bytes(response.into_body(), 4096)
                .await
                .expect("receipt body"),
        )
        .expect("receipt JSON");
        assert_eq!(replay["outcome"], "applied");
        assert_eq!(replay["value"]["eventId"], "python-event-http");
        if let Some(first) = &receipt {
            assert_eq!(&replay, first);
        }
        receipt = Some(replay);
    }
    let db = store.test_reader().expect("test store reader");
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM runtime_events WHERE id='python-event-http' AND agent='event-parent' AND text='signed owner event'",
            [],
            |row| row.get(0),
        )
        .expect("applied event is durable");
    assert_eq!(count, 1);
}

#[tokio::test]
#[allow(clippy::disallowed_methods)]
async fn lifecycle_ports_preserve_configured_accounts_for_inbound_project_add() {
    use studio_runtime_services::agent_lifecycle_runtime::{LifecycleEffect, LifecyclePorts};
    use studio_runtime_services::agent_management::ManagementError;
    use studio_runtime_store::{ProjectsRepository, Transaction};

    struct NoopLifecycle;
    impl LifecyclePorts for NoopLifecycle {
        fn stage(&self, _: &Transaction<'_>, _: &LifecycleEffect) -> Result<(), ManagementError> {
            Ok(())
        }

        fn committed(
            &self,
        ) -> studio_runtime_services::agent_lifecycle_runtime::LifecycleCommitFuture<'_> {
            Box::pin(async { Ok(()) })
        }
    }

    let caller_root = tempfile::tempdir().expect("caller state root");
    let caller_store = Arc::new(
        studio_runtime_store::RuntimeStore::open(Some(caller_root.path()))
            .await
            .expect("caller store opens"),
    );
    let caller_credentials =
        studio_runtime_services::federation_credentials::Credentials::default();
    let caller_identity = caller_credentials
        .identity(
            Arc::clone(&caller_store),
            Some("https://caller.fixture.ts.net"),
        )
        .await
        .expect("caller identity exists");
    let caller_signing =
        load_multi_server_identity(caller_root.path()).expect("caller signing identity loads");

    let receiver_root = tempfile::tempdir().expect("receiver state root");
    let receiver_store = Arc::new(
        studio_runtime_store::RuntimeStore::open(Some(receiver_root.path()))
            .await
            .expect("receiver store opens"),
    );
    let receiver_credentials =
        Arc::new(studio_runtime_services::federation_credentials::Credentials::default());
    let origin = "https://receiver.fixture.ts.net";
    let receiver_identity = receiver_credentials
        .identity(Arc::clone(&receiver_store), Some(origin))
        .await
        .expect("receiver identity exists");
    let caller_identity_for_trust = caller_identity.clone();
    receiver_store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2)",
                rusqlite::params![
                    caller_identity_for_trust["serverId"]
                        .as_str()
                        .expect("caller ID"),
                    serde_json::json!({
                        "id":caller_identity_for_trust["serverId"],
                        "clientId":caller_identity_for_trust["serverId"],
                        "kind":"server",
                        "status":"paired",
                        "label":caller_identity_for_trust["label"],
                        "created":1,
                        "publicKey":caller_identity_for_trust["publicKey"],
                        "origin":"https://caller.fixture.ts.net",
                        "tailscaleUser":"owner@example.test"
                    })
                    .to_string(),
                ],
            )?;
            Ok(())
        })
        .await
        .expect("receiver trusts the paired fake server");

    let policy = Arc::new(
        studio_runtime_services::account_policy::AccountPolicyState::from_startup(
            Some(br#"{"version":1,"defaultAccountKey":"alternate","accounts":{"alternate":{"id":"alternate","home":"/alternate","label":"Alternate","source":"Codex CLI","status":"signedOut"}},"logins":{}}"#),
            [],
        )
        .expect("configured account snapshot"),
    );
    let boundary = MultiServerBoundary::new(receiver_identity.clone(), Arc::new(FixtureOwner))
        .expect("receiver boundary is ready");
    let state = HttpState::new(
        "token".into(),
        "workspace".into(),
        receiver_root.path().display().to_string(),
        4000,
        Some(origin.into()),
        Some(Arc::clone(&receiver_store)),
        Some(policy),
    )
    .with_credentials(Arc::clone(&receiver_credentials))
    .with_multi_server_boundary(Arc::new(boundary))
    .with_tailscale_whoami(Arc::new(|| {
        Box::pin(async { Some(serde_json::json!({"user":"owner@example.test"})) })
    }))
    .with_lifecycle_ports(Arc::new(NoopLifecycle));
    let app = router(state).expect("receiver router");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_secs() as i64;
    let project_path = receiver_root.path().join("new-remote-project");
    std::fs::create_dir_all(&project_path).expect("unregistered project directory");
    let raw = serde_json::to_vec(&serde_json::json!({
        "action":"add_location",
        "requestId":"remote-project-add",
        "payload":{
            "project":"remote-home-project",
            "path":project_path.to_string_lossy(),
            "name":"Remote project"
        }
    }))
    .expect("orchestration body serializes");
    let url = format!("{origin}/api/servers/orchestration");
    let signed = MultiServerRequest {
        server_id: receiver_identity["serverId"].as_str().expect("receiver ID"),
        method: "POST",
        url: &url,
        request_id: "remote-project-add",
        timestamp: now,
        nonce: "qrstuvwxyz0123456789abcdefghijkl",
        body: &raw,
    };
    let signed_headers = multi_server_signed_headers(&caller_signing, &signed, now)
        .expect("fake caller signs request");
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/servers/orchestration")
        .header("host", "127.0.0.1:4000")
        .header("x-forwarded-host", "receiver.fixture.ts.net")
        .header("x-forwarded-proto", "https")
        .header("x-forwarded-for", "100.64.0.2")
        .header("tailscale-user-login", "owner@example.test")
        .header("content-type", "application/json")
        .header("content-length", raw.len().to_string());
    for (name, value) in signed_headers {
        builder = builder.header(name, value);
    }
    let mut request = builder
        .body(Body::from(raw))
        .expect("signed orchestration request builds");
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:4321"
            .parse::<std::net::SocketAddr>()
            .expect("loopback peer address"),
    ));
    let response = app.oneshot(request).await.expect("project add response");
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), 16 * 1024)
            .await
            .expect("project add body"),
    )
    .expect("project add JSON");
    assert_eq!(value["outcome"], "applied");
    let project_id = value["value"]["projectId"]
        .as_str()
        .expect("created project ID");
    let reader = receiver_store.test_reader().expect("project store read");
    let (_, project) = ProjectsRepository::get(&reader, project_id)
        .expect("created project loads")
        .expect("created project exists");
    assert_eq!(project["accountKey"], "alternate");
}

/// Sign `body` for the receiver as `signing` and build the request that Tailscale Serve
/// forwards to the backend.
fn signed_api_request(
    signing: &studio_runtime_services::remote_transport::RemoteIdentity,
    receiver: &Value,
    origin: &str,
    method: &'static str,
    target: &str,
    body: &[u8],
    nonce: &str,
) -> Request<Body> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_secs() as i64;
    let url = format!("{origin}{target}");
    let request_id = format!("signed-api-{nonce}");
    let signed = MultiServerRequest {
        server_id: receiver["serverId"].as_str().expect("receiver ID"),
        method,
        url: &url,
        request_id: &request_id,
        timestamp: now,
        nonce,
        body,
    };
    let mut builder = Request::builder()
        .method(method)
        .uri(target)
        .header("host", "127.0.0.1:4000")
        .header("x-forwarded-host", "receiver.fixture.ts.net")
        .header("x-forwarded-proto", "https")
        .header("x-forwarded-for", "100.64.0.2")
        .header("tailscale-user-login", "owner@example.test")
        .header("content-type", "application/json")
        .header("content-length", body.len().to_string());
    for (name, value) in multi_server_signed_headers(signing, &signed, now).expect("signs") {
        builder = builder.header(name, value);
    }
    let mut request = builder
        .body(Body::from(body.to_vec()))
        .expect("signed request builds");
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:4321"
            .parse::<std::net::SocketAddr>()
            .expect("loopback peer address"),
    ));
    request
}

/// A paired Studio window reads and writes the ordinary API with signed requests. The
/// Python boundary dispatched them; the Rust boundary answered 501 for every route except
/// server management, so a window showed no chats of a paired Rust server.
#[tokio::test]
async fn a_paired_window_reaches_the_ordinary_api_with_signed_requests() {
    let PairedReceiver {
        _caller_root,
        _receiver_root,
        receiver_store,
        receiver_identity,
        caller_signing: _,
        origin,
        app,
    } = paired_receiver().await;
    // The window's own key, paired as a UI client.
    let window_root = tempfile::tempdir().expect("window key root");
    let window_store = Arc::new(
        studio_runtime_store::RuntimeStore::open(Some(window_root.path()))
            .await
            .expect("window key store opens"),
    );
    let window_identity = studio_runtime_services::federation_credentials::Credentials::default()
        .identity(
            Arc::clone(&window_store),
            Some("https://window.fixture.ts.net"),
        )
        .await
        .expect("window key exists");
    let window_signing =
        load_multi_server_identity(window_root.path()).expect("window signing key loads");
    let window_id = window_signing.id().to_owned();
    let window_key = window_identity["publicKey"].clone();
    let row_id = window_id.clone();
    receiver_store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2)",
                rusqlite::params![
                    row_id,
                    serde_json::json!({
                        "id":row_id,"clientId":row_id,"kind":"ui","status":"paired",
                        "label":"Studio UI","created":1,"publicKey":window_key,
                        "origin":null,"serverId":null,"revoked":null,
                        "tailscaleUser":"owner@example.test"
                    })
                    .to_string(),
                ],
            )?;
            Ok(())
        })
        .await
        .expect("the receiver trusts the window");
    let last_access = || {
        receiver_store
            .test_read_transaction(|tx| {
                Ok(tx.query_row(
                    "SELECT json_extract(record,'$.lastAccess') FROM runtime_access_clients WHERE id=?1",
                    [&window_id],
                    |row| row.get::<_, Option<f64>>(0),
                )?)
            })
            .expect("the window row reads")
    };
    assert_eq!(last_access(), None);

    let read = app
        .clone()
        .oneshot(signed_api_request(
            &window_signing,
            &receiver_identity,
            origin,
            "GET",
            "/api/ui-summary",
            b"",
            "windowreadnonce00000000000000001",
        ))
        .await
        .expect("signed read answers");
    let status = read.status();
    let server_header = read
        .headers()
        .get("x-studio-server")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = to_bytes(read.into_body(), 1024 * 1024).await.expect("body");
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let summary: Value = serde_json::from_slice(&body).expect("summary JSON");
    assert!(
        summary["chats"].is_array(),
        "the window gets the chat summary"
    );
    assert_eq!(
        server_header.as_deref(),
        receiver_identity["serverId"].as_str(),
        "the response names the server that answered"
    );
    assert!(last_access().is_some(), "the access is recorded");

    // The same signed request again is a replay.
    let replay = app
        .clone()
        .oneshot(signed_api_request(
            &window_signing,
            &receiver_identity,
            origin,
            "GET",
            "/api/ui-summary",
            b"",
            "windowreadnonce00000000000000001",
        ))
        .await
        .expect("replay answers");
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);

    // A signed write reaches its route without the window holding a session token.
    let write_body = br#"{"scope":"server","fields":{}}"#;
    let write = app
        .clone()
        .oneshot(signed_api_request(
            &window_signing,
            &receiver_identity,
            origin,
            "POST",
            "/api/sync/preferences",
            write_body,
            "windowwritenonce0000000000000002",
        ))
        .await
        .expect("signed write answers");
    let status = write.status();
    let body = to_bytes(write.into_body(), 1024 * 1024)
        .await
        .expect("body");
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    // A forged signature and a revoked window reach no route.
    let mut forged = signed_api_request(
        &window_signing,
        &receiver_identity,
        origin,
        "GET",
        "/api/ui-summary",
        b"",
        "windowforgednonce000000000000003",
    );
    forged
        .headers_mut()
        .insert("x-studio-signature", "AAAA".parse().expect("header value"));
    let forged = app.clone().oneshot(forged).await.expect("forged answers");
    assert_eq!(forged.status(), StatusCode::UNAUTHORIZED);
    let revoked_id = window_id.clone();
    receiver_store
        .transaction(move |tx| {
            tx.execute(
                "UPDATE runtime_access_clients SET record=json_set(record,'$.status','revoked','$.revoked',2) WHERE id=?1",
                [&revoked_id],
            )?;
            Ok(())
        })
        .await
        .expect("the window is revoked");
    let revoked = app
        .clone()
        .oneshot(signed_api_request(
            &window_signing,
            &receiver_identity,
            origin,
            "GET",
            "/api/ui-summary",
            b"",
            "windowrevokednonce00000000000004",
        ))
        .await
        .expect("revoked answers");
    assert!(
        revoked.status().is_client_error(),
        "a revoked window is refused: {}",
        revoked.status()
    );
}
