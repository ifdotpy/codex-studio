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
        post("/api/monitor/input", OperationStatus::Implemented, input),
        post(
            "/api/monitor/restart",
            OperationStatus::Implemented,
            restart,
        ),
    ])
}

#[derive(serde::Serialize)]
struct RestartResponse { id: String, status: studio_runtime_schema::studio_errors::MonitorStatus }

async fn restart(
    State(state): State<HttpState>,
    runtime: Option<Extension<Arc<MonitorRuntime>>>,
    ApiJson(body): ApiJson<monitor_runtime::RestartRequest>,
) -> Result<Json<RestartResponse>, ApiError> {
    let runtime = runtime
        .ok_or_else(|| ApiError::dependency("provider-session/monitor-commands"))?
        .0;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let owner = body.agent().to_owned();
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
            Ok(RestartResponse { id: record.id, status: record.status })
        },
        error,
    )
    .await?;
    runtime.wake_tick();
    Ok(Json(value))
}

#[derive(serde::Deserialize)]
struct MonitorInputRequest {
    id: String,
    // Use the runtime input owner without applying schema defaults to absent
    // dimensions: an absent dimension must remain different from a resize.
    #[serde(flatten)]
    input: Input,
}

async fn input(
    State(state): State<HttpState>,
    runtime: Option<Extension<Arc<MonitorRuntime>>>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<MonitorInputRequest>,
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
    let MonitorInputRequest { id, input } = body;
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
        MonitorError::Expired => ApiError::new(StatusCode::GONE, error.to_string()),
        MonitorError::Store(error) => ApiError::stored(error),
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
        MonitorError::InvalidResultReceipt(_)
        | MonitorError::InvalidDeliveryIdentity(_)
        | MonitorError::InvalidOutputReceipt(_)
        | MonitorError::TaskFailed
        | MonitorError::ProviderRequest { .. }
        | MonitorError::ConfigurationRejected { .. }
        | MonitorError::ConfigurationReceipt { .. }
        | MonitorError::Event(_)
        | MonitorError::Rules(_)
        | MonitorError::Io(_) => ApiError::internal(),
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "monitor_runtime", routes: Some(routes),
        dependencies: &["provider-session/monitor-commands"],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_restart_uses_checked_request() {
        type RestartHandler<F> = fn(
            State<HttpState>,
            Option<Extension<Arc<MonitorRuntime>>>,
            ApiJson<monitor_runtime::RestartRequest>,
        ) -> F;
        fn typed_restart<F: Future<Output = Result<Json<RestartResponse>, ApiError>>>(_: RestartHandler<F>) {}
        typed_restart(restart);
    }

    #[test]
    fn http_input_preserves_runtime_input_type_and_dimension_presence() {
        type InputHandler<F> = fn(
            State<HttpState>,
            Option<Extension<Arc<MonitorRuntime>>>,
            HeaderMap,
            ApiJson<MonitorInputRequest>,
        ) -> F;
        fn typed_input<F: Future<Output = Result<Json<Value>, ApiError>>>(_: InputHandler<F>) {}
        typed_input(input);
        let request: MonitorInputRequest =
            serde_json::from_value(serde_json::json!({"id": "monitor"}))
                .expect("missing dimensions are valid");
        let input: Input = request.input;
        assert_eq!(input.rows, None);
        assert_eq!(input.cols, None);
        assert_eq!(input.text, "");
        assert!(!input.close_stdin);
        for field in ["rows", "cols"] {
            let mut invalid = serde_json::json!({"id": "monitor"});
            invalid[field] = Value::Null;
            assert!(serde_json::from_value::<MonitorInputRequest>(invalid).is_err());
        }
    }
}
