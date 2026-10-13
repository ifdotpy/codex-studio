//! Native controls await the account worker's retained session command port.
use super::{
    extractors::ApiJson,
    manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, post},
};
use crate::error::ApiError;
use axum::Json;
use serde_json::Value;
const SESSION: [Dependency; 1] = [Dependency::new("provider-session/claude-controls")];
fn routes() -> RouteGroup {
    RouteGroup::new(vec![post(
        "/api/claude/session",
        OperationStatus::partial(&SESSION),
        session,
    )])
}
async fn session(ApiJson(_): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(
        dependency_unavailable("POST", "/api/claude/session", &SESSION)
            .unwrap_or_else(|_| ApiError::internal()),
    )
}
inventory::submit! {crate::routes::manifest::RouteModuleRegistration{name:"claude_controls",routes:Some(routes),dependencies:&["provider-session/claude-controls"]}}
