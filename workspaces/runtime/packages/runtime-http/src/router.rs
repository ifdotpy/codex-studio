use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware as axum_middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::any;

use crate::error::error_response;
use crate::middleware;
use crate::routes;
use crate::schema::operations;

#[derive(Clone)]
pub struct HttpState {
    pub(crate) token: String,
    pub(crate) workspace_id: String,
    pub(crate) native_path: Option<String>,
    pub(crate) state_dir: String,
    pub(crate) port: u16,
    pub(crate) public_origin: Option<String>,
}

impl HttpState {
    pub fn new(
        token: String,
        workspace_id: String,
        native_path: Option<String>,
        state_dir: String,
        port: u16,
        public_origin: Option<String>,
    ) -> Self {
        Self {
            token,
            workspace_id,
            native_path,
            state_dir,
            port,
            public_origin,
        }
    }
}

pub fn router(state: HttpState) -> Router {
    Router::new()
        .fallback(any(dispatch))
        .layer(axum_middleware::from_fn_with_state(
            state.clone(),
            middleware::boundary,
        ))
        .with_state(state)
}

async fn dispatch(State(state): State<HttpState>, request: Request<Body>) -> Response {
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    if path == "/api/session" && method == "GET" {
        return routes::session::get(State(state)).await.into_response();
    }
    if path == "/api/sync/identity" && method == "GET" {
        return routes::sync::identity(State(state)).await.into_response();
    }
    if path == "/api/desktop" && method == "GET" {
        return routes::desktop::get(State(state)).await.into_response();
    }
    let documented = operations().is_ok_and(|operations| {
        operations.iter().any(|(registered_method, route)| {
            registered_method == &method && route_matches(route, &path)
        })
    });
    let hidden_voice = method == "POST"
        && path
            .strip_prefix("/api/voice/")
            .is_some_and(|action| !action.is_empty());
    if documented || hidden_voice {
        return error_response(StatusCode::NOT_IMPLEMENTED, "Not implemented");
    }
    error_response(StatusCode::NOT_FOUND, "Not found")
}

pub(crate) fn route_matches(route: &str, path: &str) -> bool {
    let route_parts = route.split('/').collect::<Vec<_>>();
    let path_parts = path.split('/').collect::<Vec<_>>();
    route_parts.len() == path_parts.len()
        && route_parts
            .iter()
            .zip(path_parts)
            .all(|(expected, actual)| expected.starts_with('{') || *expected == actual)
}
