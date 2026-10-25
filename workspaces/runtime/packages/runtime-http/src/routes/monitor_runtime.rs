//! Monitor mutations; unavailable native effects refuse before a durable write.

use super::{
    extractors::ApiJson,
    manifest::{OperationStatus, RouteGroup, post},
};
use crate::{error::ApiError, router::HttpState};
use axum::{
    Extension, Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use serde_json::Value;
use std::sync::Arc;
use studio_runtime_services::monitor_runtime_wiring::MonitorRuntime;
use studio_runtime_services::{
    event_write,
    monitor_policy::{Input, MonitorError},
    monitor_runtime,
};

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post("/api/monitor/cancel", OperationStatus::Implemented, cancel),
        post("/api/monitor/input", OperationStatus::Implemented, input),
        post(
            "/api/monitor/restart",
            OperationStatus::Implemented,
            restart,
        ),
    ])
}

async fn restart(
    State(state): State<HttpState>,
    runtime: Option<Extension<Arc<MonitorRuntime>>>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let runtime = runtime
        .ok_or_else(|| ApiError::dependency("provider-session/monitor-commands"))?
        .0;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let owner = body["agent"]
        .as_str()
        .ok_or_else(ApiError::internal)?
        .to_owned();
    let actor = store
        .read_async("monitor-restart/owner", move |tx| {
            event_write::checked_agent(tx, &owner)
        })
        .await
        .map_err(|e| error(e.into()))?;
    let workspace = super::agents_create::prepare_parent_workspace(&actor).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "The monitor workspace is not ready",
        )
    })?;
    let log_root = studio_runtime_services::monitor_output::log_root(store.root())
        .to_string_lossy()
        .into_owned();
    let now = event_write::unix_time().map_err(|_| ApiError::internal())?;
    let value = super::sync_delta::write(
        store,
        move |tx| {
            let record = monitor_runtime::restart_in_transaction(
                tx,
                &body,
                workspace.as_ref(),
                &log_root,
                now,
            )?;
            Ok(serde_json::json!({"id":record["id"],"status":record["status"]}))
        },
        error,
    )
    .await?;
    runtime.wake_tick();
    Ok(Json(value))
}

async fn cancel(
    State(state): State<HttpState>,
    runtime: Option<Extension<Arc<MonitorRuntime>>>,
    ApiJson(body): ApiJson<crate::generated::MonitorId>,
) -> Result<Json<Value>, ApiError> {
    if let Some(Extension(runtime)) = runtime {
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let id = body.id;
        let now = event_write::unix_time().map_err(|_| ApiError::internal())?;
        let result = super::sync_delta::write_external_async(
            store,
            move || async move { runtime.cancel(&id, None, now).await },
            error,
        )
        .await?;
        return Ok(Json(result));
    }
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let result = super::sync_delta::write(
        store,
        move |tx| {
            let result = monitor_runtime::cancel_in_transaction(
                tx,
                &body.id,
                None,
                event_write::unix_time()?,
            )?;
            serde_json::from_value::<crate::generated::MonitorActionResult>(result)
                .and_then(serde_json::to_value)
                .map_err(studio_runtime_store::StoreError::from)
                .map_err(MonitorError::from)
        },
        error,
    )
    .await?;
    state.event_wake.notify_one();
    Ok(Json(result))
}

async fn input(
    State(state): State<HttpState>,
    runtime: Option<Extension<Arc<MonitorRuntime>>>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let request_id = headers
        .get("x-studio-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|id| crate::middleware::safe_request_id(id))
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "Monitor input request identity is required",
            )
        })?;
    if let Some((pointer, _)) = studio_runtime_schema::first_error("MonitorInput", &body)
        .map_err(|_| ApiError::internal())?
    {
        let mut location = vec![Value::String("body".into())];
        location.extend(
            pointer
                .split('/')
                .skip(1)
                .map(|part| Value::String(part.replace("~1", "/").replace("~0", "~"))),
        );
        return Err(ApiError::validation(location));
    }
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(ApiError::internal)?
        .to_owned();
    // Preserve field presence: a resize-only request is different from an empty write.
    let input: Input = serde_json::from_value(body)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "Invalid monitor input"))?;
    if let Some(Extension(runtime)) = runtime {
        let now = event_write::unix_time().map_err(|_| ApiError::internal())?;
        return runtime
            .input_http(&id, request_id, &input, now)
            .await
            .map(Json)
            .map_err(error);
    }
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let now = event_write::unix_time().map_err(|_| ApiError::internal())?;
    monitor_runtime::input_without_session(&store, &id, &input, now)
        .await
        .map(Json)
        .map_err(error)
}

fn error(error: MonitorError) -> ApiError {
    match error {
        MonitorError::InputOutcomeUnknown => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Monitor input outcome unknown; do not repeat it",
        ),
        MonitorError::Refused(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
        MonitorError::Unavailable(name) => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            format!("Not implemented: requires {name}"),
        ),
        MonitorError::InvalidStored { id, .. } => ApiError::invalid_stored("monitors", &id),
        MonitorError::Event(event_write::EventWriteError::RemoteEventsUnavailable) => {
            ApiError::new(
                StatusCode::NOT_IMPLEMENTED,
                "Not implemented: requires cross-server/remote-events",
            )
        }
        _ => ApiError::internal(),
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "monitor_runtime", routes: Some(routes),
        dependencies: &["provider-session/monitor-commands"],
    }
}
