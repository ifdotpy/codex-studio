//! The worker owns connection recovery. Until its history port is wired, this
//! operation refuses before opening a transaction or starting a provider.
use super::{
    extractors::ApiJson,
    manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, post},
};
use crate::error::ApiError;
use axum::Json;
use serde_json::Value;
const DEPENDENCIES: &[Dependency] = &[Dependency::new("account-worker/recovery-history")];
pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![post(
        "/api/connection-recovery",
        OperationStatus::partial(DEPENDENCIES),
        recover,
    )])
}
async fn recover(
    ApiJson(_body): ApiJson<crate::generated::IdRequest>,
) -> Result<Json<Value>, ApiError> {
    Err(
        dependency_unavailable("POST", "/api/connection-recovery", DEPENDENCIES)
            .map_err(|_| ApiError::internal())?,
    )
}
inventory::submit! { crate::routes::manifest::RouteModuleRegistration {name:"recovery",routes:Some(routes),dependencies:&["account-worker/recovery-history"]} }
