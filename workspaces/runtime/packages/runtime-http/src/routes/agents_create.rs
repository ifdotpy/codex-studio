//! Typed HTTP boundary for persisted agent operations.
use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, post};
use crate::{error::ApiError, generated, router::HttpState};
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde_json::Value;
use studio_runtime_services::agents_create::{
    self, Context, CreateError, Creation, OperationError,
};
use studio_runtime_services::host_worker_create::{
    AccountIdentity, CreationCatalog, ExecutionSettings,
};

/// Compose the same fresh model admission and P02 host snapshot used by agent
/// creation for managed Spawn, Review, and monitor admission.
pub fn managed_creation_environment(
    accounts: std::sync::Arc<studio_runtime_services::account_driver::AccountDriver>,
    catalog: studio_runtime_services::model_catalog::ModelCatalogService,
) -> std::sync::Arc<dyn studio_runtime_services::provider_callbacks::ManagedCreationEnvironment> {
    std::sync::Arc::new(ManagedCreationEnvironment {
        accounts,
        catalog: RouteCreationCatalog { service: catalog },
    })
}

struct ManagedCreationEnvironment {
    accounts: std::sync::Arc<studio_runtime_services::account_driver::AccountDriver>,
    catalog: RouteCreationCatalog,
}

impl studio_runtime_services::provider_callbacks::ManagedCreationEnvironment
    for ManagedCreationEnvironment
{
    fn wait_for_catalog<'a>(
        &'a self,
        account_key: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError>> + Send + 'a>,
    >{
        Box::pin(async move {
            use studio_runtime_services::provider_callbacks::ManagedCreationAccountRead;
            use studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError as E;
            let identity = DriverManagedAccountRead(self.accounts.as_ref())
                .resolve_identity(account_key, account_key)
                .map_err(|_| E::Unavailable("account-driver/managed-child-account"))?;
            let account = host_account_identity(&identity);
            let (Some(home), Some(provider)) =
                (account.home.as_deref(), account.provider.as_deref())
            else {
                return Err(E::Unavailable("provider-session/model-list"));
            };
            match self
                .catalog
                .service
                .wait_for_admission_catalog(account_key, home, provider)
                .await
            {
                studio_runtime_services::model_catalog::AdmissionCatalogRead::Fresh { .. } => {
                    Ok(())
                }
                studio_runtime_services::model_catalog::AdmissionCatalogRead::Pending
                | studio_runtime_services::model_catalog::AdmissionCatalogRead::Unavailable => {
                    Err(E::Unavailable("provider-session/model-list"))
                }
            }
        })
    }

    fn select_worker_account(
        &self,
        parent_account_key: &str,
        requested_account_key: Option<&str>,
        default_account_key: Option<&str>,
        model: &str,
        excluded_account_keys: &[String],
        project_account_keys: Option<&[String]>,
    ) -> Result<
        (
            String,
            studio_runtime_services::host_worker_create::AccountIdentity,
        ),
        studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError,
    > {
        use studio_runtime_services::provider_callbacks::{
            ManagedAccountIdentity, WorkerAccountSelection, select_worker_account,
        };
        let accounts = DriverManagedAccountRead(self.accounts.as_ref());
        let mut model_available = |key: &str, identity: &ManagedAccountIdentity, model: &str| {
            let account = host_account_identity(identity);
            match self
                .catalog
                .resolve(key, &account, model, None, false, false)
            {
                Ok(_) => Ok(true),
                Err(
                    studio_runtime_services::agents_create::CreateError::CapabilityUnavailable(
                        name,
                    ),
                ) => Err(studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError::Unavailable(name)),
                Err(studio_runtime_services::agents_create::CreateError::InvalidCreation(_)) => {
                    Ok(false)
                }
                Err(_) => Err(studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError::Invalid(
                    "Worker account admission was refused",
                )),
            }
        };
        let (key, identity) = select_worker_account(WorkerAccountSelection {
            accounts: &accounts,
            parent_account_key,
            requested_account_key,
            default_account_key,
            model,
            excluded_account_keys,
            project_account_keys,
            model_available: &mut model_available,
        })?;
        Ok((key, host_account_identity(&identity)))
    }

    fn catalog(&self) -> &dyn CreationCatalog {
        &self.catalog
    }

    fn validate_worker_choice(
        &self,
        store: &studio_runtime_store::RuntimeStore,
        account_key: &str,
        account: &AccountIdentity,
        requested: &str,
        default: &str,
    ) -> Result<(), studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError>
    {
        use studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError as E;
        studio_runtime_services::provider_callbacks::validate_worker_model_choice(
            requested, default, None,
        )
        .map_err(|_| E::Invalid("Choose a model from the provider selected in chat settings"))?;
        if requested == default {
            return Ok(());
        }
        let path = store.root().join("model-pricing/models-dev-v1.json");
        let artifact = match std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        {
            Some(value) if value.get("version").and_then(Value::as_u64) == Some(1) => value,
            _ => return Ok(()),
        };
        let Some(catalog) = artifact.get("catalog").and_then(|value| {
            studio_runtime_services::pricing::PricingCatalog::from_value(value).ok()
        }) else {
            return Ok(());
        };
        let requested_name = self.catalog.pricing_name(account_key, account, requested)?;
        let default_name = self.catalog.pricing_name(account_key, account, default)?;
        let provider_for = |model: &str| {
            if model.starts_with("gpt-") {
                "openai"
            } else if model.starts_with("claude-")
                || model == "default"
                || model == "sonnet"
                || model == "opus"
                || model == "haiku"
            {
                "anthropic"
            } else if account.provider.as_deref() == Some("claude") {
                "anthropic"
            } else {
                "openai"
            }
        };
        let prices = Some((
            catalog.worker_choice_rates(provider_for(requested), &requested_name),
            catalog.worker_choice_rates(provider_for(default), &default_name),
        ));
        studio_runtime_services::provider_callbacks::validate_worker_model_choice(
            requested, default, prices,
        )
        .map_err(|_| {
            E::Invalid("Choose a model that costs no more than the default subagent model")
        })
    }

    fn prepare_workspace(
        &self,
        store: &studio_runtime_store::RuntimeStore,
        actor: &studio_runtime_store::AgentRecord,
        args: &Value,
        _request_id: &str,
    ) -> Result<
        Box<dyn studio_runtime_services::monitor_runtime::WorkspaceAdmission + Send + Sync>,
        studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError,
    > {
        let id = actor.id.0.clone();
        let epoch = actor.epoch;
        let cwd = actor.extra.get("cwd").and_then(Value::as_str).ok_or(
            studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError::Invalid(
                "Select a project directory",
            ),
        )?;
        let mode = args
            .get("workspace")
            .and_then(Value::as_str)
            .or_else(|| actor.extra.get("workspaceMode").and_then(Value::as_str))
            .unwrap_or("shared");
        if !matches!(mode, "shared" | "worktree") {
            return Err(studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError::Unavailable(if mode == "image" {
                "workspace-state/image-workspace"
            } else {
                "workspace-state/vm-workspace"
            }));
        }
        let prepared = studio_runtime_services::host_project_runtime::PreparedHost::load(store)
            .map_err(|_| studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError::Unavailable("workspace-state/host-preparation"))?;
        prepared
            .recheck_paths()
            .map_err(|_| studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError::Unavailable("workspace-state/host-preparation"))?;
        Ok(Box::new(PreparedManagedWorkspace {
            id,
            epoch,
            cwd: cwd.to_owned(),
            prepared,
        }))
    }
}

struct DriverManagedAccountRead<'a>(&'a studio_runtime_services::account_driver::AccountDriver);

impl studio_runtime_services::provider_callbacks::ManagedCreationAccountRead
    for DriverManagedAccountRead<'_>
{
    fn worker_accounts(
        &self,
    ) -> Result<
        studio_runtime_services::provider_callbacks::ManagedCreationAccountListing,
        studio_runtime_services::provider_callbacks::ManagedCreationAccountError,
    > {
        use studio_runtime_services::provider_callbacks::{
            ManagedAccountIdentity, ManagedCreationAccountCandidate as Candidate,
            ManagedCreationAccountListing,
        };
        let snapshot = self.0.listing_snapshot().map_err(account_read_error)?;
        let accounts = snapshot
            .accounts
            .iter()
            .filter_map(|row| {
                let key = row.get("id")?.as_str()?.to_owned();
                Some(Candidate {
                    account_key: key,
                    identity: ManagedAccountIdentity {
                        provider: row
                            .get("provider")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        home: row.get("home").and_then(Value::as_str).map(str::to_owned),
                        account_id: row
                            .get("accountId")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        email: row.get("email").and_then(Value::as_str).map(str::to_owned),
                    },
                    ready: row.get("status").and_then(Value::as_str) == Some("ready"),
                    can_attempt_native_proof: row
                        .get("canAttemptNativeProof")
                        .and_then(Value::as_bool)
                        == Some(true),
                    disconnected: row.get("disconnected").and_then(Value::as_bool) == Some(true),
                    deleted: row.get("deleted").and_then(Value::as_bool) == Some(true),
                    duplicate: row.get("duplicateOf").and_then(Value::as_str).is_some(),
                })
            })
            .collect();
        let default = snapshot.default_account_key.as_str().map(str::to_owned);
        Ok(ManagedCreationAccountListing {
            default_account_key: default,
            accounts,
        })
    }

    fn resolve_identity(
        &self,
        account_key: &str,
        _parent_account_key: &str,
    ) -> Result<
        studio_runtime_services::provider_callbacks::ManagedAccountIdentity,
        studio_runtime_services::provider_callbacks::ManagedCreationAccountError,
    > {
        let snapshot = self.0.listing_snapshot().map_err(account_read_error)?;
        let row = snapshot
            .accounts
            .iter()
            .find(|row| row.get("id").and_then(Value::as_str) == Some(account_key))
            .ok_or(
                studio_runtime_services::provider_callbacks::ManagedCreationAccountError::Unknown,
            )?;
        if row.get("deleted").and_then(Value::as_bool) == Some(true) {
            return Err(
                studio_runtime_services::provider_callbacks::ManagedCreationAccountError::Deleted,
            );
        }
        if row.get("disconnected").and_then(Value::as_bool) == Some(true) {
            return Err(studio_runtime_services::provider_callbacks::ManagedCreationAccountError::Disconnected);
        }
        Ok(
            studio_runtime_services::provider_callbacks::ManagedAccountIdentity {
                provider: row
                    .get("provider")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                home: row.get("home").and_then(Value::as_str).map(str::to_owned),
                account_id: row
                    .get("accountId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                email: row.get("email").and_then(Value::as_str).map(str::to_owned),
            },
        )
    }
}

fn account_read_error(
    error: studio_runtime_services::account_driver::AccountDriverError,
) -> studio_runtime_services::provider_callbacks::ManagedCreationAccountError {
    use studio_runtime_services::{
        account_driver::AccountDriverError as E,
        provider_callbacks::ManagedCreationAccountError as M,
    };
    match error {
        E::UnknownAccount => M::Unknown,
        E::DeletedAccount => M::Deleted,
        E::SignInRequired => M::SignInRequired,
        _ => M::Unavailable,
    }
}

fn host_account_identity(
    identity: &studio_runtime_services::provider_callbacks::ManagedAccountIdentity,
) -> studio_runtime_services::host_worker_create::AccountIdentity {
    studio_runtime_services::host_worker_create::AccountIdentity {
        // Older/default Codex registry rows omit provider; the account registry
        // contract treats that representation as Codex.
        provider: Some(identity.provider.as_deref().unwrap_or("codex").to_owned()),
        home: identity.home.clone(),
        account_id: identity.account_id.clone(),
        email: identity.email.clone(),
    }
}

struct PreparedManagedWorkspace {
    id: String,
    epoch: i64,
    cwd: String,
    prepared: studio_runtime_services::host_project_runtime::PreparedHost,
}

impl studio_runtime_services::monitor_runtime::WorkspaceAdmission for PreparedManagedWorkspace {
    fn check(
        &self,
        tx: &studio_runtime_store::Transaction<'_>,
        actor: &studio_runtime_store::AgentRecord,
    ) -> Result<(), studio_runtime_services::monitor_policy::MonitorError> {
        if actor.id.0 != self.id
            || actor.epoch != self.epoch
            || actor.extra.get("cwd").and_then(Value::as_str) != Some(self.cwd.as_str())
        {
            return Err(
                studio_runtime_services::monitor_policy::MonitorError::Refused(
                    "Workspace changed during preparation",
                ),
            );
        }
        self.prepared.recheck(tx).map_err(|_| {
            studio_runtime_services::monitor_policy::MonitorError::Refused(
                "Workspace changed during preparation",
            )
        })
    }
}

struct RouteCreationCatalog {
    service: studio_runtime_services::model_catalog::ModelCatalogService,
}

impl CreationCatalog for RouteCreationCatalog {
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

impl RouteCreationCatalog {
    fn pricing_name(
        &self,
        account_key: &str,
        account: &AccountIdentity,
        model: &str,
    ) -> Result<String, studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError>
    {
        use studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError as E;
        let (Some(home), Some(provider)) = (account.home.as_deref(), account.provider.as_deref())
        else {
            return Err(E::Unavailable("provider-session/model-list"));
        };
        let catalog = match self.service.read_for_admission(account_key, home, provider) {
            studio_runtime_services::model_catalog::AdmissionCatalogRead::Fresh {
                value,
                account_key: key,
                home: fresh_home,
                provider: fresh_provider,
                expires_at,
                ..
            } if key == account_key
                && fresh_home == home
                && fresh_provider == provider
                && expires_at > std::time::Instant::now() =>
            {
                value
            }
            _ => return Err(E::Unavailable("provider-session/model-list")),
        };
        catalog
            .get("data")
            .and_then(Value::as_array)
            .and_then(|rows| {
                rows.iter().find(|row| {
                    !row.get("hidden").and_then(Value::as_bool).unwrap_or(false)
                        && (row.get("model").and_then(Value::as_str) == Some(model)
                            || row.get("resolvedModel").and_then(Value::as_str) == Some(model))
                })
            })
            .map(|row| {
                row.get("resolvedModel")
                    .and_then(Value::as_str)
                    .or_else(|| row.get("model").and_then(Value::as_str))
                    .unwrap_or(model)
                    .to_owned()
            })
            .ok_or(E::Invalid(
                "This model is not available for the selected worker accounts",
            ))
    }
}

pub(crate) fn routes() -> RouteGroup {
    // Account-readiness refusal is kept as a runtime condition, not a missing
    // implementation: Python raises the same ValueError for cross-account
    // children in codex_runtime.py:4928-4930.
    const LEAD_DEPENDENCIES: [Dependency; 5] = [
        Dependency::new("cross-server-driver/local-identity"),
        Dependency::new("cross-server-driver/project-binding"),
        Dependency::new("project-driver/folder-selection"),
        Dependency::new("workspace-state/image-workspace"),
        Dependency::new("workspace-state/vm-workspace"),
    ];
    const RENAME_DEPENDENCIES: [Dependency; 1] =
        [Dependency::new("provider-session/title-generation")];
    const AGENT_DEPENDENCIES: [Dependency; 2] = [
        Dependency::new("workspace-state/image-workspace"),
        Dependency::new("workspace-state/vm-workspace"),
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
            OperationStatus::Implemented,
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

async fn dispatch<T: serde::de::DeserializeOwned + serde::Serialize + Send + 'static>(
    state: HttpState,
    operation: impl FnOnce(&Context<'_>) -> Result<Value, OperationError> + Send + 'static,
) -> Result<Json<T>, ApiError> {
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/agents_create",
        move || {
            let store = state.store.as_deref().ok_or_else(ApiError::internal)?;
            let catalog = RouteCreationCatalog {
                service: state.model_catalog.clone(),
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
        },
    )
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(result)
}

async fn create_lead(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::CreateLeadRequest>,
) -> Result<Json<generated::AgentResponse>, ApiError> {
    create_lead_with_model(State(state), ApiJson(body)).await
}

async fn create_lead_with_model(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::CreateLeadRequest>,
) -> Result<Json<generated::AgentResponse>, ApiError> {
    let needs_catalog = body.model.as_ref().is_some_and(Option::is_some)
        || body.effort.is_some()
        || body.fast_mode.is_some()
        || body.daybreak_enabled.is_some()
        || body.worker_defaults.is_some()
        || body.review_defaults.is_some();
    let requested_model = body.model.as_ref().and_then(Option::as_deref);
    let requested_account = body.account_key.as_deref();
    wait_for_agent_catalog(
        &state,
        None,
        requested_account,
        requested_model,
        needs_catalog,
    )
    .await?;
    if let Some(Some(defaults)) = body.worker_defaults.as_ref() {
        let worker_model = defaults.model.as_deref();
        let worker_account = defaults.account_key.as_ref().and_then(Option::as_deref);
        if worker_model.is_some() || worker_account.is_some() {
            wait_for_agent_catalog(&state, None, worker_account, worker_model, true).await?;
        }
    }
    if let Some(Some(review)) = body.review_defaults.as_ref() {
        if let Some(model) = review.model.as_deref() {
            wait_for_agent_catalog(&state, None, requested_account, Some(model), true).await?;
        }
    }
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
) -> Result<Json<generated::AgentResponse>, ApiError> {
    create_agent_with_import(State(state), ApiJson(body), None).await
}

async fn create_agent_with_import(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::CreateAgentRequest>,
    imported_from: Option<String>,
) -> Result<Json<generated::AgentResponse>, ApiError> {
    if state.store.is_none() && state.account_driver.is_none() {
        return Err(ApiError::internal());
    }
    wait_for_agent_catalog(
        &state,
        body.parent.as_ref().and_then(Option::as_deref),
        body.account_key.as_deref(),
        body.model.as_ref().and_then(Option::as_deref),
        body.parent.as_ref().is_some_and(Option::is_some)
            || body.model.as_ref().is_some_and(Option::is_some)
            || body.effort.is_some()
            || body.fast_mode.is_some()
            || body.daybreak_enabled.is_some()
            || body.profile_id.is_some(),
    )
    .await?;
    let request = Creation {
        intent: serde_json::to_value(&body).map_err(|_| ApiError::internal())?,
        catalog: body.model.as_ref().is_some_and(Option::is_some)
            || body.effort.is_some()
            || body.fast_mode.is_some()
            || body.daybreak_enabled.is_some()
            || body.profile_id.is_some(),
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
        imported_from,
        ..Creation::default()
    };
    dispatch::<generated::AgentResponse>(state, move |context| {
        agents_create::agent(context, request)
    })
    .await
}

async fn wait_for_agent_catalog(
    state: &HttpState,
    parent_id: Option<&str>,
    requested_account: Option<&str>,
    requested_model: Option<&str>,
    needs_catalog: bool,
) -> Result<(), ApiError> {
    if !needs_catalog {
        return Ok(());
    }
    let (Some(driver), Some(session)) = (&state.account_driver, &state.account_session) else {
        return Ok(());
    };
    let catalog = state.model_catalog.clone();
    catalog.set_account_session(Some(std::sync::Arc::clone(session)));
    let started = tokio::time::Instant::now();
    let deadline = started
        + std::time::Duration::from_secs(
            studio_runtime_schema::shared_values::MODEL_CATALOG_ADMISSION_WAIT_SECONDS,
        );
    let driver = std::sync::Arc::clone(driver);
    let store = state.store.clone();
    let parent_id = parent_id.map(str::to_owned);
    let requested_account = requested_account.map(str::to_owned);
    let requested_model = requested_model.map(str::to_owned);
    let candidate_data = tokio::time::timeout_at(
        deadline,
        tokio::task::spawn_blocking(move || {
            let snapshot = driver.listing_snapshot().map_err(|_| ())?;
            let mut parent_account = None;
            let mut selected_account = None;
            let mut root_model = None;
            let mut worker_model = None;
            let mut excluded = std::collections::HashSet::new();
            if let Some(parent_id) = parent_id.as_deref() {
                let connection = store.as_deref().ok_or(())?.read().map_err(|_| ())?;
                let parent = studio_runtime_store::system_reads::get(
                    &connection,
                    "runtime_agents",
                    parent_id,
                )
                .map_err(|_| ())?
                .ok_or(())?;
                parent_account = parent
                    .get("accountKey")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if let Some(root_id) = parent.get("rootId").and_then(Value::as_str) {
                    let root = studio_runtime_store::system_reads::get(
                        &connection,
                        "runtime_agents",
                        root_id,
                    )
                    .map_err(|_| ())?
                    .unwrap_or(parent);
                    selected_account = root
                        .pointer("/workerDefaults/accountKey")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    root_model = root.get("model").and_then(Value::as_str).map(str::to_owned);
                    worker_model = root
                        .pointer("/workerDefaults/model")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    excluded.extend(
                        root.get("excludedAccountKeys")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .map(str::to_owned),
                    );
                }
            }
            let explicit = requested_account;
            let model = requested_model
                .or(worker_model)
                .or(root_model)
                .unwrap_or_else(|| "gpt-6-astra".to_owned());
            let provider = if model.starts_with("gpt-") {
                Some("codex")
            } else if model.starts_with("claude-")
                || matches!(model.as_str(), "default" | "sonnet" | "opus" | "haiku")
            {
                Some("claude")
            } else {
                None
            };
            let pinned = explicit.or(selected_account);
            let mut keys = Vec::new();
            let mut add = |key: Option<&str>| {
                if let Some(key) = key.filter(|key| !excluded.contains(*key))
                    && !keys.iter().any(|known| known == key)
                {
                    keys.push(key.to_owned());
                }
            };
            if let Some(key) = pinned.as_deref() {
                add(Some(key));
            } else {
                add(parent_account.as_deref());
                add(snapshot.default_account_key.as_str());
                for row in &snapshot.accounts {
                    let key = row.get("id").and_then(Value::as_str);
                    let row_provider = row
                        .get("provider")
                        .and_then(Value::as_str)
                        .unwrap_or("codex");
                    if provider.is_none_or(|wanted| wanted == row_provider) {
                        add(key);
                    }
                }
            }
            let accounts = snapshot
                .accounts
                .into_iter()
                .filter_map(|row| {
                    let key = row.get("id")?.as_str()?.to_owned();
                    let home = row.get("home")?.as_str()?.to_owned();
                    let provider = row
                        .get("provider")
                        .and_then(Value::as_str)
                        .unwrap_or("codex")
                        .to_owned();
                    keys.contains(&key).then_some((key, home, provider))
                })
                .collect::<Vec<_>>();
            Ok::<_, ()>((pinned.is_some(), accounts))
        }),
    )
    .await
    .map_err(|_| operation_error(OperationError::Unavailable("provider-session/model-list")))?
    .map_err(|_| ApiError::internal())?
    .map_err(|_| ApiError::internal())?;
    if candidate_data.1.is_empty() {
        return Err(operation_error(OperationError::Unavailable(
            "provider-session/model-list",
        )));
    }
    for (account, home, provider) in candidate_data.1 {
        let session = std::sync::Arc::clone(session);
        let account_for_session = account.clone();
        let ensured = tokio::time::timeout_at(
            deadline,
            tokio::task::spawn_blocking(move || session.ensure_session(&account_for_session)),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .is_some_and(|result| result.is_ok());
        if !ensured {
            continue;
        }
        let fresh = tokio::time::timeout_at(
            deadline,
            catalog.wait_for_admission_catalog(&account, &home, &provider),
        )
        .await
        .is_ok_and(|result| {
            matches!(
                result,
                studio_runtime_services::model_catalog::AdmissionCatalogRead::Fresh { .. }
            )
        });
        if fresh {
            return Ok(());
        }
    }
    Err(operation_error(OperationError::Unavailable(
        "provider-session/model-list",
    )))
}

/// Import uses the same creation admission, catalog validation and durable
/// initial-event writer as POST /api/agents after it has built the transcript.
pub(super) async fn create_imported_agent(
    state: HttpState,
    body: Value,
    imported_from: String,
) -> Result<Json<generated::AgentResponse>, ApiError> {
    let body: generated::CreateAgentRequest =
        serde_json::from_value(body).map_err(|_| ApiError::internal())?;
    create_agent_with_import(State(state), ApiJson(body), Some(imported_from)).await
}

pub(super) async fn create_branch_draft(
    state: HttpState,
    body: Value,
) -> Result<Json<generated::AgentResponse>, ApiError> {
    let body: generated::CreateLeadRequest =
        serde_json::from_value(body).map_err(|_| ApiError::internal())?;
    create_lead_with_model(State(state), ApiJson(body)).await
}
async fn configure(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::ConfigureRequest>,
) -> Result<Json<generated::ConfigureResponse>, ApiError> {
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
    ApiJson(value): ApiJson<Value>,
) -> Result<Json<generated::AgentResponse>, ApiError> {
    for field in ["model", "fast_mode", "worker_defaults", "review_defaults"] {
        if value.get(field).is_some_and(Value::is_null) {
            return Err(ApiError::validation(vec![
                Value::String("body".to_owned()),
                Value::String(field.to_owned()),
            ]));
        }
    }
    let body: generated::ConversationRequest = serde_json::from_value(value.clone())
        .map_err(|_| ApiError::validation(vec![Value::String("body".to_owned())]))?;
    let needs_catalog = ["model", "effort", "fast_mode", "daybreak_enabled"]
        .iter()
        .any(|field| value.get(*field).is_some())
        || value
            .get("worker_defaults")
            .is_some_and(|entry| !entry.is_null())
        || value
            .get("review_defaults")
            .is_some_and(|entry| !entry.is_null());
    let parent_id = Some(body.id.as_str());
    let requested_model = body.model.as_ref().and_then(Option::as_deref);
    wait_for_agent_catalog(&state, parent_id, None, requested_model, needs_catalog).await?;
    if let Some(defaults) = value
        .get("worker_defaults")
        .filter(|entry| !entry.is_null())
    {
        let worker_model = defaults.get("model").and_then(Value::as_str);
        let worker_account = defaults.get("account_key").and_then(Value::as_str);
        if worker_model.is_some() || worker_account.is_some() {
            wait_for_agent_catalog(&state, parent_id, worker_account, worker_model, true).await?;
        }
    }
    if let Some(model) = value
        .get("review_defaults")
        .and_then(|defaults| defaults.get("model"))
        .and_then(Value::as_str)
    {
        wait_for_agent_catalog(&state, parent_id, None, Some(model), true).await?;
    }
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
) -> Result<axum::response::Response, ApiError> {
    let name = body.name.flatten();
    if name.is_none() {
        let request_id = body
            .request_id
            .flatten()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| ApiError::validation(vec![Value::String("body".to_owned())]))?;
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let provider = state.title_request.clone().ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Title provider session is unavailable",
            )
        })?;
        let driver = state.account_driver.clone();
        let catalog = state.model_catalog.clone();
        if let Some(driver) = driver {
            let accounts = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-http/routes/agents_create/rename",
                move || driver.snapshot().map(|snapshot| snapshot.accounts),
            )
            .await
            .map_err(|_| ApiError::internal())?
            .map_err(|_| ApiError::internal())?;
            catalog.retain_accounts(&accounts);
        }
        let agent_id = body.id.clone();
        let account_catalog = catalog.clone();
        let account_snapshot = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/routes/agents_create/rename",
            move || {
                let connection = store.read().map_err(|_| ApiError::internal())?;
                let agent =
                    studio_runtime_store::WorkspaceViewsRepository::agent(&connection, &agent_id)
                        .map_err(|_| ApiError::internal())?
                        .ok_or_else(|| {
                            ApiError::new(StatusCode::NOT_FOUND, "Unknown managed agent")
                        })?;
                let record = agent.value();
                let account = record
                    .get("accountKey")
                    .and_then(Value::as_str)
                    .unwrap_or("default");
                let provider = record
                    .get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or("codex");
                let Some((home, catalog_provider)) = account_catalog.account_profile(account)
                else {
                    return Err(ApiError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Model catalog is unavailable for this account",
                    ));
                };
                if catalog_provider != provider {
                    return Err(ApiError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Provider account changed; reload the conversation",
                    ));
                }
                Ok((account.to_owned(), provider.to_owned(), home))
            },
        )
        .await
        .map_err(|_| ApiError::internal())??;
        let (account, provider_name, home) = account_snapshot;
        let models = match catalog
            .model_list_port()
            .list(
                &account,
                &provider_name,
                serde_json::json!({"limit": studio_runtime_schema::shared_values::MODEL_CATALOG_ITEMS_PER_PAGE}),
            )
            .await
        {
            Ok(models) => models,
            Err(_) => {
                // Starting the refresh is bounded and coalesced by the catalog owner.
                let _ = catalog.read(&account, &home, &provider_name, false).await;
                return Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "Model catalog is pending or unavailable"));
            }
        };
        let store = state.store.as_deref().ok_or_else(ApiError::internal)?;
        let result = studio_runtime_services::title_generation::generate(
            store,
            provider.as_ref(),
            &body.id,
            &models,
            &request_id,
            std::time::SystemTime::now()
                + studio_runtime_services::title_generation::REQUEST_TIMEOUT,
        )
        .await
        .map_err(|error| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, error.to_string()))?;
        let (title, status, error) = match result {
            studio_runtime_services::title_generation::TitleResult::Applied { title } => {
                state.event_wake.notify_one();
                (Some(title), "applied", None)
            }
            studio_runtime_services::title_generation::TitleResult::HeldUnknown => {
                (None, "pending", None)
            }
            studio_runtime_services::title_generation::TitleResult::RetryableNotSent => (
                None,
                "failed",
                Some("The title request could not start. Try /rename again."),
            ),
            studio_runtime_services::title_generation::TitleResult::Discarded
            | studio_runtime_services::title_generation::TitleResult::Skipped => (
                None,
                "failed",
                Some("The title could not be generated. Try /rename again."),
            ),
        };
        let response = serde_json::json!({
            "id": body.id,
            "name": title,
            "request_id": request_id,
            "status": status,
            "error": error,
        });
        let mut response = Json(super::generated_json::<generated::RenameResponse>(
            response,
        )?)
        .into_response();
        if status != "applied" {
            response
                .extensions_mut()
                .insert(crate::mutation_sync::SkipMutationSyncEnvelope);
        }
        return Ok(response);
    }
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/agents_create/rename",
        move || {
            let store = state.store.as_deref().ok_or_else(ApiError::internal)?;
            let catalog = RouteCreationCatalog {
                service: state.model_catalog.clone(),
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
            let result = agents_create::rename_operation(
                &context,
                &body.id,
                name.as_deref(),
                body.request_id.flatten().as_deref(),
            )
            .map_err(operation_error)?;
            if !result.replayed {
                state.event_wake.notify_one();
            }
            let mut response = super::generated_json::<generated::RenameResponse>(result.response)
                .map(Json)
                .map_err(|_| ApiError::internal())?
                .into_response();
            if result.replayed {
                response
                    .extensions_mut()
                    .insert(crate::mutation_sync::SkipMutationSyncEnvelope);
            }
            Ok(response)
        },
    )
    .await
    .map_err(|_| ApiError::internal())?
}
fn operation_error(error: OperationError) -> ApiError {
    use agents_create::MutationError;
    if matches!(
        error,
        OperationError::Create(MutationError::RequestIdRequired)
    ) {
        return ApiError::validation(vec![Value::String("body".to_owned())]);
    }
    let implemented_but_unavailable = matches!(
        &error,
        OperationError::Create(MutationError::CapabilityUnavailable(
            "provider-session/title-generation" | "account-driver/unavailable"
        ))
    );
    if implemented_but_unavailable {
        return ApiError::new(StatusCode::SERVICE_UNAVAILABLE, error.to_string());
    }
    let status = match &error {
        OperationError::Unavailable("account-driver/unavailable")
        | OperationError::Unavailable("provider-session/model-list")
        | OperationError::Create(MutationError::CapabilityUnavailable(
            "provider-session/model-list" | "worker-driver/settings-catalog",
        )) => StatusCode::BAD_REQUEST,
        OperationError::Unavailable(_)
        | OperationError::Create(MutationError::CapabilityUnavailable(_)) => {
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
    let message = match &error {
        OperationError::Unavailable("account-driver/unavailable") => {
            "Sign in to the worker account before creating a worker"
        }
        OperationError::Unavailable("provider-session/model-list")
        | OperationError::Create(MutationError::CapabilityUnavailable(
            "provider-session/model-list",
        )) => "Model catalog is unavailable",
        OperationError::Create(MutationError::CapabilityUnavailable(
            "worker-driver/settings-catalog",
        )) => "Worker model catalog is unavailable",
        _ => return ApiError::new(status, error.to_string()),
    };
    ApiError::new(status, message)
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "agents_create",
        routes: Some(routes),
        dependencies: &[
            "cross-server-driver/local-identity",
            "cross-server-driver/project-binding",
            "provider-session/title-generation",
            "account-driver/unavailable",
            "provider-session/model-list",
            "project-driver/folder-selection",
            "worker-driver/settings-catalog",
            "workspace-state/image-workspace",
            "workspace-state/vm-workspace",
        ],
    }
}
