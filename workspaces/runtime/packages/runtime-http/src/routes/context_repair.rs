//! Explicit context repair keeps unsupported native history edits visibly refused.
use super::{
    extractors::ApiJson,
    manifest::{Dependency, OperationStatus, RouteGroup, post},
};
use crate::{error::ApiError, router::HttpState};
use axum::{Json, extract::State, http::StatusCode};
use serde_json::Value;
use studio_runtime_services::context_repair::{self, RepairError};
const DEPENDENCIES: [Dependency; 2] = [
    Dependency::new("context-repair/native-history-rewrite"),
    Dependency::new("store-bootstrap/context-repair-schema"),
];
pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![post(
        "/api/context-repair",
        OperationStatus::partial(&DEPENDENCIES),
        repair,
    )])
}
async fn repair(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::IdRequest>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || context_repair::repair_idle(&store, &body.id))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(|error| match error {
            RepairError::MissingAgent => ApiError::new(StatusCode::BAD_REQUEST, "Unknown agent"),
            RepairError::ReceiptHold => ApiError::new(
                StatusCode::BAD_REQUEST,
                "Context repair awaits its exact native receipt",
            ),
            RepairError::RewriteUnavailable | RepairError::MissingSchema => {
                ApiError::new(StatusCode::NOT_IMPLEMENTED, error.to_string())
            }
            _ => ApiError::internal(),
        })?;
    Ok(Json(super::generated_json::<
        crate::generated::ContextRepairResponse,
    >(result)?))
}
inventory::submit! { super::manifest::RouteModuleRegistration { name: "context_repair", routes: Some(routes), dependencies: &["context-repair/native-history-rewrite", "store-bootstrap/context-repair-schema"] } }
