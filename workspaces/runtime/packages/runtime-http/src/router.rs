use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware as axum_middleware;
use axum::response::Response;
use axum::routing::any;
use std::collections::BTreeSet;
use std::sync::Arc;
use studio_runtime_services::change_notifications::ChangeNotificationHub;
use studio_runtime_services::token_rate::TokenRateHub;
use studio_runtime_store::RuntimeStore;
use tokio::sync::Notify;

use crate::error::error_response;
use crate::middleware;
use crate::routes::{self, compose};
use crate::schema::{matching_template, operations};
use crate::validation::Registry;

#[derive(Clone)]
pub struct HttpState {
    pub claude_login: Option<Arc<studio_runtime_services::claude_login::service::LoginService>>,
    pub account_driver:
        Option<std::sync::Arc<studio_runtime_services::account_driver::AccountDriver>>,
    pub(crate) voice_provider:
        Option<Arc<dyn studio_runtime_services::voice::provider::VoiceProviderPort>>,
    pub(crate) credentials: Arc<studio_runtime_services::federation_credentials::Credentials>,
    pub(crate) token: String,
    pub(crate) workspace_id: String,
    pub(crate) native_path: Option<String>,
    pub(crate) state_dir: String,
    pub(crate) port: u16,
    pub(crate) public_origin: Option<String>,
    pub(crate) validators: Registry,
    pub(crate) store: Option<Arc<RuntimeStore>>,
    pub(crate) account_policy:
        Option<Arc<studio_runtime_services::account_policy::AccountPolicyState>>,
    pub(crate) tool_catalog: Option<Arc<studio_runtime_services::thread_prepare::CatalogSnapshot>>,
    pub(crate) model_catalog: studio_runtime_services::model_catalog::ModelCatalogService,
    pub(crate) capabilities_cache: Arc<studio_runtime_services::catalog_reads::CapabilitiesCache>,
    pub(crate) resource_notifications:
        Arc<studio_runtime_services::resource_notify::ResourceNotifications>,
    pub(crate) sync_changes: Arc<ChangeNotificationHub>,
    pub(crate) token_rates: Arc<TokenRateHub>,
    pub(crate) event_wake: Arc<Notify>,
    pub(crate) etag_operations: BTreeSet<(String, String)>,
}

impl HttpState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        token: String,
        workspace_id: String,
        native_path: Option<String>,
        state_dir: String,
        port: u16,
        public_origin: Option<String>,
        store: Option<Arc<RuntimeStore>>,
        account_policy: Option<Arc<studio_runtime_services::account_policy::AccountPolicyState>>,
    ) -> Self {
        let sync_changes = Arc::new(ChangeNotificationHub::new());
        let model_changes = Arc::clone(&sync_changes);
        let model_catalog =
            studio_runtime_services::model_catalog::ModelCatalogService::with_change_callback(
                native_path.clone(),
                Arc::new(move || model_changes.notify_one()),
            );
        if let Some(store) = &store {
            let changes = Arc::clone(&sync_changes);
            store.set_commit_notifier(move || changes.notify_one());
        }
        Self {
            account_driver: None,
            claude_login: None,
            voice_provider: None,
            credentials: Arc::default(),
            token,
            workspace_id,
            native_path,
            state_dir,
            port,
            public_origin,
            validators: Registry::default(),
            store,
            account_policy,
            tool_catalog: None,
            model_catalog,
            capabilities_cache: Arc::new(Default::default()),
            resource_notifications: Arc::new(
                studio_runtime_services::resource_notify::ResourceNotifications::new(Arc::clone(
                    &sync_changes,
                )),
            ),
            sync_changes: Arc::clone(&sync_changes),
            token_rates: Arc::new(TokenRateHub::with_change_callback({
                let changes = Arc::clone(&sync_changes);
                Arc::new(move || changes.notify_one())
            })),
            event_wake: Arc::new(Notify::new()),
            etag_operations: BTreeSet::new(),
        }
    }

    /// Attach the retained account refresh driver used by account admission.
    pub fn with_account_driver(
        mut self,
        driver: Arc<studio_runtime_services::account_driver::AccountDriver>,
    ) -> Self {
        self.account_driver = Some(driver);
        self
    }

    /// Attach the account worker's generic provider-session voice adapter.
    pub fn with_voice_provider(
        mut self,
        provider: Arc<dyn studio_runtime_services::voice::provider::VoiceProviderPort>,
    ) -> Self {
        self.voice_provider = Some(provider);
        self
    }

    /// Attach the full selected native tool catalog for capabilities reads.
    pub fn with_tool_catalog(
        mut self,
        catalog: studio_runtime_services::thread_prepare::CatalogSnapshot,
    ) -> Self {
        self.tool_catalog = Some(Arc::new(catalog));
        self
    }

    pub async fn shutdown_services(&self) -> std::io::Result<()> {
        let model = self.model_catalog.shutdown().await;
        if let Some(login) = &self.claude_login {
            login.shutdown().await.map_err(std::io::Error::other)?;
        }
        model
    }

    /// Shared post-commit signal for durable event writers and the scheduler.
    pub fn event_wake_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.event_wake)
    }

    pub fn resource_notifications_handle(
        &self,
    ) -> Arc<studio_runtime_services::resource_notify::ResourceNotifications> {
        Arc::clone(&self.resource_notifications)
    }

    pub fn sync_changes_handle(&self) -> Arc<ChangeNotificationHub> {
        Arc::clone(&self.sync_changes)
    }

    /// Shared publisher used by the provider account worker to publish token rates.
    pub fn token_rate_handle(&self) -> Arc<TokenRateHub> {
        Arc::clone(&self.token_rates)
    }
}

pub fn router(mut state: HttpState) -> Result<Router, Box<dyn std::error::Error>> {
    let authority = operations().map_err(|error| {
        std::io::Error::other(format!("cannot load committed OpenAPI operations: {error}"))
    })?;
    let (router, registered) = compose::compose(routes::groups(), authority).map_err(|error| {
        std::io::Error::other(format!("invalid HTTP route registration: {error}"))
    })?;
    state.validators =
        Registry::compile(&registered.iter().map(|item| item.key()).collect::<Vec<_>>())?;
    state.etag_operations = registered
        .iter()
        .filter_map(|operation| {
            let key = operation.key();
            crate::schema::operation_has_etag(&key.0, &key.1).then_some(key)
        })
        .collect();

    let router = router.merge(routes::voice::fallback());
    Ok(with_boundary(router, state))
}

#[cfg(test)]
pub(crate) fn validation_probe_router(
    mut state: HttpState,
) -> Result<Router, Box<dyn std::error::Error>> {
    let authority = operations().map_err(|error| {
        std::io::Error::other(format!("cannot load committed OpenAPI operations: {error}"))
    })?;
    let document = crate::schema::openapi_document();
    let mut registrations = Vec::with_capacity(authority.len());
    for (method, path) in authority {
        let operation = document
            .pointer(&format!(
                "/paths/{}/{}",
                path.replace('~', "~0").replace('/', "~1"),
                method.to_lowercase()
            ))
            .ok_or_else(|| {
                std::io::Error::other(format!("missing probe operation {method} {path}"))
            })?;
        let body_schema = operation.pointer("/requestBody/content/application~1json/schema");
        let required = operation
            .pointer("/requestBody/required")
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        let handler = match (body_schema.is_some(), required) {
            (true, true) => probe_required_body as ProbeHandler,
            (true, false) => probe_optional_body as ProbeHandler,
            (false, _) => probe_without_body as ProbeHandler,
        };
        let path: &'static str = Box::leak(path.clone().into_boxed_str());
        let status = crate::routes::manifest::OperationStatus::Implemented;
        let registration = match method.as_str() {
            "GET" => crate::routes::manifest::get(path, status, handler),
            "POST" => crate::routes::manifest::post(path, status, handler),
            "PUT" => crate::routes::manifest::put(path, status, handler),
            "DELETE" => crate::routes::manifest::delete(path, status, handler),
            "PATCH" => crate::routes::manifest::patch(path, status, handler),
            "OPTIONS" => crate::routes::manifest::options(path, status, handler),
            "HEAD" => crate::routes::manifest::head(path, status, handler),
            "TRACE" => crate::routes::manifest::trace(path, status, handler),
            _ => {
                return Err(
                    std::io::Error::other(format!("unsupported probe method {method}")).into(),
                );
            }
        };
        registrations.push(registration);
    }
    let probe_group = crate::routes::manifest::RouteGroup::new(registrations);
    let (router, registered) = compose::compose(vec![probe_group], authority).map_err(|error| {
        std::io::Error::other(format!("invalid HTTP route registration: {error}"))
    })?;
    state.validators =
        Registry::compile(&registered.iter().map(|item| item.key()).collect::<Vec<_>>())?;
    state.etag_operations = registered
        .iter()
        .filter_map(|operation| {
            let key = operation.key();
            crate::schema::operation_has_etag(&key.0, &key.1).then_some(key)
        })
        .collect();
    Ok(with_boundary(router, state))
}

#[cfg(test)]
type ProbeHandler = fn(
    axum::extract::State<HttpState>,
    Request<Body>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>;

#[cfg(test)]
fn probe_required_body(
    state: axum::extract::State<HttpState>,
    request: Request<Body>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    Box::pin(async move { probe_echo(state.0, request, 1).await })
}

#[cfg(test)]
fn probe_optional_body(
    state: axum::extract::State<HttpState>,
    request: Request<Body>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    Box::pin(async move { probe_echo(state.0, request, 2).await })
}

#[cfg(test)]
fn probe_without_body(
    state: axum::extract::State<HttpState>,
    request: Request<Body>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    Box::pin(async move { probe_echo(state.0, request, 0).await })
}

#[cfg(test)]
async fn probe_echo(state: HttpState, request: Request<Body>, body_kind: u8) -> Response {
    use axum::extract::{FromRequest, FromRequestParts};
    use axum::response::IntoResponse;
    use serde_json::{Map, Value};

    let (mut parts, body) = request.into_parts();
    let query = match crate::routes::extractors::QueryParams::from_request_parts(&mut parts, &state)
        .await
    {
        Ok(query) => query,
        Err(error) => return error.into_response(),
    };
    let headers =
        match crate::routes::extractors::ApiHeaders::from_request_parts(&mut parts, &state).await {
            Ok(headers) => headers,
            Err(error) => return error.into_response(),
        };
    let request = Request::from_parts(parts, body);
    let body_value: Option<Value> = match body_kind {
        1 => match crate::routes::extractors::ApiJson::<Value>::from_request(request, &state).await
        {
            Ok(value) => Some(value.0),
            Err(error) => return error.into_response(),
        },
        2 => {
            match crate::routes::extractors::ApiJsonOptional::<Value>::from_request(request, &state)
                .await
            {
                Ok(value) => value.0,
                Err(error) => return error.into_response(),
            }
        }
        _ => None,
    };
    let mut value = Map::new();
    if let Some(body) = body_value {
        value.insert("body".to_owned(), body);
    }
    value.insert("query".to_owned(), Value::Object(query.into_typed()));
    value.insert("headers".to_owned(), Value::Object(headers.0));
    axum::Json(Value::Object(value)).into_response()
}

fn with_boundary(router: Router<HttpState>, state: HttpState) -> Router {
    router
        .method_not_allowed_fallback(any(dispatch))
        .fallback(any(dispatch))
        .layer(axum_middleware::from_fn_with_state(
            state.clone(),
            middleware::boundary,
        ))
        .layer(axum_middleware::from_fn_with_state(
            state.clone(),
            middleware::response_safety,
        ))
        .with_state(state)
}

async fn dispatch(request: Request<Body>) -> Response {
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    let documented = operations().is_ok_and(|operations| {
        matching_template(
            operations
                .iter()
                .map(|(operation_method, template)| (operation_method.as_str(), template.as_str())),
            Some(&method),
            &path,
        )
        .is_some()
    });
    let hidden_voice = method == "POST"
        && path
            .strip_prefix("/api/voice/")
            .is_some_and(|action| !action.is_empty());
    if documented || hidden_voice {
        return error_response(StatusCode::NOT_IMPLEMENTED, "Not implemented");
    }
    let documented_path = operations().is_ok_and(|operations| {
        matching_template(
            operations
                .iter()
                .map(|(operation_method, template)| (operation_method.as_str(), template.as_str())),
            None,
            &path,
        )
        .is_some()
    });
    if documented_path && method != "GET" {
        return error_response(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed");
    }
    error_response(StatusCode::NOT_FOUND, "Not found")
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use axum::body::Body;
    use axum::body::to_bytes;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::middleware;

    use super::{HttpState, router};

    fn probe_app() -> Router {
        super::validation_probe_router(state()).expect("probe router starts")
    }

    async fn value_handler(
        crate::routes::extractors::ApiJson(value): crate::routes::extractors::ApiJson<Value>,
    ) -> Json<Value> {
        Json(value)
    }

    fn state() -> HttpState {
        HttpState::new(
            "token".to_owned(),
            "workspace".to_owned(),
            None,
            "state".to_owned(),
            4620,
            None,
            None,
            None,
        )
    }

    #[tokio::test]
    async fn startup_compiles_registered_validators_and_matches_python_methods() {
        let app = router(state()).expect("router and registered OpenAPI validators start");
        let peer = ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345));
        let request = Request::builder()
            .method("GET")
            .uri("/api/session?extra=1")
            .header("host", "127.0.0.1:4620")
            .extension(peer)
            .body(Body::empty())
            .expect("request builds");
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let request = Request::builder()
            .method("HEAD")
            .uri("/api/session")
            .header("host", "127.0.0.1:4620")
            .extension(peer)
            .body(Body::empty())
            .expect("request builds");
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn boundary_early_returns_include_api_headers() {
        let app = router(state()).expect("router starts");
        let request = Request::builder()
            .method("GET")
            .uri("/api/session")
            .header("host", "example.invalid")
            .body(Body::empty())
            .expect("request builds");
        let response = app.clone().oneshot(request).await.expect("response");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response.headers()[crate::schema::API_SCHEMA_HASH_HEADER],
            crate::schema::api_schema_hash()
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        assert_eq!(
            response.headers()["content-security-policy"],
            crate::error::CSP
        );

        let peer = ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345));
        let preflight = Request::builder()
            .method("OPTIONS")
            .uri("/api/accounts/login")
            .header("host", "studio-a.localhost:4620")
            .header("origin", "http://studio-a.localhost:4620")
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "content-type")
            .extension(peer)
            .body(Body::empty())
            .expect("preflight request builds");
        let response = app.oneshot(preflight).await.expect("preflight response");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response.headers()[crate::schema::API_SCHEMA_HASH_HEADER],
            crate::schema::api_schema_hash()
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        assert_eq!(
            response.headers()["content-security-policy"],
            crate::error::CSP
        );
    }

    #[tokio::test]
    async fn huge_integer_body_and_query_return_located_client_errors() {
        let app = probe_app();
        let peer = ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345));
        let huge = "9".repeat(401);
        let body = format!("{{\"prompt\":\"x\",\"concurrency\":{huge}}}");
        let request = Request::builder()
            .method("POST")
            .uri("/api/agents")
            .header("host", "127.0.0.1:4620")
            .header("content-type", "application/json")
            .header("x-canvas-token", "token")
            .header(
                crate::schema::API_SCHEMA_HASH_HEADER,
                crate::schema::api_schema_hash(),
            )
            .extension(peer)
            .body(Body::from(body))
            .expect("request builds");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_slice(
            &to_bytes(response.into_body(), 1024)
                .await
                .expect("body reads"),
        )
        .expect("error is JSON");
        assert_eq!(body["error"], "Invalid request");
        assert_eq!(body["location"], serde_json::json!(["body"]));

        let request = Request::builder()
            .method("GET")
            .uri(format!("/api/agent-chat?before={huge}"))
            .header("host", "127.0.0.1:4620")
            .extension(peer)
            .body(Body::empty())
            .expect("request builds");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_slice(
            &to_bytes(response.into_body(), 1024)
                .await
                .expect("body reads"),
        )
        .expect("error is JSON");
        assert_eq!(body["error"], "Invalid request");
        assert_eq!(body["location"], serde_json::json!(["query", "before"]));
    }

    #[tokio::test]
    async fn deeply_nested_registered_json_body_is_rejected_before_value_extraction() {
        let authority = crate::schema::operations().expect("schema parses");
        let mut app_state = state();
        app_state.validators =
            crate::validation::Registry::compile(&authority.iter().cloned().collect::<Vec<_>>())
                .expect("registered schema validators compile");
        let app = Router::new()
            .route("/api/federation", post(value_handler))
            .layer(axum::middleware::from_fn_with_state(
                app_state.clone(),
                middleware::response_safety,
            ))
            .layer(axum::middleware::from_fn_with_state(
                app_state.clone(),
                middleware::boundary,
            ))
            .with_state(app_state);
        let peer = ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345));
        let nested = format!("{}0{}", "[".repeat(10_000), "]".repeat(10_000));
        let request = Request::builder()
            .method("POST")
            .uri("/api/federation")
            .header("host", "127.0.0.1:4620")
            .header("content-type", "application/json")
            .header("x-canvas-token", "token")
            .header(
                crate::schema::API_SCHEMA_HASH_HEADER,
                crate::schema::api_schema_hash(),
            )
            .extension(peer)
            .body(Body::from(nested))
            .expect("request builds");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_slice(
            &to_bytes(response.into_body(), 1024)
                .await
                .expect("body reads"),
        )
        .expect("validation envelope is JSON");
        assert_eq!(body["error"], "Invalid request");
        assert_eq!(body["location"], serde_json::json!(["body"]));
    }

    #[tokio::test]
    async fn optional_required_and_chunked_json_bodies_follow_openapi_contract() {
        let app = probe_app();
        let peer = ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345));
        let build = |method: &str, uri: &str, body: Body, content_type: Option<&str>| {
            let mut request = Request::builder()
                .method(method)
                .uri(uri)
                .header("host", "127.0.0.1:4620")
                .header("x-canvas-token", "token")
                .header(
                    crate::schema::API_SCHEMA_HASH_HEADER,
                    crate::schema::api_schema_hash(),
                )
                .extension(peer);
            if let Some(content_type) = content_type {
                request = request.header("content-type", content_type);
            }
            request.body(body).expect("request builds")
        };

        let optional = app
            .clone()
            .oneshot(build("POST", "/api/accounts/discover", Body::empty(), None))
            .await
            .expect("optional body response");
        assert_eq!(optional.status(), StatusCode::OK);

        let unknown_length_empty = Body::from_stream(futures_util::stream::empty::<
            Result<axum::body::Bytes, std::convert::Infallible>,
        >());
        let empty_chunked = app
            .clone()
            .oneshot(build(
                "POST",
                "/api/accounts/discover",
                unknown_length_empty,
                None,
            ))
            .await
            .expect("empty chunked optional body response");
        assert_eq!(empty_chunked.status(), StatusCode::OK);

        let required = app
            .clone()
            .oneshot(build("POST", "/api/agents", Body::empty(), None))
            .await
            .expect("required body response");
        assert_eq!(required.status(), StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_slice(
            &to_bytes(required.into_body(), 1024)
                .await
                .expect("body reads"),
        )
        .expect("validation error JSON");
        assert_eq!(body["location"], serde_json::json!(["body"]));

        // A Body with no Content-Length models a chunked request in the in-process harness.
        let chunked = app
            .oneshot(build(
                "POST",
                "/api/agents",
                Body::from(r#"{"prompt":"x"}"#),
                Some("application/json"),
            ))
            .await
            .expect("chunked body response");
        assert_eq!(chunked.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn encoded_repeated_scalar_query_parameter_is_refused_by_router() {
        let app = probe_app();
        let peer = ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345));
        let request = Request::builder()
            .method("GET")
            .uri("/api/agent-chat?%62efore=1&before=1")
            .header("host", "127.0.0.1:4620")
            .extension(peer)
            .body(Body::empty())
            .expect("request builds");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_slice(
            &to_bytes(response.into_body(), 1024)
                .await
                .expect("body reads"),
        )
        .expect("error envelope parses");
        assert_eq!(body["location"], serde_json::json!(["query", "before"]));
    }

    #[tokio::test]
    async fn boundary_accepts_each_python_body_limit_and_rejects_the_next_byte() {
        let cases = [
            ("/api/agents", r#"{"prompt":"x"}"#, 256 * 1024),
            (
                "/api/assets",
                r#"{"agent":"x","name":"x"}"#,
                28 * 1024 * 1024,
            ),
            (
                "/api/projects",
                r#"{"action":"add_location","project":"x","request_id":"00000000-0000-4000-8000-000000000001","server":"x"}"#,
                6 * 1024 * 1024,
            ),
            (
                "/api/voice/audio",
                r#"{"agent":"x","audio":"x","chunk_id":"x","mime":"x","session_id":"x"}"#,
                6 * 1024 * 1024,
            ),
            (
                "/api/federation",
                r#"{"action":"accept_peer","invitation":{"expires":1,"inviteId":"x","label":"x","origin":"x","protocol":1,"publicKey":"x","stateId":"x","token":"x"}}"#,
                256 * 1024,
            ),
        ];
        let peer = ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345));
        for (path, valid_json, limit) in cases {
            let app = router(state()).expect("router starts");
            let at_limit_status = if path == "/api/assets" {
                StatusCode::BAD_REQUEST
            } else if path == "/api/voice/audio" {
                // This route is implemented; the boundary-only fixture has no store.
                StatusCode::INTERNAL_SERVER_ERROR
            } else {
                StatusCode::NOT_IMPLEMENTED
            };
            for (length, expected) in [
                (limit, at_limit_status),
                (limit + 1, StatusCode::PAYLOAD_TOO_LARGE),
            ] {
                let opaque_scans_before = crate::validation::opaque_body_scan_count();
                let mut body = Vec::with_capacity(length);
                body.extend_from_slice(valid_json.as_bytes());
                body.resize(length, b' ');
                let request = Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("host", "127.0.0.1:4620")
                    .header("content-type", "application/json")
                    .header("content-length", length.to_string())
                    .header("x-canvas-token", "token")
                    .header(
                        crate::schema::API_SCHEMA_HASH_HEADER,
                        crate::schema::api_schema_hash(),
                    )
                    .extension(peer)
                    .body(Body::from(body))
                    .expect("request builds");
                let response = app.clone().oneshot(request).await.expect("response");
                assert_eq!(response.status(), expected, "{path} with {length} bytes");
                if path == "/api/assets" && length == limit {
                    assert_eq!(
                        crate::validation::opaque_body_scan_count(),
                        opaque_scans_before,
                        "asset upload schema has no opaque JSON positions and must not run the raw-token scan"
                    );
                }
            }
        }
    }
}
