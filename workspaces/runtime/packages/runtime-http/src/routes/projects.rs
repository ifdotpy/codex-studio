//! HTTP adapters for projects, locations and peer teams.

use super::extractors::{ApiJson, QueryParams};
use super::manifest::{Dependency, OperationStatus, RouteGroup, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;
use studio_runtime_services::projects::{self, ProjectsError};

use crate::error::ApiError;
use crate::router::HttpState;

const PROJECT_WRITE_DEPENDENCIES: [Dependency; 5] = [
    Dependency::new("cross-server-driver/project-location-write"),
    Dependency::new("cross-server-driver/project-registration"),
    Dependency::new("workspace-state/logical-projects"),
    Dependency::new("workspace-state/project-organization"),
    Dependency::new("workspace-state/worker-environment"),
];
const LOCATION_READ_DEPENDENCIES: [Dependency; 1] =
    [Dependency::new("cross-server-driver/project-location-read")];
const PEER_TEAM_DEPENDENCIES: [Dependency; 2] = [
    Dependency::new("cross-server-driver/peer-team-transport"),
    Dependency::new("workspace-state/peer-radio"),
];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get("/api/projects", OperationStatus::Implemented, list_projects),
        post(
            "/api/projects",
            OperationStatus::partial(&PROJECT_WRITE_DEPENDENCIES),
            write_project,
        ),
        get(
            "/api/project-locations",
            OperationStatus::partial(&LOCATION_READ_DEPENDENCIES),
            project_locations,
        ),
        post(
            "/api/peer-teams",
            OperationStatus::partial(&PEER_TEAM_DEPENDENCIES),
            peer_teams,
        ),
    ])
}

async fn list_projects(State(state): State<HttpState>) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let value = tokio::task::spawn_blocking(move || {
        let value = projects::list(&store).map_err(projects_error)?;
        super::generated_json::<crate::generated::ProjectReadResponse>(value)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(value))
}

async fn write_project(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    if let Some(dependency) = project_write_dependency(&body) {
        return Err(ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            format!("Not implemented: requires {dependency}"),
        ));
    }
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let account_policy = state
        .account_policy
        .clone()
        .ok_or_else(ApiError::internal)?;
    let accounts = account_policy.accounts();
    let known: Vec<Value> = accounts.accounts.clone();
    let default = accounts
        .default_account_key
        .as_str()
        .ok_or_else(ApiError::internal)?
        .to_owned();
    let is_remove = body.get("action").and_then(Value::as_str) == Some("remove");
    let is_sidebar = body.get("action").and_then(Value::as_str) == Some("reorder");
    let value = tokio::task::spawn_blocking(move || {
        let value = projects::write(&store, &body, &default, &known).map_err(projects_error)?;
        if is_remove {
            super::generated_json::<crate::generated::ProjectRemovalResponse>(value)
        } else if is_sidebar {
            super::generated_json::<crate::generated::SidebarOrderResponse>(value)
        } else {
            super::generated_json::<crate::generated::ProjectMutationRecord>(value)
        }
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(value))
}

async fn project_locations(
    State(_state): State<HttpState>,
    _query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    Err(ApiError::new(
        StatusCode::NOT_IMPLEMENTED,
        "Not implemented: requires cross-server-driver/project-location-read",
    ))
}

async fn peer_teams(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let account_policy = state
        .account_policy
        .clone()
        .ok_or_else(ApiError::internal)?;
    let default_account = account_policy
        .accounts()
        .default_account_key
        .as_str()
        .ok_or_else(ApiError::internal)?
        .to_owned();
    let result = tokio::task::spawn_blocking(move || {
        let value =
            projects::peer_team_write(&store, &body, &default_account).map_err(projects_error)?;
        super::generated_json::<crate::generated::PeerProjectResponse>(value)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(result))
}

fn projects_error(error: ProjectsError) -> ApiError {
    match error {
        ProjectsError::Invalid(_) => ApiError::validation(vec![Value::String("body".to_owned())]),
        ProjectsError::Conflict(message) => ApiError::new(StatusCode::CONFLICT, message),
        ProjectsError::Requires(dependency) => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            format!("Not implemented: requires {dependency}"),
        ),
        ProjectsError::InvalidStored(id) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored project {id}"),
        ),
        ProjectsError::Store(_) => ApiError::internal(),
    }
}

fn project_write_dependency(body: &Value) -> Option<&'static str> {
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("register");
    if body
        .get("server")
        .and_then(Value::as_str)
        .is_some_and(|server| !server.is_empty())
    {
        Some("cross-server-driver/project-registration")
    } else if matches!(action, "add_location" | "remove_location") {
        Some("cross-server-driver/project-location-write")
    } else if matches!(
        action,
        "rename" | "add_folder" | "rename_folder" | "remove_folder"
    ) {
        Some("workspace-state/project-organization")
    } else if body.get("action").and_then(Value::as_str) == Some("set_worker_environment")
        && body.get("environment").and_then(Value::as_str) == Some("linux")
    {
        Some("workspace-state/worker-environment")
    } else if body
        .get("path")
        .and_then(Value::as_str)
        .is_some_and(|path| path.starts_with("project:"))
    {
        Some("workspace-state/logical-projects")
    } else {
        None
    }
}
