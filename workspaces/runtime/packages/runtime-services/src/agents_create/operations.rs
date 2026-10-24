//! Async orchestration of persisted agent operations.
use super::*;
use crate::account_driver::{AccountDriver, AccountDriverError, AccountStatus};
use crate::account_policy::AccountPolicyState;
use crate::event_write::{python_json, python_strip, truthy};
use crate::host_worker_create::{AccountIdentity, ExecutionSettings};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use studio_runtime_providers::ProviderKind;
use studio_runtime_schema::agent_limits::{
    DEFAULT_MAX_TEAM_AGENTS, DEFAULT_SUBAGENT_CONCURRENCY, MAX_SUBAGENT_CONCURRENCY,
    MAX_TEAM_AGENTS, MIN_SUBAGENT_CONCURRENCY, MIN_TEAM_AGENTS, TEAM_AGENT_HEADROOM,
};
use studio_runtime_store::RuntimeStore;

/// Accepted subagent concurrency, in running workers; owner `codex_agent_modes.py`.
const SUBAGENT_CONCURRENCY: std::ops::RangeInclusive<i64> =
    MIN_SUBAGENT_CONCURRENCY..=MAX_SUBAGENT_CONCURRENCY;
/// Accepted stored team size, in agent records; owner `codex_agent_modes.py`.
const TEAM_AGENTS: std::ops::RangeInclusive<i64> = MIN_TEAM_AGENTS..=MAX_TEAM_AGENTS as i64;
/// Refusals that name the bounds above; `limit_refusals_name_the_owned_bounds` checks them.
const CREATE_LIMITS_REFUSAL: &str = "Concurrency must be 0 to 2048 and team size must be 1 to 2050";
const TEAM_SIZE_REFUSAL: &str =
    "Team size must be 1 to 2050 and token budget must be positive or null";
const CONCURRENCY_REFUSAL: &str = "Subagent concurrency must be an integer from 0 to 2048";

pub type PersistedRecordValidator = std::sync::Arc<dyn Fn(&str, &Value) -> bool + Send + Sync>;

pub struct Context<'a> {
    pub store: std::sync::Arc<RuntimeStore>,
    pub accounts: Option<&'a AccountPolicyState>,
    pub driver: Option<std::sync::Arc<AccountDriver>>,
    pub catalog: Option<std::sync::Arc<dyn crate::host_worker_create::CreationCatalog>>,
    pub managed_creation_environment:
        Option<&'a dyn crate::managed_child_create::ManagedCreationEnvironment>,
    pub validate: PersistedRecordValidator,
}

async fn project_directory_async(value: Option<String>) -> Result<Option<String>, OperationError> {
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/agents_create/project_directory",
        move || value.as_deref().map(project_directory).transpose(),
    )
    .await
    .map_err(|_| OperationError::Internal)?
}

async fn project_directory_default_async() -> Result<String, OperationError> {
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/agents_create/project_directory_default",
        project_directory_default,
    )
    .await
    .map_err(|_| OperationError::Internal)?
}

#[derive(Debug, Error)]
pub enum OperationError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("{0}")]
    Forbidden(&'static str),
    #[error("Not implemented: requires {0}")]
    Unavailable(&'static str),
    #[error(transparent)]
    Create(#[from] CreateError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("Internal server error")]
    Internal,
}

#[derive(Clone, Default)]
pub struct Creation {
    pub id: Option<String>,
    pub cwd: Option<String>,
    pub account: Option<String>,
    pub name: Option<String>,
    pub prompt: String,
    pub role: Option<String>,
    pub workspace: Option<String>,
    pub yolo: Option<bool>,
    pub concurrency: Option<i64>,
    pub max_agents: Option<i64>,
    pub token_budget: Option<i64>,
    pub profile: Option<String>,
    pub parent: Option<String>,
    pub previous: Option<String>,
    pub reuse_empty: Option<bool>,
    pub catalog: bool,
    pub worker_catalog: bool,
    pub project_binding: bool,
    pub intent: Value,
    /// Source native thread attached atomically for provider-history imports.
    pub imported_from: Option<String>,
}

pub struct Configuration {
    pub id: String,
    pub concurrency: Option<i64>,
    pub max_agents: Option<i64>,
    pub token_budget: Option<Option<i64>>,
    pub expected_mode_revision: Option<i64>,
    pub request_id: Option<String>,
}

pub struct Conversation {
    pub id: String,
    pub expected_account: Option<String>,
    pub excluded: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub yolo_mode: Option<bool>,
    pub model: Option<String>,
    pub model_present: bool,
    pub effort: Option<Option<String>>,
    pub effort_present: bool,
    pub fast_mode: Option<bool>,
    pub fast_mode_present: bool,
    pub daybreak_enabled: Option<bool>,
    pub daybreak_present: bool,
    pub next_turn: Option<Option<bool>>,
    pub request_id: Option<Option<String>>,
    pub mode_settings: bool,
    pub mode_limit: Option<i64>,
    pub mode_revision: Option<i64>,
    pub mode_request_id: Option<String>,
    pub worker_settings: bool,
    pub worker_defaults: Option<Value>,
    pub review_defaults: Option<Value>,
    pub non_defaults: bool,
}

/// Read-only result of the ordered create admission phase. The HTTP layer uses
/// it before waiting on a provider catalog, then `agent` repeats the durable
/// replay check inside its write path to cover concurrent creation.
pub struct AgentAdmission {
    pub replay: Option<Value>,
    pub account: Option<String>,
    pub model: Option<String>,
    pub needs_catalog: bool,
}

pub(crate) const DEFAULT_LEAD_MODEL: &str = "gpt-6-astra";
/// The workspace mode of a new macOS lead chat whose request has no `workspaceMode`.
const MACOS_LEAD_WORKSPACE_MODE: &str = "layr";
/// The workspace mode of every other new top-level chat without `workspaceMode`: a task
/// agent (`POST /api/agents`) and every chat on another system.
const DEFAULT_WORKSPACE_MODE: &str = "worktree";

/// The workspace mode of a new top-level chat whose request omits it (`lead` is a
/// `POST /api/leads` chat).
fn default_workspace_mode(lead: bool) -> &'static str {
    if lead && cfg!(target_os = "macos") {
        MACOS_LEAD_WORKSPACE_MODE
    } else {
        DEFAULT_WORKSPACE_MODE
    }
}

pub async fn lead(context: &Context<'_>, request: Creation) -> Result<Value, OperationError> {
    create(context, request, true).await
}

/// Return a durable lead replay without consulting catalog or provider state.
/// The HTTP admission edge calls this before any catalog wait, matching the
/// Python `new_lead` replay check at `codex_runtime.py:5067-5131`, which runs
/// before `self.catalog(...)`.
pub async fn replay_lead_request(
    context: &Context<'_>,
    request: &Creation,
) -> Result<Option<Value>, OperationError> {
    let Some(id) = request.id.as_deref() else {
        return Ok(None);
    };
    let workspace = request
        .workspace
        .as_deref()
        .unwrap_or(default_workspace_mode(true));
    let cwd = request.cwd.as_deref().map(project_directory).transpose()?;
    let signature = lead_signature(&request.intent, workspace, cwd.as_deref());
    let validate = std::sync::Arc::clone(&context.validate);
    let id = id.to_owned();
    context
        .store
        .transaction_with_error(move |tx| {
            let validate = |schema: &str, value: &Value| validate(schema, value);
            let Some(record) = super::replay_lead(tx, &id, &signature, validate)? else {
                return Ok(None);
            };
            Ok(Some(sync_projection(tx, &record)?))
        })
        .await
}

/// Resolve the same project-bound account used by lead creation before the
/// HTTP edge admits a model catalog request, matching Python's project account
/// selection at `codex_runtime.py:5067-5131`.
pub async fn creation_account_key(
    context: &Context<'_>,
    request: &Creation,
    lead: bool,
) -> Result<String, OperationError> {
    let requested_cwd = project_directory_async(request.cwd.clone()).await?;
    let previous_id = request.previous.clone();
    let validate = std::sync::Arc::clone(&context.validate);
    let previous = context
        .store
        .read_async::<_, OperationError>("runtime-services/agents_create/read", move |read| {
            previous_lead(read, previous_id.as_deref(), &|schema, value| {
                validate(schema, value)
            })
        })
        .await?;
    let cwd = if let Some(cwd) = requested_cwd {
        cwd
    } else if let Some(prior) = previous.as_ref() {
        let raw = prior
            .get("hostProjectPath")
            .filter(|value| !value.is_null())
            .or_else(|| prior.get("cwd"))
            .and_then(Value::as_str)
            .ok_or(OperationError::Internal)?
            .to_owned();
        project_directory_async(Some(raw))
            .await?
            .ok_or(OperationError::Internal)?
    } else {
        project_directory_default_async().await?
    };
    if lead {
        new_chat_account(
            context,
            request.account.clone(),
            cwd,
            requested_model(request),
        )
        .await
    } else {
        account_key(context, request.account.clone(), cwd).await
    }
}

/// The non-empty model named by a creation request.
fn requested_model(request: &Creation) -> Option<String> {
    request
        .intent
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.trim().is_empty())
        .map(str::to_owned)
}

pub async fn conversation_account_key(
    context: &Context<'_>,
    id: &str,
    expected_account: Option<&str>,
    cwd: Option<&str>,
) -> Result<String, OperationError> {
    let store = std::sync::Arc::clone(&context.store);
    let validate = std::sync::Arc::clone(&context.validate);
    let id = id.to_owned();
    let expected = expected_account.map(str::to_owned);
    let old_account = store
        .read_async::<_, OperationError>("runtime-services/agents_create/read", move |read| {
            let initial = load_agent_value(read, &id, &|schema, value| validate(schema, value))?
                .ok_or(MutationError::UnknownAgent)?;
            let account = initial
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default");
            if expected
                .as_deref()
                .is_some_and(|expected| expected != account)
            {
                return Err(MutationError::Stale.into());
            }
            Ok(account.to_owned())
        })
        .await?;
    // Python's conversation update uses the requested project account when cwd
    // changes and otherwise keeps this conversation's account; settings then
    // resolve their catalogs separately (`codex_runtime.py:5484-5491`).
    if let Some(cwd) = cwd {
        let cwd = project_directory_async(Some(cwd.to_owned()))
            .await?
            .ok_or(OperationError::Internal)?;
        account_key(context, None, cwd).await
    } else {
        Ok(old_account)
    }
}

/// Return the currently selected worker account for Python's review-defaults
/// settings-catalog path (`codex_runtime.py:5487-5491`).
pub async fn conversation_worker_account_key(
    context: &Context<'_>,
    id: &str,
    expected_account: Option<&str>,
) -> Result<Option<String>, OperationError> {
    let store = std::sync::Arc::clone(&context.store);
    let validate = std::sync::Arc::clone(&context.validate);
    let id = id.to_owned();
    let expected = expected_account.map(str::to_owned);
    store
        .read_async::<_, OperationError>("runtime-services/agents_create/read", move |read| {
            let record = load_agent_value(read, &id, &|schema, value| validate(schema, value))?
                .ok_or(MutationError::UnknownAgent)?;
            let account = record
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default");
            if expected
                .as_deref()
                .is_some_and(|expected| expected != account)
            {
                return Err(MutationError::Stale.into());
            }
            Ok(record
                .pointer("/workerDefaults/accountKey")
                .and_then(Value::as_str)
                .map(str::to_owned))
        })
        .await
}

pub async fn agent(context: &Context<'_>, request: Creation) -> Result<Value, OperationError> {
    create(context, request, false).await
}

/// Transcribe `Runtime.create`'s pre-write ordering for the HTTP admission
/// edge: replay first, then profile and account resolution, then decide whether
/// Python's catalog-dependent fields require a catalog. The write path repeats
/// replay and profile validation to handle races after this read-only phase.
pub async fn prepare_agent_admission(
    context: &Context<'_>,
    request: &Creation,
) -> Result<AgentAdmission, OperationError> {
    // Python's durable existing-agent replay is the no-side-effect fast path;
    // `codex_runtime.py:4902-4913`. Keep it ahead of profile/account/catalog work.
    let Some(id) = request.id.clone() else {
        return prepare_new_agent_admission(context, request).await;
    };
    let intent = agent_creation_intent(request).await?;
    let validate = std::sync::Arc::clone(&context.validate);
    let read_id = id.clone();
    let read_intent = intent.clone();
    let replayed = {
        context
            .store
            .read_async::<_, OperationError>("runtime-services/agents_create/read", move |read| {
                Ok(super::replay_agent(
                    read,
                    &read_id,
                    &read_intent,
                    |schema, value| validate(schema, value),
                )?)
            })
            .await?
    };
    if replayed.is_some() {
        let validate = std::sync::Arc::clone(&context.validate);
        let replay = context
            .store
            .transaction_with_error(move |tx| {
                let validate = |schema: &str, value: &Value| validate(schema, value);
                let record = super::replay_agent(tx, &id, &intent, validate)?
                    .ok_or(CreateError::Conflict)?;
                Ok::<Value, OperationError>(sync_projection(tx, &record)?)
            })
            .await?;
        return Ok(AgentAdmission {
            replay: Some(replay),
            account: None,
            model: None,
            needs_catalog: false,
        });
    }
    prepare_new_agent_admission(context, request).await
}

async fn prepare_new_agent_admission(
    context: &Context<'_>,
    request: &Creation,
) -> Result<AgentAdmission, OperationError> {
    // Python resolves the parent delegation/profile before deciding whether a
    // catalog is needed (`codex_runtime.py:4853-4888`). A profile contributes
    // only non-null model/effort values that the request did not override.
    let validate = std::sync::Arc::clone(&context.validate);
    let profile_id = request.profile.clone().filter(|id| !id.is_empty());
    let profile = context
        .store
        .read_async::<_, OperationError>("runtime-services/agents_create/read", move |read| {
            profile_id
                .map(|id| {
                    load_record(
                        read,
                        "runtime_profiles",
                        "ProfileRecord",
                        &id,
                        &|schema, value| validate(schema, value),
                    )?
                    .ok_or(OperationError::Invalid("Unknown worker profile"))
                })
                .transpose()
        })
        .await?;
    let explicit = request.intent.as_object();
    let profile_model_value = profile
        .as_ref()
        .and_then(|profile| profile.get("model"))
        .filter(|value| !value.is_null());
    let profile_model = profile_model_value.and_then(Value::as_str);
    let profile_effort = profile
        .as_ref()
        .and_then(|profile| profile.get("effort"))
        .filter(|value| !value.is_null());
    let model = explicit
        .and_then(|body| body.get("model"))
        .and_then(Value::as_str)
        .or(profile_model)
        .map(str::to_owned);
    let profile_supplies_model =
        explicit.is_none_or(|body| !body.contains_key("model")) && profile_model_value.is_some();
    let profile_supplies_effort =
        explicit.is_none_or(|body| !body.contains_key("effort")) && profile_effort.is_some();
    // Python's `needs_catalog` is based on the merged data keys, not on the
    // mere presence of `profile_id` (`codex_runtime.py:4875-4888`).
    let needs_catalog = agent_needs_catalog(
        request.parent.is_some(),
        explicit,
        profile_supplies_model,
        profile_supplies_effort,
    );
    // Standalone create resolves its project/account binding before fetching a
    // catalog; child account selection is resolved from the parent at the
    // admission boundary (`codex_runtime.py:4876-4888`).
    let account = if request.parent.is_none() {
        Some(creation_account_key(context, request, false).await?)
    } else {
        None
    };
    Ok(AgentAdmission {
        replay: None,
        account,
        model,
        needs_catalog,
    })
}

fn agent_needs_catalog(
    is_child: bool,
    explicit: Option<&serde_json::Map<String, Value>>,
    profile_supplies_model: bool,
    profile_supplies_effort: bool,
) -> bool {
    is_child
        || [
            "model",
            "effort",
            "fast_mode",
            "daybreak_enabled",
            "worker_defaults",
        ]
        .iter()
        .any(|field| explicit.is_some_and(|body| body.contains_key(*field)))
        || profile_supplies_model
        || profile_supplies_effort
}

fn merge_profile_catalog_fields(intent: &mut Value, original: &Value, profile: &Value) -> bool {
    let mut needs_catalog = false;
    for field in ["model", "effort"] {
        let supplied = !original
            .as_object()
            .is_some_and(|body| body.contains_key(field))
            && profile.get(field).is_some_and(|value| !value.is_null());
        if supplied {
            intent[field] = profile[field].clone();
            needs_catalog = true;
        }
    }
    needs_catalog
}

#[cfg(test)]
mod agent_catalog_requirement_tests {
    use super::{agent_needs_catalog, merge_profile_catalog_fields};
    use serde_json::json;

    #[test]
    fn catalog_is_required_for_merged_model_or_effort_but_not_profile_presence() {
        // Python merges non-null profile fields before evaluating `needs_catalog`
        // (`codex_runtime.py:4859-4888`).
        let profile_only = json!({"profile_id": "role-only", "role": "reviewer"});
        assert!(!agent_needs_catalog(
            false,
            profile_only.as_object(),
            false,
            false
        ));
        assert!(agent_needs_catalog(
            false,
            profile_only.as_object(),
            true,
            false
        ));
        assert!(agent_needs_catalog(
            false,
            profile_only.as_object(),
            false,
            true
        ));
        let explicit_model = json!({"model": "gpt-6-astra"});
        assert!(agent_needs_catalog(
            false,
            explicit_model.as_object(),
            false,
            false
        ));
        assert!(!agent_needs_catalog(false, None, false, false));
        assert!(agent_needs_catalog(true, None, false, false));
    }

    #[test]
    fn profile_merges_model_and_effort_without_losing_either_field() {
        // Python's `{profile fields, **data}` merge keeps both profile values
        // when the request omitted them (`codex_runtime.py:4859-4872`).
        let original = json!({"profile_id": "execution-profile"});
        let profile = json!({"model": "gpt-6-luna", "effort": "high"});
        let mut effective = original.clone();
        assert!(merge_profile_catalog_fields(
            &mut effective,
            &original,
            &profile
        ));
        assert_eq!(effective["model"], "gpt-6-luna");
        assert_eq!(effective["effort"], "high");
    }
}

async fn agent_creation_intent(request: &Creation) -> Result<Value, OperationError> {
    let mut intent = request.intent.clone();
    let object = intent.as_object_mut().ok_or(OperationError::Internal)?;
    if let Some(imported_from) = &request.imported_from {
        object.insert("imported_from".to_owned(), json!(imported_from));
    }
    object.remove("id");
    intent["prompt"] = json!(python_strip(&request.prompt));
    intent["name"] = json!(python_strip(request.name.as_deref().unwrap_or("Lead")));
    if let Some(cwd) = project_directory_async(request.cwd.clone()).await? {
        intent["cwd"] = json!(cwd);
    }
    Ok(intent)
}

async fn create(
    context: &Context<'_>,
    mut request: Creation,
    draft: bool,
) -> Result<Value, OperationError> {
    let id = request.id.clone().map_or_else(new_id, Ok)?;
    let workspace = request
        .workspace
        .as_deref()
        .unwrap_or(default_workspace_mode(draft))
        .to_owned();
    let requested_cwd = project_directory_async(request.cwd.clone()).await?;
    if !draft {
        // Import source is part of the durable creation identity. Keep it in
        // the intent compared by replay_agent, including the transaction's
        // concurrent recheck, rather than relying on an earlier route read.
        request.intent = agent_creation_intent(&request).await?;
    }
    let signature = lead_signature(&request.intent, &workspace, requested_cwd.as_deref());
    let validate = std::sync::Arc::clone(&context.validate);
    let intent = request.intent.clone();
    let read_id = id.clone();
    let read_signature = signature.clone();
    let read_intent = intent.clone();
    let read_validate = std::sync::Arc::clone(&validate);
    let existing = context
        .store
        .read_async::<_, OperationError>("runtime-services/agents_create/read", move |read| {
            let validate = |schema: &str, value: &Value| read_validate(schema, value);
            if draft {
                Ok(replay_lead(read, &read_id, &read_signature, validate)?)
            } else {
                Ok(replay_agent(read, &read_id, &read_intent, validate)?)
            }
        })
        .await?;
    if existing.is_some() {
        let validate = std::sync::Arc::clone(&validate);
        let replay_id = id.clone();
        let signature = signature.clone();
        let intent = intent.clone();
        let replayed = context
            .store
            .transaction_with_error(move |tx| {
                let validate = |schema: &str, value: &Value| validate(schema, value);
                let replay = |connection: &Connection| {
                    if draft {
                        replay_lead(connection, &replay_id, &signature, validate)
                    } else {
                        replay_agent(connection, &replay_id, &intent, validate)
                    }
                };
                let current = replay(tx)?.ok_or(CreateError::Conflict)?;
                Ok::<Value, OperationError>(sync_projection(tx, &current)?)
            })
            .await?;
        #[cfg(target_os = "macos")]
        // Replays use the persisted child workspace mode, even when the retry
        // omitted an explicit workspace (Python `codex_runtime.py:5175-5201`).
        let replayed = if should_resume_image_provisioning(&replayed) {
            provision_image_workspace(context, &replayed, &id).await?
        } else {
            replayed
        };
        return Ok(replayed);
    }
    let previous_id = request.previous.clone();
    let profile_id = request.profile.clone().filter(|id| !id.is_empty());
    let read_validate = std::sync::Arc::clone(&validate);
    let (previous, profile) = context
        .store
        .read_async::<_, OperationError>("runtime-services/agents_create/read", move |read| {
            let previous = previous_lead(read, previous_id.as_deref(), &|schema, value| {
                read_validate(schema, value)
            })?;
            let profile = profile_id
                .map(|id| {
                    load_record(
                        read,
                        "runtime_profiles",
                        "ProfileRecord",
                        &id,
                        &|schema, value| read_validate(schema, value),
                    )?
                    .ok_or(OperationError::Invalid("Unknown worker profile"))
                })
                .transpose()?;
            Ok((previous, profile))
        })
        .await?;
    if let Some(profile) = &profile {
        if request.role.is_none() {
            request.role = profile
                .get("role")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        request.catalog |= merge_profile_catalog_fields(&mut request.intent, &intent, profile);
    }
    if request.parent.is_some() {
        if request.worker_catalog {
            return Err(OperationError::Invalid(
                "Only a lead can change worker defaults",
            ));
        }
        return create_child(
            context,
            &request,
            &id,
            &request.intent,
            &intent,
            std::sync::Arc::clone(&validate),
        )
        .await;
    }
    if matches!(workspace.as_str(), "image" | "layr") {
        if !cfg!(target_os = "macos") {
            return Err(OperationError::Invalid(
                "This server supports only worktree chats",
            ));
        }
    } else if workspace != "worktree" {
        return Err(OperationError::Invalid("Unknown workspace mode"));
    }
    if !draft && request.cwd.as_deref().is_none_or(str::is_empty) {
        return Err(OperationError::Invalid(
            "Select an existing project directory",
        ));
    }
    let cwd = if let Some(cwd) = requested_cwd {
        cwd
    } else if let Some(prior) = &previous {
        let path = prior
            .get("hostProjectPath")
            .filter(|value| !value.is_null())
            .or_else(|| prior.get("cwd"))
            .and_then(Value::as_str)
            .ok_or(OperationError::Internal)?
            .to_owned();
        project_directory_async(Some(path))
            .await?
            .ok_or(OperationError::Internal)?
    } else {
        project_directory_default_async().await?
    };
    let binding = if request.project_binding {
        Some(
            resolve_project_binding(
                context,
                cwd.clone(),
                request
                    .intent
                    .get("project_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                request
                    .intent
                    .get("project_server_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                request.intent.get("project_folder").cloned(),
            )
            .await?,
        )
    } else {
        None
    };
    let account = if draft {
        new_chat_account(
            context,
            request.account.clone(),
            cwd.clone(),
            requested_model(&request),
        )
        .await?
    } else {
        account_key(context, request.account.clone(), cwd.clone()).await?
    };
    let identity = account_identity(context, &account).await?;
    let provider = identity.provider.key().to_owned();
    let settings = if request.catalog || request.worker_catalog {
        let named_model = request
            .intent
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.trim().is_empty());
        let model = named_model.unwrap_or(new_chat_model(identity.provider, draft));
        let effort = request
            .intent
            .get("effort")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|effort| !effort.is_empty());
        let effort = effort.or_else(|| {
            (request
                .intent
                .get("model")
                .and_then(Value::as_str)
                .is_some())
            .then(|| previous.as_ref()?.get("effort")?.as_str())
            .flatten()
        });
        let fast_mode = request
            .intent
            .get("fast_mode")
            .and_then(Value::as_bool)
            .or_else(|| {
                request.intent.get("model").and_then(Value::as_str)?;
                previous.as_ref()?.get("fastMode")?.as_bool()
            })
            .unwrap_or(false);
        let daybreak_enabled = request
            .intent
            .get("daybreak_enabled")
            .and_then(Value::as_bool)
            .or_else(|| {
                request.intent.get("model").and_then(Value::as_str)?;
                previous.as_ref()?.get("daybreakEnabled")?.as_bool()
            })
            .unwrap_or(false);
        let catalog = context
            .catalog
            .as_deref()
            .ok_or(OperationError::Unavailable(
                crate::model_catalog::MODEL_CATALOG_DEPENDENCY,
            ))?;
        let provider_default = crate::provider_adapter::default_model(identity.provider);
        match catalog.resolve(
            &account,
            &identity,
            model,
            effort,
            fast_mode,
            daybreak_enabled,
        ) {
            // A catalog without the new-chat default model keeps the provider default.
            Err(_) if named_model.is_none() && model != provider_default => catalog.resolve(
                &account,
                &identity,
                provider_default,
                effort,
                fast_mode,
                daybreak_enabled,
            )?,
            result => result?,
        }
    } else {
        ExecutionSettings {
            model: new_chat_model(identity.provider, draft).to_owned(),
            effort: Some(new_chat_effort(identity.provider, draft).to_owned()),
            native_effort: new_chat_effort(identity.provider, draft).to_owned(),
            fast_mode: false,
            daybreak_enabled: false,
            cyber_access_program: "standard".to_owned(),
        }
    };
    let yolo = request.yolo.unwrap_or_else(|| {
        previous
            .as_ref()
            .and_then(|record| record.get("yoloMode"))
            .and_then(Value::as_bool)
            .unwrap_or(true)
    });
    let concurrency = request.concurrency.unwrap_or(DEFAULT_SUBAGENT_CONCURRENCY);
    let requested_max_agents = request.max_agents.unwrap_or(DEFAULT_MAX_TEAM_AGENTS);
    if !SUBAGENT_CONCURRENCY.contains(&concurrency) || !TEAM_AGENTS.contains(&requested_max_agents)
    {
        return Err(OperationError::Invalid(CREATE_LIMITS_REFUSAL));
    }
    if request.token_budget.is_some_and(|budget| budget <= 0) {
        return Err(OperationError::Invalid(
            "Token budget must be a positive integer",
        ));
    }
    let role = request.role.as_deref().unwrap_or("orchestrator");
    let max_agents = if request.max_agents.is_some() || role != "orchestrator" {
        requested_max_agents
    } else {
        requested_max_agents.max(concurrency + TEAM_AGENT_HEADROOM)
    };
    let prompt = python_strip(&request.prompt).to_owned();
    let created = crate::event_write::unix_time().map_err(|_| OperationError::Internal)?;
    let (worker_defaults, review_defaults) = if role == "orchestrator" {
        (
            Some(
                resolve_worker_defaults(
                    context,
                    request.intent.get("worker_defaults"),
                    &account,
                    &identity,
                    &settings.model,
                    &[],
                )
                .await?,
            ),
            Some(
                resolve_review_defaults(
                    context,
                    request.intent.get("review_defaults"),
                    &account,
                    &identity,
                    request
                        .intent
                        .pointer("/worker_defaults/account_key")
                        .and_then(Value::as_str),
                    &[],
                )
                .await?,
            ),
        )
    } else {
        (None, None)
    };
    let mut record = build_agent_record(
        &id,
        if draft {
            "New chat"
        } else {
            python_strip(request.name.as_deref().unwrap_or("Lead"))
        },
        &prompt,
        &cwd,
        &account,
        identity.provider,
        yolo,
        role,
        role == "orchestrator",
        if draft { "idle" } else { "queued" },
        &workspace,
        Some(concurrency),
        max_agents,
        json!(request.token_budget),
        draft,
        created,
        draft.then_some(signature.clone()),
    );
    if !draft && role == "orchestrator" {
        record["maxAgentsExplicit"] = json!(request.max_agents.is_some());
    }
    if let Some(imported_from) = &request.imported_from {
        record["importedFrom"] = json!(imported_from);
        record["autoWake"] = json!(true);
    }
    if role == "orchestrator" {
        record["workerDefaults"] = worker_defaults.unwrap_or_else(|| {
            json!({"model":"gpt-6-luna","effort":"high","fastMode":false,"daybreakEnabled":false,"delegationDepth":1})
        });
        record["reviewDefaults"] = review_defaults.unwrap_or(json!({"model":null,"effort":null}));
    }
    mode_fields(&mut record);
    if workspace == "image" {
        set_image_workspace_fields(&mut record, &cwd);
    }
    if workspace == "layr" {
        set_vm_workspace_fields(&mut record, &cwd);
    }
    record["model"] = json!(settings.model);
    record["effort"] = json!(settings.effort);
    record["fastMode"] = json!(settings.fast_mode);
    record["daybreakEnabled"] = json!(settings.daybreak_enabled);
    record["cyberAccessProgram"] = json!(settings.cyber_access_program);
    record["profileId"] = json!(request.profile);
    if let Some(profile) = profile {
        record["profileInstructions"] = profile["instructions"].clone();
    }
    if !draft {
        record["creationIntent"] = intent.clone();
    }
    if let Some(binding) = &binding {
        record["projectId"] = binding["projectId"].clone();
        record["projectServerId"] = binding["projectServerId"].clone();
        record["projectFolder"] = binding["projectFolder"].clone();
    }
    let reuse_execution_fields = [
        "effort",
        "fast_mode",
        "daybreak_enabled",
        "worker_defaults",
        "review_defaults",
    ]
    .iter()
    .any(|field| request.intent.get(*field).is_some());
    let id_for_write = id.clone();
    let workspace_for_write = workspace.clone();
    let result = mutate(context, move |tx| {
        let validate_arc = validate;
        let validate = |schema: &str, value: &Value| validate_arc(schema, value);
        let replay_validate = std::sync::Arc::clone(&validate_arc);
        let replay_id = id_for_write.clone();
        let replay_signature = signature.clone();
        let replay_intent = intent.clone();
        let replay = |connection: &Connection| {
            let validate = |schema: &str, value: &Value| replay_validate(schema, value);
            if draft {
                replay_lead(connection, &replay_id, &replay_signature, validate)
            } else {
                replay_agent(connection, &replay_id, &replay_intent, validate)
            }
        };
        if let Some(existing) = replay(tx)? {
            return Ok(sync_projection(tx, &existing)?);
        }
        if let Some(prior) = previous_lead(tx, request.previous.as_deref(), &validate)?
            && request.reuse_empty.unwrap_or(true)
            && draft
            && !reuse_execution_fields
            && !request.worker_catalog
            && reusable_previous_workspace(&prior, &workspace_for_write, &cwd)
            && !truthy(prior.get("layrReady"))
            && prior
                .get("workspaceMode")
                .and_then(Value::as_str)
                .unwrap_or("worktree")
                == workspace_for_write.as_str()
            && studio_runtime_providers::ProviderKind::key_of(&prior) == Some(provider.as_str())
            && crate::sync_entity_context::agent_view_context(tx, &prior, None)?.empty_lead
        {
            let mut reused = prior;
            let previous_account = reused
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
                .to_owned();
            if reused.get("provider").and_then(Value::as_str) != Some(provider.as_str()) {
                return Err(OperationError::Create(CreateError::IdentityChanged));
            }
            reused["cwd"] = json!(cwd);
            reused["hostProjectPath"] = json!(cwd);
            reused["accountKey"] = json!(account);
            reused["executionSettingsAccountKey"] = json!(account);
            reused["yoloMode"] = json!(yolo);
            if request
                .intent
                .get("model")
                .and_then(Value::as_str)
                .is_some()
            {
                reused["model"] = json!(settings.model);
                reused["effort"] = json!(settings.effort);
                reused["fastMode"] = json!(settings.fast_mode);
                reused["daybreakEnabled"] = json!(settings.daybreak_enabled);
                reused["cyberAccessProgram"] = json!(settings.cyber_access_program);
            }
            if let Some(binding) = &binding {
                reused["projectId"] = binding["projectId"].clone();
                reused["projectServerId"] = binding["projectServerId"].clone();
                reused["projectFolder"] = binding["projectFolder"].clone();
            } else if let Some(fields) = reused.as_object_mut() {
                fields.remove("projectId");
                fields.remove("projectServerId");
                fields.remove("projectFolder");
            }
            if previous_account != account {
                reused["daybreakEnabled"] = json!(false);
                reused["cyberAccessProgram"] = json!("standard");
                reused
                    .as_object_mut()
                    .ok_or(OperationError::Internal)?
                    .remove("pendingSettings");
                reused
                    .as_object_mut()
                    .ok_or(OperationError::Internal)?
                    .remove("pendingSettingsAccountKey");
            }
            if workspace_for_write == "layr" {
                set_vm_workspace_fields(&mut reused, &cwd);
            } else if workspace_for_write == "image" {
                set_image_workspace_fields(&mut reused, &cwd);
            }
            ensure_project(tx, &reused)?;
            save_agent(tx, &reused, &validate)?;
            tx.execute(
                "INSERT INTO runtime_lead_requests(id,agent,signature) VALUES(?1,?2,?3)",
                rusqlite::params![id_for_write, reused["id"].as_str(), signature],
            )
            .map_err(StoreError::from)?;
            return Ok(sync_projection(tx, &reused)?);
        }
        if let Some(binding) = &binding {
            let current = resolve_project_binding_from(
                tx,
                binding["cwd"].as_str().ok_or(OperationError::Internal)?,
                binding["projectId"].as_str(),
                binding["projectServerId"].as_str(),
                binding["localServerId"]
                    .as_str()
                    .ok_or(OperationError::Internal)?,
                Some(&binding["projectFolder"]),
            )?;
            if current != *binding {
                return Err(CreateError::IdentityChanged.into());
            }
        }
        Ok(super::create_agent_with_identity(
            tx,
            &id_for_write,
            &prompt,
            record,
            created,
            validate,
        )?)
    })
    .await?;
    #[cfg(target_os = "macos")]
    let result = if workspace == "image" {
        provision_image_workspace(context, &result, &id).await?
    } else {
        result
    };
    Ok(result)
}

async fn create_child(
    context: &Context<'_>,
    request: &Creation,
    id: &str,
    execution_intent: &Value,
    creation_intent: &Value,
    validate: PersistedRecordValidator,
) -> Result<Value, OperationError> {
    let parent_id = request.parent.as_deref().ok_or(OperationError::Internal)?;
    let parent_id_for_read = parent_id.to_owned();
    let validate_fn = std::sync::Arc::clone(&validate);
    let (parent, root, project_binding) = context
        .store
        .read_async::<_, OperationError>("runtime-services/agents_create/read", move |db| {
            let validate = |schema: &str, value: &Value| validate_fn(schema, value);
            let parent = load_agent_value(db, &parent_id_for_read, &validate)?
                .ok_or(MutationError::UnknownAgent)?;
            if truthy(parent.get("deletedAt")) {
                return Err(MutationError::Deleted.into());
            }
            let root_id = parent
                .get("rootId")
                .and_then(Value::as_str)
                .ok_or(OperationError::Internal)?;
            let root =
                load_agent_value(db, root_id, &validate)?.ok_or(MutationError::UnknownAgent)?;
            let project = parent_project_binding(db, &parent, &root)?;
            Ok((parent, root, project))
        })
        .await?;
    if !truthy(root.get("isLead"))
        || root.get("id").and_then(Value::as_str) != root.get("rootId").and_then(Value::as_str)
    {
        return Err(MutationError::LeadRequired.into());
    }
    if !truthy(parent.get("autoWake")) || !truthy(root.get("autoWake")) {
        return Err(OperationError::Invalid("This team is stopped"));
    }
    if root
        .get("concurrency")
        .and_then(Value::as_i64)
        .unwrap_or(32)
        == 0
    {
        return Err(OperationError::Invalid(
            "Single agent mode disables new delegation",
        ));
    }
    let parent_workspace = parent
        .get("workspaceMode")
        .and_then(Value::as_str)
        .unwrap_or("worktree");
    let (role, workspace, worktree) = child_role_workspace(
        request.role.as_deref(),
        request.workspace.as_deref(),
        parent_workspace,
        parent.get("executionMode").and_then(Value::as_str) == Some("vm"),
    )?;
    validate_vm_parent_for_create(workspace, &parent)?;
    if matches!(workspace, "image" | "layr") && !cfg!(target_os = "macos") {
        return Err(OperationError::Invalid(
            "This server supports only worktree chats",
        ));
    }
    let cwd = parent
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or(OperationError::Internal)?;
    let host_cwd = if workspace == "layr" {
        root.get("hostProjectPath")
            .or_else(|| root.get("cwd"))
            .and_then(Value::as_str)
            .unwrap_or(cwd)
    } else {
        cwd
    };
    let defaults = root
        .get("workerDefaults")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let account = request
        .account
        .as_deref()
        .or_else(|| defaults.get("accountKey").and_then(Value::as_str))
        .unwrap_or_else(|| {
            parent
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
        });
    let (identity, account_status, can_attempt_native_proof) =
        account_identity_with_status(context, account.to_owned()).await?;
    let parent_account_key = parent
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    if account != parent_account_key
        && account_status != AccountStatus::Ready
        && !can_attempt_native_proof
    {
        return Err(OperationError::Unavailable("account-driver/unavailable"));
    }
    if defaults
        .get("accountKey")
        .and_then(Value::as_str)
        .is_some_and(|selected| selected != account)
    {
        return Err(MutationError::IdentityChanged.into());
    }
    let provider = identity.provider.key().to_owned();
    let model = execution_intent
        .get("model")
        .and_then(Value::as_str)
        .or_else(|| defaults.get("model").and_then(Value::as_str))
        .or_else(|| root.get("model").and_then(Value::as_str))
        .unwrap_or(crate::provider_adapter::default_model(identity.provider));
    let effort = execution_intent
        .get("effort")
        .and_then(Value::as_str)
        .or_else(|| defaults.get("effort").and_then(Value::as_str));
    let fast_mode = execution_intent
        .get("fast_mode")
        .and_then(Value::as_bool)
        .or_else(|| defaults.get("fastMode").and_then(Value::as_bool))
        .unwrap_or(false);
    let daybreak = execution_intent
        .get("daybreak_enabled")
        .and_then(Value::as_bool)
        .or_else(|| defaults.get("daybreakEnabled").and_then(Value::as_bool))
        .unwrap_or(false);
    let catalog = context.catalog.clone().ok_or(OperationError::Unavailable(
        crate::model_catalog::MODEL_CATALOG_DEPENDENCY,
    ))?;
    let body = json!({
        "account_key":account,"name":request.name.as_deref().unwrap_or(model),
        "model":model,"effort":effort,"fast_mode":fast_mode,
        "daybreak_enabled":daybreak,"prompt":request.prompt,
        "workspace_explicit":request.workspace.is_some()
    });
    let root_agent = root
        .get("id")
        .and_then(Value::as_str)
        .ok_or(OperationError::Internal)?;
    let epoch = parent.get("epoch").and_then(Value::as_i64).unwrap_or(0);
    let parent_cwd = parent
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or(OperationError::Internal)?;
    // Python persists VM workers against the host project identity
    // (`codex_runtime.py:5018-5020`), while `cwd` is guest-local. Freeze that
    // same identity for the writer recheck in `host_worker_create`.
    let parent_project_path = parent_project_path(&parent, &root, parent_cwd);
    let parent_account_key = parent
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let parent_project_id = parent.get("projectId").cloned();
    let parent_project_server_id = parent.get("projectServerId").cloned();
    let root_yolo = root.get("yoloMode").cloned().unwrap_or(json!(true));
    let root_max_agents = root
        .get("maxAgents")
        .cloned()
        .unwrap_or(json!(DEFAULT_MAX_TEAM_AGENTS));
    let root_token_budget = root.get("tokenBudget").cloned().unwrap_or(Value::Null);
    let prepare_cwd = cwd.to_owned();
    let prepare_host_cwd = host_cwd.to_owned();
    let prepare_parent_cwd = parent_cwd.to_owned();
    let prepare_parent_project_path = parent_project_path.to_owned();
    let prepare_parent_account_key = parent_account_key.to_owned();
    let prepare_role = role.to_owned();
    let prepare_id = id.to_owned();
    let prepare_root_agent = root_agent.to_owned();
    let prepare_parent_id = parent_id.to_owned();
    let prepare_identity = identity.clone();
    let (draft, mut record) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/agents_create/child_prepare",
        move || {
            let (base_ref, base_commit) = if worktree {
                crate::host_worker_create::freeze_base(PathBuf::from(&prepare_cwd).as_path())?
            } else {
                (String::new(), String::new())
            };
            let draft = crate::host_worker_create::prepare_work_item(
                &body,
                PathBuf::from(&prepare_host_cwd).as_path(),
                studio_runtime_store::AgentId::from(prepare_id),
                studio_runtime_store::AgentId::from(prepare_root_agent),
                studio_runtime_store::AgentId::from(prepare_parent_id),
                &base_ref,
                &base_commit,
                epoch,
                &prepare_parent_cwd,
                &prepare_parent_project_path,
                &prepare_parent_account_key,
                parent_project_id.as_ref(),
                parent_project_server_id.as_ref(),
                project_binding.as_ref(),
                &prepare_identity,
                catalog.as_ref(),
                &prepare_role,
                worktree,
                fast_mode,
                daybreak,
            )?;
            let checked = crate::host_worker_create::persisted_record(&draft)?;
            Ok::<_, CreateError>((draft, checked.value()))
        },
    )
    .await
    .map_err(|_| OperationError::Internal)??;
    if workspace == "layr" {
        record["cwd"] = json!(cwd);
        set_vm_workspace_fields(&mut record, host_cwd);
        if role == "reviewer" {
            record["worktree"] = json!(false);
        }
    }
    if workspace == "image" {
        set_image_workspace_fields(&mut record, cwd);
    }
    record["creationIntent"] = creation_intent.clone();
    record["provider"] = json!(provider);
    record["yoloMode"] = root_yolo;
    record["maxAgents"] = root_max_agents;
    record["tokenBudget"] = root_token_budget;
    let created = record
        .get("created")
        .and_then(Value::as_f64)
        .ok_or(OperationError::Internal)?;
    let prompt = request.prompt.trim().to_owned();
    let task_id = creation_intent
        .get("task_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let id = id.to_owned();
    let id_for_write = id.clone();
    let parent_id = parent_id.to_owned();
    let validate = std::sync::Arc::clone(&validate);
    let result = mutate(context, move |tx| {
        let validate = |schema: &str, value: &Value| validate(schema, value);
        crate::host_worker_create::recheck_worker(tx, &draft, epoch, &identity)?;
        let current_parent =
            load_agent_value(tx, &parent_id, &validate)?.ok_or(MutationError::UnknownAgent)?;
        let current_root = load_agent_value(tx, &draft.root_id.0, &validate)?
            .ok_or(MutationError::UnknownAgent)?;
        let permission_intent = record["creationIntent"].clone();
        crate::child_permissions::inherit_claude_permissions(
            &mut record,
            &current_parent,
            &current_root,
            &permission_intent,
        )
        .map_err(OperationError::Invalid)?;
        let mut prompt = prompt;
        if let Some(task_id) = &task_id {
            let work = studio_runtime_store::WorkItemsRepository::work_record(tx, task_id)?
                .ok_or(OperationError::Invalid("Unknown task_id"))?;
            prompt.push_str(&studio_runtime_schema::task_results::assignment_text(&work));
        }
        let result = super::create_agent_with_identity(
            tx,
            &id_for_write,
            &prompt,
            record,
            created,
            validate,
        )?;
        if let Some(task_id) = &task_id {
            let actor = studio_runtime_store::AgentRepository::get(
                tx,
                &studio_runtime_store::AgentId::from(parent_id),
            )?
            .ok_or(OperationError::Internal)?;
            let now = crate::event_write::unix_time().map_err(|_| OperationError::Internal)?;
            crate::work_assignment_runtime::assign_in_transaction(
                tx,
                task_id,
                &id_for_write,
                &actor,
                now,
            )
            .map_err(|_| OperationError::Invalid("Work item assignment was rejected"))?;
        }
        Ok(result)
    })
    .await?;
    #[cfg(target_os = "macos")]
    let result = if workspace == "image" {
        provision_image_workspace(context, &result, &id).await?
    } else {
        result
    };
    Ok(result)
}

fn validate_vm_parent_for_create(workspace: &str, parent: &Value) -> Result<(), OperationError> {
    if workspace == "layr" && parent.get("executionMode").and_then(Value::as_str) != Some("vm") {
        return Err(OperationError::Invalid(
            "A VM workspace requires a VM parent",
        ));
    }
    Ok(())
}

/// Match Runtime.create's child defaults: implementers use the selected
/// workspace, while reviewer children default to a shared, non-worktree cwd.
/// Both roles still inherit the root's execution settings; provider permission
/// preparation applies the reviewer's read-only policy when YOLO is disabled.
fn child_role_workspace<'a>(
    role: Option<&'a str>,
    requested_workspace: Option<&'a str>,
    parent_workspace: &'a str,
    vm_parent: bool,
) -> Result<(&'a str, &'a str, bool), OperationError> {
    let role = role.unwrap_or("implementer");
    if !matches!(role, "implementer" | "reviewer") {
        return Err(OperationError::Invalid("Invalid agent role"));
    }
    if role == "reviewer" && vm_parent && requested_workspace.is_some_and(|mode| mode != "layr") {
        return Err(OperationError::Invalid(
            "A VM reviewer must use the parent's layr workspace",
        ));
    }
    let workspace = requested_workspace.unwrap_or(if role == "reviewer" && !vm_parent {
        "shared"
    } else {
        parent_workspace
    });
    let worktree = match workspace {
        "worktree" => true,
        "shared" => false,
        "image" => false,
        "layr" => false,
        _ => return Err(OperationError::Invalid("Unknown workspace mode")),
    };
    Ok((role, workspace, worktree))
}

fn parent_project_path<'a>(parent: &'a Value, root: &'a Value, parent_cwd: &'a str) -> &'a str {
    if parent.get("executionMode").and_then(Value::as_str) == Some("vm") {
        parent
            .get("hostProjectPath")
            .or_else(|| root.get("hostProjectPath"))
            .or_else(|| root.get("cwd"))
            .and_then(Value::as_str)
            .unwrap_or(parent_cwd)
    } else {
        parent_cwd
    }
}

fn parent_project_binding(
    db: &rusqlite::Connection,
    parent: &Value,
    root: &Value,
) -> Result<Option<Value>, OperationError> {
    let parent_cwd = parent
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or(OperationError::Internal)?;
    let project_path = parent_project_path(parent, root, parent_cwd);
    Ok(
        studio_runtime_store::ProjectsRepository::get(db, project_path)?
            .map(|(_, project)| project),
    )
}

pub(crate) fn image_provision_is_current(record: &Value, expected_epoch: i64) -> bool {
    record.get("epoch").and_then(Value::as_i64) == Some(expected_epoch)
        && record.get("deletedAt").is_none_or(Value::is_null)
        && record.get("imageWorkspace").and_then(Value::as_bool) == Some(true)
        && record.get("imageWorkspaceReady").and_then(Value::as_bool) != Some(true)
        && matches!(
            record.get("imageWorkspacePhase").and_then(Value::as_str),
            Some("creating" | "read_only")
        )
        && !matches!(
            record.get("status").and_then(Value::as_str),
            Some("completed" | "cancelled" | "archived" | "deleted")
        )
}

#[cfg(any(target_os = "macos", test))]
fn should_resume_image_provisioning(record: &Value) -> bool {
    // Cleanup archives remain unready by design; only pending states may remount
    // (`workspace_cleanup_runtime.rs:706-710`).
    record.get("workspaceMode").and_then(Value::as_str) == Some("image")
        && record
            .get("epoch")
            .and_then(Value::as_i64)
            .is_some_and(|epoch| image_provision_is_current(record, epoch))
}

/// Record image intent so readiness can remount the durable ASIF state. On macOS,
/// provisioning is queued like Python's `codex_runtime.py:5757` base-build callback;
/// Python provisions with `create_workspace` at line 5878 after the base is ready.
pub(crate) fn set_image_workspace_fields(record: &mut Value, root: &str) {
    record["workspaceMode"] = json!("image");
    record["imageWorkspace"] = json!(true);
    record["imageWorkspaceReady"] = json!(false);
    record["imageWorkspacePhase"] = json!("creating");
    record["imageWorkspaceRepo"] = json!(root);
    record["workspaceBackend"] = json!("asif");
}

/// Persist the host identity while the VM guest creates the project and line
/// before its first turn. Python's `codex_runtime.py:5018-5026` stores these
/// fields at creation; `codex_vm_agents.py:196-230` fills the guest association
/// during preparation.
pub(crate) fn set_vm_workspace_fields(record: &mut Value, host_root: &str) {
    record["workspaceMode"] = json!("layr");
    record["executionMode"] = json!("vm");
    record["hostProjectPath"] = json!(host_root);
    record["environment"] = json!("linux");
    record["worktree"] = json!(false);
    record["layrReady"] = json!(false);
}

#[cfg(target_os = "macos")]
async fn provision_image_workspace(
    context: &Context<'_>,
    result: &Value,
    fallback_id: &str,
) -> Result<Value, OperationError> {
    let id = result
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(fallback_id);
    let root = result
        .get("imageWorkspaceRepo")
        .or_else(|| result.get("cwd"))
        .and_then(Value::as_str)
        .ok_or(OperationError::Internal)?;
    let expected_epoch = result
        .get("epoch")
        .and_then(Value::as_i64)
        .ok_or(OperationError::Internal)?;
    let root = root.to_owned();
    let id = id.to_owned();
    let create_id = id.clone();
    let workspace = match studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/agents_create/image_workspace_create",
        move || crate::image_workspace::create_workspace(PathBuf::from(root).as_path(), &create_id),
    )
    .await
    .map_err(|_| OperationError::Internal)?
    {
        Ok(workspace) => workspace,
        Err(error) => {
            discard_image_workspace(id.to_owned()).await;
            persist_image_provision_failure(
                context.store.as_ref(),
                &id,
                expected_epoch,
                &error.to_string(),
            )
            .await?;
            return Err(OperationError::Unavailable(
                "workspace-state/image-provisioning",
            ));
        }
    };
    if workspace.get("path").and_then(Value::as_str).is_none()
        || workspace.get("mount").and_then(Value::as_str).is_none()
    {
        discard_image_workspace(id.to_owned()).await;
        persist_image_provision_failure(
            context.store.as_ref(),
            &id,
            expected_epoch,
            "Image workspace provider returned no path",
        )
        .await?;
        return Err(OperationError::Unavailable(
            "workspace-state/image-provisioning",
        ));
    };
    let agent_id = id.to_owned();
    let projection = context
        .store
        .transaction(move |tx| {
            let Some(agent) = studio_runtime_store::AgentRepository::get(
                tx,
                &studio_runtime_store::AgentId::from(agent_id.clone()),
            )?
            else {
                return Ok(None);
            };
            let value = agent.into_value();
            if !image_provision_is_current(&value, expected_epoch) {
                return Ok(None);
            }
            let now =
                crate::event_write::unix_time().map_err(|_| StoreError::InvalidStoredObject {
                    table: "runtime_agents",
                    id: agent_id.to_owned(),
                })?;
            crate::image_workspace_agents::record_ready(tx, value, &workspace, now)
                .map(Some)
                .map_err(|error| match error {
                    crate::event_write::EventWriteError::Store(error) => error,
                    _ => StoreError::InvalidStoredObject {
                        table: "runtime_agents",
                        id: agent_id.to_owned(),
                    },
                })
        })
        .await?;
    if let Some(projection) = projection {
        Ok(projection)
    } else {
        discard_image_workspace(id.to_owned()).await;
        Err(OperationError::Create(CreateError::IdentityChanged))
    }
}

/// Remove the image workspace of a provisioning attempt that will not finish.
/// The caller returns its own error to the request, so a cleanup failure is
/// logged with the agent identity instead of replacing that error.
#[cfg(target_os = "macos")]
async fn discard_image_workspace(id: String) {
    let agent = id.clone();
    let removed = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/agents_create/image_workspace_remove",
        move || {
            crate::image_workspace::remove_workspace(&id, true)
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
    )
    .await;
    match removed {
        Ok(Ok(())) => {}
        Ok(Err(error)) => eprintln!("image workspace cleanup failed for agent {agent}: {error}"),
        Err(_) => eprintln!("image workspace cleanup task did not run for agent {agent}"),
    }
}

pub(crate) async fn persist_image_provision_failure(
    store: &RuntimeStore,
    agent_id: &str,
    expected_epoch: i64,
    message: &str,
) -> Result<(), OperationError> {
    let message = message.chars().take(1200).collect::<String>();
    let agent_id = agent_id.to_owned();
    store.transaction(move |tx| {
        let Some(agent) = studio_runtime_store::AgentRepository::get(
            tx,
            &studio_runtime_store::AgentId::from(agent_id.clone()),
        )? else {
            return Ok(());
        };
        let mut value = agent.into_value();
        if image_provision_is_current(&value, expected_epoch) {
            let notice = format!("[Studio workspace error] The requested image workspace failed: {message}. The worker remains read-only in the selected folder.");
            value["imageWorkspacePhase"] = json!("failed");
            value["imageWorkspaceError"] = json!(message);
            value["status"] = json!("paused");
            value["autoWake"] = json!(false);
            value["error"] = json!(notice);
            let record = studio_runtime_store::AgentRecord::from_value(value.clone()).map_err(|_| {
                StoreError::InvalidStoredObject { table: "runtime_agents", id: agent_id.to_owned() }
            })?;
            studio_runtime_store::AgentRepository::put(tx, &record)?;
            crate::agents_create::publish_agent(tx, &record.value())?;
        }
        Ok(())
    })
    .await?;
    Ok(())
}

pub async fn configure(
    context: &Context<'_>,
    request: Configuration,
) -> Result<Value, OperationError> {
    let validate = std::sync::Arc::clone(&context.validate);
    if request
        .max_agents
        .is_some_and(|max_agents| !TEAM_AGENTS.contains(&max_agents))
        || request
            .token_budget
            .is_some_and(|budget| budget.is_some_and(|budget| budget <= 0))
    {
        return Err(OperationError::Invalid(TEAM_SIZE_REFUSAL));
    }
    if request.concurrency.is_some() {
        if request.max_agents.is_some() || request.token_budget.is_some() {
            return Err(OperationError::Invalid(
                "Configure only team storage and token limits",
            ));
        }
        let limit = request.concurrency.ok_or(OperationError::Internal)?;
        let revision = request
            .expected_mode_revision
            .filter(|revision| *revision >= 0)
            .ok_or(OperationError::Invalid(
                "A nonnegative expected mode revision is required",
            ))?;
        let request_id = request
            .request_id
            .as_deref()
            .filter(|id| (1..=200).contains(&id.chars().count()))
            .ok_or(OperationError::Invalid(
                "A subagent concurrency request id is required",
            ))?
            .to_owned();
        if !SUBAGENT_CONCURRENCY.contains(&limit) {
            return Err(OperationError::Invalid(CONCURRENCY_REFUSAL));
        }
        let signature = crate::event_write::request_signature(&json!({
            "operation":"subagent_concurrency","agent":request.id,
            "concurrency":limit,"expectedRevision":revision
        }));
        let validate = std::sync::Arc::clone(&validate);
        return mutate(context, move |tx| {
            let validate = |schema: &str, value: &Value| validate(schema, value);
            let mut root =
                load_agent_value(tx, &request.id, &validate)?.ok_or(MutationError::UnknownAgent)?;
            if root.get("isLead").and_then(Value::as_bool) != Some(true)
                || root.get("rootId").and_then(Value::as_str) != Some(request.id.as_str())
            {
                return Err(MutationError::LeadRequired.into());
            }
            if let Some(receipt) = EventWriteRepository::receipt(tx, &request_id)? {
                if receipt.signature != signature {
                    return Err(MutationError::Conflict.into());
                }
                return serde_json::from_str(&receipt.result)
                    .map_err(|_| MutationError::InvalidStoredObject {
                        table: "runtime_operation_receipts",
                        id: request_id.to_owned(),
                    })
                    .map_err(OperationError::from);
            }
            mode_fields(&mut root);
            let current = root.get("concurrency").and_then(Value::as_i64).unwrap_or(0);
            let current_revision = root
                .get("agentModeRevision")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            if current_revision != revision {
                return Err(MutationError::Stale.into());
            }
            let prior_max = root
                .get("maxAgents")
                .and_then(Value::as_i64)
                .unwrap_or(DEFAULT_MAX_TEAM_AGENTS);
            if current != limit {
                root["concurrency"] = json!(limit);
                root["agentModeRevision"] = json!(revision + 1);
                root["agentModeChangedAt"] =
                    json!(crate::event_write::unix_time().map_err(|_| MutationError::Internal)?);
                root["agentModeChangedBy"] = json!("user");
            }
            if root.get("maxAgentsExplicit").and_then(Value::as_bool) != Some(true) {
                root["maxAgents"] = json!(prior_max.max(limit + TEAM_AGENT_HEADROOM));
            }
            mode_fields(&mut root);
            let changed = current != limit || root["maxAgents"].as_i64() != Some(prior_max);
            if changed {
            save_agent(tx, &root, &validate)?;
            }
            let result = json!({"id":request.id,"concurrency":root["concurrency"],
                "agentMode":root["agentMode"],"agentModeRevision":root["agentModeRevision"],
                "maxAgents":root["maxAgents"],"tokenBudget":root.get("tokenBudget").cloned().unwrap_or(Value::Null)});
            EventWriteRepository::put_receipt(
                tx,
                &request_id,
                &signature,
                &serde_json::to_string(&result).map_err(StoreError::from)?,
            )?;
            Ok(result)
        }).await;
    }
    if request.expected_mode_revision.is_some() || request.request_id.is_some() {
        return Err(OperationError::Invalid(
            "Configure only team storage and token limits",
        ));
    }
    let max_agents = request.max_agents;
    let validate = std::sync::Arc::clone(&validate);
    mutate(context, move |tx| {
        let validate = |schema: &str, value: &Value| validate(schema, value);
        let result = configure_limits(
            tx,
            &request.id,
            max_agents,
            request.token_budget,
            |schema, value| validate(schema, value),
        )?;
        if !validate("ConfigureResponse", &result) {
            return Err(MutationError::InvalidStoredObject {
                table: "runtime_agents",
                id: request.id.clone(),
            }
            .into());
        }
        Ok(result)
    })
    .await
}
pub async fn conversation(
    context: &Context<'_>,
    request: Conversation,
) -> Result<Value, OperationError> {
    let validate = std::sync::Arc::clone(&context.validate);
    if (request.model_present && request.model.is_none())
        || (request.fast_mode_present && request.fast_mode.is_none())
    {
        return Err(OperationError::Invalid(
            "Model must be available and fast_mode must be a boolean",
        ));
    }
    if request.worker_settings {
        let id = request.id.clone();
        let read_validate = std::sync::Arc::clone(&validate);
        let initial = context
            .store
            .read_async::<_, OperationError>("runtime-services/agents_create/read", move |db| {
                load_agent_value(db, &id, &|schema, value| read_validate(schema, value))?
                    .ok_or(MutationError::UnknownAgent.into())
            })
            .await?;
        if initial.get("isLead").and_then(Value::as_bool) != Some(true) {
            return Err(MutationError::LeadRequired.into());
        }
        let account_key = initial
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default");
        if request
            .expected_account
            .as_deref()
            .is_some_and(|expected| expected != account_key)
        {
            return Err(MutationError::Stale.into());
        }
        let identity = account_identity(context, account_key).await?;
        let excluded_account_keys = initial
            .get("excludedAccountKeys")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let root_model = initial
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_LEAD_MODEL);
        let worker_defaults = match request.worker_defaults.as_ref() {
            Some(value) => resolve_worker_defaults(
                context,
                Some(value),
                account_key,
                &identity,
                root_model,
                &excluded_account_keys,
            ).await?,
            None => initial
                .get("workerDefaults")
                .cloned()
                .unwrap_or_else(|| json!({"model":"gpt-6-luna","effort":"high","fastMode":false,"daybreakEnabled":false,"delegationDepth":1})),
        };
        let review_defaults = match request.review_defaults.as_ref() {
            Some(value) => {
                let worker_account_key = request
                    .worker_defaults
                    .as_ref()
                    .and_then(|defaults| defaults.get("account_key"))
                    .and_then(Value::as_str)
                    .or_else(|| {
                        request
                            .worker_defaults
                            .is_none()
                            .then(|| {
                                initial
                                    .pointer("/workerDefaults/accountKey")
                                    .and_then(Value::as_str)
                            })
                            .flatten()
                    });
                resolve_review_defaults(
                    context,
                    Some(value),
                    account_key,
                    &identity,
                    worker_account_key,
                    &excluded_account_keys,
                )
                .await?
            }
            None => initial
                .get("reviewDefaults")
                .cloned()
                .unwrap_or(json!({"model":null,"effort":null})),
        };
        let account_key = account_key.to_owned();
        let validate = std::sync::Arc::clone(&validate);
        return mutate(context, move |tx| {
            let validate = |schema: &str, value: &Value| validate(schema, value);
            let mut target =
                load_agent_value(tx, &request.id, &validate)?.ok_or(MutationError::UnknownAgent)?;
            if target.get("accountKey").and_then(Value::as_str) != Some(account_key.as_str()) {
                return Err(MutationError::IdentityChanged.into());
            }
            target["workerDefaults"] = worker_defaults;
            target["reviewDefaults"] = review_defaults;
            Ok(save_agent(tx, &target, &validate)?)
        })
        .await;
    }
    if request.mode_settings {
        let expected = request.mode_revision.ok_or(OperationError::Invalid(
            "A nonnegative expected mode revision is required",
        ))?;
        if let Some(expected_account) = request.expected_account.as_deref() {
            let id = request.id.clone();
            let read_validate = std::sync::Arc::clone(&validate);
            let record = context
                .store
                .read_async::<_, OperationError>(
                    "runtime-services/agents_create/read",
                    move |connection| {
                        load_agent_value(connection, &id, &|schema, value| {
                            read_validate(schema, value)
                        })?
                        .ok_or(MutationError::UnknownAgent.into())
                    },
                )
                .await?;
            if record
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
                != expected_account
            {
                return Err(MutationError::Stale.into());
            }
        }
        configure(
            context,
            Configuration {
                id: request.id.clone(),
                concurrency: Some(request.mode_limit.ok_or(OperationError::Invalid(
                    "A subagent concurrency setting is required",
                ))?),
                max_agents: None,
                token_budget: None,
                expected_mode_revision: Some(expected),
                request_id: request.mode_request_id.clone(),
            },
        )
        .await?;
        let id = request.id.clone();
        let read_validate = std::sync::Arc::clone(&validate);
        let record = context
            .store
            .read_async::<_, OperationError>(
                "runtime-services/agents_create/read",
                move |connection| {
                    load_agent_value(connection, &id, &|schema, value| {
                        read_validate(schema, value)
                    })?
                    .ok_or(MutationError::UnknownAgent.into())
                },
            )
            .await?;
        return Ok(record);
    }
    if request.cwd.is_none()
        && request.yolo_mode.is_none()
        && request.model.is_none()
        && request.effort.is_none()
        && request.fast_mode.is_none()
        && request.daybreak_enabled.is_none()
        && request.next_turn.flatten().is_none()
        && !request.worker_settings
    {
        let validate = std::sync::Arc::clone(&validate);
        return mutate(context, move |tx| {
            let validate = |schema: &str, value: &Value| validate(schema, value);
            Ok(update_exclusions(
                tx,
                &request.id,
                request.expected_account.as_deref(),
                request.excluded.as_deref(),
                request.non_defaults,
                |schema, value| validate(schema, value),
            )?)
        })
        .await;
    }
    if request.excluded.is_some() && request.non_defaults {
        return Err(OperationError::Invalid(
            "Save chat account exclusions separately from agent mode settings",
        ));
    }
    let id = request.id.clone();
    let read_validate = std::sync::Arc::clone(&validate);
    let initial = context
        .store
        .read_async::<_, OperationError>("runtime-services/agents_create/read", move |db| {
            load_agent_value(db, &id, &|schema, value| read_validate(schema, value))?
                .ok_or(MutationError::UnknownAgent.into())
        })
        .await?;
    let old_account = initial
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_owned();
    if request
        .expected_account
        .as_deref()
        .is_some_and(|expected| expected != old_account)
    {
        return Err(MutationError::Stale.into());
    }
    let cwd = request
        .cwd
        .as_deref()
        .map(project_directory)
        .transpose()?
        .or_else(|| {
            initial
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .ok_or(OperationError::Internal)?;
    let account = if request.cwd.is_some() {
        account_key(context, None, cwd.clone()).await?
    } else {
        old_account.clone()
    };
    let identity = account_identity(context, &account).await?;
    let provider = identity.provider.key().to_owned();
    let provider_changed =
        initial.get("provider").and_then(Value::as_str) != Some(provider.as_str());
    let execution_requested = request.model.is_some()
        || request.model_present
        || request.effort.is_some()
        || request.effort_present
        || request.fast_mode.is_some()
        || request.fast_mode_present
        || request.daybreak_enabled.is_some()
        || request.daybreak_present;
    let catalog_requested = request.model.is_some()
        || request.effort.as_ref().is_some_and(Option::is_some)
        || request.fast_mode.is_some()
        || request.daybreak_enabled.is_some();
    let base_pending = initial
        .get("pendingSettings")
        .filter(|_| {
            initial
                .get("pendingSettingsAccountKey")
                .and_then(Value::as_str)
                .unwrap_or(&account)
                == account
        })
        .cloned()
        .unwrap_or_else(|| json!({}));
    let model = request
        .model
        .as_deref()
        .or_else(|| {
            (!provider_changed)
                .then(|| base_pending.get("model").and_then(Value::as_str))
                .flatten()
        })
        .or_else(|| {
            (!provider_changed)
                .then(|| initial.get("model").and_then(Value::as_str))
                .flatten()
        })
        .unwrap_or(crate::provider_adapter::default_model(identity.provider));
    let effort = if request.effort_present {
        request.effort.as_ref().and_then(Option::as_deref)
    } else {
        (!provider_changed)
            .then(|| base_pending.get("effort").and_then(Value::as_str))
            .flatten()
            .or_else(|| {
                (!provider_changed)
                    .then(|| initial.get("effort").and_then(Value::as_str))
                    .flatten()
            })
    };
    let fast_mode = request
        .fast_mode
        .or_else(|| {
            (!provider_changed)
                .then(|| base_pending.get("fastMode").and_then(Value::as_bool))
                .flatten()
        })
        .or_else(|| {
            (!provider_changed)
                .then(|| initial.get("fastMode").and_then(Value::as_bool))
                .flatten()
        })
        .unwrap_or(false);
    let daybreak = request
        .daybreak_enabled
        .or_else(|| {
            (!provider_changed)
                .then(|| base_pending.get("daybreakEnabled").and_then(Value::as_bool))
                .flatten()
        })
        .or_else(|| {
            (!provider_changed)
                .then(|| initial.get("daybreakEnabled").and_then(Value::as_bool))
                .flatten()
        })
        .unwrap_or(false);
    let settings = if catalog_requested {
        Some(
            context
                .catalog
                .as_deref()
                .ok_or(OperationError::Unavailable(
                    crate::model_catalog::MODEL_CATALOG_DEPENDENCY,
                ))?
                .resolve(&account, &identity, model, effort, fast_mode, daybreak)?,
        )
    } else if request.effort_present {
        Some(ExecutionSettings {
            model: model.to_owned(),
            effort: None,
            native_effort: initial
                .get("nativeEffort")
                .and_then(Value::as_str)
                .unwrap_or("medium")
                .to_owned(),
            fast_mode,
            daybreak_enabled: daybreak,
            cyber_access_program: initial
                .get("cyberAccessProgram")
                .and_then(Value::as_str)
                .unwrap_or("standard")
                .to_owned(),
        })
    } else {
        None
    };
    let next_turn = request.next_turn.flatten() == Some(true);
    let request_id = request.request_id.flatten();
    if next_turn {
        if request.cwd.is_some()
            || request.yolo_mode.is_some()
            || request.excluded.is_some()
            || request.worker_settings
            || request.mode_settings
        {
            return Err(OperationError::Invalid(
                "Only execution settings can apply to the next turn",
            ));
        }
        let request_id = request_id
            .as_deref()
            .filter(|id| (1..=200).contains(&id.chars().count()))
            .ok_or(OperationError::Invalid("A settings request id is required"))?
            .to_owned();
        let mut receipt_body = json!({
            "operation": "next_turn_settings",
            "agent": request.id,
            "id": request.id,
            "next_turn": true,
            "request_id": request_id,
        });
        for (field, value) in [
            ("model", request.model.as_ref().map(|value| json!(value))),
            ("effort", request.effort.as_ref().map(|value| json!(value))),
            (
                "fast_mode",
                request.fast_mode.as_ref().map(|value| json!(value)),
            ),
            (
                "daybreak_enabled",
                request.daybreak_enabled.as_ref().map(|value| json!(value)),
            ),
            (
                "expected_account_key",
                request.expected_account.as_ref().map(|value| json!(value)),
            ),
        ] {
            if let Some(value) = value {
                receipt_body[field] = value;
            }
        }
        let signature = crate::event_write::request_signature(&receipt_body);
        let validate = std::sync::Arc::clone(&validate);
        return mutate(context, move |tx| {
            let validate = |schema: &str, value: &Value| validate(schema, value);
            let mut target =
                load_agent_value(tx, &request.id, &validate)?.ok_or(MutationError::UnknownAgent)?;
            if target
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
                != old_account
            {
                return Err(MutationError::IdentityChanged.into());
            }
            if let Some(receipt) = EventWriteRepository::receipt(tx, &request_id)? {
                if receipt.signature != signature {
                    return Err(MutationError::Conflict.into());
                }
                return load_agent_value(tx, &request.id, &validate)?
                    .ok_or(MutationError::UnknownAgent.into());
            }
            let resolved = settings.as_ref().ok_or(OperationError::Internal)?;
            target["pendingSettings"] = json!({"model":resolved.model,"effort":resolved.effort,
                "nativeEffort":resolved.native_effort,"fastMode":resolved.fast_mode,
                "daybreakEnabled":resolved.daybreak_enabled,"cyberAccessProgram":resolved.cyber_access_program});
            target["pendingSettingsAccountKey"] = json!(account);
            save_agent(tx, &target, &validate)?;
            EventWriteRepository::put_receipt(tx, &request_id, &signature, "{\"applied\":true}")?;
            Ok(target)
        }).await;
    }
    let validate = std::sync::Arc::clone(&validate);
    mutate(context, move |tx| {
        let validate = |schema: &str, value: &Value| validate(schema, value);
        let mut target =
            load_agent_value(tx, &request.id, &validate)?.ok_or(MutationError::UnknownAgent)?;
        if target
            .get("deletedAt")
            .is_some_and(|value| truthy(Some(value)))
        {
            return Err(MutationError::Deleted.into());
        }
        if target
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            != old_account
        {
            return Err(MutationError::IdentityChanged.into());
        }
        if request.expected_account.as_deref().is_some_and(|expected| {
            target
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
                != expected
        }) {
            return Err(MutationError::Stale.into());
        }
        if (execution_requested || request.cwd.is_some() || request.yolo_mode.is_some())
            && (truthy(target.get("inFlight"))
                || matches!(
                    target.get("status").and_then(Value::as_str),
                    Some("running" | "starting" | "approval")
                ))
        {
            return Err(MutationError::InFlight.into());
        }
        if settings.is_none()
            && request.cwd.is_none()
            && request.yolo_mode.is_none()
            && request.excluded.is_none()
        {
            return Ok(sync_projection(tx, &target)?);
        }
        if let Some(excluded) = request.excluded.as_ref() {
            if target.get("isLead").and_then(Value::as_bool) != Some(true) {
                return Err(MutationError::LeadRequired.into());
            }
            if excluded.iter().any(|value| value == &account) {
                return Err(MutationError::CurrentAccountExcluded.into());
            }
            target["excludedAccountKeys"] = json!(excluded);
        }
        if let Some(yolo) = request.yolo_mode {
            if target.get("isLead").and_then(Value::as_bool) != Some(true) {
                return Err(MutationError::LeadRequired.into());
            }
            target["yoloMode"] = json!(yolo);
        }
        if request.cwd.is_some() {
            if target
                .get("threadId")
                .is_some_and(|thread| !thread.is_null())
            {
                return Err(OperationError::Invalid(
                    "Choose the project before the first message, or create a new chat",
                ));
            }
            target["cwd"] = json!(cwd);
            target["hostProjectPath"] = json!(cwd);
            target["accountKey"] = json!(account);
            target["provider"] = json!(provider);
            ensure_project(tx, &target)?;
            if account != old_account {
                target["daybreakEnabled"] = json!(false);
                target["cyberAccessProgram"] = json!("standard");
                target
                    .as_object_mut()
                    .ok_or(OperationError::Internal)?
                    .remove("pendingSettings");
                target
                    .as_object_mut()
                    .ok_or(OperationError::Internal)?
                    .remove("pendingSettingsAccountKey");
            }
        }
        if let Some(settings) = settings.as_ref() {
            target["model"] = json!(settings.model);
            target["effort"] = json!(settings.effort);
            target["nativeEffort"] = json!(settings.native_effort);
            target["fastMode"] = json!(settings.fast_mode);
            target["daybreakEnabled"] = json!(settings.daybreak_enabled);
            target["cyberAccessProgram"] = json!(settings.cyber_access_program);
            target["executionSettingsAccountKey"] = json!(account);
            target
                .as_object_mut()
                .ok_or(OperationError::Internal)?
                .remove("pendingSettings");
            target
                .as_object_mut()
                .ok_or(OperationError::Internal)?
                .remove("pendingSettingsAccountKey");
        }
        let projection = save_agent(tx, &target, &validate)?;
        if let Some(yolo) = request.yolo_mode {
            update_team_yolo(tx, &request.id, yolo, &validate)?;
        }
        Ok(projection)
    })
    .await
}

fn update_team_yolo(
    tx: &Transaction<'_>,
    root_id: &str,
    yolo: bool,
    validate: &impl Fn(&str, &Value) -> bool,
) -> Result<(), OperationError> {
    let mut statement = tx
        .prepare("SELECT id,record FROM runtime_agents")
        .map_err(StoreError::from)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(StoreError::from)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)?;
    drop(statement);
    let mut team = Vec::new();
    for (id, raw) in rows {
        let record = decode_record(
            tx,
            "runtime_agents",
            "PersistedAgentRecord",
            &id,
            &raw,
            validate,
        )?;
        if record.get("rootId").and_then(Value::as_str) == Some(root_id) {
            team.push((id, record));
        }
    }
    let team_ids = team
        .iter()
        .map(|(id, _)| id.as_str())
        .collect::<std::collections::HashSet<_>>();
    for (_, record) in &team {
        if truthy(record.get("workspaceOperation"))
            || truthy(record.get("inFlight"))
            || matches!(
                record.get("status").and_then(Value::as_str),
                Some("running" | "starting" | "approval")
            )
        {
            return Err(MutationError::InFlight.into());
        }
    }
    for table in ["runtime_monitors", "runtime_tasks"] {
        let query = if table == "runtime_monitors" {
            "SELECT record FROM runtime_monitors WHERE json_extract(record,'$.status') IN ('starting','running','approval')"
        } else {
            "SELECT record FROM runtime_tasks WHERE json_extract(record,'$.status') IN ('running','starting','pending','unknown')"
        };
        let mut statement = tx.prepare(query).map_err(StoreError::from)?;
        let records = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(StoreError::from)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)?;
        if records.iter().any(|raw| {
            serde_json::from_str::<Value>(raw)
                .ok()
                .and_then(|record| {
                    record
                        .get("agent")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .is_some_and(|agent| team_ids.contains(agent.as_str()))
        }) {
            return Err(MutationError::InFlight.into());
        }
    }
    for (id, mut record) in team {
        if id == root_id {
            continue;
        }
        record["yoloMode"] = json!(yolo);
        save_agent(tx, &record, validate)?;
    }
    Ok(())
}
pub struct RenameOperationResult {
    pub response: Value,
    pub replayed: bool,
}

pub async fn rename_operation(
    context: &Context<'_>,
    id: &str,
    name: Option<&str>,
    request_id: Option<&str>,
) -> Result<RenameOperationResult, OperationError> {
    let name = name.map(python_strip);
    if name.is_some_and(str::is_empty) {
        return Err(OperationError::Invalid(
            "A name must have 1 to 80 characters",
        ));
    }
    let signature = rename_signature(id, name);
    if let Some(request_id) = request_id {
        let validate = std::sync::Arc::clone(&context.validate);
        let id = id.to_owned();
        let request_id = request_id.to_owned();
        let signature_for_read = signature.clone();
        let replay = context
            .store
            .read_async::<_, OperationError>("runtime-services/agents_create/read", move |read| {
                load_agent_value(read, &id, &|schema, value| validate(schema, value))?
                    .ok_or(MutationError::UnknownAgent)?;
                if let Some(receipt) = EventWriteRepository::receipt(read, &request_id)? {
                    if receipt.signature != signature_for_read {
                        return Err(MutationError::Conflict.into());
                    }
                    let response = serde_json::from_str(&receipt.result).map_err(|_| {
                        OperationError::Create(MutationError::InvalidStoredObject {
                            table: "runtime_operation_receipts",
                            id: request_id.to_owned(),
                        })
                    })?;
                    return Ok(Some(RenameOperationResult {
                        response,
                        replayed: true,
                    }));
                }
                Ok(None)
            })
            .await?;
        if let Some(replay) = replay {
            return Ok(replay);
        }
    }
    let id = id.to_owned();
    let name = name.map(str::to_owned);
    let request_id = request_id.map(str::to_owned);
    let signature = signature.clone();
    let validate = std::sync::Arc::clone(&context.validate);
    let response = mutate(context, move |tx| {
        Ok(rename(
            tx,
            &id,
            name.as_deref(),
            request_id.as_deref(),
            &signature,
            |schema, value| validate(schema, value),
        )?)
    })
    .await?;
    Ok(RenameOperationResult {
        response,
        replayed: false,
    })
}
fn rename_signature(id: &str, name: Option<&str>) -> String {
    let body = python_json(&json!({"chat": id, "name": name, "operation": "rename"}));
    data_encoding::HEXLOWER.encode(&Sha256::digest(body.as_bytes()))
}

fn previous_lead(
    connection: &Connection,
    id: Option<&str>,
    validate: &impl Fn(&str, &Value) -> bool,
) -> Result<Option<Value>, OperationError> {
    id.filter(|id| !id.is_empty())
        .map(|id| {
            let record =
                load_agent_value(connection, id, validate)?.ok_or(MutationError::UnknownAgent)?;
            if !truthy(record.get("isLead")) {
                return Err(OperationError::Invalid("Select a lead conversation"));
            }
            if truthy(record.get("deletedAt")) {
                return Err(MutationError::Deleted.into());
            }
            Ok(record)
        })
        .transpose()
}

/// The startup account snapshot, or the live registry when a driver is present.
fn account_snapshot(
    context: &Context<'_>,
) -> Result<crate::account_policy::AccountsSnapshot, OperationError> {
    if let Some(driver) = context.driver.as_ref()
        && let Ok(snapshot) = driver.snapshot()
    {
        return Ok(snapshot);
    }
    context
        .accounts
        .map(AccountPolicyState::accounts)
        .ok_or(OperationError::Internal)
}

/// The deepest project whose folder holds `cwd` and whose record satisfies `filter`.
fn project_for_cwd(
    connection: &Connection,
    cwd: &str,
    filter: fn(&Value) -> bool,
) -> Result<Option<Value>, OperationError> {
    let mut statement = connection
        .prepare("SELECT id,record FROM runtime_projects")
        .map_err(|_| OperationError::Internal)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|_| OperationError::Internal)?;
    let mut selected: Option<(usize, Value)> = None;
    for row in rows {
        let (id, raw) = row.map_err(|_| OperationError::Internal)?;
        let invalid = || MutationError::InvalidStoredObject {
            table: "runtime_projects",
            id: id.clone(),
        };
        let record: Value = serde_json::from_str(&raw).map_err(|_| invalid())?;
        if !record.is_object() || record.get("id").and_then(Value::as_str) != Some(id.as_str()) {
            return Err(invalid().into());
        }
        let project_path = record
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        if !matches!(
            record.get("accountKey"),
            None | Some(Value::Null | Value::String(_))
        ) {
            return Err(invalid().into());
        }
        let path = PathBuf::from(project_path);
        if PathBuf::from(cwd).starts_with(&path) && filter(&record) {
            let depth = path.components().count();
            if selected
                .as_ref()
                .is_none_or(|(old_depth, _)| depth > *old_depth)
            {
                selected = Some((depth, record));
            }
        }
    }
    Ok(selected.map(|(_, record)| record))
}

/// The project account of `cwd`, or the server default account.
async fn account_key(
    context: &Context<'_>,
    requested: Option<String>,
    cwd: String,
) -> Result<String, OperationError> {
    if let Some(account) = requested {
        return Ok(account);
    }
    let default = account_snapshot(context)?
        .default_account_key
        .as_str()
        .map(str::to_owned)
        .ok_or(OperationError::Internal)?;
    context
        .store
        .read_async::<_, OperationError>(
            "runtime-services/agents_create/account_key",
            move |connection| {
                let project = project_for_cwd(connection, &cwd, |record| {
                    record.get("accountKey").and_then(Value::as_str).is_some()
                })?;
                Ok(project
                    .and_then(|record| {
                        record
                            .get("accountKey")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or(default))
            },
        )
        .await
}

/// The account of a new chat: the requested account, else the default of the
/// project's account order (see [`crate::account_preference::new_chat_default`]).
async fn new_chat_account(
    context: &Context<'_>,
    requested: Option<String>,
    cwd: String,
    model: Option<String>,
) -> Result<String, OperationError> {
    if let Some(account) = requested {
        return Ok(account);
    }
    let snapshot = account_snapshot(context)?;
    let default = snapshot
        .default_account_key
        .as_str()
        .map(str::to_owned)
        .ok_or(OperationError::Internal)?;
    let project = context
        .store
        .read_async::<_, OperationError>(
            "runtime-services/agents_create/account_key",
            move |connection| project_for_cwd(connection, &cwd, |_| true),
        )
        .await?;
    Ok(crate::account_preference::new_chat_default(
        &snapshot.accounts,
        &default,
        &snapshot.default_account_groups,
        project.as_ref(),
        model.as_deref(),
    )
    .account_key)
}

/// The effort of a new chat whose request names no execution settings.
fn new_chat_effort(provider: studio_runtime_providers::ProviderKind, lead: bool) -> &'static str {
    if lead && provider == studio_runtime_providers::ProviderKind::Claude {
        studio_runtime_schema::shared_values::DEFAULT_CLAUDE_LEAD_EFFORT
    } else {
        "medium"
    }
}

/// The model of a new chat whose request names no model: Claude Opus for a Claude
/// orchestrator, else the provider's default model.
fn new_chat_model(provider: studio_runtime_providers::ProviderKind, lead: bool) -> &'static str {
    if lead && provider == studio_runtime_providers::ProviderKind::Claude {
        studio_runtime_schema::shared_values::DEFAULT_CLAUDE_LEAD_MODEL
    } else {
        crate::provider_adapter::default_model(provider)
    }
}

async fn account_identity(
    context: &Context<'_>,
    account_key: &str,
) -> Result<AccountIdentity, OperationError> {
    let driver = context.driver.clone().ok_or(OperationError::Internal)?;
    let account_key = account_key.to_owned();
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/agents_create/account_identity",
        move || account_identity_blocking(&driver, &account_key).map(|(identity, _, _)| identity),
    )
    .await
    .map_err(|_| OperationError::Internal)?
}

async fn account_identity_with_status(
    context: &Context<'_>,
    account_key: String,
) -> Result<(AccountIdentity, AccountStatus, bool), OperationError> {
    let driver = context.driver.clone().ok_or(OperationError::Internal)?;
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/agents_create/account_identity",
        move || account_identity_blocking(&driver, &account_key),
    )
    .await
    .map_err(|_| OperationError::Internal)?
}

fn account_identity_blocking(
    driver: &AccountDriver,
    account_key: &str,
) -> Result<(AccountIdentity, AccountStatus, bool), OperationError> {
    let status = driver.get(account_key).map_err(|error| match error {
        AccountDriverError::UnknownAccount => OperationError::Invalid("Unknown account"),
        _ => OperationError::Internal,
    })?;
    if status == AccountStatus::Changed {
        return Err(OperationError::Invalid(
            crate::account_driver::CHANGED_ERROR,
        ));
    }
    let snapshot = driver.snapshot().map_err(|_| OperationError::Internal)?;
    let row = snapshot
        .accounts
        .into_iter()
        .find(|row| row.get("id").and_then(Value::as_str) == Some(account_key))
        .ok_or(OperationError::Invalid("Unknown account"))?;
    if truthy(row.get("deleted")) {
        return Err(OperationError::Invalid(
            "This account was deleted. Select another account for new chats",
        ));
    }
    if truthy(row.get("disconnected")) {
        return Err(OperationError::Invalid(
            "Reconnect this account before creating a chat",
        ));
    }
    let string_field = |key: &str| row.get(key).and_then(Value::as_str).map(str::to_owned);
    // AccountDriver snapshots expose the eligibility projection; private
    // registry fields used to derive it are intentionally not in this row.
    // This matches `codex_runtime.py:4843-4935`'s account read and gate.
    let can_attempt_native_proof = can_attempt_native_proof_projection(&row);
    Ok((
        AccountIdentity {
            provider: studio_runtime_providers::ProviderKind::of_record(&row)
                .map_err(|_| OperationError::Invalid("Account provider is unknown"))?,
            home: string_field("home"),
            account_id: string_field("accountId"),
            email: string_field("email"),
        },
        status,
        can_attempt_native_proof,
    ))
}

fn can_attempt_native_proof_projection(row: &Value) -> bool {
    row.get("canAttemptNativeProof").and_then(Value::as_bool) == Some(true)
}

async fn resolve_project_binding(
    context: &Context<'_>,
    cwd: String,
    project_id: Option<String>,
    home_server: Option<String>,
    folder: Option<Value>,
) -> Result<Value, OperationError> {
    let local = context
        .store
        .local_server_id()?
        .ok_or(OperationError::Unavailable(
            "cross-server-driver/local-identity",
        ))?;
    context
        .store
        .read_async(
            "runtime-services/agents_create/project_binding",
            move |connection| {
                resolve_project_binding_from(
                    connection,
                    &cwd,
                    project_id.as_deref(),
                    home_server.as_deref(),
                    &local,
                    folder.as_ref(),
                )
            },
        )
        .await
}

async fn resolve_worker_defaults(
    context: &Context<'_>,
    value: Option<&Value>,
    account_key: &str,
    account: &AccountIdentity,
    root_model: &str,
    excluded_account_keys: &[String],
) -> Result<Value, OperationError> {
    let Some(value) = value else {
        return Ok(
            json!({"model":"gpt-6-luna","effort":"high","fastMode":false,"daybreakEnabled":false,"delegationDepth":1}),
        );
    };
    let object = value
        .as_object()
        .ok_or(OperationError::Invalid("worker_defaults must be an object"))?;
    if object.keys().any(|key| {
        ![
            "model",
            "effort",
            "fast_mode",
            "daybreak_enabled",
            "account_key",
            "delegation_depth",
        ]
        .contains(&key.as_str())
    }) {
        return Err(OperationError::Invalid(
            "worker_defaults has unknown fields",
        ));
    }
    if !["model", "effort", "fast_mode"]
        .iter()
        .all(|field| object.contains_key(*field))
    {
        return Err(OperationError::Invalid(
            "worker_defaults requires model, effort, and fast_mode",
        ));
    }
    let requested_account_key = object.get("account_key").and_then(Value::as_str);
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.trim().is_empty())
        .unwrap_or(root_model);
    let (worker_account_key, worker_account) =
        if let Some(environment) = context.managed_creation_environment {
            environment
                .select_worker_account(crate::provider_callbacks::WorkerAccountRequest {
                    parent_account_key: account_key,
                    requested_account_key,
                    default_account_key: None,
                    model,
                    excluded_account_keys,
                    project_account_keys: None,
                    project_account_groups: None,
                    active_agents: &std::collections::HashMap::new(),
                })
                .map_err(|error| match error {
                    crate::managed_child_create::ManagedCreationEnvironmentError::Invalid(
                        message,
                    ) => OperationError::Invalid(message),
                    crate::managed_child_create::ManagedCreationEnvironmentError::Unavailable(
                        name,
                    ) => OperationError::Unavailable(name),
                })?
        } else {
            let worker_account_key = requested_account_key.unwrap_or(account_key);
            let worker_account = if worker_account_key == account_key {
                account.clone()
            } else {
                account_identity(context, worker_account_key).await?
            };
            (worker_account_key.to_owned(), worker_account)
        };
    let effort = match object.get("effort") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if !value.trim().is_empty() => Some(value.as_str()),
        _ => return Err(OperationError::Invalid("worker_defaults effort is invalid")),
    };
    let fast_mode =
        object
            .get("fast_mode")
            .and_then(Value::as_bool)
            .ok_or(OperationError::Invalid(
                "worker_defaults fast_mode must be a boolean",
            ))?;
    let daybreak = object
        .get("daybreak_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if object
        .get("daybreak_enabled")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err(OperationError::Invalid(
            "worker_defaults daybreak_enabled must be a boolean",
        ));
    }
    let depth = object
        .get("delegation_depth")
        .and_then(Value::as_i64)
        .unwrap_or(1);
    if !(1..=3).contains(&depth) {
        return Err(OperationError::Invalid("delegation_depth must be 1 to 3"));
    }
    let catalog = if let Some(environment) = context.managed_creation_environment {
        Some(environment.catalog())
    } else {
        context.catalog.as_deref()
    }
    .ok_or(OperationError::Unavailable(
        crate::model_catalog::MODEL_CATALOG_DEPENDENCY,
    ))?;
    let settings = catalog.resolve(
        &worker_account_key,
        &worker_account,
        model,
        effort,
        fast_mode,
        daybreak,
    )?;
    let mut defaults = json!({"model":object.get("model").cloned().unwrap_or(Value::Null),
        "effort":settings.effort,"fastMode":settings.fast_mode,"daybreakEnabled":settings.daybreak_enabled,
        "cyberAccessProgram":settings.cyber_access_program,"delegationDepth":depth});
    if requested_account_key.is_some() {
        defaults["accountKey"] = json!(worker_account_key);
    }
    Ok(defaults)
}

async fn resolve_review_defaults(
    context: &Context<'_>,
    value: Option<&Value>,
    account_key: &str,
    account: &AccountIdentity,
    worker_account_key: Option<&str>,
    excluded_account_keys: &[String],
) -> Result<Value, OperationError> {
    let Some(value) = value else {
        return Ok(json!({"model":null,"effort":null}));
    };
    let object = value
        .as_object()
        .ok_or(OperationError::Invalid("review_defaults must be an object"))?;
    if object
        .keys()
        .any(|key| !["model", "effort"].contains(&key.as_str()))
    {
        return Err(OperationError::Invalid(
            "review_defaults has unknown fields",
        ));
    }
    let Some(model) = object.get("model").and_then(Value::as_str) else {
        if object.get("effort").is_some_and(|effort| !effort.is_null()) {
            return Err(OperationError::Invalid(
                "Select a review model before setting its effort",
            ));
        }
        return Ok(json!({"model":null,"effort":null}));
    };
    if !model.starts_with("gpt-") {
        return Err(OperationError::Invalid("Native review needs a Codex model"));
    }
    let effort = match object.get("effort") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if !value.trim().is_empty() => Some(value.as_str()),
        _ => return Err(OperationError::Invalid("review_defaults effort is invalid")),
    };
    let (catalog_account_key, catalog_account) =
        if let Some(environment) = context.managed_creation_environment {
            environment
                .select_worker_account(crate::provider_callbacks::WorkerAccountRequest {
                    parent_account_key: account_key,
                    requested_account_key: worker_account_key,
                    default_account_key: None,
                    model,
                    excluded_account_keys,
                    project_account_keys: None,
                    project_account_groups: None,
                    active_agents: &std::collections::HashMap::new(),
                })
                .map_err(|error| match error {
                    crate::managed_child_create::ManagedCreationEnvironmentError::Invalid(
                        message,
                    ) => OperationError::Invalid(message),
                    crate::managed_child_create::ManagedCreationEnvironmentError::Unavailable(
                        name,
                    ) => OperationError::Unavailable(name),
                })?
        } else if let Some(worker_account_key) = worker_account_key {
            (
                worker_account_key.to_owned(),
                account_identity(context, worker_account_key).await?,
            )
        } else {
            (account_key.to_owned(), account.clone())
        };
    let catalog = if let Some(environment) = context.managed_creation_environment {
        Some(environment.catalog())
    } else {
        context.catalog.as_deref()
    }
    .ok_or(OperationError::Unavailable(
        crate::model_catalog::MODEL_CATALOG_DEPENDENCY,
    ))?;
    let settings = catalog.resolve(
        &catalog_account_key,
        &catalog_account,
        model,
        effort,
        false,
        false,
    )?;
    Ok(json!({"model":settings.model,"effort":settings.effort}))
}

fn resolve_project_binding_from(
    connection: &Connection,
    cwd: &str,
    project_id: Option<&str>,
    home_server: Option<&str>,
    local: &str,
    folder: Option<&Value>,
) -> Result<Value, OperationError> {
    let project_id = project_id
        .filter(|value| !value.is_empty())
        .ok_or(OperationError::Invalid("A project ID is required"))?;
    let home_server = home_server
        .filter(|value| !value.is_empty())
        .unwrap_or(local);
    if home_server != local {
        let paired: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_access_clients WHERE id=?1 AND json_extract(record,'$.kind')='server' AND json_extract(record,'$.status')='paired')",
            [home_server],
            |row| row.get(0),
        ).map_err(StoreError::from)?;
        if !paired {
            return Err(OperationError::Forbidden(
                "The project server is not paired or is revoked",
            ));
        }
    }
    let projects = studio_runtime_store::ProjectsRepository::list(connection)?;
    for (_, project) in projects {
        let direct = project.get("id").and_then(Value::as_str) == Some(project_id)
            && project.get("homeServerId").and_then(Value::as_str) == Some(home_server)
            && project
                .get("locations")
                .and_then(Value::as_array)
                .is_some_and(|rows| {
                    rows.iter().any(|row| {
                        row.get("serverId").and_then(Value::as_str) == Some(local)
                            && row.get("path").and_then(Value::as_str) == Some(cwd)
                    })
                });
        let alias = project.get("path").and_then(Value::as_str) == Some(cwd)
            && project
                .get("projectAliases")
                .and_then(Value::as_array)
                .is_some_and(|rows| {
                    rows.iter().any(|row| {
                        row.get("serverId").and_then(Value::as_str) == Some(home_server)
                            && row.get("projectId").and_then(Value::as_str) == Some(project_id)
                    })
                });
        if direct || alias {
            let folder_id = match folder {
                None | Some(Value::Null) => Value::Null,
                Some(Value::String(id)) => {
                    if !project
                        .get("folders")
                        .and_then(Value::as_array)
                        .is_some_and(|folders| {
                            folders
                                .iter()
                                .any(|item| item.get("id").and_then(Value::as_str) == Some(id))
                        })
                    {
                        return Err(OperationError::Invalid(
                            "This folder is not in the selected project",
                        ));
                    }
                    json!(id)
                }
                Some(_) => return Err(OperationError::Invalid("Select a project folder")),
            };
            return Ok(
                json!({"cwd":cwd,"projectId":project_id,"projectServerId":home_server,"localServerId":local,"projectFolder":folder_id}),
            );
        }
    }
    Err(OperationError::Invalid(
        "The selected folder is not a location of this project",
    ))
}

fn project_directory_default() -> Result<String, OperationError> {
    let path = std::env::var_os("CODEX_CANVAS_CWD")
        .map(PathBuf::from)
        .map_or_else(std::env::current_dir, Ok)
        .map_err(|_| OperationError::Internal)?;
    project_directory(&path.to_string_lossy())
}
pub(crate) fn project_directory(value: &str) -> Result<String, OperationError> {
    let path = if value == "~" || value.starts_with("~/") {
        std::env::var_os("HOME").map_or_else(
            || PathBuf::from(value),
            |home| {
                let home = PathBuf::from(home);
                if value == "~" {
                    home
                } else {
                    home.join(&value[2..])
                }
            },
        )
    } else {
        PathBuf::from(value)
    };
    studio_runtime_store::path_identity::CanonicalPath::existing_utf8_directory(&path)
        .map_err(|_| OperationError::Invalid("Select an existing project directory"))
        .map(|path| path.to_string_lossy().into_owned())
}

fn lead_signature(body: &Value, workspace_mode: &str, cwd: Option<&str>) -> String {
    let mut settings = body.clone();
    if let Some(fields) = settings.as_object_mut() {
        fields.remove("id");
        fields.entry("model").or_insert(Value::Null);
        fields.entry("previous").or_insert(Value::Null);
        fields
            .entry("workspaceMode")
            .or_insert(json!(workspace_mode));
    }
    if let Some(cwd) = cwd {
        settings["cwd"] = json!(cwd);
    }
    python_json(&settings)
}

fn reusable_previous_workspace(prior: &Value, workspace: &str, cwd: &str) -> bool {
    workspace != "image" || prior.get("imageWorkspaceRepo").and_then(Value::as_str) == Some(cwd)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_agent_record(
    id: &str,
    name: &str,
    prompt: &str,
    cwd: &str,
    account: &str,
    provider: ProviderKind,
    yolo: bool,
    role: &str,
    is_lead: bool,
    status: &str,
    workspace_mode: &str,
    concurrency: Option<i64>,
    max_agents: i64,
    token_budget: Value,
    draft: bool,
    created: f64,
    quick_create_signature: Option<String>,
) -> Value {
    let mut record = merge_records([
        json!({
          "id":id,"threadId":null,"accountKey":account,"executionSettingsAccountKey":account,
          "provider":provider.key(),"yoloMode":yolo,"name":name,"prompt":prompt,"cwd":cwd,
          "role":role,"isLead":is_lead,"needsTitle":draft,"parentId":null,"rootId":id,
          "model":crate::provider_adapter::default_model(provider),"effort":"medium",
          "fastMode":false,"daybreakEnabled":false,"cyberAccessProgram":"standard",
          "maxAgents":max_agents,"tokenBudget":token_budget,"status":status,"autoWake":true,
          "epoch":0,"turnId":null,"inFlight":false,"turnEpoch":0,"usageResumeEnabled":true,
          "tokensUsed":0,"compactions":0,"compactionsObservedOnly":false,"contextUsage":null,
          "events":0,"created":created,"error":null,"profileId":null,"profileInstructions":""
        }),
        json!({
          "tail":"","worktree":false,"workspaceMode":workspace_mode,"executionMode":"native",
          "hostProjectPath":cwd,"layrReady":false,"workspaceInGit":false,"worktreeReady":false,
          "environment":"host","imageWorkspace":false,"imageWorkspaceReady":false,
          "imageWorkspacePhase":null,"imageWorkspaceRepo":null,"imageWorkspaceSubpath":".",
          "imageWorkspaceBaseRef":null,"imageWorkspaceHasGit":false,"imageWorkspaceCreatedAt":null,
          "imageWorkspaceError":null,"workspaceBackend":null,"workspaceModeExplicit":false,
          "worktreeWarning":null,"workerBaseRef":null,"workerBaseCommit":null,
          "workerBaseBehindMain":null,"workerBaseMainRef":null
        }),
    ]);
    if let Some(concurrency) = concurrency {
        record["concurrency"] = json!(concurrency);
    }
    if is_lead {
        record["maxAgentsExplicit"] = json!(false);
        record["workerDefaults"] = json!({"model":"gpt-6-luna","effort":"high","fastMode":false,"daybreakEnabled":false,"delegationDepth":1});
        record["reviewDefaults"] = json!({"model":null,"effort":null});
    }
    if draft {
        record["quickCreate"] = json!(true);
        record["quickCreateRequest"] = json!(quick_create_signature);
    }
    record
}

fn merge_records<const N: usize>(parts: [Value; N]) -> Value {
    let mut merged = serde_json::Map::new();
    for part in parts {
        if let Some(fields) = part.as_object() {
            merged.extend(fields.clone());
        }
    }
    Value::Object(merged)
}

async fn mutate(
    context: &Context<'_>,
    operation: impl FnOnce(&Transaction<'_>) -> Result<Value, OperationError> + Send + 'static,
) -> Result<Value, OperationError> {
    context
        .store
        .transaction_with_error(move |tx| {
            let before = SyncDocumentsRepository::entity_sequence(tx)?;
            let result = operation(tx)?;
            Ok(crate::sync_documents::with_entity_delta(
                tx, before, result,
            )?)
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_worker_create::CreationCatalog as _;

    #[test]
    fn default_workspace_mode_follows_platform_defaults() {
        assert_eq!(default_workspace_mode(false), DEFAULT_WORKSPACE_MODE);
        let lead = if cfg!(target_os = "macos") {
            MACOS_LEAD_WORKSPACE_MODE
        } else {
            DEFAULT_WORKSPACE_MODE
        };
        assert_eq!(default_workspace_mode(true), lead);
    }

    #[test]
    fn limit_refusals_name_the_owned_bounds() {
        let (concurrency, team) = (SUBAGENT_CONCURRENCY, TEAM_AGENTS);
        let concurrency = format!("{} to {}", concurrency.start(), concurrency.end());
        let team = format!("{} to {}", team.start(), team.end());
        assert_eq!(
            CREATE_LIMITS_REFUSAL,
            format!("Concurrency must be {concurrency} and team size must be {team}")
        );
        assert!(TEAM_SIZE_REFUSAL.starts_with(&format!("Team size must be {team} ")));
        assert!(CONCURRENCY_REFUSAL.ends_with(&format!("from {concurrency}")));
    }

    struct CatalogFixture {
        models: std::collections::BTreeMap<String, Vec<String>>,
    }

    impl crate::host_worker_create::CreationCatalog for CatalogFixture {
        fn resolve(
            &self,
            account_key: &str,
            account: &AccountIdentity,
            model: &str,
            effort: Option<&str>,
            fast_mode: bool,
            daybreak_enabled: bool,
        ) -> Result<ExecutionSettings, CreateError> {
            if !self
                .models
                .get(account_key)
                .is_some_and(|models| models.iter().any(|candidate| candidate == model))
            {
                return Err(CreateError::ModelUnavailable);
            }
            if daybreak_enabled && account.provider != studio_runtime_providers::ProviderKind::Codex
            {
                return Err(CreateError::InvalidCreation("daybreak_enabled"));
            }
            Ok(ExecutionSettings {
                model: model.to_owned(),
                effort: effort.map(str::to_owned),
                native_effort: effort.unwrap_or("medium").to_owned(),
                fast_mode,
                daybreak_enabled,
                cyber_access_program: if daybreak_enabled {
                    "daybreakBlue"
                } else {
                    "standard"
                }
                .to_owned(),
            })
        }
    }

    struct AccountFixture {
        listing: crate::provider_callbacks::ManagedCreationAccountListing,
    }

    impl crate::provider_callbacks::ManagedCreationAccountRead for AccountFixture {
        fn worker_accounts(
            &self,
        ) -> Result<
            crate::provider_callbacks::ManagedCreationAccountListing,
            crate::provider_callbacks::ManagedCreationAccountError,
        > {
            Ok(self.listing.clone())
        }

        fn resolve_identity(
            &self,
            account_key: &str,
            _parent_account_key: &str,
        ) -> Result<
            crate::provider_callbacks::ManagedAccountIdentity,
            crate::provider_callbacks::ManagedCreationAccountError,
        > {
            self.listing
                .accounts
                .iter()
                .find(|candidate| candidate.account_key == account_key)
                .map(|candidate| candidate.identity.clone())
                .ok_or(crate::provider_callbacks::ManagedCreationAccountError::Unknown)
        }
    }

    struct ManagedFixture {
        accounts: AccountFixture,
        catalog: CatalogFixture,
    }

    impl crate::managed_child_create::ManagedCreationEnvironment for ManagedFixture {
        fn select_worker_account(
            &self,
            request: crate::provider_callbacks::WorkerAccountRequest<'_>,
        ) -> Result<
            (String, AccountIdentity),
            crate::managed_child_create::ManagedCreationEnvironmentError,
        > {
            let mut available = |key: &str,
                                 identity: &crate::provider_callbacks::ManagedAccountIdentity,
                                 model: &str| {
                let account = AccountIdentity {
                    provider: identity.provider,
                    home: identity.home.clone(),
                    account_id: identity.account_id.clone(),
                    email: identity.email.clone(),
                };
                match self
                    .catalog
                    .resolve(key, &account, model, None, false, false)
                {
                    Ok(_) => Ok(true),
                    Err(CreateError::ModelUnavailable | CreateError::InvalidCreation(_)) => {
                        Ok(false)
                    }
                    Err(CreateError::CapabilityUnavailable(name)) => Err(
                        crate::managed_child_create::ManagedCreationEnvironmentError::Unavailable(
                            name,
                        ),
                    ),
                    Err(_) => Err(
                        crate::managed_child_create::ManagedCreationEnvironmentError::Invalid(
                            "Worker account admission was refused",
                        ),
                    ),
                }
            };
            let (key, identity) = crate::provider_callbacks::select_worker_account(
                crate::provider_callbacks::WorkerAccountSelection {
                    accounts: &self.accounts,
                    model_available: &mut available,
                    request,
                },
            )?;
            Ok((
                key,
                AccountIdentity {
                    provider: identity.provider,
                    home: identity.home,
                    account_id: identity.account_id,
                    email: identity.email,
                },
            ))
        }

        fn validate_worker_choice<'a>(
            &'a self,
            _store: &'a RuntimeStore,
            _account_key: &'a str,
            _account: &'a AccountIdentity,
            _requested: &'a str,
            _default: &'a str,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            (),
                            crate::managed_child_create::ManagedCreationEnvironmentError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async move { Ok(()) })
        }

        fn catalog(&self) -> &dyn crate::host_worker_create::CreationCatalog {
            &self.catalog
        }

        fn prepare_workspace(
            &self,
            _store: &RuntimeStore,
            _actor: &AgentRecord,
            _args: &Value,
            _request_id: &str,
        ) -> Result<
            Box<dyn crate::monitor_runtime::WorkspaceAdmission + Send + Sync>,
            crate::managed_child_create::ManagedCreationEnvironmentError,
        > {
            Err(
                crate::managed_child_create::ManagedCreationEnvironmentError::Unavailable(
                    "workspace-state/host-preparation",
                ),
            )
        }
    }

    fn managed_fixture() -> ManagedFixture {
        use crate::provider_callbacks::{
            ManagedAccountIdentity, ManagedCreationAccountCandidate as Candidate,
            ManagedCreationAccountListing,
        };
        let candidate = |key: &str, provider: &str, ready: bool| Candidate {
            account_key: key.to_owned(),
            identity: ManagedAccountIdentity {
                provider: provider.parse().expect("test provider"),
                home: Some(format!("/profiles/{key}")),
                account_id: None,
                email: None,
            },
            ready,
            can_attempt_native_proof: false,
            disconnected: false,
            deleted: false,
            duplicate: false,
            max_running_turns: studio_runtime_schema::shared_values::ACCOUNT_RUNNING_TURNS_MAX,
            group: None,
        };
        ManagedFixture {
            accounts: AccountFixture {
                listing: ManagedCreationAccountListing {
                    accounts: vec![
                        candidate("claude-local", "claude", true),
                        candidate("default", "codex", true),
                    ],
                    default_account_groups: Vec::new(),
                },
            },
            catalog: CatalogFixture {
                models: std::collections::BTreeMap::from([
                    (
                        "claude-local".to_owned(),
                        vec!["default".to_owned(), "opus".to_owned()],
                    ),
                    (
                        "default".to_owned(),
                        vec![
                            "gpt-6-luna".to_owned(),
                            "gpt-6.1-sol".to_owned(),
                            "gpt-6-astra".to_owned(),
                        ],
                    ),
                ]),
            },
        }
    }

    fn worker_defaults_context<'a>(
        store: std::sync::Arc<RuntimeStore>,
        environment: &'a dyn crate::managed_child_create::ManagedCreationEnvironment,
        validate: PersistedRecordValidator,
    ) -> Context<'a> {
        Context {
            store,
            accounts: None,
            driver: None,
            catalog: None,
            managed_creation_environment: Some(environment),
            validate,
        }
    }

    #[tokio::test]
    async fn managed_worker_settings_use_ordered_account_selection_and_keep_automatic_unpinned() {
        let root = tempfile::tempdir().expect("isolated settings state");
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("state opens");
        let environment = managed_fixture();
        let validate = |_: &str, _: &Value| true;
        let context = worker_defaults_context(
            std::sync::Arc::new(store),
            &environment,
            std::sync::Arc::new(validate),
        );
        let claude = AccountIdentity {
            provider: studio_runtime_providers::ProviderKind::Claude,
            home: Some("/profiles/claude-local".to_owned()),
            account_id: None,
            email: None,
        };
        let automatic_luna = json!({
            "account_key": null,
            "model": "gpt-6-luna",
            "effort": "high",
            "fast_mode": false
        });
        let resolved = resolve_worker_defaults(
            &context,
            Some(&automatic_luna),
            "claude-local",
            &claude,
            "default",
            &[],
        )
        .await
        .expect("automatic Luna resolves through the eligible Codex account");
        assert_eq!(resolved["model"], "gpt-6-luna");
        assert_eq!(
            resolved.get("accountKey"),
            None,
            "automatic choice stays unpinned"
        );

        let explicit_claude_luna = json!({
            "account_key": "claude-local",
            "model": "gpt-6-luna",
            "effort": "high",
            "fast_mode": false
        });
        assert!(matches!(
            resolve_worker_defaults(
                &context,
                Some(&explicit_claude_luna),
                "claude-local",
                &claude,
                "default",
                &[],
            )
            .await,
            Err(OperationError::Invalid(_))
        ));

        let cleared_pin = json!({
            "account_key": null,
            "model": null,
            "effort": null,
            "fast_mode": false
        });
        let cleared = resolve_worker_defaults(
            &context,
            Some(&cleared_pin),
            "claude-local",
            &claude,
            "gpt-6-astra",
            &[],
        )
        .await
        .expect("null model inherits the current lead model");
        assert_eq!(cleared["model"], Value::Null);
        assert_eq!(
            cleared.get("accountKey"),
            None,
            "null clears an old account pin"
        );
        assert_eq!(cleared["cyberAccessProgram"], "standard");
    }

    #[tokio::test]
    async fn lead_worker_and_review_models_validate_on_the_explicit_codex_worker_account() {
        let root = tempfile::tempdir().expect("isolated create state");
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("state opens");
        let environment = managed_fixture();
        let validate = |_: &str, _: &Value| true;
        let context = worker_defaults_context(
            std::sync::Arc::new(store),
            &environment,
            std::sync::Arc::new(validate),
        );
        let claude = AccountIdentity {
            provider: studio_runtime_providers::ProviderKind::Claude,
            home: Some("/profiles/claude-local".to_owned()),
            account_id: None,
            email: None,
        };
        let payload = json!({
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
        let workers = resolve_worker_defaults(
            &context,
            Some(&payload["worker_defaults"]),
            "claude-local",
            &claude,
            "default",
            &[],
        )
        .await
        .expect("Codex worker model validates");
        assert_eq!(workers["accountKey"], "default");
        assert_eq!(workers["model"], "gpt-6-luna");
        let review = resolve_review_defaults(
            &context,
            Some(&payload["review_defaults"]),
            "claude-local",
            &claude,
            payload
                .pointer("/worker_defaults/account_key")
                .and_then(Value::as_str),
            &[],
        )
        .await
        .expect("review model validates against the worker's Codex catalog");
        assert_eq!(review, json!({"model":"gpt-6.1-sol","effort":"high"}));
        assert_eq!(payload["account_key"], "claude-local");
        assert_eq!(payload["model"], "default");
    }

    #[tokio::test]
    async fn image_provision_failure_persists_a_visible_paused_phase() {
        let root = tempfile::tempdir().expect("temporary state root creates");
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("runtime store opens");
        let mut value = build_agent_record(
            "image-worker",
            "Worker",
            "Task",
            "/workspace",
            "default",
            ProviderKind::Codex,
            false,
            "implementer",
            false,
            "running",
            "image",
            None,
            DEFAULT_MAX_TEAM_AGENTS,
            Value::Null,
            false,
            1.0,
            None,
        );
        value["epoch"] = json!(4);
        set_image_workspace_fields(&mut value, "/workspace");
        let record = AgentRecord::from_value(value).expect("image agent record validates");
        store
            .transaction(move |tx| studio_runtime_store::AgentRepository::put(tx, &record))
            .await
            .expect("image intent stores");

        persist_image_provision_failure(&store, "image-worker", 4, "ASIF mount failed")
            .await
            .expect("failure phase stores");
        let read = store.test_reader().expect("saved failure reads");
        let saved = studio_runtime_store::AgentRepository::get(
            &read,
            &studio_runtime_store::AgentId::from("image-worker"),
        )
        .expect("agent lookup succeeds")
        .expect("failed agent remains visible")
        .into_value();
        assert_eq!(saved["imageWorkspacePhase"], "failed");
        assert_eq!(saved["imageWorkspaceError"], "ASIF mount failed");
        assert_eq!(saved["status"], "paused");
        assert_eq!(saved["autoWake"], false);
        assert!(
            saved["error"]
                .as_str()
                .is_some_and(|error| { error.contains("requested image workspace failed") })
        );
    }

    #[test]
    fn image_lead_reuse_requires_the_same_repository() {
        let prior = json!({
            "id":"reused","cwd":"/fake-mount/project-a",
            "workspaceMode":"image","imageWorkspace":true,
            "imageWorkspaceReady":true,"imageWorkspacePhase":"ready",
            "imageWorkspaceRepo":"/project-a"
        });
        assert!(!reusable_previous_workspace(&prior, "image", "/project-b"));
        assert!(reusable_previous_workspace(&prior, "image", "/project-a"));

        let mut same_repository = prior;
        set_image_workspace_fields(&mut same_repository, "/project-a");
        assert_eq!(same_repository["imageWorkspaceRepo"], "/project-a");
        assert_eq!(same_repository["imageWorkspaceReady"], false);
        assert_eq!(same_repository["imageWorkspacePhase"], "creating");
    }

    #[test]
    fn vm_child_recheck_freezes_host_project_path_not_guest_cwd() {
        let parent = json!({
            "cwd":"/guest/project/lines/main",
            "executionMode":"vm",
            "hostProjectPath":"/host/project"
        });
        let root = json!({"cwd":"/host/project","hostProjectPath":"/host/project"});
        assert_eq!(
            parent_project_path(&parent, &root, "/guest/project/lines/main"),
            "/host/project"
        );
    }

    #[test]
    fn vm_child_loads_project_binding_from_frozen_host_project_path() {
        let connection = rusqlite::Connection::open_in_memory().expect("open project fixture");
        connection
            .execute(
                "CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT NOT NULL)",
                [],
            )
            .expect("project table");
        let project = json!({"id":"host-project","path":"/host/project"});
        connection
            .execute(
                "INSERT INTO runtime_projects(id,record) VALUES(?1,?2)",
                ("/host/project", project.to_string()),
            )
            .expect("save host project");

        let parent = json!({
            "cwd":"/guest/project/lines/main",
            "executionMode":"vm",
            "hostProjectPath":"/host/project"
        });
        let root = json!({"cwd":"/host/project","hostProjectPath":"/host/project"});
        assert_eq!(
            parent_project_binding(&connection, &parent, &root)
                .expect("load frozen project binding")
                .expect("host project exists"),
            project
        );
    }

    #[test]
    fn image_replay_resumes_pending_inherited_provisioning_from_persisted_mode() {
        let pending = json!({
            "epoch":7,
            "deletedAt":null,
            "workspaceMode":"image",
            "imageWorkspace":true,
            "imageWorkspaceReady":false,
            "imageWorkspacePhase":"creating",
            "status":"running"
        });
        let ready = json!({
            "epoch":7,
            "deletedAt":null,
            "workspaceMode":"image",
            "imageWorkspace":true,
            "imageWorkspaceReady":true
        });
        let archived = json!({
            "epoch":7,
            "deletedAt":null,
            "workspaceMode":"image",
            "imageWorkspace":true,
            "imageWorkspaceReady":false,
            "imageWorkspacePhase":"archived"
        });
        let non_image = json!({"workspaceMode":"worktree", "imageWorkspace":false});
        assert!(should_resume_image_provisioning(&pending));
        assert!(!should_resume_image_provisioning(&ready));
        assert!(!should_resume_image_provisioning(&archived));
        let terminal = json!({"epoch":7,"deletedAt":null,"workspaceMode":"image",
            "imageWorkspace":true,"imageWorkspaceReady":false,
            "imageWorkspacePhase":"creating","status":"completed"});
        assert!(!should_resume_image_provisioning(&terminal));
        assert!(!should_resume_image_provisioning(&non_image));
    }

    #[test]
    fn image_provision_completion_rejects_changed_tombstoned_or_terminal_agents() {
        let pending = json!({"epoch":7,"deletedAt":null,"imageWorkspace":true,
            "imageWorkspaceReady":false,"imageWorkspacePhase":"creating","status":"running"});
        assert!(image_provision_is_current(&pending, 7));
        assert!(!image_provision_is_current(&pending, 8));
        let mut deleted = pending.clone();
        deleted["deletedAt"] = json!(123);
        assert!(!image_provision_is_current(&deleted, 7));
        let mut terminal = pending;
        terminal["status"] = json!("completed");
        assert!(!image_provision_is_current(&terminal, 7));
    }

    #[test]
    fn lead_project_folder_must_belong_to_selected_project() {
        let connection = rusqlite::Connection::open_in_memory().expect("open project fixture");
        connection
            .execute(
                "CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT NOT NULL)",
                [],
            )
            .expect("project table");
        connection
            .execute(
                "CREATE TABLE runtime_access_clients(id TEXT PRIMARY KEY,record TEXT NOT NULL)",
                [],
            )
            .expect("paired clients table");
        let project = json!({
            "id":"project:example",
            "path":"project:example",
            "name":"Example",
            "homeServerId":"local-server",
            "locations":[{"serverId":"local-server","path":"/work/example"}],
            "folders":[{"id":"folder-a","name":"Nested","parentId":null}]
        });
        connection
            .execute(
                "INSERT INTO runtime_projects(id,record) VALUES(?1,?2)",
                ("project:example", project.to_string()),
            )
            .expect("save local project");
        let alias = json!({
            "id":"/work/alias",
            "path":"/work/alias",
            "name":"Alias",
            "homeServerId":"local-server",
            "locations":[{"serverId":"local-server","path":"/work/alias"}],
            "projectAliases":[{"serverId":"home-server","projectId":"project:remote","name":"Remote"}],
            "folders":[{"id":"remote-folder","name":"Nested","parentId":null}]
        });
        connection
            .execute(
                "INSERT INTO runtime_projects(id,record) VALUES(?1,?2)",
                ("/work/alias", alias.to_string()),
            )
            .expect("save remote project alias");
        connection
            .execute(
                "INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2)",
                (
                    "home-server",
                    json!({"id":"home-server","kind":"server","status":"paired"}).to_string(),
                ),
            )
            .expect("paired home server");
        let valid = resolve_project_binding_from(
            &connection,
            "/work/example",
            Some("project:example"),
            Some("local-server"),
            "local-server",
            Some(&json!("folder-a")),
        )
        .expect("accept folder in selected project");
        assert_eq!(valid["projectFolder"], "folder-a");
        let remote = resolve_project_binding_from(
            &connection,
            "/work/alias",
            Some("project:remote"),
            Some("home-server"),
            "local-server",
            Some(&json!("remote-folder")),
        )
        .expect("accept registered remote project alias");
        assert_eq!(remote["projectServerId"], "home-server");
        assert!(matches!(
            resolve_project_binding_from(
                &connection,
                "/work/alias",
                Some("project:remote"),
                Some("revoked-server"),
                "local-server",
                None,
            ),
            Err(OperationError::Forbidden(
                "The project server is not paired or is revoked"
            ))
        ));
        assert!(matches!(
            resolve_project_binding_from(
                &connection,
                "/work/example",
                Some("project:example"),
                Some("local-server"),
                "local-server",
                Some(&json!("another-project-folder")),
            ),
            Err(OperationError::Invalid(
                "This folder is not in the selected project"
            ))
        ));
    }

    #[test]
    fn native_proof_admission_uses_the_public_account_projection() {
        assert!(can_attempt_native_proof_projection(
            &json!({"id":"claude-account","canAttemptNativeProof":true})
        ));
        assert!(!can_attempt_native_proof_projection(
            &json!({"id":"claude-account","_authErrorKind":"keychain"})
        ));
    }

    #[tokio::test]
    async fn conversation_catalog_account_is_not_the_child_worker_default() {
        let root = tempfile::tempdir().expect("temporary state");
        let schema: Value = serde_json::from_str(include_str!(
            "../../../runtime-store/src/runtime-state-schema.json"
        ))
        .expect("schema");
        for name in ["canvas.sqlite3", "analytics.sqlite3"] {
            let connection = Connection::open(root.path().join(name)).expect("database");
            connection
                .execute_batch("PRAGMA journal_mode=WAL;")
                .expect("WAL");
            let mut entries = schema[name]["sqlite_master"]
                .as_array()
                .expect("schema entries")
                .clone();
            entries.sort_by_key(|entry| match entry["type"].as_str() {
                Some("table") => 0,
                Some("index") => 1,
                Some("view") => 2,
                _ => 3,
            });
            for entry in entries {
                let exists: bool = connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
                        [entry["name"].as_str().expect("object name")],
                        |row| row.get(0),
                    )
                    .expect("schema object lookup");
                if let Some(sql) = entry["sql"].as_str().filter(|_| !exists) {
                    connection.execute_batch(sql).expect("schema statement");
                }
            }
        }
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("runtime store");
        let record = json!({
            "id":"conversation",
            "accountKey":"conversation-account",
            "rootId":"conversation",
            "workerDefaults":{"accountKey":"worker-default-account"}
        });
        store
            .transaction(move |tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                    rusqlite::params!["conversation", record.to_string()],
                )?;
                Ok(())
            })
            .await
            .expect("seed conversation");
        let validate = |_: &str, _: &Value| true;
        let context = Context {
            store: std::sync::Arc::new(store),
            accounts: None,
            driver: None,
            catalog: None,
            managed_creation_environment: None,
            validate: std::sync::Arc::new(validate),
        };
        let request = Conversation {
            id: "conversation".to_owned(),
            expected_account: None,
            excluded: None,
            cwd: None,
            yolo_mode: None,
            model: None,
            model_present: false,
            effort: None,
            effort_present: false,
            fast_mode: None,
            fast_mode_present: false,
            daybreak_enabled: None,
            daybreak_present: false,
            next_turn: None,
            request_id: None,
            mode_settings: false,
            mode_limit: None,
            mode_revision: None,
            mode_request_id: None,
            worker_settings: false,
            worker_defaults: None,
            review_defaults: None,
            non_defaults: false,
        };
        assert_eq!(
            conversation_account_key(&context, &request.id, None, None)
                .await
                .expect("conversation account"),
            "conversation-account"
        );
    }

    #[tokio::test]
    async fn account_admission_releases_snapshot_before_project_account_lookup() {
        let state = tempfile::tempdir().expect("temporary state");
        let project = tempfile::tempdir().expect("existing project directory");
        // A registered project stores its canonical path; the macOS temporary
        // folder is under the `/var` link.
        let project_path = studio_runtime_store::path_identity::resolve(project.path())
            .expect("resolved project directory")
            .to_string_lossy()
            .into_owned();
        let store = RuntimeStore::open(Some(state.path()))
            .await
            .expect("runtime store");
        let project_path_for_write = project_path.clone();
        store
            .transaction(move |tx| {
                tx.execute(
                    "INSERT INTO runtime_projects(id,record) VALUES(?1,?2)",
                    rusqlite::params![
                        project_path_for_write,
                        json!({
                            "id": project_path_for_write,
                            "path": project_path_for_write,
                            "accountKey": "project-account"
                        })
                        .to_string()
                    ],
                )?;
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                    rusqlite::params![
                        "conversation",
                        json!({"id":"conversation","accountKey":"old-account"}).to_string()
                    ],
                )?;
                Ok(())
            })
            .await
            .expect("seed project and conversation");
        let accounts = AccountPolicyState::from_startup(
            Some(
                br#"{"version":1,"defaultAccountKey":"default","accounts":{"default":{"id":"default","home":"/default","label":"Default","source":"Codex CLI","status":"signedOut"}},"logins":{},"deleteReceipts":{}}"#,
            ),
            [],
        )
        .expect("valid account policy");
        let validate = |_: &str, _: &Value| true;
        let context = Context {
            store: std::sync::Arc::new(store),
            accounts: Some(&accounts),
            driver: None,
            catalog: None,
            managed_creation_environment: None,
            validate: std::sync::Arc::new(validate),
        };

        let creation = Creation {
            cwd: Some(project_path.clone()),
            ..Creation::default()
        };
        assert_eq!(
            creation_account_key(&context, &creation, true)
                .await
                .expect("creation account"),
            "project-account"
        );
        assert_eq!(
            conversation_account_key(&context, "conversation", None, Some(&project_path))
                .await
                .expect("conversation account"),
            "project-account"
        );
    }

    /// A new chat prefers the project's account order and a Claude orchestrator;
    /// a task agent and an existing chat keep the project account.
    #[tokio::test]
    async fn new_chat_account_follows_the_project_order_and_prefers_claude() {
        let state = tempfile::tempdir().expect("temporary state");
        let project = tempfile::tempdir().expect("existing project directory");
        let project_path = studio_runtime_store::path_identity::resolve(project.path())
            .expect("resolved project directory")
            .to_string_lossy()
            .into_owned();
        let store = RuntimeStore::open(Some(state.path()))
            .await
            .expect("runtime store");
        let write_path = project_path.clone();
        store
            .transaction(move |tx| {
                tx.execute(
                    "INSERT INTO runtime_projects(id,record) VALUES(?1,?2)",
                    rusqlite::params![
                        write_path,
                        json!({
                            "id": write_path,
                            "path": write_path,
                            "accountKey": "codex-work",
                            "accountGroups": ["work", "own"]
                        })
                        .to_string()
                    ],
                )?;
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                    rusqlite::params![
                        "conversation",
                        json!({"id":"conversation","accountKey":"claude-own"}).to_string()
                    ],
                )?;
                Ok(())
            })
            .await
            .expect("seed project and conversation");
        // Claude profiles name their own config folders, so the credential watch
        // stays inside the fixture.
        let config = |name: &str| state.path().join(name).to_string_lossy().into_owned();
        let registry = json!({"version":1,"defaultAccountKey":"codex-own","accounts":{
            "codex-own":{"id":"codex-own","home":config("a"),"label":"A","source":"Codex CLI","status":"ready"},
            "codex-work":{"id":"codex-work","home":config("b"),"label":"B","source":"Codex CLI","status":"ready"},
            "claude-own":{"id":"claude-own","home":config("c"),"label":"C","source":"Claude Code","provider":"claude","status":"ready","claudeOptions":{"configDir":config("c")}},
            "claude-work":{"id":"claude-work","home":config("d"),"label":"D","source":"Claude Code","provider":"claude","status":"ready","claudeOptions":{"configDir":config("d")}}
        },"logins":{},"deleteReceipts":{}});
        let registry_path = state.path().join("accounts/registry.json");
        std::fs::create_dir_all(registry_path.parent().expect("registry parent"))
            .expect("registry folder");
        std::fs::write(&registry_path, registry.to_string()).expect("write registry");
        std::fs::write(
            registry_path.with_file_name(crate::account_preference::ACCOUNT_ORDER_FILE),
            json!({"version":1,"defaultAccountGroups":["own"],"groups":{
                "codex-own":"own","codex-work":"work","claude-own":"own","claude-work":"work"}})
            .to_string(),
        )
        .expect("write account order");
        let driver = std::sync::Arc::new(
            crate::account_driver::AccountDriver::open(&registry_path).expect("open accounts"),
        );
        let validate = |_: &str, _: &Value| true;
        let context = Context {
            store: std::sync::Arc::new(store),
            accounts: None,
            driver: Some(driver),
            catalog: None,
            managed_creation_environment: None,
            validate: std::sync::Arc::new(validate),
        };
        let creation = |model: Option<&str>| Creation {
            cwd: Some(project_path.clone()),
            intent: model.map_or_else(|| json!({}), |model| json!({"model": model})),
            ..Creation::default()
        };
        let choose = |request: Creation, lead: bool| {
            let context = &context;
            async move {
                creation_account_key(context, &request, lead)
                    .await
                    .expect("creation account")
            }
        };
        // The project order puts "work" first, and a Claude account exists.
        assert_eq!(choose(creation(None), true).await, "claude-work");
        // A requested Codex model picks the first Codex account of the order.
        assert_eq!(
            choose(creation(Some("gpt-6-astra")), true).await,
            "codex-work"
        );
        // An explicit account stays the choice.
        let mut explicit = creation(None);
        explicit.account = Some("claude-own".to_owned());
        assert_eq!(choose(explicit, true).await, "claude-own");
        // A task agent (not a new chat) keeps the project account.
        assert_eq!(choose(creation(None), false).await, "codex-work");
        // Moving an existing chat to the project keeps the project account rule.
        assert_eq!(
            conversation_account_key(&context, "conversation", None, Some(&project_path))
                .await
                .expect("conversation account"),
            "codex-work"
        );
        assert_eq!(
            new_chat_model(ProviderKind::Claude, true),
            studio_runtime_schema::shared_values::DEFAULT_CLAUDE_LEAD_MODEL
        );
        assert_eq!(new_chat_effort(ProviderKind::Claude, true), "medium");
        assert_eq!(
            new_chat_model(ProviderKind::Codex, true),
            DEFAULT_LEAD_MODEL
        );
        assert_eq!(
            new_chat_model(ProviderKind::Claude, false),
            crate::provider_adapter::default_model(ProviderKind::Claude)
        );
    }

    #[test]
    fn explicit_shared_workspace_gives_an_implementer_the_parent_directory() {
        assert_eq!(
            child_role_workspace(None, Some("shared"), "worktree", false)
                .expect("shared implementer"),
            ("implementer", "shared", false),
        );
    }

    #[test]
    fn reviewer_child_defaults_to_shared_workspace() {
        assert_eq!(
            child_role_workspace(Some("reviewer"), None, "image", false)
                .expect("host reviewer defaults to shared workspace"),
            ("reviewer", "shared", false),
        );
        assert_eq!(
            child_role_workspace(None, None, "image", false)
                .expect("image implementer inherits image workspace"),
            ("implementer", "image", false),
        );
        assert!(matches!(
            child_role_workspace(Some("orchestrator"), None, "worktree", false),
            Err(OperationError::Invalid("Invalid agent role"))
        ));
        let mut record = json!({"id":"worker","cwd":"/repo"});
        set_image_workspace_fields(&mut record, "/repo");
        assert_eq!(record["workspaceMode"], "image");
        assert_eq!(record["imageWorkspace"], true);
        assert_eq!(record["imageWorkspaceRepo"], "/repo");
        let mut vm = json!({"id":"vm-worker","cwd":"/guest/project/lines/main"});
        set_vm_workspace_fields(&mut vm, "/host/project");
        assert_eq!(vm["workspaceMode"], "layr");
        assert_eq!(vm["executionMode"], "vm");
        assert_eq!(vm["environment"], "linux");
        assert_eq!(vm["hostProjectPath"], "/host/project");
        assert_eq!(vm["cwd"], "/guest/project/lines/main");
        assert_eq!(vm["worktree"], false);
        assert_eq!(
            child_role_workspace(None, None, "layr", true)
                .expect("VM implementer inherits layr workspace"),
            ("implementer", "layr", false)
        );
        assert!(matches!(
            validate_vm_parent_for_create("layr", &json!({"executionMode":"native"})),
            Err(OperationError::Invalid(
                "A VM workspace requires a VM parent"
            ))
        ));
        assert!(validate_vm_parent_for_create("layr", &json!({"executionMode":"vm"})).is_ok());
        assert_eq!(
            child_role_workspace(Some("reviewer"), None, "layr", true)
                .expect("VM reviewer inherits layr workspace"),
            ("reviewer", "layr", false)
        );
        assert!(matches!(
            child_role_workspace(Some("reviewer"), Some("shared"), "layr", true),
            Err(OperationError::Invalid(
                "A VM reviewer must use the parent's layr workspace"
            ))
        ));
        assert!(
            validate_vm_parent_for_create("worktree", &json!({"executionMode":"native"})).is_ok()
        );
    }

    fn snapshot(store: &RuntimeStore) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
        // The controlled interleaving validator runs while the operation's
        // pooled read lease is live. Observe its committed writer transaction
        // through an independent test connection instead of checking out a
        // nested pooled lease.
        let connection = rusqlite::Connection::open(store.root().join("canvas.sqlite3"))
            .expect("snapshot connection");
        let mut tables = connection
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .expect("tables");
        tables
            .query_map([], |row| row.get::<_, String>(0))
            .expect("table rows")
            .map(|name| {
                let mut rows = connection
                    .prepare(&format!("SELECT * FROM \"{}\"", name.expect("table")))
                    .expect("read table");
                let columns = rows.column_count();
                rows.query_map([], |row| (0..columns).map(|index| row.get(index)).collect())
                    .expect("rows")
                    .collect::<Result<Vec<_>, _>>()
                    .expect("snapshot")
            })
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn creation_rechecks_replay_and_previous_eligibility_after_controlled_interleaving() {
        let directory = tempfile::tempdir().expect("owned test directory");
        let root = directory.path().to_path_buf();
        std::fs::create_dir_all(&root).expect("temporary state");
        let schema: Value = serde_json::from_str(include_str!(
            "../../../runtime-store/src/runtime-state-schema.json"
        ))
        .expect("schema");
        for name in ["canvas.sqlite3", "analytics.sqlite3"] {
            let connection = Connection::open(root.join(name)).expect("database");
            connection
                .execute_batch("PRAGMA journal_mode=WAL;")
                .expect("WAL");
            let mut entries = schema[name]["sqlite_master"]
                .as_array()
                .expect("schema entries")
                .clone();
            entries.sort_by_key(|entry| match entry["type"].as_str() {
                Some("table") => 0,
                Some("index") => 1,
                Some("view") => 2,
                _ => 3,
            });
            for entry in entries {
                let exists: bool = connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
                        [entry["name"].as_str().expect("object name")],
                        |row| row.get(0),
                    )
                    .expect("schema object lookup");
                if let Some(sql) = entry["sql"].as_str().filter(|_| !exists) {
                    connection.execute_batch(sql).expect("schema statement");
                }
            }
        }
        let store = std::sync::Arc::new(
            RuntimeStore::open(Some(&root))
                .await
                .expect("runtime store"),
        );
        let registry_path = root.join("accounts/registry.json");
        std::fs::create_dir_all(registry_path.parent().expect("registry parent"))
            .expect("create registry directory");
        std::fs::write(
            &registry_path,
            json!({
                "version":1,
                "defaultAccountKey":"default",
                "accounts":{"default":{"id":"default","home":root,"label":"Codex","source":"Codex CLI","status":"signedOut"}},
                "logins":{},
                "deleteReceipts":{}
            })
            .to_string(),
        )
        .expect("write fixture account registry");
        let driver = std::sync::Arc::new(
            crate::account_driver::AccountDriver::open(&registry_path)
                .expect("open fixture account driver"),
        );
        for scenario in ["agent", "lead", "previous"] {
            let intent = if scenario == "agent" {
                json!({"cwd":root,"name":"Lead","prompt":"Task"})
            } else {
                json!({"cwd":root})
            };
            let mut record = json!({"id":scenario,"isLead":true,"cwd":root,"status":"idle"});
            record["creationIntent"] = intent.clone();
            record["quickCreate"] = json!(true);
            record["quickCreateRequest"] =
                json!(lead_signature(&intent, "worktree", root.to_str()));
            let seed_record = record.clone();
            let scenario_id = scenario.to_owned();
            store
                .transaction(move |tx| {
                    tx.execute(
                        "INSERT INTO runtime_agents VALUES(?1,?2)",
                        rusqlite::params![scenario_id, seed_record.to_string()],
                    )?;
                    Ok(())
                })
                .await
                .expect("seed existing agent");
            const INTERLEAVING_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
            let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let observed_tx = std::sync::Mutex::new(Some(observed_tx));
            let release_rx = std::sync::Mutex::new(release_rx);
            let first_validation = std::sync::atomic::AtomicBool::new(true);
            let validate = std::sync::Arc::new(move |schema: &str, _: &Value| {
                if schema == "PersistedAgentRecord"
                    && first_validation.swap(false, std::sync::atomic::Ordering::AcqRel)
                {
                    if let Some(signal) = observed_tx.lock().ok().and_then(|mut tx| tx.take())
                        && signal.send(()).is_err()
                    {
                        return false;
                    }
                    if release_rx
                        .lock()
                        .ok()
                        .is_none_or(|rx| rx.recv_timeout(INTERLEAVING_DEADLINE).is_err())
                    {
                        return false;
                    }
                }
                true
            });
            let context = Context {
                store: std::sync::Arc::clone(&store),
                accounts: None,
                driver: Some(std::sync::Arc::clone(&driver)),
                catalog: None,
                managed_creation_environment: None,
                validate,
            };
            let writer_store = std::sync::Arc::clone(&store);
            let writer_scenario = scenario.to_owned();
            let writer_record = record.clone();
            let writer = tokio::spawn(async move {
                struct ReleaseOnDrop {
                    release: std::sync::mpsc::Sender<()>,
                }
                impl Drop for ReleaseOnDrop {
                    fn drop(&mut self) {
                        if self.release.send(()).is_err() {
                            // Receiver closure means no callback remains blocked.
                        }
                    }
                }
                let release_on_drop = ReleaseOnDrop {
                    release: release_tx,
                };
                tokio::time::timeout(INTERLEAVING_DEADLINE, observed_rx)
                    .await
                    .map_err(|_| "validation signal deadline elapsed")?
                    .map_err(|_| "validation signal was lost")?;
                let mut changed = writer_record;
                match writer_scenario.as_str() {
                    "agent" => changed["creationIntent"]["prompt"] = json!("Another request"),
                    "lead" => changed["quickCreateRequest"] = json!("{}"),
                    _ => changed["deletedAt"] = json!(1),
                }
                let result = writer_store
                    .transaction(move |tx| {
                        tx.execute(
                            "UPDATE runtime_agents SET record=?1 WHERE id=?2",
                            rusqlite::params![changed.to_string(), writer_scenario],
                        )?;
                        Ok(())
                    })
                    .await;
                drop(release_on_drop);
                result.map_err(|error| error.to_string())
            });
            let request = Creation {
                id: Some(
                    if scenario == "previous" {
                        "new-agent"
                    } else {
                        scenario
                    }
                    .to_owned(),
                ),
                cwd: root.to_str().map(str::to_owned),
                workspace: Some("worktree".to_owned()),
                account: Some("default".to_owned()),
                prompt: if scenario == "agent" { "Task" } else { "" }.to_owned(),
                previous: (scenario == "previous").then(|| scenario.to_owned()),
                reuse_empty: Some(false),
                intent,
                ..Creation::default()
            };
            let result = if scenario == "agent" {
                agent(&context, request).await
            } else {
                lead(&context, request).await
            };
            tokio::time::timeout(INTERLEAVING_DEADLINE * 2, writer)
                .await
                .expect("controlled writer completes before the test deadline")
                .expect("controlled writer task joins")
                .expect("interleaved writer commits");
            let after_interleaving = snapshot(&store);
            assert!(
                matches!(
                    result,
                    Err(OperationError::Create(
                        MutationError::Conflict | MutationError::Deleted
                    ))
                ),
                "{scenario}: {result:?}"
            );
            assert_eq!(
                Some(snapshot(&store)),
                Some(after_interleaving),
                "refusal writes nothing after {scenario} interleaving"
            );
        }
        drop(store);
    }
}
