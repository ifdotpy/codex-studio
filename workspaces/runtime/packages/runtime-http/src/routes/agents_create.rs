//! Typed HTTP boundary for persisted agent operations.
use super::extractors::ApiJson;
use super::manifest::{OperationStatus, RouteGroup, post};
use crate::{error::ApiError, generated, router::HttpState};
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde_json::Value;
use studio_runtime_services::agents_create::{
    self, Context, CreateError, Creation, OperationError,
};
use studio_runtime_services::host_worker_create::{
    AccountIdentity, CreationCatalog, ExecutionSettings,
};
use studio_runtime_services::provider_adapter::{adapter, model_provider};

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
            let (Some(home), provider) = (account.home.as_deref(), account.provider.key()) else {
                return Err(E::Unavailable("provider-session/model-list"));
            };
            // Worker selection reads every candidate's catalog synchronously, so
            // refresh the other eligible accounts within the same bounded wait.
            let workers = DriverManagedAccountRead(self.accounts.as_ref())
                .worker_accounts()
                .map(|listing| listing.accounts)
                .unwrap_or_default()
                .into_iter()
                .filter(|row| {
                    row.account_key != account_key
                        && !row.disconnected
                        && !row.deleted
                        && !row.duplicate
                        && (row.ready || row.can_attempt_native_proof)
                })
                .filter_map(|row| {
                    let worker = host_account_identity(&row.identity);
                    Some((row.account_key, worker.home?, worker.provider.key()))
                })
                .collect::<Vec<_>>();
            let (parent, _) = futures_util::future::join(
                self.catalog
                    .service
                    .wait_for_admission_catalog(account_key, home, provider),
                futures_util::future::join_all(workers.iter().map(|(key, home, provider)| {
                    self.catalog
                        .service
                        .wait_for_admission_catalog(key, home, provider)
                })),
            )
            .await;
            match parent {
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
        request: studio_runtime_services::provider_callbacks::WorkerAccountRequest<'_>,
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
                .contains_model(key, &account, model)
            {
                Ok(available) => Ok(available),
                Err(studio_runtime_services::agents_create::CreateError::CapabilityUnavailable(name)) => Err(
                    studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError::Unavailable(name),
                ),
                Err(_) => Err(studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError::Unavailable(
                    "provider-session/model-list",
                )),
            }
        };
        let (key, identity) = select_worker_account(WorkerAccountSelection {
            accounts: &accounts,
            request,
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
        )?;
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
            adapter(model_provider(model).unwrap_or(account.provider)).pricing_vendor()
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
        _store: &studio_runtime_store::RuntimeStore,
        actor: &studio_runtime_store::AgentRecord,
        _args: &Value,
        _request_id: &str,
    ) -> Result<
        Box<dyn studio_runtime_services::monitor_runtime::WorkspaceAdmission + Send + Sync>,
        studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError,
    > {
        prepare_parent_workspace(actor)
    }
}

/// Admission fence for a child or monitor of `actor`: only the parent's own
/// workspace identity, never other agents' rows.
fn prepare_parent_workspace(
    actor: &studio_runtime_store::AgentRecord,
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
    // Admission validates the parent's existing workspace. The requested
    // child mode is independently validated and provisioned by creation.
    let mode = parent_workspace_mode(actor);
    if matches!(mode, "layr" | "linux") {
        let project_id = actor
                .extra
                .get("layrProjectId")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or(studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError::Invalid(
                    "The parent layr line is not ready",
                ))?;
        let line = actor
                .extra
                .get("layrLine")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or(studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError::Invalid(
                    "The parent layr line is not ready",
                ))?;
        if actor.extra.get("layrReady").and_then(Value::as_bool) != Some(true) {
            return Err(studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError::Invalid(
                    "The parent layr line is not ready",
                ));
        }
        return Ok(Box::new(VmManagedWorkspace {
            id,
            epoch,
            cwd: cwd.to_owned(),
            project_id: project_id.to_owned(),
            line: line.to_owned(),
        }));
    }
    if mode == "image" {
        let repo = actor
                .extra
                .get("imageWorkspaceRepo")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or(studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError::Invalid(
                    "The image workspace is not ready",
                ))?;
        if actor.extra.get("imageWorkspace").and_then(Value::as_bool) != Some(true)
            || actor
                .extra
                .get("imageWorkspaceReady")
                .and_then(Value::as_bool)
                != Some(true)
        {
            return Err(studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError::Invalid(
                    "The image workspace is not ready",
                ));
        }
        return Ok(Box::new(ImageManagedWorkspace {
            id,
            epoch,
            cwd: cwd.to_owned(),
            repo: repo.to_owned(),
        }));
    }
    if !matches!(mode, "shared" | "worktree") {
        return Err(
            studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError::Invalid(
                "Unknown workspace mode",
            ),
        );
    }
    // Python `spawn_agents` fenced only the parent: a shared or worktree
    // child inherits the parent's folder, so unrelated agents' rows are
    // not part of this admission.
    Ok(Box::new(HostManagedWorkspace {
        id,
        epoch,
        cwd: cwd.to_owned(),
    }))
}

fn parent_workspace_mode(actor: &studio_runtime_store::AgentRecord) -> &str {
    actor
        .extra
        .get("workspaceMode")
        .and_then(Value::as_str)
        .unwrap_or("shared")
}

#[cfg(test)]
mod workspace_admission_tests {
    use super::*;

    #[test]
    fn child_image_request_does_not_require_parent_image_repository() {
        let native = studio_runtime_store::AgentRecord::from_value(serde_json::json!({
            "id":"native-parent","rootId":"native-parent","epoch":1,"status":"queued",
            "workspaceMode":"worktree","cwd":"/fake/project"
        }))
        .expect("native parent");
        assert_eq!(parent_workspace_mode(&native), "worktree");

        let image_lead = studio_runtime_store::AgentRecord::from_value(serde_json::json!({
            "id":"image-parent","rootId":"image-parent","epoch":2,"status":"queued",
            "workspaceMode":"image","imageWorkspace":true,
            "imageWorkspaceReady":true,"imageWorkspaceRepo":"/fake/project",
            "cwd":"/fake/mount/repo"
        }))
        .expect("image lead");
        assert_eq!(parent_workspace_mode(&image_lead), "image");
    }

    /// Live 128-worker run: a second spawn batch was refused while the first
    /// batch's workers changed status, because admission fenced every agent row.
    #[tokio::test]
    async fn shared_child_admission_fences_only_the_parent() {
        let directory = tempfile::tempdir().expect("isolated state");
        let store = studio_runtime_store::RuntimeStore::open(Some(directory.path()))
            .await
            .expect("runtime store opens");
        let record = |id: &str, epoch: i64, status: &str| {
            studio_runtime_store::AgentRecord::from_value(serde_json::json!({
                "id":id,"rootId":"lead","epoch":epoch,"status":status,
                "workspaceMode":"shared","cwd":"/fake/project"
            }))
            .expect("agent record")
        };
        let lead = record("lead", 3, "running");
        let admission = prepare_parent_workspace(&lead).expect("shared parent admits");
        let worker = record("worker", 1, "starting");
        store
            .transaction(move |tx| studio_runtime_store::AgentRepository::put(tx, &worker))
            .await
            .expect("first batch worker");
        let restarted = record("lead", 4, "running");
        let (sibling_changed, parent_changed) = store
            .transaction(move |tx| {
                studio_runtime_store::AgentRepository::put(tx, &record("worker", 1, "idle"))?;
                Ok((
                    admission.check(tx, &lead).is_ok(),
                    admission.check(tx, &restarted).is_ok(),
                ))
            })
            .await
            .expect("admission transaction");
        assert!(sibling_changed, "another agent's status is not a fence");
        assert!(!parent_changed, "a new parent epoch is refused");
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
                        provider: studio_runtime_providers::ProviderKind::of_record(row).ok()?,
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
                    max_running_turns:
                        studio_runtime_services::account_start_gate::StartThrottle::of_record(row)
                            .max_running,
                })
            })
            .collect();
        Ok(ManagedCreationAccountListing { accounts })
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
                provider: studio_runtime_providers::ProviderKind::of_record(row).map_err(|_| {
                    studio_runtime_services::provider_callbacks::ManagedCreationAccountError::Unknown
                })?,
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
        provider: identity.provider,
        home: identity.home.clone(),
        account_id: identity.account_id.clone(),
        email: identity.email.clone(),
    }
}

struct HostManagedWorkspace {
    id: String,
    epoch: i64,
    cwd: String,
}

struct VmManagedWorkspace {
    id: String,
    epoch: i64,
    cwd: String,
    project_id: String,
    line: String,
}

struct ImageManagedWorkspace {
    id: String,
    epoch: i64,
    cwd: String,
    repo: String,
}

impl studio_runtime_services::monitor_runtime::WorkspaceAdmission for ImageManagedWorkspace {
    fn check(
        &self,
        _tx: &studio_runtime_store::Transaction<'_>,
        actor: &studio_runtime_store::AgentRecord,
    ) -> Result<(), studio_runtime_services::monitor_policy::MonitorError> {
        let extra = &actor.extra;
        if actor.id.0 != self.id
            || actor.epoch != self.epoch
            || extra.get("cwd").and_then(Value::as_str) != Some(self.cwd.as_str())
            || extra.get("imageWorkspace").and_then(Value::as_bool) != Some(true)
            || extra.get("imageWorkspaceReady").and_then(Value::as_bool) != Some(true)
            || extra.get("imageWorkspaceRepo").and_then(Value::as_str) != Some(self.repo.as_str())
        {
            return Err(
                studio_runtime_services::monitor_policy::MonitorError::Refused(
                    "Image workspace changed during preparation",
                ),
            );
        }
        Ok(())
    }
}

impl studio_runtime_services::monitor_runtime::WorkspaceAdmission for VmManagedWorkspace {
    fn check(
        &self,
        _tx: &studio_runtime_store::Transaction<'_>,
        actor: &studio_runtime_store::AgentRecord,
    ) -> Result<(), studio_runtime_services::monitor_policy::MonitorError> {
        let extra = &actor.extra;
        if actor.id.0 != self.id
            || actor.epoch != self.epoch
            || extra.get("cwd").and_then(Value::as_str) != Some(self.cwd.as_str())
            || extra.get("executionMode").and_then(Value::as_str) != Some("vm")
            || extra.get("layrReady").and_then(Value::as_bool) != Some(true)
            || extra.get("layrProjectId").and_then(Value::as_str) != Some(self.project_id.as_str())
            || extra.get("layrLine").and_then(Value::as_str) != Some(self.line.as_str())
        {
            return Err(
                studio_runtime_services::monitor_policy::MonitorError::Refused(
                    "VM workspace changed during preparation",
                ),
            );
        }
        Ok(())
    }
}

impl studio_runtime_services::monitor_runtime::WorkspaceAdmission for HostManagedWorkspace {
    fn check(
        &self,
        _tx: &studio_runtime_store::Transaction<'_>,
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
        Ok(())
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
        let row = self.model_row(account_key, account, model)?;
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
    fn model_row(
        &self,
        account_key: &str,
        account: &AccountIdentity,
        model: &str,
    ) -> Result<Value, CreateError> {
        let home = account
            .home
            .as_deref()
            .ok_or(CreateError::CapabilityUnavailable(
                "provider-session/model-list",
            ))?;
        let provider = account.provider.key();
        let (value, fresh_key, fresh_home, fresh_provider, expires_at) =
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
        if fresh_key != account_key
            || fresh_home != home
            || fresh_provider != provider
            || expires_at <= std::time::Instant::now()
        {
            return Err(CreateError::CapabilityUnavailable(
                "provider-session/model-list",
            ));
        }
        value
            .get("data")
            .and_then(Value::as_array)
            .and_then(|rows| {
                rows.iter().find(|row| {
                    !row.get("hidden").and_then(Value::as_bool).unwrap_or(false)
                        && (row.get("model").and_then(Value::as_str) == Some(model)
                            || row.get("resolvedModel").and_then(Value::as_str) == Some(model))
                })
            })
            .cloned()
            .ok_or(CreateError::ModelUnavailable)
    }

    fn contains_model(
        &self,
        account_key: &str,
        account: &AccountIdentity,
        model: &str,
    ) -> Result<bool, CreateError> {
        match self.model_row(account_key, account, model) {
            Ok(_) => Ok(true),
            Err(CreateError::ModelUnavailable) => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn pricing_name(
        &self,
        account_key: &str,
        account: &AccountIdentity,
        model: &str,
    ) -> Result<String, studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError>
    {
        use studio_runtime_services::provider_callbacks::ManagedCreationEnvironmentError as E;
        let (Some(home), provider) = (account.home.as_deref(), account.provider.key()) else {
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
    RouteGroup::new(vec![
        post("/api/agents", OperationStatus::Implemented, create_agent),
        post("/api/configure", OperationStatus::Implemented, configure),
        post(
            "/api/conversation",
            OperationStatus::Implemented,
            conversation,
        ),
        post("/api/leads", OperationStatus::Implemented, create_lead),
        post("/api/rename", OperationStatus::Implemented, rename),
    ])
}

async fn dispatch<T: serde::de::DeserializeOwned + serde::Serialize + Send + 'static>(
    state: HttpState,
    operation: impl for<'a> FnOnce(
        &'a Context<'a>,
    )
        -> futures_util::future::BoxFuture<'a, Result<Value, OperationError>>,
) -> Result<Json<T>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let catalog = RouteCreationCatalog {
        service: state.model_catalog.clone(),
    };
    let managed_environment = state
        .account_driver
        .clone()
        .map(|driver| managed_creation_environment(driver, state.model_catalog.clone()));
    let context = Context {
        store,
        accounts: state.account_policy.as_deref(),
        driver: state.account_driver.clone(),
        catalog: Some(std::sync::Arc::new(catalog)),
        managed_creation_environment: managed_environment.as_deref(),
        validate: validation_callback(&state),
    };
    let result = operation(&context).await.map_err(operation_error)?;
    state.event_wake.notify_one();
    super::generated_json::<T>(result).map(Json)
}

fn validation_callback(state: &HttpState) -> agents_create::PersistedRecordValidator {
    let validators = state.validators.clone();
    std::sync::Arc::new(move |schema, value| match schema {
        "PersistedAgentRecord" => validators.validate_table_record("runtime_agents", value),
        _ => validators.validate_stored(schema, value),
    })
}

async fn preflight_lead_replay<
    T: serde::de::DeserializeOwned + serde::Serialize + Send + 'static,
>(
    state: &HttpState,
    request: agents_create::Creation,
) -> Result<Option<Json<T>>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let context = Context {
        store,
        accounts: state.account_policy.as_deref(),
        driver: state.account_driver.clone(),
        catalog: None,
        managed_creation_environment: None,
        validate: validation_callback(state),
    };
    let replay = agents_create::replay_lead_request(&context, &request)
        .await
        .map_err(operation_error)?;
    match replay {
        Some(value) => Ok(Some(Json(super::generated_json::<T>(value)?))),
        None => Ok(None),
    }
}

async fn lead_catalog_account(
    state: &HttpState,
    request: agents_create::Creation,
) -> Result<String, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let context = Context {
        store,
        accounts: state.account_policy.as_deref(),
        driver: state.account_driver.clone(),
        catalog: None,
        managed_creation_environment: None,
        validate: validation_callback(state),
    };
    agents_create::creation_account_key(&context, &request)
        .await
        .map_err(operation_error)
}

async fn prepare_agent_admission(
    state: &HttpState,
    request: agents_create::Creation,
) -> Result<agents_create::AgentAdmission, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let context = Context {
        store,
        accounts: state.account_policy.as_deref(),
        driver: state.account_driver.clone(),
        catalog: None,
        managed_creation_environment: None,
        validate: validation_callback(state),
    };
    agents_create::prepare_agent_admission(&context, &request)
        .await
        .map_err(operation_error)
}

async fn conversation_catalog_account(
    state: &HttpState,
    id: String,
    expected_account: Option<String>,
    cwd: Option<String>,
) -> Result<String, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let context = Context {
        store,
        accounts: state.account_policy.as_deref(),
        driver: state.account_driver.clone(),
        catalog: None,
        managed_creation_environment: None,
        validate: validation_callback(state),
    };
    agents_create::conversation_account_key(
        &context,
        &id,
        expected_account.as_deref(),
        cwd.as_deref(),
    )
    .await
    .map_err(operation_error)
}

async fn conversation_worker_catalog_account(
    state: &HttpState,
    id: String,
    expected_account: Option<String>,
) -> Result<Option<String>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let context = Context {
        store,
        accounts: state.account_policy.as_deref(),
        driver: state.account_driver.clone(),
        catalog: None,
        managed_creation_environment: None,
        validate: validation_callback(state),
    };
    agents_create::conversation_worker_account_key(&context, &id, expected_account.as_deref())
        .await
        .map_err(operation_error)
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
        id: body.id.clone().flatten(),
        cwd: body.cwd.clone(),
        account: body.account_key.clone(),
        workspace: body.workspace_mode.map(|value| value.to_string()),
        yolo: body.yolo_mode,
        previous: body.previous.clone().flatten(),
        reuse_empty: body.reuse_empty,
        ..Creation::default()
    };
    // Python `new_lead` checks the durable request alias before resolving the
    // project/account or reading a catalog (`codex_runtime.py:5099-5120`).
    if needs_catalog
        && let Some(replay) =
            preflight_lead_replay::<generated::AgentResponse>(&state, request.clone()).await?
    {
        return Ok(replay);
    }
    // Python resolves project binding/account after replay and before catalog
    // validation (`codex_runtime.py:5120-5130`).
    let catalog_account = if needs_catalog {
        Some(lead_catalog_account(&state, request.clone()).await?)
    } else {
        None
    };
    // Python validates execution only after obtaining the required catalog
    // (`codex_runtime.py:5124-5131`).
    wait_for_agent_catalog(
        &state,
        AgentCatalogWaitRequest {
            parent_id: None,
            preferred_parent_account: None,
            requested_account: catalog_account.as_deref(),
            requested_model,
            needs_catalog,
            ignore_stored_worker_account: false,
            require_model_match: requested_model.is_some(),
        },
    )
    .await?;
    if let Some(Some(defaults)) = body.worker_defaults.as_ref() {
        let worker_model = defaults.model.as_deref().or(requested_model);
        let worker_account = defaults.account_key.as_ref().and_then(Option::as_deref);
        wait_for_agent_catalog(
            &state,
            AgentCatalogWaitRequest {
                parent_id: None,
                preferred_parent_account: catalog_account.as_deref(),
                requested_account: worker_account,
                requested_model: worker_model,
                needs_catalog: true,
                ignore_stored_worker_account: true,
                require_model_match: true,
            },
        )
        .await?;
    }
    if let Some(Some(review)) = body.review_defaults.as_ref() {
        let review_account = body
            .worker_defaults
            .as_ref()
            .and_then(Option::as_ref)
            .and_then(|defaults| defaults.account_key.as_ref())
            .and_then(Option::as_deref);
        wait_for_agent_catalog(
            &state,
            AgentCatalogWaitRequest {
                parent_id: None,
                preferred_parent_account: catalog_account.as_deref(),
                requested_account: review_account,
                requested_model: review.model.as_deref(),
                needs_catalog: true,
                ignore_stored_worker_account: true,
                require_model_match: review.model.is_some(),
            },
        )
        .await?;
    }
    dispatch::<generated::AgentResponse>(state, move |context| {
        Box::pin(agents_create::lead(context, request))
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
        if body.parent.as_ref().is_none_or(Option::is_none)
            && body.cwd.as_deref().is_none_or(str::is_empty)
        {
            return Err(operation_error(OperationError::Invalid(
                "Select an existing project directory",
            )));
        }
        return Err(ApiError::internal());
    }
    let mut request = Creation {
        intent: serde_json::to_value(&body).map_err(|_| ApiError::internal())?,
        catalog: body.model.as_ref().is_some_and(Option::is_some)
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
        imported_from,
        ..Creation::default()
    };
    // Python resolves a durable existing-agent replay before profile/account
    // admission, so an accepted request remains replayable if its catalog has
    // since gone cold (`codex_runtime.py:4902-4913`).
    let admission = prepare_agent_admission(&state, request.clone()).await?;
    if let Some(replay) = admission.replay {
        return Ok(Json(super::generated_json::<generated::AgentResponse>(
            replay,
        )?));
    }
    request.catalog = admission.needs_catalog;
    // Python merges profile model/effort before computing `needs_catalog`; a
    // role/instructions-only profile does not itself require provider access
    // (`codex_runtime.py:4859-4888`).
    wait_for_agent_catalog(
        &state,
        AgentCatalogWaitRequest {
            parent_id: request.parent.as_deref(),
            preferred_parent_account: None,
            requested_account: admission.account.as_deref().or(request.account.as_deref()),
            requested_model: admission.model.as_deref(),
            needs_catalog: admission.needs_catalog,
            ignore_stored_worker_account: false,
            require_model_match: admission.model.is_some(),
        },
    )
    .await?;
    dispatch::<generated::AgentResponse>(state, move |context| {
        Box::pin(agents_create::agent(context, request))
    })
    .await
}

fn catalog_admission_candidates(
    rows: &[Value],
    ordered_keys: &[String],
    parent_account: Option<&str>,
    account_is_pinned: bool,
    expected_provider: Option<&str>,
) -> Vec<(String, String, String)> {
    ordered_keys
        .iter()
        .filter_map(|key| {
            let row = rows
                .iter()
                .find(|row| row.get("id").and_then(Value::as_str) == Some(key.as_str()))?;
            let home = row.get("home")?.as_str()?.to_owned();
            if row.get("disconnected") == Some(&Value::Bool(true))
                || row.get("deleted") == Some(&Value::Bool(true))
                || row.get("duplicateOf").is_some_and(|value| !value.is_null())
                || (Some(key.as_str()) != parent_account
                    && row.get("status").and_then(Value::as_str) != Some("ready")
                    && row.get("canAttemptNativeProof") != Some(&Value::Bool(true)))
            {
                return None;
            }
            let provider = studio_runtime_providers::ProviderKind::of_record(row)
                .ok()?
                .key();
            if !account_is_pinned && expected_provider.is_some_and(|wanted| wanted != provider) {
                return None;
            }
            Some((key.clone(), home, provider.to_owned()))
        })
        .collect()
}

struct AgentCatalogWaitRequest<'a> {
    parent_id: Option<&'a str>,
    preferred_parent_account: Option<&'a str>,
    requested_account: Option<&'a str>,
    requested_model: Option<&'a str>,
    needs_catalog: bool,
    ignore_stored_worker_account: bool,
    require_model_match: bool,
}

async fn wait_for_agent_catalog(
    state: &HttpState,
    request: AgentCatalogWaitRequest<'_>,
) -> Result<(), ApiError> {
    if !request.needs_catalog {
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
    let parent_id = request.parent_id.map(str::to_owned);
    let requested_account = request.requested_account.map(str::to_owned);
    let requested_model = request.requested_model.map(str::to_owned);
    let preferred_parent_account = request.preferred_parent_account.map(str::to_owned);
    let ignore_stored_worker_account = request.ignore_stored_worker_account;
    let require_model_match = request.require_model_match;
    let candidate_data = tokio::time::timeout_at(
        deadline,
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/routes/agents_create",
            move || {
                let snapshot = driver.listing_snapshot().map_err(|_| ())?;
                let mut parent_account = preferred_parent_account;
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
                        if !ignore_stored_worker_account {
                            selected_account = root
                                .pointer("/workerDefaults/accountKey")
                                .and_then(Value::as_str)
                                .map(str::to_owned);
                        }
                        root_model = root.get("model").and_then(Value::as_str).map(str::to_owned);
                        if !ignore_stored_worker_account {
                            worker_model = root
                                .pointer("/workerDefaults/model")
                                .and_then(Value::as_str)
                                .map(str::to_owned);
                        }
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
                let explicit_account = requested_account.is_some();
                let explicit = requested_account;
                let model = requested_model
                    .or(worker_model)
                    .or(root_model)
                    .unwrap_or_else(|| "gpt-6-astra".to_owned());
                let provider =
                    model_provider(&model).map(studio_runtime_providers::ProviderKind::key);
                let pinned = explicit.or(selected_account);
                let mut keys = Vec::new();
                let mut add = |key: Option<&str>, expected_provider: Option<&str>| {
                    if let Some(key) = key.filter(|key| !excluded.contains(*key))
                        && !keys.iter().any(|known| known == key)
                        && expected_provider.is_none_or(|expected| {
                            snapshot
                                .accounts
                                .iter()
                                .find(|row| row.get("id").and_then(Value::as_str) == Some(key))
                                .and_then(studio_runtime_providers::ProviderKind::key_of)
                                == Some(expected)
                        })
                    {
                        keys.push(key.to_owned());
                    }
                };
                if let Some(key) = pinned.as_deref() {
                    add(Some(key), None);
                } else {
                    add(parent_account.as_deref(), provider);
                    add(snapshot.default_account_key.as_str(), provider);
                    for row in &snapshot.accounts {
                        let key = row.get("id").and_then(Value::as_str);
                        add(key, provider);
                    }
                }
                let accounts = catalog_admission_candidates(
                    &snapshot.accounts,
                    &keys,
                    parent_account.as_deref(),
                    pinned.is_some(),
                    provider,
                );
                Ok::<_, ()>((accounts, model, explicit_account))
            },
        ),
    )
    .await
    .map_err(|_| operation_error(OperationError::Unavailable("provider-session/model-list")))?
    .map_err(|_| ApiError::internal())?
    .map_err(|_| ApiError::internal())?;
    let (accounts, expected_model, explicit_account) = candidate_data;
    if accounts.is_empty() {
        // Keep invalid automatic provider choices and explicit unknown,
        // disabled, or excluded accounts on the synchronous selector, which
        // reports the established 400 response.
        return Ok(());
    }
    let has_requested_model = |value: &Value| {
        value
            .get("data")
            .and_then(Value::as_array)
            .is_some_and(|rows| {
                rows.iter().any(|row| {
                    !row.get("hidden").and_then(Value::as_bool).unwrap_or(false)
                        && (row.get("model").and_then(Value::as_str)
                            == Some(expected_model.as_str())
                            || row.get("resolvedModel").and_then(Value::as_str)
                                == Some(expected_model.as_str()))
                })
            })
    };
    let mut saw_fresh_catalog = false;
    for (account, home, provider) in &accounts {
        if let studio_runtime_services::model_catalog::AdmissionCatalogRead::Fresh {
            value, ..
        } = catalog.read_for_admission(account, home, provider)
        {
            saw_fresh_catalog = true;
            if !require_model_match || explicit_account || has_requested_model(&value) {
                return Ok(());
            }
        }
    }
    for (account, home, provider) in accounts {
        let session = std::sync::Arc::clone(session);
        let ensured = tokio::time::timeout_at(deadline, session.ensure_session(&account))
            .await
            .is_ok_and(|result| result.is_ok());
        if !ensured {
            continue;
        }
        let catalog_read = tokio::time::timeout_at(
            deadline,
            catalog.wait_for_admission_catalog(&account, &home, &provider),
        )
        .await
        .ok();
        let Some(studio_runtime_services::model_catalog::AdmissionCatalogRead::Fresh {
            value, ..
        }) = catalog_read
        else {
            continue;
        };
        saw_fresh_catalog = true;
        if !require_model_match || explicit_account || has_requested_model(&value) {
            return Ok(());
        }
    }
    // Let the synchronous admission validator return its established 400
    // model error when provider data is available but no eligible account has
    // the requested model. Only a genuinely cold/unavailable catalog is 501.
    if saw_fresh_catalog {
        return Ok(());
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
        Box::pin(agents_create::configure(context, request))
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
    let request = agents_create::Conversation {
        non_defaults: body.next_turn.is_some() || body.request_id.is_some(),
        model_present: body.model.is_some(),
        effort_present: body.effort.is_some(),
        fast_mode_present: body.fast_mode.is_some(),
        daybreak_present: body.daybreak_enabled.is_some(),
        cwd: body.cwd.clone(),
        yolo_mode: body.yolo_mode,
        model: body.model.clone().flatten(),
        effort: body.effort.clone(),
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
        id: body.id.clone(),
        expected_account: body.expected_account_key.clone(),
        excluded: body.excluded_account_keys.clone().map(|mut keys| {
            keys.sort();
            keys
        }),
    };
    let needs_conversation_catalog = conversation_needs_catalog(&value);
    let requested_model = body.model.as_ref().and_then(Option::as_deref);
    let needs_settings_conversation_account =
        conversation_settings_need_conversation_account(&value);
    let catalog_account = if needs_conversation_catalog || needs_settings_conversation_account {
        // Python first resolves the existing target and its expected account;
        // worker/review defaults use that conversation context at 5484-5491.
        let catalog_cwd =
            catalog_cwd_for_conversation(request.worker_settings, request.cwd.as_deref())
                .map(str::to_owned);
        Some(
            conversation_catalog_account(
                &state,
                request.id.clone(),
                request.expected_account.clone(),
                catalog_cwd,
            )
            .await?,
        )
    } else {
        None
    };
    // This updates an existing conversation. Main execution uses its account;
    // automatic worker defaults use the same parent-first account ordering as
    // the worker catalog and exclude accounts disabled for this chat.
    let parent_id = existing_conversation_catalog_parent();
    // Runtime.update only reads the target catalog for execution changes
    // outside defaults-only updates (`codex_runtime.py:5481-5483`).
    wait_for_agent_catalog(
        &state,
        AgentCatalogWaitRequest {
            parent_id,
            preferred_parent_account: None,
            requested_account: catalog_account.as_deref(),
            requested_model,
            needs_catalog: needs_conversation_catalog,
            ignore_stored_worker_account: false,
            require_model_match: requested_model.is_some(),
        },
    )
    .await?;
    if let Some(defaults) = value
        .get("worker_defaults")
        .filter(|entry| !entry.is_null())
    {
        // Python resolves worker_defaults against settings_catalog after the
        // target account is known (`codex_runtime.py:5484-5486`).
        let worker_model = defaults.get("model").and_then(Value::as_str);
        let worker_account = worker_settings_catalog_account(defaults);
        if worker_model.is_some() || worker_account.is_some() {
            wait_for_agent_catalog(
                &state,
                AgentCatalogWaitRequest {
                    parent_id: Some(&request.id),
                    preferred_parent_account: None,
                    requested_account: worker_account.as_deref(),
                    requested_model: worker_model,
                    needs_catalog: true,
                    ignore_stored_worker_account: true,
                    require_model_match: true,
                },
            )
            .await?;
        }
    }
    if value.get("review_defaults").is_some() {
        // Runtime.update calls settings_catalog for review defaults even when
        // model is null; its account comes from supplied worker_defaults or
        // the target's stored worker account (`codex_runtime.py:5487-5491`).
        let stored_worker_account = if value.get("worker_defaults").is_none() {
            conversation_worker_catalog_account(
                &state,
                request.id.clone(),
                request.expected_account.clone(),
            )
            .await?
        } else {
            None
        };
        let review_account = review_settings_catalog_account(
            value.get("worker_defaults"),
            stored_worker_account.as_deref(),
        );
        let review_model = value
            .pointer("/review_defaults/model")
            .and_then(Value::as_str);
        wait_for_agent_catalog(
            &state,
            AgentCatalogWaitRequest {
                parent_id: Some(&request.id),
                preferred_parent_account: None,
                requested_account: review_account.as_deref(),
                requested_model: review_model,
                needs_catalog: true,
                ignore_stored_worker_account: true,
                require_model_match: review_model.is_some(),
            },
        )
        .await?;
    }
    dispatch::<generated::AgentResponse>(state, move |context| {
        Box::pin(agents_create::conversation(context, request))
    })
    .await
}
async fn finish_title_request(
    store: std::sync::Arc<studio_runtime_store::RuntimeStore>,
    provider: Option<
        std::sync::Arc<dyn studio_runtime_services::title_generation::TitleRequestPort>,
    >,
    driver: Option<std::sync::Arc<studio_runtime_services::account_driver::AccountDriver>>,
    catalog: studio_runtime_services::model_catalog::ModelCatalogService,
    event_wake: std::sync::Arc<tokio::sync::Notify>,
    agent_id: String,
    request_id: String,
) {
    async fn settle_failure(
        store: std::sync::Arc<studio_runtime_store::RuntimeStore>,
        agent_id: String,
        request_id: String,
    ) {
        let result = studio_runtime_services::title_generation::settle_catalog_failure(
            &store,
            &agent_id,
            &request_id,
        )
        .await;
        match result {
            Ok(()) => {}
            Err(error) => eprintln!("Could not settle unavailable title catalog: {error}"),
        }
    }
    let Some(provider) = provider else {
        settle_failure(store, agent_id, request_id).await;
        return;
    };
    if let Some(driver) = driver {
        let account_snapshot = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/routes/agents_create/rename-accounts",
            move || driver.snapshot().map(|snapshot| snapshot.accounts),
        )
        .await;
        match account_snapshot {
            Ok(Ok(accounts)) => catalog.retain_accounts(&accounts),
            _ => {
                settle_failure(store, agent_id, request_id).await;
                return;
            }
        }
    }
    let read_store = store.clone();
    let read_agent = agent_id.clone();
    let account_snapshot = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/agents_create/rename-agent",
        move || {
            let connection = read_store.read().map_err(|_| ApiError::internal())?;
            let agent =
                studio_runtime_store::WorkspaceViewsRepository::agent(&connection, &read_agent)
                    .map_err(|_| ApiError::internal())?
                    .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Unknown managed agent"))?;
            let record = agent.value();
            Ok::<_, ApiError>((
                record
                    .get("accountKey")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
                    .to_owned(),
                studio_runtime_providers::ProviderKind::of_record(&record)
                    .map_err(|_| ApiError::internal())?
                    .key()
                    .to_owned(),
            ))
        },
    )
    .await;
    let (account, provider_name) = match account_snapshot {
        Ok(Ok(snapshot)) => snapshot,
        _ => {
            settle_failure(store, agent_id, request_id).await;
            return;
        }
    };
    let Some((home, catalog_provider)) = catalog.account_profile(&account) else {
        settle_failure(store, agent_id, request_id).await;
        return;
    };
    if catalog_provider != provider_name {
        settle_failure(store, agent_id, request_id).await;
        return;
    }
    let models = match catalog
        .wait_for_admission_catalog(&account, &home, &provider_name)
        .await
    {
        studio_runtime_services::model_catalog::AdmissionCatalogRead::Fresh { value, .. } => value,
        studio_runtime_services::model_catalog::AdmissionCatalogRead::Pending
        | studio_runtime_services::model_catalog::AdmissionCatalogRead::Unavailable => {
            settle_failure(store, agent_id, request_id).await;
            return;
        }
    };
    match studio_runtime_services::title_generation::generate_admitted(
        &store,
        provider.as_ref(),
        &agent_id,
        &models,
        &request_id,
        std::time::SystemTime::now() + studio_runtime_services::title_generation::REQUEST_TIMEOUT,
    )
    .await
    {
        Ok(studio_runtime_services::title_generation::TitleResult::Applied { .. }) => {
            event_wake.notify_one();
        }
        Ok(_) => {}
        Err(error) => {
            eprintln!("Title generation did not complete: {error}");
            settle_failure(store, agent_id, request_id).await;
        }
    }
}

fn conversation_needs_catalog(value: &Value) -> bool {
    ["model", "effort", "fast_mode", "daybreak_enabled"]
        .iter()
        .any(|field| value.get(*field).is_some())
}

fn conversation_settings_need_conversation_account(value: &Value) -> bool {
    // Runtime.update calls settings_catalog for every review_defaults payload,
    // including model:null (`codex_runtime.py:5487-5491`).
    let has_review_defaults = value.get("review_defaults").is_some();
    let has_unpinned_worker_defaults = value
        .get("worker_defaults")
        .filter(|defaults| !defaults.is_null())
        .is_some_and(|defaults| {
            defaults
                .get("account_key")
                .is_none_or(|account| account.is_null())
        });
    has_review_defaults || has_unpinned_worker_defaults
}

fn catalog_cwd_for_conversation(
    worker_settings: bool,
    requested_cwd: Option<&str>,
) -> Option<&str> {
    if worker_settings { None } else { requested_cwd }
}

fn existing_conversation_catalog_parent() -> Option<&'static str> {
    None
}

#[cfg(test)]
mod title_http_tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use serde_json::json;
    use std::{
        future::Future,
        pin::Pin,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use studio_runtime_providers::{Method, RequestId, RequestTicket};
    use studio_runtime_services::{
        account_session_runtime::{
            AccountSessionError, AccountSessionPort, ConnectionScope, ProviderRequest,
            ProviderResponse, RequestCertainty, SessionReceipt,
        },
        title_generation::{TitleReceipt, TitleRequest, TitleRequestPort, WriteOutcome},
    };
    use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore};
    use tokio::sync::{Notify, oneshot};
    use tower::ServiceExt;

    struct CatalogSession;

    impl AccountSessionPort for CatalogSession {
        fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
            Some(ConnectionScope {
                account: account.to_owned(),
                connection: "fake-title-catalog".into(),
                generation: 4,
            })
        }

        fn provider_request<'a>(
            &'a self,
            request: ProviderRequest,
        ) -> Pin<Box<dyn Future<Output = Result<SessionReceipt, AccountSessionError>> + Send + 'a>>
        {
            Box::pin(async move {
                let scope = self
                    .current_scope(&request.account)
                    .expect("fake catalog scope");
                Ok(SessionReceipt {
                    scope,
                    operation: request.operation,
                    ticket: Some(RequestTicket {
                        id: RequestId::String("fake-model-list".into()),
                        generation: 4,
                        method: Method::ModelList,
                    }),
                    certainty: RequestCertainty::Written,
                    result: Some(ProviderResponse::Reply(json!({
                        "data":[{"model":"gpt-6-mini"}],
                        "nextCursor":null
                    }))),
                    not_sent_reason: None,
                    replayed: false,
                })
            })
        }
    }

    struct GeneratedTitle {
        requested: Mutex<Option<oneshot::Sender<TitleRequest>>>,
    }

    impl TitleRequestPort for GeneratedTitle {
        fn request<'a>(
            &'a self,
            request: TitleRequest,
        ) -> Pin<Box<dyn Future<Output = TitleReceipt> + Send + 'a>> {
            let requested = self.requested.lock().expect("title request sender").take();
            Box::pin(async move {
                requested
                    .expect("HTTP title request is observed")
                    .send(request.clone())
                    .expect("test receives title request");
                TitleReceipt {
                    request_id: request.request_id,
                    agent_id: request.agent_id,
                    epoch: request.epoch,
                    account_key: request.account_key,
                    outcome: WriteOutcome::Written,
                    title: Some("Generated through HTTP".into()),
                }
            })
        }
    }

    #[tokio::test]
    async fn rename_http_route_uses_injected_title_provider_and_settles() {
        let directory = tempfile::tempdir().expect("isolated HTTP state");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("runtime store opens"),
        );
        let agent = AgentRecord::from_value(json!({
            "id":"title-http-agent",
            "rootId":"title-http-agent",
            "epoch":1,
            "status":"idle",
            "name":"Original conversation name",
            "provider":"codex",
            "model":"gpt-6",
            "prompt":"Start the title fixture",
            "cwd":"/fixture",
            "accountKey":"default",
            "isLead":false,
            "needsTitle":true,
            "manualName":false,
            "autoWake":false
        }))
        .expect("valid persisted agent");
        store
            .transaction_with(move |tx| {
                AgentRepository::put(tx, &agent)?;
                tx.execute(
                    "INSERT INTO runtime_items(id,agent,record,created) VALUES(?1,?2,?3,?4)",
                    rusqlite::params![
                        "title-http-item",
                        "title-http-agent",
                        json!({"id":"title-http-item","role":"user","title":"You","text":"Plan a dashboard","created":1.0}).to_string(),
                        1.0,
                    ],
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .await
            .expect("seed title agent and transcript");
        let (requested_tx, requested_rx) = oneshot::channel();
        let mut state = crate::router::HttpState::new(
            "token".into(),
            "workspace".into(),
            directory.path().to_string_lossy().into_owned(),
            4620,
            None,
            Some(store.clone()),
            None,
        );
        let catalog = state.model_catalog_handle();
        catalog.set_account_session(Some(Arc::new(CatalogSession)));
        catalog.retain_accounts(&[json!({
            "id":"default", "provider":"codex", "home":"/fixture"
        })]);
        state = state.with_title_request_port(Arc::new(GeneratedTitle {
            requested: Mutex::new(Some(requested_tx)),
        }));
        let app = crate::router::router(state).expect("HTTP routes register");
        let request = Request::builder()
            .method("POST")
            .uri("/api/rename")
            .header("x-canvas-token", "token")
            .header("host", "127.0.0.1:4620")
            .header("content-type", "application/json")
            .header(
                crate::schema::API_SCHEMA_HASH_HEADER,
                crate::schema::api_schema_hash(),
            )
            .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                12345,
            ))))
            .body(Body::from(
                json!({
                    "id":"title-http-agent",
                    "name":null,
                    "request_id":"title-http-request"
                })
                .to_string(),
            ))
            .expect("valid HTTP request");
        let response = app.oneshot(request).await.expect("HTTP route response");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let receipt: Value = serde_json::from_slice(
            &to_bytes(response.into_body(), 65536)
                .await
                .expect("rename response body"),
        )
        .expect("rename receipt JSON");
        assert_eq!(receipt["status"], "pending");

        let provider_request = tokio::time::timeout(Duration::from_secs(5), requested_rx)
            .await
            .expect("HTTP finisher reaches title provider")
            .expect("title provider request delivered");
        assert_eq!(provider_request.agent_id, "title-http-agent");
        assert_eq!(provider_request.request_id, "title-http-request");
        assert_eq!(provider_request.model, "gpt-6-mini");

        let changed = Arc::new(Notify::new());
        let notify = changed.clone();
        store.set_commit_notifier(move || notify.notify_one());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let next = changed.notified();
                tokio::pin!(next);
                next.as_mut().enable();
                let record = store
                    .read()
                    .expect("read generated title")
                    .query_row(
                        "SELECT record FROM runtime_agents WHERE id='title-http-agent'",
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .expect("title agent exists");
                let record: Value = serde_json::from_str(&record).expect("agent JSON");
                if record["name"] == "Generated through HTTP"
                    && record["needsTitle"] == false
                    && record["manualName"] == true
                {
                    break;
                }
                next.await;
            }
        })
        .await
        .expect("HTTP title finisher commits the fake provider result");
    }
}

#[cfg(test)]
mod creation_catalog_flow_tests {
    use super::{
        catalog_admission_candidates, catalog_cwd_for_conversation,
        existing_conversation_catalog_parent,
    };
    use serde_json::json;

    #[test]
    fn conversation_catalog_wait_does_not_apply_new_worker_account_exclusions() {
        // Runtime.update operates on the existing target account. Passing its
        // id as a worker-selection parent would apply exclusions that Python
        // only applies to new children (`codex_runtime.py:5484-5491`).
        assert_eq!(catalog_cwd_for_conversation(true, Some("/ignored")), None);
        assert_eq!(existing_conversation_catalog_parent(), None);
    }

    #[test]
    fn automatic_catalog_candidates_keep_selector_order_and_model_provider() {
        let rows = vec![
            json!({"id":"parent-claude","home":"/claude","provider":"claude","status":"ready"}),
            json!({"id":"default-codex","home":"/codex","provider":"codex","status":"ready"}),
            json!({"id":"later-codex","home":"/codex-2","provider":"codex","status":"ready"}),
        ];
        let candidates = catalog_admission_candidates(
            &rows,
            &[
                "parent-claude".to_owned(),
                "default-codex".to_owned(),
                "later-codex".to_owned(),
            ],
            Some("parent-claude"),
            false,
            Some("codex"),
        );
        assert_eq!(
            candidates,
            vec![
                (
                    "default-codex".to_owned(),
                    "/codex".to_owned(),
                    "codex".to_owned()
                ),
                (
                    "later-codex".to_owned(),
                    "/codex-2".to_owned(),
                    "codex".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn no_automatic_provider_candidate_is_validated_by_the_selector() {
        let rows =
            vec![json!({"id":"only-claude","home":"/claude","provider":"claude","status":"ready"})];
        assert!(
            catalog_admission_candidates(
                &rows,
                &["only-claude".to_owned()],
                None,
                false,
                Some("codex"),
            )
            .is_empty()
        );
    }
}

fn worker_settings_catalog_account(defaults: &Value) -> Option<String> {
    defaults
        .get("account_key")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn review_settings_catalog_account(
    request_worker_defaults: Option<&Value>,
    stored_worker_account: Option<&str>,
) -> Option<String> {
    request_worker_defaults.map_or_else(
        || stored_worker_account.map(str::to_owned),
        worker_settings_catalog_account,
    )
}

#[cfg(test)]
mod stale_revision_tests {
    use super::operation_error;
    use axum::{body::to_bytes, http::StatusCode, response::IntoResponse};
    use studio_runtime_services::agents_create::{MutationError, OperationError};

    #[tokio::test]
    async fn a_stale_revision_is_a_confirmed_rejection() {
        let response =
            operation_error(OperationError::Create(MutationError::Stale)).into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 4096)
            .await
            .expect("response body");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("JSON body");
        assert_eq!(value["outcome"], "not_applied");
    }
}

#[cfg(test)]
mod conversation_catalog_admission_tests {
    use super::{
        catalog_cwd_for_conversation, conversation_needs_catalog,
        conversation_settings_need_conversation_account, review_settings_catalog_account,
        worker_settings_catalog_account,
    };
    use serde_json::json;

    #[test]
    fn worker_defaults_only_update_does_not_require_conversation_catalog() {
        assert!(!conversation_needs_catalog(&json!({
            "worker_defaults": {
                "account_key": "worker-account",
                "model": "gpt-6-luna"
            }
        })));
        assert!(!conversation_needs_catalog(&json!({
            "review_defaults": {"model": "gpt-6-astra"}
        })));
        assert!(conversation_needs_catalog(&json!({"model": "gpt-6-astra"})));
    }

    #[test]
    fn review_defaults_admission_uses_the_effective_worker_account() {
        let request = json!({
            "review_defaults": {"model": "gpt-6-astra", "effort": "medium"}
        });
        assert!(conversation_settings_need_conversation_account(&request));
        assert!(!conversation_needs_catalog(&request));
        assert_eq!(
            review_settings_catalog_account(None, Some("worker-account")).as_deref(),
            Some("worker-account")
        );
        assert_eq!(
            review_settings_catalog_account(
                Some(&json!({"account_key":"selected-worker"})),
                Some("stored-worker")
            )
            .as_deref(),
            Some("selected-worker")
        );
        assert_eq!(
            review_settings_catalog_account(Some(&json!({"account_key": null})), None),
            None
        );
        assert_eq!(review_settings_catalog_account(None, None), None);
    }

    #[test]
    fn lead_worker_and_review_catalogs_follow_the_selected_worker_account() {
        let request = json!({
            "account_key": "claude-local",
            "model": "default",
            "worker_defaults": {
                "account_key": "default",
                "model": "gpt-6-luna",
                "effort": "high",
                "fast_mode": false
            },
            "review_defaults": {"model": "gpt-6.1-sol", "effort": "high"}
        });
        let worker = &request["worker_defaults"];
        let worker_account = worker_settings_catalog_account(worker);
        assert_eq!(worker_account.as_deref(), Some("default"));
        assert_eq!(
            review_settings_catalog_account(Some(worker), None).as_deref(),
            Some("default")
        );
    }

    #[test]
    fn unpinned_worker_defaults_admission_uses_automatic_account_selection() {
        for account_key in [None, Some(json!(null))] {
            let mut defaults = json!({"model": null, "effort": null, "fast_mode": false});
            if let Some(account_key) = account_key {
                defaults["account_key"] = account_key;
            }
            assert!(conversation_settings_need_conversation_account(&json!({
                "worker_defaults": defaults
            })));
            assert_eq!(worker_settings_catalog_account(&defaults), None);
        }
        let request = json!({
            "worker_defaults": {
                "account_key": "worker-account",
                "model": "gpt-6-luna"
            }
        });
        assert!(!conversation_settings_need_conversation_account(&request));
        assert_eq!(
            worker_settings_catalog_account(&request["worker_defaults"]).as_deref(),
            Some("worker-account")
        );
    }

    #[test]
    fn worker_settings_catalog_admission_uses_stored_account_when_cwd_is_present() {
        let requested_cwd = "/different-project";
        assert_eq!(
            catalog_cwd_for_conversation(true, Some(requested_cwd)),
            None
        );
        assert_eq!(
            catalog_cwd_for_conversation(false, Some(requested_cwd)),
            Some(requested_cwd)
        );
    }
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
        let agent_id = body.id.clone();
        let (receipt, should_finish) =
            studio_runtime_services::title_generation::admit_python_request(
                &store,
                &agent_id,
                &request_id,
            )
            .await
            .map_err(|error| match error {
                studio_runtime_services::title_generation::TitleError::AgentNotFound => {
                    ApiError::new(StatusCode::NOT_FOUND, "Unknown managed agent")
                }
                studio_runtime_services::title_generation::TitleError::DeletedAgent => {
                    ApiError::new(StatusCode::BAD_REQUEST, "This conversation was deleted")
                }
                error @ studio_runtime_services::title_generation::TitleError::Conflict => {
                    ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
                }
                _ => ApiError::internal(),
            })?;
        if should_finish {
            let store = store.clone();
            let provider = state.title_request.clone();
            let driver = state.account_driver.clone();
            let catalog = state.model_catalog.clone();
            let event_wake = state.event_wake.clone();
            let agent_id = agent_id.clone();
            let request_id = request_id.clone();
            drop(tokio::spawn(async move {
                finish_title_request(
                    store, provider, driver, catalog, event_wake, agent_id, request_id,
                )
                .await;
            }));
        }
        let mut response =
            Json(super::generated_json::<generated::RenameResponse>(receipt)?).into_response();
        response
            .extensions_mut()
            .insert(crate::mutation_sync::SkipMutationSyncEnvelope);
        return Ok(response);
    }
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let catalog = RouteCreationCatalog {
        service: state.model_catalog.clone(),
    };
    let context = Context {
        store,
        accounts: state.account_policy.as_deref(),
        driver: state.account_driver.clone(),
        catalog: Some(std::sync::Arc::new(catalog)),
        managed_creation_environment: None,
        validate: validation_callback(&state),
    };
    let result = agents_create::rename_operation(
        &context,
        &body.id,
        name.as_deref(),
        body.request_id.flatten().as_deref(),
    )
    .await
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
}
fn operation_error(error: OperationError) -> ApiError {
    use agents_create::MutationError;
    // A stale revision is rejected before any write, so the client may drop the
    // request and refresh from the canonical value.
    if matches!(error, OperationError::Create(MutationError::Stale)) {
        return ApiError::not_applied(error.to_string());
    }
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
        OperationError::Forbidden(_) => StatusCode::FORBIDDEN,
        OperationError::Unavailable(_)
        | OperationError::Create(MutationError::CapabilityUnavailable(_)) => {
            StatusCode::NOT_IMPLEMENTED
        }
        OperationError::Internal
        | OperationError::Store(_)
        | OperationError::Create(MutationError::Store(_)) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    if matches!(
        error,
        OperationError::Store(_) | OperationError::Create(MutationError::Store(_))
    ) {
        return ApiError::internal();
    }
    if let OperationError::Create(MutationError::InvalidStoredObject { table, id }) = &error {
        return ApiError::invalid_stored(table, id);
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
            "account-driver/unavailable",
            "provider-session/model-list",
            "project-driver/folder-selection",
            "worker-driver/settings-catalog",
        ],
    }
}
