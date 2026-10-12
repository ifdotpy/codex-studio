use crate::{
    error::{CSP, error_response, with_api_headers},
    middleware::trusted,
    router::route_matches,
    schema::{API_SCHEMA_HASH_HEADER, api_schema_hash, operations},
};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};

#[test]
fn schema_hash_matches_python_generated_identity() {
    let client_schema =
        include_str!("../../../../../workspaces/client/apps/web/src/generated/apiSchema.ts");
    let client_hash = client_schema
        .split('"')
        .nth(1)
        .expect("generated TypeScript schema hash is present");
    assert_eq!(api_schema_hash(), client_hash);
}

#[test]
fn route_template_matching_is_segment_exact() {
    assert!(route_matches("/api/agents/{id}", "/api/agents/a"));
    assert!(!route_matches("/api/agents/{id}", "/api/agents/a/b"));
}

#[test]
fn trust_requires_loopback_origin_and_constant_time_token_match() {
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:4312"));
    assert!(trusted(
        &headers,
        false,
        "token",
        true,
        4312,
        &None,
        "/api/probe"
    ));
    assert!(!trusted(
        &headers,
        false,
        "token",
        false,
        4312,
        &None,
        "/api/probe"
    ));
    assert!(!trusted(
        &headers,
        true,
        "token",
        true,
        4312,
        &None,
        "/api/probe"
    ));
    headers.insert("x-canvas-token", HeaderValue::from_static("token"));
    assert!(trusted(
        &headers,
        true,
        "token",
        true,
        4312,
        &None,
        "/api/probe"
    ));
    headers.insert(
        header::ORIGIN,
        HeaderValue::from_static("https://attacker.example"),
    );
    assert!(!trusted(
        &headers,
        false,
        "token",
        true,
        4312,
        &None,
        "/api/probe"
    ));
}

#[test]
fn forwarded_origins_require_the_configured_tailnet_proxy() {
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:4312"));
    headers.insert(
        header::ORIGIN,
        HeaderValue::from_static("https://studio.ts.net"),
    );
    headers.insert(
        "x-forwarded-host",
        HeaderValue::from_static("studio.ts.net"),
    );
    headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
    headers.insert("x-forwarded-for", HeaderValue::from_static("100.64.0.9"));
    let public_origin = Some("https://studio.ts.net".to_owned());
    assert!(trusted(
        &headers,
        false,
        "token",
        true,
        4312,
        &public_origin,
        "/api/probe"
    ));
    headers.insert("x-forwarded-for", HeaderValue::from_static("192.0.2.9"));
    assert!(!trusted(
        &headers,
        false,
        "token",
        true,
        4312,
        &public_origin,
        "/api/probe"
    ));
}

#[test]
fn signed_federation_writes_do_not_require_the_local_session_token() {
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:4312"));
    assert!(trusted(
        &headers,
        true,
        "token",
        true,
        4312,
        &None,
        "/api/federation/v1/message"
    ));
}

#[test]
fn api_responses_include_schema_and_security_headers() {
    let response = with_api_headers(
        error_response(StatusCode::NOT_IMPLEMENTED, "Not implemented"),
        None,
    );
    assert_eq!(
        response.headers()[API_SCHEMA_HASH_HEADER],
        api_schema_hash()
    );
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(response.headers()["referrer-policy"], "no-referrer");
    assert_eq!(response.headers()["content-security-policy"], CSP);
}

#[test]
fn schema_contains_159_api_operations() {
    assert_eq!(
        operations()
            .expect("committed schema parses")
            .iter()
            .filter(|(_, path)| path.starts_with("/api/"))
            .count(),
        159
    );
}
