//! Server-wide orchestration settings that the scheduler reads on every use.

use axum::{Json, extract::State};
use serde_json::{Value, json};
use studio_runtime_store::OrchestrationSettingsRepository;

use super::{
    extractors::ApiJson,
    manifest::{OperationStatus, RouteGroup, get, post},
};
use crate::{error::ApiError, generated, router::HttpState};

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/orchestration/settings",
            OperationStatus::Implemented,
            read_settings,
        ),
        post(
            "/api/orchestration/settings",
            OperationStatus::Implemented,
            write_settings,
        ),
    ])
}

fn response(seconds: u64) -> Result<Json<generated::OrchestrationSettingsResponse>, ApiError> {
    Ok(Json(super::generated_json(json!({
        "settings": {"childResultCoalesceSeconds": seconds}
    }))?))
}

async fn read_settings(
    State(state): State<HttpState>,
) -> Result<Json<generated::OrchestrationSettingsResponse>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let hold = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/orchestration_settings",
        move || {
            let connection = store.read()?;
            OrchestrationSettingsRepository::child_result_coalesce(&connection)
        },
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(|_| ApiError::internal())?;
    response(hold.as_secs())
}

async fn write_settings(
    State(state): State<HttpState>,
    ApiJson(settings): ApiJson<generated::OrchestrationSettings>,
) -> Result<Json<generated::OrchestrationSettingsResponse>, ApiError> {
    let seconds = u64::try_from(settings.child_result_coalesce_seconds)
        .ok()
        .filter(|seconds| OrchestrationSettingsRepository::valid_child_result_coalesce(*seconds))
        .ok_or_else(|| {
            ApiError::validation(vec![
                Value::String("body".to_owned()),
                Value::String("childResultCoalesceSeconds".to_owned()),
            ])
        })?;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    store
        .transaction(move |transaction| {
            OrchestrationSettingsRepository::set_child_result_coalesce(transaction, seconds)
        })
        .await
        .map_err(|_| ApiError::internal())?;
    // Held results are re-evaluated against the new hold at once.
    state.event_wake.notify_one();
    response(seconds)
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "orchestration_settings", routes: Some(routes), dependencies: &[],
    }
}
