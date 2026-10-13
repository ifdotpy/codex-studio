//! Thin HTTP lifecycle edge. Native actions require the account worker.
use super::{
    extractors::ApiJson,
    manifest::{Dependency, OperationStatus, RouteGroup, post},
};
use crate::{error::ApiError, generated, router::HttpState};
use axum::{Json, extract::State, http::StatusCode};
use serde_json::Value;
use studio_runtime_services::{
    agent_management::{Action, ManagementError},
    agent_management_store,
};

const LIFECYCLE: &[Dependency] = &[
    Dependency::new("account-worker/agent-interrupt"),
    Dependency::new("agent-management/snapshot-limit"),
    Dependency::new("cross-server/agent-management"),
    Dependency::new("event-driver/child-stop-notification"),
    Dependency::new("monitor-driver/agent-stop"),
    Dependency::new("usage-policy/agent-stop"),
    Dependency::new("voice-driver/agent-stop"),
    Dependency::new("work-driver/deletion-release"),
    Dependency::new("workspace-state/agent-lifecycle"),
];
const NATIVE: &[Dependency] = &[Dependency::new("account-worker/native-action")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post(
            "/api/action",
            OperationStatus::partial(NATIVE),
            native_action,
        ),
        post(
            "/api/conversation/delete",
            OperationStatus::partial(LIFECYCLE),
            delete,
        ),
        post("/api/stop", OperationStatus::partial(LIFECYCLE), stop),
    ])
}

async fn stop(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::StopRequest>,
) -> Result<Json<Value>, ApiError> {
    dispatch::<generated::StopResponse>(
        state,
        Action::Stop {
            target: body.id,
            descendants: body.descendants.unwrap_or(true),
        },
    )
    .await
}
async fn delete(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::IdRequest>,
) -> Result<Json<Value>, ApiError> {
    dispatch::<generated::DeletedResponse>(state, Action::Delete { target: body.id }).await
}
async fn native_action(
    ApiJson(body): ApiJson<generated::NativeActionRequest>,
) -> Result<Json<Value>, ApiError> {
    let value = serde_json::to_value(body).map_err(|_| ApiError::internal())?;
    if value.get("action").is_some_and(Value::is_string)
        && !value.get("request_id").is_some_and(Value::is_string)
    {
        return Err(ApiError::validation(vec![
            Value::String("body".to_owned()),
            Value::String("request_id".to_owned()),
        ]));
    }
    Err(ApiError::new(
        StatusCode::NOT_IMPLEMENTED,
        "Not implemented: requires account-worker/native-action",
    ))
}
async fn dispatch<T: serde::de::DeserializeOwned + serde::Serialize>(
    state: HttpState,
    action: Action,
) -> Result<Json<Value>, ApiError> {
    tokio::task::spawn_blocking(move || {
        let store = state.store.as_deref().ok_or_else(ApiError::internal)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ApiError::internal())?
            .as_secs_f64();
        let result =
            agent_management_store::execute(store, &action, now).map_err(|error| match error {
                ManagementError::Unavailable(_) => {
                    ApiError::new(StatusCode::NOT_IMPLEMENTED, error.to_string())
                }
                ManagementError::Invalid(_) => {
                    ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
                }
                ManagementError::Stored { .. } => {
                    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
                }
                ManagementError::Store(_) => ApiError::internal(),
            })?;
        super::generated_json::<T>(result).map(Json)
    })
    .await
    .map_err(|_| ApiError::internal())?
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "agent_management", routes: Some(routes), dependencies: &[
            "account-worker/agent-interrupt", "account-worker/native-action",
            "agent-management/snapshot-limit", "cross-server/agent-management",
            "event-driver/child-stop-notification", "monitor-driver/agent-stop",
            "usage-policy/agent-stop", "voice-driver/agent-stop", "work-driver/deletion-release",
            "workspace-state/agent-lifecycle",
        ],
    }
}
