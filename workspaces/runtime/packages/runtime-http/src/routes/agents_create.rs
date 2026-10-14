//! Typed HTTP boundary for persisted agent operations.
use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, post};
use crate::{error::ApiError, generated, router::HttpState};
use axum::{Json, extract::State, http::StatusCode};
use serde_json::Value;
use studio_runtime_services::agents_create::{
    self, Context, CreateError, Creation, OperationError,
};
use studio_runtime_services::host_worker_create::{
    AccountIdentity, CreationCatalog, ExecutionSettings,
};

struct RouteCreationCatalog<'a> {
    service: &'a studio_runtime_services::model_catalog::ModelCatalogService,
}

impl CreationCatalog for RouteCreationCatalog<'_> {
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
        let (value, fresh_key, fresh_home, fresh_provider, _, expires_at) =
            match self.service.read_for_admission(account_key, home, provider) {
                studio_runtime_services::model_catalog::AdmissionCatalogRead::Fresh {
                    value,
                    account_key,
                    home,
                    provider,
                    generation,
                    expires_at,
                } => (value, account_key, home, provider, generation, expires_at),
                _ => {
                    return Err(CreateError::CapabilityUnavailable(
                        "provider-session/model-list",
                    ));
                }
            };
        if fresh_key != account_key
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
        let native_effort = effort.map(str::to_owned).or_else(|| {
            row.get("defaultReasoningEffort")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
        let programs = row
            .get("availableAccessPrograms")
            .and_then(|programs| programs.get("cyber"))
            .and_then(Value::as_array);
        let program = if daybreak_enabled {
            programs
                .and_then(|programs| {
                    ["daybreakBlue", "daybreakRed"]
                        .into_iter()
                        .find(|name| programs.iter().any(|entry| entry.as_str() == Some(name)))
                })
                .ok_or(CreateError::InvalidCreation("daybreak_enabled"))?
        } else if programs.is_some_and(|programs| {
            !programs
                .iter()
                .any(|entry| entry.as_str() == Some("standard"))
        }) {
            return Err(CreateError::InvalidCreation("daybreak_enabled"));
        } else {
            "standard"
        };
        Ok(ExecutionSettings {
            model: model.to_owned(),
            effort: effort.map(str::to_owned),
            native_effort: native_effort.unwrap_or_else(|| "medium".to_owned()),
            fast_mode,
            daybreak_enabled,
            cyber_access_program: program.to_owned(),
        })
    }
}

pub(crate) fn routes() -> RouteGroup {
    const LEAD_DEPENDENCIES: [Dependency; 7] = [
        Dependency::new("account-driver/unavailable"),
        Dependency::new("cross-server-driver/local-identity"),
        Dependency::new("cross-server-driver/project-binding"),
        Dependency::new("provider-session/model-list"),
        Dependency::new("project-driver/folder-selection"),
        Dependency::new("workspace-state/image-workspace"),
        Dependency::new("workspace-state/vm-workspace"),
    ];
    const RENAME_DEPENDENCIES: [Dependency; 1] =
        [Dependency::new("provider-session/title-generation")];
    const AGENT_DEPENDENCIES: [Dependency; 4] = [
        Dependency::new("account-driver/unavailable"),
        Dependency::new("provider-session/model-list"),
        Dependency::new("workspace-state/image-workspace"),
        Dependency::new("workspace-state/vm-workspace"),
    ];
    const CONVERSATION_DEPENDENCIES: [Dependency; 2] = [
        Dependency::new("account-driver/unavailable"),
        Dependency::new("provider-session/model-list"),
    ];
    RouteGroup::new(vec![
        post(
            "/api/agents",
            OperationStatus::partial(&AGENT_DEPENDENCIES),
            create_agent,
        ),
        post("/api/configure", OperationStatus::Implemented, configure),
        post(
            "/api/conversation",
            OperationStatus::partial(&CONVERSATION_DEPENDENCIES),
            conversation,
        ),
        post(
            "/api/leads",
            OperationStatus::partial(&LEAD_DEPENDENCIES),
            create_lead,
        ),
        post(
            "/api/rename",
            OperationStatus::partial(&RENAME_DEPENDENCIES),
            rename,
        ),
    ])
}

async fn dispatch<T: serde::de::DeserializeOwned + serde::Serialize>(
    state: HttpState,
    operation: impl FnOnce(&Context<'_>) -> Result<Value, OperationError> + Send + 'static,
) -> Result<Json<Value>, ApiError> {
    let result = tokio::task::spawn_blocking(move || {
        let store = state.store.as_deref().ok_or_else(ApiError::internal)?;
        let catalog = RouteCreationCatalog {
            service: &state.model_catalog,
        };
        let context = Context {
            store,
            accounts: state.account_policy.as_deref(),
            driver: state.account_driver.as_deref(),
            catalog: Some(&catalog),
            validate: &|schema, value| match schema {
                "PersistedAgentRecord" => state
                    .validators
                    .validate_table_record("runtime_agents", value),
                _ => state.validators.validate_stored(schema, value),
            },
        };
        let result = operation(&context).map_err(operation_error)?;
        state.event_wake.notify_one();
        super::generated_json::<T>(result).map(Json)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(result)
}

async fn create_lead(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::CreateLeadRequest>,
) -> Result<Json<Value>, ApiError> {
    let request = Creation {
        intent: serde_json::to_value(&body).map_err(|_| ApiError::internal())?,
        catalog: body.effort.is_some()
            || body.fast_mode.is_some()
            || body.daybreak_enabled.is_some()
            || body.model.as_ref().is_some_and(Option::is_some),
        worker_catalog: body.worker_defaults.is_some() || body.review_defaults.is_some(),
        project_binding: body.project_id.flatten().is_some()
            || body.project_server_id.flatten().is_some()
            || body.project_folder.flatten().is_some(),
        id: body.id.flatten(),
        cwd: body.cwd,
        account: body.account_key,
        workspace: body.workspace_mode.map(|value| value.to_string()),
        yolo: body.yolo_mode,
        previous: body.previous.flatten(),
        reuse_empty: body.reuse_empty,
        ..Creation::default()
    };
    dispatch::<generated::AgentResponse>(state, move |context| {
        agents_create::lead(context, request)
    })
    .await
}
async fn create_agent(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::CreateAgentRequest>,
) -> Result<Json<Value>, ApiError> {
    if state.store.is_none() && state.account_driver.is_none() {
        return Err(operation_error(OperationError::Unavailable(
            "account-driver/unavailable",
        )));
    }
    let request = Creation {
        intent: serde_json::to_value(&body).map_err(|_| ApiError::internal())?,
        catalog: body.model.is_some()
            || body.effort.is_some()
            || body.fast_mode.is_some()
            || body.daybreak_enabled.is_some(),
        worker_catalog: body.worker_defaults.is_some(),
        id: body.id.flatten(),
        cwd: body.cwd,
        account: body.account_key,
        workspace: body.workspace_mode.map(|value| value.to_string()),
        name: body.name,
        prompt: body.prompt,
        role: body.role.map(|value| value.to_string()),
        yolo: body.yolo_mode,
        concurrency: body.concurrency,
        max_agents: body.max_agents,
        token_budget: body.token_budget.flatten(),
        profile: body.profile_id.flatten(),
        parent: body.parent.flatten(),
        ..Creation::default()
    };
    dispatch::<generated::AgentResponse>(state, move |context| {
        agents_create::agent(context, request)
    })
    .await
}
async fn configure(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::ConfigureRequest>,
) -> Result<Json<Value>, ApiError> {
    let request = agents_create::Configuration {
        id: body.id,
        concurrency: body.concurrency,
        max_agents: body.max_agents,
        token_budget: body.token_budget,
        expected_mode_revision: body.expected_mode_revision.flatten(),
        request_id: body.request_id.flatten(),
    };
    dispatch::<generated::ConfigureResponse>(state, move |context| {
        agents_create::configure(context, request)
    })
    .await
}
async fn conversation(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::ConversationRequest>,
) -> Result<Json<Value>, ApiError> {
    let request = agents_create::Conversation {
        non_defaults: body.next_turn.is_some() || body.request_id.is_some(),
        model_present: body.model.is_some(),
        effort_present: body.effort.is_some(),
        fast_mode_present: body.fast_mode.is_some(),
        daybreak_present: body.daybreak_enabled.is_some(),
        cwd: body.cwd,
        yolo_mode: body.yolo_mode,
        model: body.model.flatten(),
        effort: body.effort,
        fast_mode: body.fast_mode.flatten(),
        daybreak_enabled: body.daybreak_enabled.flatten(),
        next_turn: body.next_turn,
        request_id: body.request_id.clone(),
        mode_settings: body.agent_mode.is_some()
            || body.subagent_concurrency.is_some()
            || body.expected_mode_revision.is_some(),
        mode_limit: body.subagent_concurrency.or_else(|| {
            body.agent_mode.map(|mode| {
                if serde_json::to_value(mode)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .as_deref()
                    == Some("single")
                {
                    0
                } else {
                    32
                }
            })
        }),
        mode_revision: body.expected_mode_revision.flatten(),
        mode_request_id: body.request_id.flatten(),
        worker_settings: body.worker_defaults.is_some() || body.review_defaults.is_some(),
        worker_defaults: body
            .worker_defaults
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|_| ApiError::internal())?,
        review_defaults: body
            .review_defaults
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|_| ApiError::internal())?,
        id: body.id,
        expected_account: body.expected_account_key,
        excluded: body.excluded_account_keys.map(|mut keys| {
            keys.sort();
            keys
        }),
    };
    dispatch::<generated::AgentResponse>(state, move |context| {
        agents_create::conversation(context, request)
    })
    .await
}
async fn rename(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::RenameRequest>,
) -> Result<Json<Value>, ApiError> {
    dispatch::<generated::RenameResponse>(state, move |context| {
        agents_create::rename_operation(
            context,
            &body.id,
            body.name.flatten().as_deref(),
            body.request_id.flatten().as_deref(),
        )
    })
    .await
}
fn operation_error(error: OperationError) -> ApiError {
    use agents_create::MutationError;
    if matches!(
        error,
        OperationError::Create(MutationError::RequestIdRequired)
    ) {
        return ApiError::validation(vec![Value::String("body".to_owned())]);
    }
    let status = match &error {
        OperationError::Unavailable(_)
        | OperationError::Create(MutationError::CapabilityUnavailable(_))
        | OperationError::Create(MutationError::TitleGenerationUnavailable) => {
            StatusCode::NOT_IMPLEMENTED
        }
        OperationError::Internal
        | OperationError::Store(_)
        | OperationError::Create(
            MutationError::Store(_) | MutationError::InvalidStoredObject { .. },
        ) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    if matches!(
        error,
        OperationError::Store(_) | OperationError::Create(MutationError::Store(_))
    ) {
        return ApiError::internal();
    }
    ApiError::new(status, error.to_string())
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "agents_create",
        routes: Some(routes),
        dependencies: &[
        "account-driver/unavailable",
        "cross-server-driver/local-identity",
        "cross-server-driver/project-binding",
        "project-driver/folder-selection",
        "provider-session/model-list",
        "provider-session/title-generation",
        "worker-driver/settings-catalog",
        "workspace-state/image-workspace",
        "workspace-state/vm-workspace",
        ],
    }
}
