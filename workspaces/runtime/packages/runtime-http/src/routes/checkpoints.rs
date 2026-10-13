//! Capture and preview local Git checkpoints; provider-dependent effects stay gated.

use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;
use studio_runtime_services::checkpoints::{self, CheckpointsError};
use studio_runtime_store::StoreError;

use crate::error::ApiError;
use crate::router::HttpState;

const IMAGE_EXECUTION_DEPENDENCY: &str = "workspace-state/image-execution";
const CHECKPOINT_SCHEMA_DEPENDENCY: &str = "store-bootstrap/checkpoints-schema";
const THREAD_FORK_DEPENDENCY: &str = "provider-session/thread-fork";
const IMAGE_DEPENDENCIES: [Dependency; 1] = [Dependency::new(IMAGE_EXECUTION_DEPENDENCY)];
const THREAD_FORK_DEPENDENCIES: [Dependency; 1] = [Dependency::new(THREAD_FORK_DEPENDENCY)];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post(
            "/api/branch",
            OperationStatus::partial(&THREAD_FORK_DEPENDENCIES),
            branch,
        ),
        post(
            "/api/checkpoint",
            OperationStatus::partial(&IMAGE_DEPENDENCIES),
            capture,
        ),
        post(
            "/api/checkpoint/preview",
            OperationStatus::partial(&IMAGE_DEPENDENCIES),
            preview,
        ),
        post(
            "/api/checkpoint/restore",
            OperationStatus::partial(&THREAD_FORK_DEPENDENCIES),
            restore,
        ),
    ])
}

pub(crate) async fn capture(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::CheckpointCaptureRequest>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || {
        let snapshot = checkpoints::capture_context(&store, &body.agent)
            .map_err(|error| map_error(error, "/api/checkpoint"))?;
        let label = body.label.unwrap_or_else(|| "Checkpoint".to_owned());
        let record = checkpoints::capture(&store, snapshot, &label)
            .map_err(|error| map_error(error, "/api/checkpoint"))?;
        super::generated_json::<crate::generated::CheckpointCaptureResponse>(record)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(result))
}

pub(crate) async fn preview(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::CheckpointPreviewRequest>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || {
        let context = checkpoints::preview_context(&store, &body.agent, &body.checkpoint)
            .map_err(|error| map_error(error, "/api/checkpoint/preview"))?;
        let value = checkpoints::preview(&store, context)
            .map_err(|error| map_error(error, "/api/checkpoint/preview"))?;
        super::generated_json::<crate::generated::CheckpointPreviewResponse>(value)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(result))
}

pub(crate) async fn restore(
    ApiJson(_body): ApiJson<crate::generated::CheckpointRestoreRequest>,
) -> Result<Json<Value>, ApiError> {
    Err(
        dependency_unavailable("POST", "/api/checkpoint/restore", &THREAD_FORK_DEPENDENCIES)
            .unwrap_or_else(|_| ApiError::internal()),
    )
}

pub(crate) async fn branch(
    ApiJson(_body): ApiJson<crate::generated::BranchRequest>,
) -> Result<Json<Value>, ApiError> {
    Err(
        dependency_unavailable("POST", "/api/branch", &THREAD_FORK_DEPENDENCIES)
            .unwrap_or_else(|_| ApiError::internal()),
    )
}

fn map_error(error: CheckpointsError, path: &'static str) -> ApiError {
    match error {
        CheckpointsError::UnknownAgent => {
            ApiError::new(StatusCode::BAD_REQUEST, "Unknown managed agent")
        }
        CheckpointsError::UnknownCheckpoint => {
            ApiError::new(StatusCode::BAD_REQUEST, "Unknown checkpoint")
        }
        CheckpointsError::WrongAgent => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Checkpoint belongs to another agent",
        ),
        CheckpointsError::DeletedAgent => {
            ApiError::new(StatusCode::BAD_REQUEST, "This agent was deleted")
        }
        CheckpointsError::MissingSchema => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            format!("Not implemented: requires {CHECKPOINT_SCHEMA_DEPENDENCY}"),
        ),
        CheckpointsError::ExecutionWorkspace => {
            dependency_unavailable("POST", path, &IMAGE_DEPENDENCIES)
                .unwrap_or_else(|_| ApiError::internal())
        }
        CheckpointsError::ActivityChanged => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Workspace activity changed before checkpoint capture",
        ),
        CheckpointsError::SettlementFailed { .. } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Checkpoint capture failed and workspace reservation cleanup failed",
        ),
        CheckpointsError::RecoveryRequired => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Workspace recovery is required before using this workspace",
        ),
        CheckpointsError::AgentBusy => {
            ApiError::new(StatusCode::BAD_REQUEST, "An agent is using this workspace")
        }
        CheckpointsError::MonitorBusy => {
            ApiError::new(StatusCode::BAD_REQUEST, "A monitor is using this workspace")
        }
        CheckpointsError::TaskBusy => {
            ApiError::new(StatusCode::BAD_REQUEST, "A command or tool is still active")
        }
        CheckpointsError::GitFailed => {
            ApiError::new(StatusCode::BAD_REQUEST, "Git operation failed")
        }
        CheckpointsError::GitTimeout => {
            ApiError::new(StatusCode::BAD_REQUEST, "Git operation timed out")
        }
        CheckpointsError::WorkspaceUnavailable => {
            ApiError::new(StatusCode::BAD_REQUEST, "The workspace cannot be accessed")
        }
        CheckpointsError::InvalidStoredObject { object, id } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {object}/{id}"),
        ),
        CheckpointsError::Store(StoreError::SchemaMismatch(message))
            if message.starts_with("Invalid stored object ") =>
        {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, message)
        }
        CheckpointsError::Store(_) => ApiError::internal(),
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "checkpoints",
        routes: Some(routes),
        dependencies: &[
        "provider-session/thread-fork",
        "workspace-state/image-execution",
        "store-bootstrap/checkpoints-schema",
        ],
    }
}
