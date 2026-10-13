//! Native Codex thread import routes.

use super::extractors::{ApiJson, QueryParams};
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get, post};
use axum::Json;
use axum::extract::State;

use crate::error::ApiError;
use crate::router::HttpState;

const THREAD_IMPORT_DEPENDENCY: [Dependency; 1] =
    [Dependency::new("provider-session/thread-import")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/import",
            OperationStatus::partial(&THREAD_IMPORT_DEPENDENCY),
            import_list,
        ),
        post(
            "/api/import",
            OperationStatus::partial(&THREAD_IMPORT_DEPENDENCY),
            import_thread,
        ),
    ])
}

async fn import_list(
    State(_state): State<HttpState>,
    _query: QueryParams,
) -> Result<Json<crate::generated::ImportListResponse>, ApiError> {
    Err(
        dependency_unavailable("GET", "/api/import", &THREAD_IMPORT_DEPENDENCY)
            .map_err(|_| ApiError::internal())?,
    )
}

async fn import_thread(
    State(_state): State<HttpState>,
    ApiJson(_body): ApiJson<crate::generated::ImportRequest>,
) -> Result<Json<crate::generated::AgentResponse>, ApiError> {
    Err(
        dependency_unavailable("POST", "/api/import", &THREAD_IMPORT_DEPENDENCY)
            .map_err(|_| ApiError::internal())?,
    )
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "imports",
        routes: Some(routes),
        dependencies: &["provider-session/thread-import"],
    }
}
