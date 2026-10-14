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

const PROJECT_WRITE_DEPENDENCIES: [Dependency; 4] = [
    Dependency::new("cross-server-driver/local-identity"),
    Dependency::new("cross-server-driver/project-location-write"),
    Dependency::new("cross-server-driver/project-registration"),
    Dependency::new("workspace-state/worker-environment"),
];
const LOCATION_READ_DEPENDENCIES: [Dependency; 1] =
    [Dependency::new("cross-server-driver/project-location-read")];
const PEER_TEAM_DEPENDENCIES: [Dependency; 3] = [
    Dependency::new("cross-server-driver/peer-team-transport"),
    Dependency::new("host-project/radio-create"),
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
    if let Some(dependency) = projects::write_dependency(&body) {
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
    let account_snapshot = account_policy.accounts();
    let known = account_snapshot.accounts;
    let default_account = account_snapshot
        .default_account_key
        .as_str()
        .ok_or_else(ApiError::internal)?
        .to_owned();
    let result = tokio::task::spawn_blocking(move || {
        let value = projects::peer_team_write(&store, &body, &default_account, &known)
            .map_err(projects_error)?;
        match body["action"].as_str() {
            Some("radio") => super::generated_json::<crate::generated::PeerRadioResponse>(value),
            Some("convert") => {
                super::generated_json::<crate::generated::PeerConversionResponse>(value)
            }
            _ => super::generated_json::<crate::generated::PeerProjectResponse>(value),
        }
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
        ProjectsError::InvalidStored { table, id } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored {table} record {id}"),
        ),
        ProjectsError::Store(studio_runtime_store::StoreError::InvalidStoredRecord {
            object,
            id,
        }) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {object}/{id}"),
        ),
        ProjectsError::Store(_) => ApiError::internal(),
        ProjectsError::Sqlite(_) => ApiError::internal(),
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "projects",
        routes: Some(routes),
        dependencies: &[
            "cross-server-driver/local-identity",
            "cross-server-driver/peer-team-transport",
            "cross-server-driver/project-location-read",
            "cross-server-driver/project-location-write",
            "cross-server-driver/project-registration",
            "host-project/radio-create",
            "workspace-state/peer-radio",
            "workspace-state/worker-environment",
        ],
    }
}
