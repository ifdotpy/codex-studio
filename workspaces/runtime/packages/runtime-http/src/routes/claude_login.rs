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
    state.claude_login.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires provider-session/claude-login",
        )
    })
}
fn error(error: LoginError) -> ApiError {
    match error {
        LoginError::Persistence | LoginError::InvalidRecord(_) => ApiError::internal(),
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
inventory::submit! {crate::routes::manifest::RouteModuleRegistration{name:"claude_login",routes:Some(routes),dependencies:&["provider-session/claude-login"]}}
