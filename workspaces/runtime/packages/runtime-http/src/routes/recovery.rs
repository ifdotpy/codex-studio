//! Synchronous connection recovery runs through the owning account worker.
use super::{
    extractors::ApiJson,
    manifest::{OperationStatus, RouteGroup, post},
};
use crate::error::ApiError;
use crate::router::HttpState;
use axum::Json;
use axum::extract::State;
use serde_json::Value;
pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![post(
        "/api/connection-recovery",
        OperationStatus::Implemented,
        recover,
    )])
}
async fn recover(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::IdRequest>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let id = body.id;
    let lookup_id = id.clone();
    let account = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let agent = studio_runtime_store::AgentRepository::get(
            &connection,
            &studio_runtime_store::AgentId::from(lookup_id.as_str()),
        )
        .map_err(|_| ApiError::internal())?
        .ok_or_else(|| ApiError::new(axum::http::StatusCode::BAD_REQUEST, "Unknown agent"))?;
        Ok::<_, ApiError>(
            agent
                .extra
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
                .to_owned(),
        )
    })
    .await
    .map_err(|_| ApiError::internal())??;
    let port = state
        .account_session
        .clone()
        .ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || port.recover_connection(&account, &id))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(|_| ApiError::internal())?;
    Ok(Json(result))
}
inventory::submit! { crate::routes::manifest::RouteModuleRegistration {name:"recovery",routes:Some(routes),dependencies:&[]} }
