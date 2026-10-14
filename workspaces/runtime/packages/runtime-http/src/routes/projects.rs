//! HTTP adapters for projects, locations and peer teams.

use super::extractors::{ApiJson, QueryParams};
use super::manifest::{Dependency, OperationStatus, RouteGroup, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;
use std::path::Path;
use studio_runtime_services::agents_create::CreateError;
use studio_runtime_services::host_worker_create::{
    AccountIdentity, CreationCatalog, ExecutionSettings,
};
use studio_runtime_services::projects::{self, ProjectsError};

use crate::error::ApiError;
use crate::router::HttpState;

struct RadioCreationAdapter {
    service: studio_runtime_services::model_catalog::ModelCatalogService,
    rows: Vec<Value>,
    identities: Option<[AccountIdentity; 2]>,
}

impl CreationCatalog for RadioCreationAdapter {
    fn resolve(
        &self,
        account_key: &str,
        account: &AccountIdentity,
        model: &str,
        effort: Option<&str>,
        fast_mode: bool,
        daybreak_enabled: bool,
    ) -> Result<ExecutionSettings, CreateError> {
        let home = account
            .home
            .as_deref()
            .ok_or(CreateError::CapabilityUnavailable(
                "provider-session/model-list",
            ))?;
        let provider = account
            .provider
            .as_deref()
            .ok_or(CreateError::CapabilityUnavailable(
                "provider-session/model-list",
            ))?;
        let (value, key, fresh_home, fresh_provider, expires_at) =
            match self.service.read_for_admission(account_key, home, provider) {
                studio_runtime_services::model_catalog::AdmissionCatalogRead::Fresh {
                    value,
                    account_key,
                    home,
                    provider,
                    expires_at,
                    ..
                } => (value, account_key, home, provider, expires_at),
                _ => {
                    return Err(CreateError::CapabilityUnavailable(
                        "provider-session/model-list",
                    ));
                }
            };
        if key != account_key
            || fresh_home != home
            || fresh_provider != provider
            || expires_at <= std::time::Instant::now()
        {
            return Err(CreateError::CapabilityUnavailable(
                "provider-session/model-list",
            ));
        }
        let row = value
            .get("data")
            .and_then(Value::as_array)
            .and_then(|rows| {
                rows.iter().find(|row| {
                    !row.get("hidden").and_then(Value::as_bool).unwrap_or(false)
                        && (row.get("model").and_then(Value::as_str) == Some(model)
                            || row.get("resolvedModel").and_then(Value::as_str) == Some(model))
                })
            })
            .ok_or(CreateError::InvalidCreation("model"))?;
        let supported = row
            .get("supportedReasoningEfforts")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| row.get("reasoningEffort").and_then(Value::as_str))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if effort.is_some_and(|effort| !supported.contains(&effort)) {
            return Err(CreateError::InvalidCreation("effort"));
        }
        if fast_mode
            && !row
                .get("serviceTiers")
                .and_then(Value::as_array)
                .is_some_and(|tiers| {
                    tiers
                        .iter()
                        .any(|tier| tier.get("id").and_then(Value::as_str) == Some("priority"))
                })
        {
            return Err(CreateError::InvalidCreation("fast_mode"));
        }
        let native_effort = effort
            .map(str::to_owned)
            .or_else(|| {
                row.get("defaultReasoningEffort")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "medium".to_owned());
        let programs = row
            .get("availableAccessPrograms")
            .and_then(|programs| programs.get("cyber"))
            .and_then(Value::as_array);
        let program = if daybreak_enabled {
            programs
                .and_then(|items| {
                    ["daybreakBlue", "daybreakRed"]
                        .into_iter()
                        .find(|name| items.iter().any(|item| item.as_str() == Some(name)))
                })
                .ok_or(CreateError::InvalidCreation("daybreak_enabled"))?
        } else {
            "standard"
        };
        Ok(ExecutionSettings {
            model: model.to_owned(),
            effort: effort.map(str::to_owned),
            native_effort,
            fast_mode,
            daybreak_enabled,
            cyber_access_program: program.to_owned(),
        })
    }
}

impl studio_runtime_services::radio_runtime::RadioCreation for RadioCreationAdapter {
    fn prepare(
        &mut self,
        body: &Value,
        path: &Path,
        ids: [studio_runtime_store::AgentId; 2],
    ) -> Result<[studio_runtime_store::AgentRecord; 2], ProjectsError> {
        let participants = body
            .get("participants")
            .and_then(Value::as_array)
            .ok_or(ProjectsError::Invalid("participants"))?;
        let identities = (0..2)
            .map(|index| {
                let key = participants
                    .get(index)
                    .and_then(|p| p.get("account_key"))
                    .and_then(Value::as_str)
                    .ok_or(ProjectsError::Invalid("account_key"))?;
                let row = self
                    .rows
                    .iter()
                    .find(|row| row.get("id").and_then(Value::as_str) == Some(key))
                    .ok_or(ProjectsError::Invalid("Unknown account"))?;
                if row.get("deleted").and_then(Value::as_bool) == Some(true)
                    || row.get("disconnected").and_then(Value::as_bool) == Some(true)
                {
                    return Err(ProjectsError::Requires("account-driver/unavailable"));
                }
                Ok(AccountIdentity {
                    provider: Some(
                        row.get("provider")
                            .and_then(Value::as_str)
                            .unwrap_or("codex")
                            .to_owned(),
                    ),
                    home: row.get("home").and_then(Value::as_str).map(str::to_owned),
                    account_id: row
                        .get("accountId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    email: row.get("email").and_then(Value::as_str).map(str::to_owned),
                })
            })
            .collect::<Result<Vec<_>, _>>()?
            .try_into()
            .map_err(|_| ProjectsError::Invalid("participants"))?;
        let drafts = studio_runtime_services::host_worker_create::prepare(
            body,
            path,
            ids,
            &identities,
            self,
        )
        .map_err(radio_create_error)?;
        self.identities = Some(identities);
        Ok(drafts)
    }
    fn recheck(
        &mut self,
        tx: &studio_runtime_store::Transaction<'_>,
        drafts: &[studio_runtime_store::AgentRecord; 2],
    ) -> Result<(), ProjectsError> {
        let accounts = self
            .identities
            .as_ref()
            .ok_or(ProjectsError::Requires("account-driver/unavailable"))?;
        studio_runtime_services::host_worker_create::recheck(tx, drafts, accounts)
            .map_err(radio_create_error)
    }
}

fn radio_create_error(error: CreateError) -> ProjectsError {
    match error {
        CreateError::Store(error) => ProjectsError::Store(error),
        CreateError::CapabilityUnavailable(dependency) => ProjectsError::Requires(dependency),
        CreateError::InvalidCreation(_) | CreateError::IdentityChanged => {
            ProjectsError::Invalid("radio participant creation was rejected")
        }
        CreateError::Internal => ProjectsError::Invalid("radio participant creation failed"),
        CreateError::InvalidStoredObject { .. } => {
            ProjectsError::Invalid("radio participant record is invalid")
        }
        _ => ProjectsError::Invalid("radio participant creation was rejected"),
    }
}

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
    if body.get("action").and_then(Value::as_str) == Some("radio")
        && body.get("radio_action").and_then(Value::as_str) == Some("create")
    {
        let rows = state
            .account_driver
            .as_ref()
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::NOT_IMPLEMENTED,
                    "Not implemented: requires account-driver/unavailable",
                )
            })?
            .snapshot()
            .map_err(|_| ApiError::internal())?
            .accounts;
        let mut creation = RadioCreationAdapter {
            service: state.model_catalog.clone(),
            rows,
            identities: None,
        };
        let value = tokio::task::spawn_blocking(move || {
            studio_runtime_services::radio_runtime::create_with(&store, &body, &mut creation)
                .map_err(projects_error)
        })
        .await
        .map_err(|_| ApiError::internal())??;
        return Ok(Json(value));
    }
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
