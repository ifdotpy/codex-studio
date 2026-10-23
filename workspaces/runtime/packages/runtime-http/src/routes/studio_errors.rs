//! The owner's list of agent work that ended because of a Studio error.

use axum::{Json, extract::State};

use super::{
    extractors::ApiJson,
    manifest::{OperationStatus, RouteGroup, get, post},
};
use crate::{error::ApiError, generated, router::HttpState};

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get("/api/studio-errors", OperationStatus::Implemented, read),
        post(
            "/api/studio-errors/dismiss",
            OperationStatus::Implemented,
            dismiss,
        ),
    ])
}

fn now() -> Result<f64, ApiError> {
    studio_runtime_services::event_write::unix_time().map_err(|_| ApiError::internal())
}

async fn read(
    State(state): State<HttpState>,
) -> Result<Json<generated::StudioErrorsResponse>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let now = now()?;
    let value = store
        .read_async("runtime-http/routes/studio_errors", move |tx| {
            studio_runtime_services::studio_errors::list(tx, now)
        })
        .await
        .map_err(|_| ApiError::internal())?;
    Ok(Json(super::generated_json(value)?))
}

async fn dismiss(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::IdRequest>,
) -> Result<Json<generated::StudioErrorsResponse>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let now = now()?;
    let id = body.id;
    let value = store
        .transaction(move |tx| {
            studio_runtime_services::studio_errors::dismiss(tx, &id, now)?;
            studio_runtime_services::studio_errors::list(tx, now)
        })
        .await
        .map_err(|_| ApiError::internal())?;
    Ok(Json(super::generated_json(value)?))
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "studio_errors", routes: Some(routes), dependencies: &[],
    }
}
