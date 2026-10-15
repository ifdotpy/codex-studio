//! HTTP adapters for projects, locations and peer teams.

use super::extractors::{ApiJson, QueryParams};
use super::manifest::{Dependency, OperationStatus, RouteGroup, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use studio_runtime_services::agents_create::CreateError;
use studio_runtime_services::host_worker_create::{
    AccountIdentity, CreationCatalog, ExecutionSettings,
};
use studio_runtime_services::projects::{self, ProjectsError};

use crate::error::ApiError;
use crate::router::HttpState;

struct RadioCreationAdapter {
    service: studio_runtime_services::model_catalog::ModelCatalogService,
    runtime: tokio::runtime::Handle,
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
        let provider = account
            .provider
            .as_deref()
            .ok_or(CreateError::InvalidCreation("provider"))?;
        let value = self
            .runtime
            .block_on(studio_runtime_services::model_catalog::ModelListPort::list(
                &self.service,
                account_key,
                provider,
                json!({"limit": studio_runtime_schema::shared_values::MODEL_CATALOG_ITEMS_PER_PAGE}),
            ))
            .map_err(|_| CreateError::Internal)?;
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
                    return Err(ProjectsError::Invalid(
                        "radio participant account is unavailable",
                    ));
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
            .ok_or(ProjectsError::Invalid("radio participant creation failed"))?;
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

const PROJECT_WRITE_DEPENDENCIES: [Dependency; 1] = [Dependency::new(
    "cross-server-driver/project-location-write",
)];
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
            OperationStatus::Implemented,
            project_locations,
        ),
        post("/api/peer-teams", OperationStatus::Implemented, peer_teams),
    ])
}

async fn list_projects(
    State(state): State<HttpState>,
) -> Result<Json<crate::generated::ProjectReadResponse>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let value = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/projects",
        move || {
            let value = projects::list(&store).map_err(projects_error)?;
            super::generated_json::<crate::generated::ProjectReadResponse>(value)
        },
    )
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(value))
}

async fn write_project(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Response, ApiError> {
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
    if projects::write_dependency(&body).is_some() {
        let credentials = Arc::clone(&state.credentials);
        let origin = state.public_origin.clone();
        let identity_store = Arc::clone(&store);
        let identity = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/routes/projects/identity",
            move || {
                credentials
                    .identity(&identity_store, origin.as_deref())
                    .map_err(|_| {
                        ApiError::new(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "The local server identity is unavailable",
                        )
                    })
            },
        )
        .await
        .map_err(|_| ApiError::internal())??;
        let local = identity
            .get("serverId")
            .and_then(Value::as_str)
            .ok_or_else(ApiError::internal)?
            .to_owned();
        let requested_server = body
            .get("server")
            .and_then(Value::as_str)
            .unwrap_or("local");
        let is_registration = body
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("register")
            == "register";
        if is_registration && (requested_server == "local" || requested_server == local) {
            let local_server = local.clone();
            let value = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-http/routes/projects/local-registration",
                move || local_project_registration(&store, &body, &default, &known, &local_server),
            )
            .await
            .map_err(|_| ApiError::internal())??;
            if studio_runtime_schema::first_error("RemoteProjectRegistrationResponse", &value)
                .map_err(|_| ApiError::internal())?
                .is_some()
            {
                return Err(ApiError::internal());
            }
            return Ok(Json(ProjectWriteResponse::Remote(value)).into_response());
        }
        if requested_server != "local" && local != requested_server {
            let driver = state.orchestration_driver.ok_or_else(|| {
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The paired server transport is unavailable",
                )
            })?;
            let now = studio_runtime_services::event_write::unix_time()
                .map_err(|_| ApiError::internal())? as i64;
            let is_registration = body
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("register")
                == "register";
            let value = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-http/routes/projects/remote-write",
                move || remote_project_write(&store, &body, &default, &known, &driver, now),
            )
            .await
            .map_err(|_| ApiError::internal())??;
            let response_schema = if is_registration {
                "RemoteProjectRegistrationResponse"
            } else {
                "ServerOperationResponse"
            };
            if studio_runtime_schema::first_error(response_schema, &value)
                .map_err(|_| ApiError::internal())?
                .is_some()
            {
                return Err(ApiError::internal());
            }
            return Ok(Json(ProjectWriteResponse::Remote(value)).into_response());
        }
    }
    let mut body = body;
    if body.get("server").is_some() {
        body["server"] = Value::Null;
    }
    let is_remove = body.get("action").and_then(Value::as_str) == Some("remove");
    let is_sidebar = body.get("action").and_then(Value::as_str) == Some("reorder");
    let value = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/projects",
        move || {
            let value = projects::write(&store, &body, &default, &known).map_err(projects_error)?;
            if is_remove {
                super::generated_json::<crate::generated::ProjectRemovalResponse>(value)
                    .map(ProjectWriteResponse::Removal)
            } else if is_sidebar {
                super::generated_json::<crate::generated::SidebarOrderResponse>(value)
                    .map(ProjectWriteResponse::Sidebar)
            } else {
                super::generated_json::<crate::generated::ProjectMutationRecord>(value)
                    .map(ProjectWriteResponse::Mutation)
            }
        },
    )
    .await
    .map_err(|_| ApiError::internal())??;
    let mut response = Json(value).into_response();
    if is_sidebar {
        // Python preserves the durable sidebar receipt on both first apply and
        // exact retry; request-specific sync decoration would alter its shape.
        response
            .extensions_mut()
            .insert(crate::mutation_sync::SkipMutationSyncEnvelope);
    }
    Ok(response)
}

fn local_project_registration(
    store: &studio_runtime_store::RuntimeStore,
    body: &Value,
    default_account: &str,
    known_accounts: &[Value],
    local_server: &str,
) -> Result<Value, ApiError> {
    let path = body
        .get("path")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 4096)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "A location path is required"))?;
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty() && value.chars().count() <= 255)
        .unwrap_or_else(|| {
            Path::new(path)
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("Project")
        });
    let request_id = body
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "A request ID is required"))?;
    let project_id = format!(
        "project:{}",
        projects::cross_server_identity(&["logical-project", local_server, request_id])
    );
    let operation_id = projects::cross_server_identity(&["project-location", "ui", request_id]);
    let registration = json!({"server":local_server,"path":path,"name":name});
    let operation =
        json!({"action":"add_location","project":project_id,"server":local_server,"path":path});
    let registration_signature = studio_runtime_store::sha256_hex(
        studio_runtime_services::remote_transport::canonical_json(&registration)
            .map_err(|_| ApiError::internal())?
            .as_bytes(),
    );
    let operation_signature = studio_runtime_store::sha256_hex(
        studio_runtime_services::remote_transport::canonical_json(&operation)
            .map_err(|_| ApiError::internal())?
            .as_bytes(),
    );
    let now =
        studio_runtime_services::event_write::unix_time().map_err(|_| ApiError::internal())? as i64;
    store
        .transaction_with(|tx| -> Result<(), ProjectsError> {
            let receipt_id = format!("project-registration:{project_id}");
            if let Some((saved, previous)) =
                studio_runtime_store::ProjectsRepository::receipt(tx, &receipt_id)?
            {
                if saved != registration_signature {
                    return Err(ProjectsError::Conflict(
                        "This request ID has different content",
                    ));
                }
                if previous.get("outcome").and_then(Value::as_str) == Some("applied") {
                    return Ok(());
                }
            } else {
                let project = json!({"id":project_id,"path":project_id,"name":name,"created":now,
                    "homeServerId":local_server,"locations":[],"locationsRevision":0,
                    "accountKey":default_account,"accountRevision":1,"registration":registration});
                studio_runtime_store::ProjectsRepository::put_project(tx, &project)?;
                studio_runtime_store::ProjectsRepository::save_receipt(
                    tx,
                    &receipt_id,
                    &registration_signature,
                    &json!({"outcome":"unknown","projectId":project_id,"requestId":operation_id}),
                )?;
            }
            Ok(())
        })
        .map_err(projects_error)?;
    let replay = store
        .transaction_with(|tx| -> Result<Option<Value>, ProjectsError> {
            if let Some((saved, mut previous)) =
                studio_runtime_store::ProjectsRepository::receipt(tx, &operation_id)?
            {
                if saved != operation_signature {
                    return Err(ProjectsError::Conflict(
                        "This request ID has different content",
                    ));
                }
                previous["projectId"] = json!(project_id);
                return Ok(Some(previous));
            }
            studio_runtime_store::ProjectsRepository::save_receipt(
                tx,
                &operation_id,
                &operation_signature,
                &json!({"requestId":operation_id,"outcome":"unknown","projectId":project_id}),
            )?;
            Ok(None)
        })
        .map_err(projects_error)?;
    if let Some(previous) = replay {
        return Ok(previous);
    }
    let registered = projects::write(
        store,
        &json!({"action":"register","path":path,"name":name}),
        default_account,
        known_accounts,
    )
    .map_err(projects_error)?;
    let canonical = registered
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(ApiError::internal)?;
    let physical_id = registered
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(ApiError::internal)?;
    let git_origin = projects::peer_location_info(canonical)
        .ok()
        .and_then(|value| value.get("gitOrigin").cloned())
        .unwrap_or(Value::Null);
    let value = json!({"serverId":local_server,"path":path,"canonicalPath":canonical,
        "projectId":physical_id,"gitOrigin":git_origin,"homeProjectId":project_id,
        "requestId":operation_id});
    store
        .transaction_with(|tx| -> Result<(), ProjectsError> {
            if physical_id != project_id {
                if let Some((_, mut physical)) =
                    studio_runtime_store::ProjectsRepository::get(tx, physical_id)?
                {
                    let mut aliases = physical
                        .get("projectAliases")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    aliases.retain(|alias| {
                        alias.get("serverId").and_then(Value::as_str) != Some(local_server)
                            || alias.get("projectId").and_then(Value::as_str)
                                != Some(&project_id)
                    });
                    aliases.push(json!({"serverId":local_server,"projectId":project_id,"name":name}));
                    physical["projectAliases"] = Value::Array(aliases);
                    studio_runtime_store::ProjectsRepository::put_project(tx, &physical)?;
                }
            }
            let (_, mut logical) = studio_runtime_store::ProjectsRepository::get(tx, &project_id)?
                .ok_or(ProjectsError::Invalid("project is unavailable"))?;
            let mut locations = logical
                .get("locations")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if !locations.iter().any(|location| {
                location.get("serverId").and_then(Value::as_str) == Some(local_server)
            }) {
                locations.push(json!({"serverId":local_server,"path":canonical,
                    "projectId":physical_id,"gitOrigin":git_origin}));
            }
            logical["locations"] = Value::Array(locations);
            logical["locationsRevision"] = json!(logical.get("locationsRevision")
                .and_then(Value::as_u64).unwrap_or(0).saturating_add(1));
            studio_runtime_store::ProjectsRepository::put_project(tx, &logical)?;
            tx.execute(
                "UPDATE runtime_operation_receipts SET result=?1 WHERE id=?2 AND signature=?3",
                rusqlite::params![json!({"requestId":operation_id,"outcome":"applied","value":value,"projectId":project_id}).to_string(), operation_id, operation_signature],
            )?;
            tx.execute(
                "UPDATE runtime_operation_receipts SET result=?1 WHERE id=?2",
                rusqlite::params![json!({"requestId":operation_id,"outcome":"applied","value":value,"projectId":project_id}).to_string(),format!("project-registration:{project_id}")],
            )?;
            Ok(())
        })
        .map_err(projects_error)?;
    Ok(json!({"requestId":operation_id,"outcome":"applied","value":value,"projectId":project_id}))
}

#[derive(serde::Serialize)]
#[serde(untagged)]
enum ProjectWriteResponse {
    Remote(Value),
    Removal(crate::generated::ProjectRemovalResponse),
    Sidebar(crate::generated::SidebarOrderResponse),
    Mutation(crate::generated::ProjectMutationRecord),
}

fn remote_project_write(
    store: &studio_runtime_store::RuntimeStore,
    body: &Value,
    default_account: &str,
    known_accounts: &[Value],
    driver: &studio_runtime_services::multi_server_orchestration::OrchestrationDriver,
    now: i64,
) -> Result<Value, ApiError> {
    use studio_runtime_services::multi_server_orchestration::ExchangeOutcome;
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("register");
    let server = body
        .get("server")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "A paired server is required"))?;
    let request_id = body
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "A request ID is required"))?;
    let local_server = store
        .local_server_id()
        .map_err(ApiError::stored)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "The local server identity is unavailable",
            )
        })?;
    if server != local_server {
        let connection = store.read().map_err(ApiError::stored)?;
        let paired = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_access_clients WHERE id=?1 AND json_extract(record,'$.kind')='server' AND json_extract(record,'$.status')='paired')",
                [server],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|_| ApiError::internal())?;
        if !paired {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "The server is not paired or is revoked",
            ));
        }
    }
    if !known_accounts
        .iter()
        .any(|account| account.get("id").and_then(Value::as_str) == Some(default_account))
    {
        return Err(ApiError::internal());
    }
    let (project_id, operation_id, signature_data, mut payload, creation_signature) = if action
        == "register"
    {
        let path = body
            .get("path")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= 4096)
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "A location path is required"))?;
        let name = body
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty() && value.chars().count() <= 255)
            .unwrap_or_else(|| {
                Path::new(path)
                    .file_name()
                    .and_then(|v| v.to_str())
                    .unwrap_or("Project")
            });
        let id = format!(
            "project:{}",
            projects::cross_server_identity(&["logical-project", &local_server, request_id])
        );
        let op = projects::cross_server_identity(&["project-location", "ui", request_id]);
        let registration = json!({"server":server,"path":path,"name":name});
        let signature_data =
            json!({"action":"add_location","project":id,"server":server,"path":path});
        let payload = json!({"project":id,"name":name,"path":path,"request":signature_data});
        (id, op, signature_data, payload, Some(registration))
    } else if matches!(action, "add_location" | "remove_location") {
        let project = body
            .get("project")
            .or_else(|| body.get("path"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Select an existing project"))?;
        let operation = projects::cross_server_identity(&["project-location", "ui", request_id]);
        let project_row = store.read().map_err(ApiError::stored)?;
        let (_, record) = studio_runtime_store::ProjectsRepository::get(&project_row, project)
            .map_err(ApiError::stored)?
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Select an existing project"))?;
        let locations = record
            .get("locations")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let existing = locations
            .iter()
            .find(|row| row.get("serverId").and_then(Value::as_str) == Some(server));
        let path = if action == "add_location" {
            Some(
                body.get("path")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty() && value.len() <= 4096)
                    .ok_or_else(|| {
                        ApiError::new(StatusCode::BAD_REQUEST, "A location path is required")
                    })?,
            )
        } else {
            None
        };
        if action == "remove_location" && locations.len() <= 1 {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "Keep at least one project location",
            ));
        }
        let signature_data = json!({"action":action,"project":project,"server":server,"path":path});
        let payload = json!({"project":project,"name":record.get("name"),"path":path,
            "request":signature_data,"signature":studio_runtime_store::sha256_hex(
                studio_runtime_services::remote_transport::canonical_json(&signature_data)
                    .map_err(|_| ApiError::internal())?.as_bytes()),
            "location":existing});
        (project.to_owned(), operation, signature_data, payload, None)
    } else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Unknown project action",
        ));
    };
    let signature = studio_runtime_store::sha256_hex(
        studio_runtime_services::remote_transport::canonical_json(&signature_data)
            .map_err(|_| ApiError::internal())?
            .as_bytes(),
    );
    payload["signature"] = json!(signature);
    let mut registration_signature = None;
    if let Some(registration) = &creation_signature {
        let creation_signature = studio_runtime_store::sha256_hex(
            studio_runtime_services::remote_transport::canonical_json(registration)
                .map_err(|_| ApiError::internal())?
                .as_bytes(),
        );
        registration_signature = Some(creation_signature.clone());
        store.transaction_with(|tx| -> Result<(), ProjectsError> {
            let receipt_id = format!("project-registration:{project_id}");
            let mut has_receipt = false;
            if let Some((saved, previous)) = studio_runtime_store::ProjectsRepository::receipt(tx, &receipt_id)? {
                has_receipt = true;
                if saved != creation_signature {
                    return Err(ProjectsError::Conflict("This request ID has different content"));
                }
                if previous.get("outcome").and_then(Value::as_str) == Some("applied") {
                    return Ok(());
                }
            }
            if studio_runtime_store::ProjectsRepository::get(tx, &project_id)?.is_none() {
                let name = registration["name"].as_str().unwrap_or("Project");
                let project = json!({"id":project_id,"path":project_id,"name":name,"created":now,
                    "homeServerId":local_server,"locations":[],"locationsRevision":0,"accountKey":default_account,
                    "accountRevision":1,"registration":registration});
                studio_runtime_store::ProjectsRepository::put_project(tx, &project)?;
            }
            if !has_receipt {
                studio_runtime_store::ProjectsRepository::save_receipt(tx, &receipt_id, &creation_signature,
                    &json!({"outcome":"unknown","projectId":project_id,"requestId":operation_id}))?;
            }
            Ok(())
        }).map_err(projects_error)?;
    }
    {
        let connection = store.read().map_err(ApiError::stored)?;
        if let Some((saved, mut previous)) =
            studio_runtime_store::ProjectsRepository::receipt(&connection, &operation_id)
                .map_err(ApiError::stored)?
        {
            if saved != signature {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "This request ID has different content",
                ));
            }
            if action == "register" {
                previous["projectId"] = json!(project_id);
            }
            return Ok(previous);
        }
    }
    let outcome = driver
        .exchange(
            server,
            if action == "register" {
                "add_location"
            } else {
                action
            },
            &payload,
            &operation_id,
            now,
        )
        .map_err(|error| {
            ApiError::new(
                StatusCode::from_u16(error.status()).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
                error.to_string(),
            )
        })?;
    match &outcome {
        ExchangeOutcome::Applied(value) => {
            let remote = value;
            if action != "remove_location"
                && (remote.get("serverId").and_then(Value::as_str) != Some(server)
                    || remote.get("homeProjectId").and_then(Value::as_str)
                        != Some(project_id.as_str())
                    || remote.get("requestId").and_then(Value::as_str)
                        != Some(operation_id.as_str()))
            {
                return Err(ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "The paired server returned a different project location",
                ));
            }
            let project = project_id.clone();
            let remote = remote.clone();
            let location_action = if action == "register" {
                "add_location"
            } else {
                action
            };
            store.transaction_with(|tx| -> Result<(), ProjectsError> {
                let Some((_, mut record)) = studio_runtime_store::ProjectsRepository::get(tx, &project)? else {
                    return Ok(());
                };
                let mut locations = record.get("locations").and_then(Value::as_array).cloned().unwrap_or_default();
                locations.retain(|row| row.get("serverId").and_then(Value::as_str) != Some(server));
                if location_action == "add_location" {
                    let canonical = remote.get("canonicalPath").and_then(Value::as_str).unwrap_or_default();
                    let remote_project = remote.get("projectId").and_then(Value::as_str).unwrap_or_default();
                    if !Path::new(canonical).is_absolute() || canonical != remote_project {
                        return Err(ProjectsError::BadRequest("The server returned a different project location"));
                    }
                    let mut location = json!({"serverId":server,"path":canonical,"projectId":remote_project});
                    if !remote["gitOrigin"].is_null() { location["gitOrigin"] = remote["gitOrigin"].clone(); }
                    if remote.get("path").and_then(Value::as_str) != Some(canonical) { location["requestedPath"] = remote["path"].clone(); }
                    locations.push(location);
                }
                record["locations"] = Value::Array(locations);
                record["locationsRevision"] = json!(record.get("locationsRevision").and_then(Value::as_u64).unwrap_or(0) + 1);
                studio_runtime_store::ProjectsRepository::put_project(tx, &record)?;
                studio_runtime_store::ProjectsRepository::save_receipt(tx, &operation_id, &signature, &outcome.to_value(&operation_id))?;
                if registration_signature.is_some() {
                    tx.execute(
                        "UPDATE runtime_operation_receipts SET result=?1 WHERE id=?2",
                        rusqlite::params![json!({"requestId":operation_id,"outcome":"applied","value":remote,"projectId":project}).to_string(), format!("project-registration:{project}")],
                    )?;
                }
                Ok(())
            }).map_err(projects_error)?;
        }
        ExchangeOutcome::NotApplied(message) => {
            return Err(ApiError::new(StatusCode::BAD_REQUEST, message));
        }
        ExchangeOutcome::Unknown => {
            return Ok(
                json!({"requestId":operation_id,"outcome":"unknown","projectId":project_id}),
            );
        }
    }
    let mut response = outcome.to_value(&operation_id);
    if action == "register" {
        response["projectId"] = json!(project_id);
    }
    Ok(response)
}

async fn project_locations(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<crate::generated::ProjectLocationQueryResponse>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let query = Value::Object((*query.into_typed()).clone());
    let remote_request = {
        let store = store.clone();
        let query = query.clone();
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/routes/projects/prepare-remote-location",
            move || remote_location_request(&store, &query),
        )
        .await
        .map_err(|_| ApiError::internal())??
    };
    if let Some((server, action, payload)) = remote_request {
        let driver = state.orchestration_driver.ok_or_else(ApiError::internal)?;
        let request_id = studio_runtime_services::remote_transport::new_nonce()
            .map_err(|_| ApiError::internal())?;
        let now = studio_runtime_services::event_write::unix_time()
            .map_err(|_| ApiError::internal())? as i64;
        let outcome = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/routes/projects/remote-location",
            move || driver.exchange(&server, &action, &payload, &request_id, now),
        )
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(|error| {
            ApiError::new(
                StatusCode::from_u16(error.status()).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
                error.to_string(),
            )
        })?;
        let value = match outcome {
            studio_runtime_services::multi_server_orchestration::ExchangeOutcome::Applied(
                value,
            ) => value,
            studio_runtime_services::multi_server_orchestration::ExchangeOutcome::NotApplied(_) => {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "The paired server could not read this project folder",
                ));
            }
            studio_runtime_services::multi_server_orchestration::ExchangeOutcome::Unknown => {
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The paired server read outcome is unknown",
                ));
            }
        };
        return super::generated_json::<crate::generated::ProjectLocationQueryResponse>(value)
            .map(Json);
    }
    let value = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/projects",
        move || projects::location_query(&store, &query).map_err(projects_error),
    )
    .await
    .map_err(|_| ApiError::internal())??;
    super::generated_json::<crate::generated::ProjectLocationQueryResponse>(value).map(Json)
}

fn remote_location_request(
    store: &studio_runtime_store::RuntimeStore,
    query: &Value,
) -> Result<Option<(String, String, Value)>, ApiError> {
    let requested = query
        .get("server")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let local = store.local_server_id().map_err(ApiError::stored)?;
    let server = if requested == "local" {
        local.as_deref().unwrap_or_default()
    } else {
        requested
    };
    if server.is_empty() || local.as_deref() == Some(server) {
        return Ok(None);
    }
    let paired = store
        .read()
        .map_err(ApiError::stored)?
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_access_clients WHERE id=?1 AND json_extract(record,'$.kind')='server' AND json_extract(record,'$.status')='paired')",
            [server],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|_| ApiError::internal())?;
    if !paired {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "The server is not paired or is revoked",
        ));
    }
    let project_id = query
        .get("project")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Select an existing project"))?;
    let db = store.read().map_err(ApiError::stored)?;
    let project = studio_runtime_store::ProjectsRepository::get(&db, project_id)
        .map_err(ApiError::stored)?
        .map(|(_, value)| value)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Select an existing project"))?;
    drop(db);
    let action = if query.get("action").and_then(Value::as_str) == Some("matches") {
        let origin = project
            .get("locations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find_map(|row| {
                row.get("gitOrigin")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .or_else(|| {
                project
                    .get("path")
                    .and_then(Value::as_str)
                    .filter(|path| !path.starts_with("project:"))
                    .and_then(|path| {
                        studio_runtime_services::projects::peer_location_info(path).ok()
                    })
                    .and_then(|value| {
                        value
                            .get("gitOrigin")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
            })
            .ok_or_else(|| {
                ApiError::new(StatusCode::BAD_REQUEST, "The project has no Git origin")
            })?;
        (
            "location_matches".to_owned(),
            json!({"origin":origin,"cwd":query.get("path")}),
        )
    } else {
        let path = query
            .get("path")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .or_else(|| {
                project
                    .get("locations")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .find(|row| row.get("serverId").and_then(Value::as_str) == Some(server))
                    .and_then(|row| row.get("path").and_then(Value::as_str))
            })
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "This server has no folder for the project",
                )
            })?;
        ("location_info".to_owned(), json!({"cwd":path}))
    };
    Ok(Some((server.to_owned(), action.0, action.1)))
}

async fn peer_teams(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<PeerTeamWriteResponse>, ApiError> {
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
            .ok_or_else(ApiError::internal)?
            .snapshot()
            .map_err(|_| ApiError::internal())?
            .accounts;
        let mut creation = RadioCreationAdapter {
            service: state.model_catalog.clone(),
            runtime: tokio::runtime::Handle::current(),
            rows,
            identities: None,
        };
        let value = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/routes/projects",
            move || {
                studio_runtime_services::radio_runtime::create_with(&store, &body, &mut creation)
                    .map_err(projects_error)
            },
        )
        .await
        .map_err(|_| ApiError::internal())??;
        state.event_wake.notify_one();
        return Ok(Json(PeerTeamWriteResponse::Raw(value)));
    }
    let is_radio = body.get("action").and_then(Value::as_str) == Some("radio");
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/projects",
        move || {
            let value = projects::peer_team_write(&store, &body, &default_account, &known)
                .map_err(projects_error)?;
            match body["action"].as_str() {
                Some("radio") => {
                    super::generated_json::<crate::generated::PeerRadioResponse>(value)
                        .map(PeerTeamWriteResponse::Radio)
                }
                Some("convert") => {
                    super::generated_json::<crate::generated::PeerConversionResponse>(value)
                        .map(PeerTeamWriteResponse::Conversion)
                }
                _ => super::generated_json::<crate::generated::PeerProjectResponse>(value)
                    .map(PeerTeamWriteResponse::Project),
            }
        },
    )
    .await
    .map_err(|_| ApiError::internal())??;
    if is_radio {
        state.event_wake.notify_one();
    }
    Ok(Json(result))
}

#[derive(serde::Serialize)]
#[serde(untagged)]
enum PeerTeamWriteResponse {
    Raw(Value),
    Radio(crate::generated::PeerRadioResponse),
    Conversion(crate::generated::PeerConversionResponse),
    Project(crate::generated::PeerProjectResponse),
}

fn projects_error(error: ProjectsError) -> ApiError {
    match error {
        ProjectsError::Invalid(_) => ApiError::validation(vec![Value::String("body".to_owned())]),
        ProjectsError::BadRequest(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
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
            "cross-server-driver/project-location-write",
            "provider-session/model-list",
        ],
    }
}

#[cfg(test)]
mod local_registration_tests {
    use super::{local_project_registration, remote_project_write};
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use studio_runtime_services::account_policy::AccountPolicyState;
    use studio_runtime_services::{
        multi_server_auth::AccessError,
        multi_server_orchestration::{
            ExchangeOutcome, OrchestrationDriver, OrchestrationEffects, PairedTransport,
        },
    };
    use studio_runtime_store::{ProjectsRepository, RuntimeStore};

    struct ProjectPeer(Arc<AtomicUsize>);

    impl PairedTransport for ProjectPeer {
        fn exchange(
            &self,
            _server: &str,
            envelope: &serde_json::Value,
            _timeout: std::time::Duration,
        ) -> Result<serde_json::Value, AccessError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let payload = &envelope["payload"];
            let path = payload["path"].as_str().expect("remote path");
            Ok(json!({
                "requestId": envelope["requestId"],
                "outcome": "applied",
                "value": {
                    "serverId": "peer",
                    "path": path,
                    "canonicalPath": path,
                    "projectId": path,
                    "gitOrigin": null,
                    "homeProjectId": payload["project"],
                    "requestId": envelope["requestId"]
                }
            }))
        }
    }

    struct NoEffects;

    impl OrchestrationEffects for NoEffects {
        fn supports(&self, _action: &str) -> bool {
            false
        }

        fn execute(
            &self,
            _principal: &str,
            _envelope: &serde_json::Value,
        ) -> Result<ExchangeOutcome, AccessError> {
            unreachable!("outbound project registration does not execute local effects")
        }
    }

    #[test]
    fn local_registration_saves_logical_location_and_replays_its_receipt() {
        let root = tempfile::tempdir().expect("state directory");
        let store = RuntimeStore::open(Some(root.path())).expect("runtime store");
        let project_path = root.path().join("project");
        std::fs::create_dir_all(&project_path).expect("project directory");
        let policy = AccountPolicyState::from_startup(
            Some(br#"{"version":1,"defaultAccountKey":"default","accounts":{"default":{"id":"default","home":"/default","label":"Default","source":"Codex CLI","status":"signedOut"}},"logins":{}}"#),
            [],
        )
        .expect("account snapshot");
        let accounts = policy.accounts();
        let body = json!({
            "action":"register",
            "server":"local",
            "path":project_path.to_string_lossy(),
            "request_id":"local-registration-1"
        });
        let identity = studio_runtime_services::federation_credentials::Credentials::default()
            .identity(&store, Some("https://local.fixture.ts.net"))
            .expect("persisted identity");
        let first = local_project_registration(
            &store,
            &body,
            "default",
            &accounts.accounts,
            identity["serverId"].as_str().expect("server ID"),
        )
        .expect("local registration");
        let replay = local_project_registration(
            &store,
            &body,
            "default",
            &accounts.accounts,
            identity["serverId"].as_str().expect("server ID"),
        )
        .expect("receipt replay");
        assert_eq!(first, replay);
        assert_eq!(first["outcome"], "applied");
        let connection = store.read().expect("store read");
        let (_, logical) = ProjectsRepository::get(
            &connection,
            first["projectId"].as_str().expect("project ID"),
        )
        .expect("logical project read")
        .expect("logical project");
        assert_eq!(logical["locations"].as_array().expect("locations").len(), 1);
        assert_eq!(logical["locations"][0]["serverId"], identity["serverId"]);
    }

    #[test]
    fn remote_project_registration_replays_saved_receipts_without_second_send() {
        let root = tempfile::tempdir().expect("state directory");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).expect("runtime store"));
        studio_runtime_services::federation_credentials::Credentials::default()
            .identity(&store, Some("https://local.fixture.ts.net"))
            .expect("persisted local identity");
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2)",
                    [
                        "peer",
                        r#"{"id":"peer","clientId":"local","kind":"server","label":"Peer","created":1,"status":"paired","origin":"https://peer.fixture.ts.net","publicKey":"fixture"}"#,
                    ],
                )?;
                Ok(())
            })
            .expect("paired fake server");
        let calls = Arc::new(AtomicUsize::new(0));
        let driver = OrchestrationDriver::new(
            Arc::clone(&store),
            Arc::new(ProjectPeer(Arc::clone(&calls))),
            Arc::new(NoEffects),
        );
        let policy = AccountPolicyState::from_startup(
            Some(br#"{"version":1,"defaultAccountKey":"default","accounts":{"default":{"id":"default","home":"/default","label":"Default","source":"Codex CLI","status":"signedOut"}},"logins":{}}"#),
            [],
        )
        .expect("account snapshot");
        let body = json!({
            "action":"register",
            "server":"peer",
            "path":"/remote/project",
            "name":"Remote project",
            "request_id":"remote-registration-1"
        });
        let first = remote_project_write(
            store.as_ref(),
            &body,
            "default",
            &policy.accounts().accounts,
            &driver,
            10,
        )
        .expect("fake peer registration");
        let replay = remote_project_write(
            store.as_ref(),
            &body,
            "default",
            &policy.accounts().accounts,
            &driver,
            11,
        )
        .expect("saved receipt replay");
        assert_eq!(first, replay);
        assert_eq!(first["outcome"], "applied");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let connection = store.read().expect("store read");
        let project = first["projectId"].as_str().expect("project ID");
        let (_, record) = ProjectsRepository::get(&connection, project)
            .expect("project read")
            .expect("logical project");
        assert_eq!(record["locations"][0]["serverId"], "peer");
        assert_eq!(record["locations"][0]["path"], "/remote/project");
    }
}
