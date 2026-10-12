//! Tool request endpoints whose durable and live recovery drivers are not yet available.

use axum::Json;
use serde_json::Value;

use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get, post};
use crate::error::ApiError;

const LIST_DEPENDENCIES: [Dependency; 2] = [
    Dependency::new("payload-store/tool-request-list"),
    Dependency::new("worker-driver/connection-state"),
];
const CANCEL_DEPENDENCIES: [Dependency; 1] = [Dependency::new("tool-driver/request-lifecycle")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/tool-requests",
            OperationStatus::partial(&LIST_DEPENDENCIES),
            list,
        ),
        post(
            "/api/tool-requests/cancel",
            OperationStatus::partial(&CANCEL_DEPENDENCIES),
            cancel,
        ),
    ])
}

async fn list() -> Result<Json<Value>, ApiError> {
    Err(dependency_error(
        "GET",
        "/api/tool-requests",
        &LIST_DEPENDENCIES,
    ))
}

async fn cancel(_body: ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(dependency_error(
        "POST",
        "/api/tool-requests/cancel",
        &CANCEL_DEPENDENCIES,
    ))
}

fn dependency_error(method: &str, path: &str, dependencies: &[Dependency]) -> ApiError {
    dependency_unavailable(method, path, dependencies).unwrap_or_else(|_| ApiError::internal())
}
