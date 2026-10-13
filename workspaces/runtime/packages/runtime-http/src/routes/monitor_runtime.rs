//! Monitor mutations; unavailable native effects refuse before a durable write.

use super::{
    extractors::ApiJson,
    manifest::{Dependency, OperationStatus, RouteGroup, post},
};
use crate::{error::ApiError, router::HttpState};
use axum::{Json, extract::State, http::StatusCode};
use serde_json::Value;
use studio_runtime_services::{
    event_write,
    monitor_policy::{Input, MonitorError},
    monitor_runtime,
};

const DEPENDENCIES: [Dependency; 3] = [
    Dependency::new(monitor_runtime::SESSION_DEPENDENCY),
    Dependency::new(monitor_runtime::RULE_DEPENDENCY),
    Dependency::new("cross-server/remote-events"),
];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post(
            "/api/monitor/cancel",
            OperationStatus::partial(&DEPENDENCIES),
            cancel,
        ),
        post(
            "/api/monitor/input",
            OperationStatus::partial(&DEPENDENCIES[..1]),
            input,
        ),
    ])
}

async fn cancel(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::MonitorId>,
) -> Result<Json<Value>, ApiError> {
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
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
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
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    tokio::task::spawn_blocking(move || {
        monitor_runtime::input_without_session(&store, &id, &input, event_write::unix_time()?)
    })
    .await
    .map_err(|_| ApiError::internal())?
    .map(Json)
    .map_err(error)
}

fn error(error: MonitorError) -> ApiError {
    match error {
        MonitorError::Refused(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
        MonitorError::Unavailable(name) => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            format!("Not implemented: requires {name}"),
        ),
        MonitorError::InvalidStored { id, location } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object monitors/{id} at {location}"),
        ),
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
        dependencies: &["provider-session/monitor-commands", "rule-driver/monitor-finish", "cross-server/remote-events"],
    }
}
