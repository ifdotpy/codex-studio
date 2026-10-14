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
const NATIVE_REQUEST_DEPENDENCY: &str = "provider-session/native-request";
const IMAGE_DEPENDENCIES: [Dependency; 1] = [Dependency::new(IMAGE_EXECUTION_DEPENDENCY)];
const THREAD_FORK_DEPENDENCIES: [Dependency; 1] = [Dependency::new(THREAD_FORK_DEPENDENCY)];
const RESTORE_DEPENDENCIES: [Dependency; 1] = [Dependency::new(NATIVE_REQUEST_DEPENDENCY)];

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
            OperationStatus::partial(&RESTORE_DEPENDENCIES),
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
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::CheckpointRestoreRequest>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let preparation = state.preparation_resources.clone();
    let account = state.account_session.clone().ok_or_else(|| {
        dependency_unavailable("POST", "/api/checkpoint/restore", &RESTORE_DEPENDENCIES)
            .unwrap_or_else(|_| ApiError::internal())
    })?;
    let checkpoint = body
        .checkpoint_id
        .or(body.checkpoint)
        .flatten()
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown checkpoint"))?;
    let agent = body.agent;
    let expected_tree = body.expected_tree;
    let result = tokio::task::spawn_blocking(move || {
        checkpoints::restore(
            &store,
            &agent,
            &checkpoint,
            &expected_tree,
            preparation.as_deref(),
            account.as_ref(),
        )
        .map_err(|error| map_error(error, "/api/checkpoint/restore"))
    })
    .await
    .map_err(|_| ApiError::internal())??;
    super::generated_json::<crate::generated::CheckpointRestoreResponse>(result).map(Json)
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
        CheckpointsError::ProviderUnavailable => {
            dependency_unavailable("POST", path, &RESTORE_DEPENDENCIES)
                .unwrap_or_else(|_| ApiError::internal())
        }
        CheckpointsError::ProviderUnknown => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Workspace recovery is required before retrying this restore",
        ),
        CheckpointsError::ProviderRejected => ApiError::new(
            StatusCode::BAD_REQUEST,
            "The provider rejected the checkpoint restore",
        ),
        CheckpointsError::Preparation(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
        CheckpointsError::WorkspaceOperation(_) => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Checkpoint history is unavailable for restore",
        ),
        CheckpointsError::Json(_) => ApiError::internal(),
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
        "provider-session/native-request",
        "workspace-state/image-execution",
        "store-bootstrap/checkpoints-schema",
        ],
    }
}
