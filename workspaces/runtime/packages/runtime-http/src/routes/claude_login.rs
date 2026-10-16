//! Native Claude sign-in with durable request identities and code delivery latches.
use super::extractors::{ApiJson, QueryParams};
use super::manifest::{OperationStatus, RouteGroup, get, post};
use crate::{error::ApiError, router::HttpState};
use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use studio_runtime_services::claude_login::{LoginError, service::LoginService};
pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post(
            "/api/accounts/claude/add",
            OperationStatus::Implemented,
            add,
        ),
        get(
            "/api/accounts/claude/login",
            OperationStatus::Implemented,
            status,
        ),
        post(
            "/api/accounts/claude/login",
            OperationStatus::Implemented,
            start,
        ),
        post(
            "/api/accounts/claude/login/code",
            OperationStatus::Implemented,
            code,
        ),
        post(
            "/api/accounts/claude/login/cancel",
            OperationStatus::Implemented,
            cancel,
        ),
    ])
}
#[derive(Deserialize)]
struct Start {
    login_id: String,
    account_key: String,
}
#[derive(Deserialize)]
struct Add {
    login_id: String,
    email: Option<String>,
    label: Option<String>,
}
#[derive(Deserialize)]
struct Code {
    login_id: String,
    code: String,
}
#[derive(Deserialize)]
struct Cancel {
    login_id: String,
}
fn service(state: &HttpState) -> Result<Arc<LoginService>, ApiError> {
    state
        .claude_login
        .clone()
        .ok_or_else(|| ApiError::dependency("provider-session/claude-login"))
}
fn error(error: LoginError) -> ApiError {
    match error {
        LoginError::Persistence | LoginError::InvalidRecord(_) => ApiError::internal(),
        invalid @ (LoginError::InvalidCode | LoginError::InvalidAuthorizationResponse) => {
            ApiError::not_applied(invalid.to_string())
        }
        other => ApiError::new(StatusCode::BAD_REQUEST, other.to_string()),
    }
}
async fn status(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let id = query
        .nonempty_str("request_id")
        .ok_or_else(|| error(LoginError::InvalidIdentity))?;
    service(&state)?.status(id).await.map(Json).map_err(error)
}
async fn start(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Start>,
) -> Result<Json<Value>, ApiError> {
    service(&state)?
        .start(body.login_id, Some(body.account_key), None, None)
        .await
        .map(Json)
        .map_err(error)
}
async fn add(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Add>,
) -> Result<Json<Value>, ApiError> {
    service(&state)?
        .start(body.login_id, None, body.email, body.label)
        .await
        .map(Json)
        .map_err(error)
}
async fn code(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Code>,
) -> Result<Json<Value>, ApiError> {
    service(&state)?
        .code(&body.login_id, body.code)
        .await
        .map(Json)
        .map_err(error)
}
async fn cancel(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Cancel>,
) -> Result<Json<Value>, ApiError> {
    service(&state)?
        .cancel(&body.login_id)
        .await
        .map(Json)
        .map_err(error)
}
inventory::submit! {crate::routes::manifest::RouteModuleRegistration{name:"claude_login",routes:Some(routes),dependencies:&[]}}

#[cfg(test)]
mod tests {
    use super::{routes, service};
    use crate::router::HttpState;
    use axum::{body::to_bytes, http::StatusCode, response::IntoResponse};
    use serde_json::Value;

    #[test]
    fn claude_login_routes_are_registered_as_implemented() {
        let routes = routes();
        for path in [
            "/api/accounts/claude/add",
            "/api/accounts/claude/login",
            "/api/accounts/claude/login/code",
            "/api/accounts/claude/login/cancel",
        ] {
            let route = routes
                .registrations()
                .iter()
                .find(|route| route.operation().path() == path)
                .unwrap_or_else(|| panic!("missing route {path}"));
            assert_eq!(
                route.operation().status(),
                super::super::manifest::OperationStatus::Implemented,
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn missing_service_names_the_uncomposed_provider_adapter() {
        let state_dir = tempfile::tempdir().expect("isolated state directory");
        let state = HttpState::new(
            String::new(),
            String::new(),
            None,
            state_dir.path().to_string_lossy().into_owned(),
            0,
            None,
            None,
            None,
        );
        let Err(error) = service(&state) else {
            panic!("service is not attached");
        };
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body = to_bytes(response.into_body(), 4096)
            .await
            .expect("response body");
        assert_eq!(
            serde_json::from_slice::<Value>(&body).expect("JSON body")["error"],
            "Not implemented: requires provider-session/claude-login"
        );
    }
}
