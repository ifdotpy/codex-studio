//! Synchronous orchestration of persisted agent operations.
use super::*;
use crate::account_driver::{AccountDriver, AccountDriverError, AccountStatus};
use crate::account_policy::AccountPolicyState;
use crate::event_write::{python_json, python_strip, truthy};
use crate::host_worker_create::{AccountIdentity, ExecutionSettings};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use studio_runtime_schema::agent_limits_generated::{
    DEFAULT_MAX_TEAM_AGENTS, DEFAULT_SUBAGENT_CONCURRENCY, TEAM_AGENT_HEADROOM,
};
use studio_runtime_store::RuntimeStore;

pub struct Context<'a> {
    pub store: &'a RuntimeStore,
    pub accounts: Option<&'a AccountPolicyState>,
    pub driver: Option<&'a AccountDriver>,
    pub catalog: Option<&'a dyn crate::host_worker_create::CreationCatalog>,
    pub validate: &'a dyn Fn(&str, &Value) -> bool,
}

#[derive(Debug, Error)]
pub enum OperationError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("Not implemented: requires {0}")]
    Unavailable(&'static str),
    #[error(transparent)]
    Create(#[from] CreateError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("Internal server error")]
    Internal,
}

#[derive(Default)]
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

pub(crate) const DEFAULT_LEAD_MODEL: &str = "gpt-6-astra";

pub fn lead(context: &Context<'_>, request: Creation) -> Result<Value, OperationError> {
    create(context, request, true)
}
pub fn agent(context: &Context<'_>, request: Creation) -> Result<Value, OperationError> {
    create(context, request, false)
}
fn create(
    context: &Context<'_>,
    mut request: Creation,
    draft: bool,
) -> Result<Value, OperationError> {
    let id = request.id.clone().map_or_else(new_id, Ok)?;
    let workspace = request
        .workspace
        .as_deref()
        .unwrap_or(if draft && cfg!(target_os = "macos") {
            "layr"
        } else {
            "worktree"
        });
    let requested_cwd = request.cwd.as_deref().map(project_directory).transpose()?;
    if !draft {
        // Import source is part of the durable creation identity. Keep it in
        // the intent compared by replay_agent, including the transaction's
        // concurrent recheck, rather than relying on an earlier route read.
        if let Some(imported_from) = &request.imported_from {
            request
                .intent
                .as_object_mut()
                .ok_or(OperationError::Internal)?
                .insert("imported_from".to_owned(), json!(imported_from));
        }
        request
            .intent
            .as_object_mut()
            .ok_or(OperationError::Internal)?
            .remove("id");
        request.intent["prompt"] = json!(python_strip(&request.prompt));
        request.intent["name"] = json!(python_strip(request.name.as_deref().unwrap_or("Lead")));
        if let Some(cwd) = &requested_cwd {
            request.intent["cwd"] = json!(cwd);
        }
    }
    let signature = lead_signature(&request.intent, workspace, requested_cwd.as_deref());
    let read = context.store.read()?;
    let validate = |schema: &str, value: &Value| (context.validate)(schema, value);
    let intent = request.intent.clone();
    let replay = |connection: &Connection| {
        if draft {
            replay_lead(connection, &id, &signature, validate)
        } else {
            replay_agent(connection, &id, &intent, validate)
        }
    };
    let existing = replay(&read)?;
    drop(read);
    if existing.is_some() {
        let replayed = context.store.transaction_with_error(|tx| {
            let current = replay(tx)?.ok_or(CreateError::Conflict)?;
            Ok::<Value, OperationError>(sync_projection(tx, &current)?)
        })?;
        #[cfg(target_os = "macos")]
        if workspace == "image" {
            provision_image_workspace(&replayed, &id)?;
        }
        return Ok(replayed);
    }
    let read = context.store.read()?;
    let previous = previous_lead(&read, request.previous.as_deref(), &validate)?;
    let profile = request
        .profile
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(|id| {
            load_record(&read, "runtime_profiles", "ProfileRecord", id, &validate)?
                .ok_or(OperationError::Invalid("Unknown worker profile"))
        })
        .transpose()?;
    if let Some(profile) = &profile {
        if request.role.is_none() {
            request.role = profile
                .get("role")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        request.catalog |= ["model", "effort"].iter().any(|field| {
            !request
                .intent
                .as_object()
                .is_some_and(|body| body.contains_key(*field))
                && profile.get(*field).is_some_and(|value| !value.is_null())
        });
    }
    drop(read);
    if request.parent.is_some() {
        if request.worker_catalog {
            return Err(OperationError::Invalid(
                "Only a lead can change worker defaults",
            ));
        }
        return create_child(context, &request, &id, &intent, &validate);
    }
    if workspace != "worktree" {
        return Err(OperationError::Unavailable(match workspace {
            "image" => "workspace-state/image-workspace",
            "layr" => "workspace-state/vm-workspace",
            _ => return Err(OperationError::Invalid("Unknown workspace mode")),
        }));
    }
    if !draft && request.cwd.as_deref().is_none_or(str::is_empty) {
        return Err(OperationError::Invalid(
            "Select an existing project directory",
        ));
    }
    let cwd = requested_cwd.map_or_else(
        || {
            if let Some(prior) = &previous {
                project_directory(
                    prior
                        .get("hostProjectPath")
                        .filter(|value| !value.is_null())
                        .or_else(|| prior.get("cwd"))
                        .and_then(Value::as_str)
                        .ok_or(OperationError::Internal)?,
                )
            } else {
                project_directory_default()
            }
        },
        Ok,
    )?;
    if request
        .intent
        .get("project_folder")
        .is_some_and(|value| !value.is_null())
    {
        return Err(OperationError::Unavailable(
            "project-driver/folder-selection",
        ));
    }
    let binding = if request.project_binding {
        Some(resolve_project_binding(
            context,
            &cwd,
            request.intent.get("project_id").and_then(Value::as_str),
            request
                .intent
                .get("project_server_id")
                .and_then(Value::as_str),
        )?)
    } else {
        None
    };
    let account = account_key(context, request.account.as_deref(), &cwd)?;
    let identity = account_identity(context, &account)?;
    let provider = identity.provider.as_deref().unwrap_or("codex");
    let settings = if request.catalog || request.worker_catalog {
        let model = request
            .intent
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.trim().is_empty())
            .unwrap_or(if provider == "claude" {
                "default"
            } else {
                DEFAULT_LEAD_MODEL
            });
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
        context
            .catalog
            .ok_or(OperationError::Unavailable("provider-session/model-list"))?
            .resolve(
                &account,
                &identity,
                model,
                effort,
                fast_mode,
                daybreak_enabled,
            )?
    } else {
        ExecutionSettings {
            model: if provider == "claude" {
                "default".to_owned()
            } else {
                DEFAULT_LEAD_MODEL.to_owned()
            },
            effort: Some("medium".to_owned()),
            native_effort: "medium".to_owned(),
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
    if !(0..=512).contains(&concurrency) || !(1..=1024).contains(&requested_max_agents) {
        return Err(OperationError::Invalid(
            "Concurrency must be 0 to 512 and team size must be 1 to 1024",
        ));
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
    let prompt = python_strip(&request.prompt);
    let created = crate::event_write::unix_time().map_err(|_| OperationError::Internal)?;
    let (worker_defaults, review_defaults) = if role == "orchestrator" {
        (
            Some(resolve_worker_defaults(
                context,
                request.intent.get("worker_defaults"),
                &account,
                &identity,
                &settings.model,
            )?),
            Some(resolve_review_defaults(
                context,
                request.intent.get("review_defaults"),
                &account,
                &identity,
            )?),
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
        prompt,
        &cwd,
        &account,
        provider,
        yolo,
        role,
        role == "orchestrator",
        if draft { "idle" } else { "queued" },
        workspace,
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
    mutate(context, |tx| {
        if let Some(existing) = replay(tx)? {
            return Ok(sync_projection(tx, &existing)?);
        }
        if let Some(prior) = previous_lead(tx, request.previous.as_deref(), &validate)?
            && request.reuse_empty.unwrap_or(true)
            && draft
            && !reuse_execution_fields
            && !request.worker_catalog
            && !truthy(prior.get("layrReady"))
            && prior
                .get("workspaceMode")
                .and_then(Value::as_str)
                .unwrap_or("worktree")
                == workspace
            && prior
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or("codex")
                == provider
            && crate::sync_entity_context::agent_view_context(tx, &prior, None)?.empty_lead
        {
            let mut reused = prior;
            let previous_account = reused
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
                .to_owned();
            if reused.get("provider").and_then(Value::as_str) != Some(provider) {
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
            } else if let Some(fields) = reused.as_object_mut() {
                fields.remove("projectId");
                fields.remove("projectServerId");
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
            ensure_project(tx, &reused)?;
            save_agent(tx, &reused, &validate)?;
            tx.execute(
                "INSERT INTO runtime_lead_requests(id,agent,signature) VALUES(?1,?2,?3)",
                rusqlite::params![id, reused["id"].as_str(), signature],
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
            )?;
            if current != *binding {
                return Err(CreateError::IdentityChanged.into());
            }
        }
        Ok(super::create_agent_with_identity(
            tx, &id, prompt, record, created, validate,
        )?)
    })
}

fn create_child(
    context: &Context<'_>,
    request: &Creation,
    id: &str,
    intent: &Value,
    validate: &impl Fn(&str, &Value) -> bool,
) -> Result<Value, OperationError> {
    let parent_id = request.parent.as_deref().ok_or(OperationError::Internal)?;
    let (parent, root, project_binding) = {
        let db = context.store.read()?;
        let parent =
            load_agent_value(&db, parent_id, validate)?.ok_or(MutationError::UnknownAgent)?;
        if truthy(parent.get("deletedAt")) {
            return Err(MutationError::Deleted.into());
        }
        let root_id = parent
            .get("rootId")
            .and_then(Value::as_str)
            .ok_or(OperationError::Internal)?;
        let root = load_agent_value(&db, root_id, validate)?.ok_or(MutationError::UnknownAgent)?;
        let cwd = parent
            .get("cwd")
            .and_then(Value::as_str)
            .ok_or(OperationError::Internal)?;
        let project =
            studio_runtime_store::ProjectsRepository::get(&db, cwd)?.map(|(_, project)| project);
        (parent, root, project)
    };
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
    let (role, worktree) =
        child_role_workspace(request.role.as_deref(), request.workspace.as_deref())?;
    let cwd = parent
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or(OperationError::Internal)?;
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
    let identity = account_identity(context, account)?;
    if defaults
        .get("accountKey")
        .and_then(Value::as_str)
        .is_some_and(|selected| selected != account)
    {
        return Err(MutationError::IdentityChanged.into());
    }
    let provider = identity.provider.as_deref().unwrap_or("codex");
    let model = intent
        .get("model")
        .and_then(Value::as_str)
        .or_else(|| defaults.get("model").and_then(Value::as_str))
        .or_else(|| root.get("model").and_then(Value::as_str))
        .unwrap_or(if provider == "claude" {
            "default"
        } else {
            DEFAULT_LEAD_MODEL
        });
    let effort = intent
        .get("effort")
        .and_then(Value::as_str)
        .or_else(|| defaults.get("effort").and_then(Value::as_str));
    let fast_mode = intent
        .get("fast_mode")
        .and_then(Value::as_bool)
        .or_else(|| defaults.get("fastMode").and_then(Value::as_bool))
        .unwrap_or(false);
    let daybreak = intent
        .get("daybreak_enabled")
        .and_then(Value::as_bool)
        .or_else(|| defaults.get("daybreakEnabled").and_then(Value::as_bool))
        .unwrap_or(false);
    let catalog = context
        .catalog
        .ok_or(OperationError::Unavailable("provider-session/model-list"))?;
    let (base_ref, base_commit) = if worktree {
        crate::host_worker_create::freeze_base(PathBuf::from(cwd).as_path())?
    } else {
        (String::new(), String::new())
    };
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
    let parent_account_key = parent
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let draft = crate::host_worker_create::prepare_work_item(
        &body,
        PathBuf::from(cwd).as_path(),
        studio_runtime_store::AgentId::from(id),
        studio_runtime_store::AgentId::from(root_agent),
        studio_runtime_store::AgentId::from(parent_id),
        &base_ref,
        &base_commit,
        epoch,
        parent_cwd,
        parent_account_key,
        parent.get("projectId"),
        parent.get("projectServerId"),
        project_binding.as_ref(),
        &identity,
        catalog,
        role,
        worktree,
        fast_mode,
        daybreak,
    )?;
    let checked = crate::host_worker_create::persisted_record(&draft)?;
    let mut record = checked.value();
    record["creationIntent"] = intent.clone();
    record["provider"] = json!(provider);
    record["yoloMode"] = root.get("yoloMode").cloned().unwrap_or(json!(true));
    record["maxAgents"] = root
        .get("maxAgents")
        .cloned()
        .unwrap_or(json!(DEFAULT_MAX_TEAM_AGENTS));
    record["tokenBudget"] = root.get("tokenBudget").cloned().unwrap_or(Value::Null);
    let created = record
        .get("created")
        .and_then(Value::as_f64)
        .ok_or(OperationError::Internal)?;
    let prompt = request.prompt.trim();
    let task_id = intent
        .get("task_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let result = mutate(context, |tx| {
        crate::host_worker_create::recheck_worker(tx, &draft, epoch, &identity)?;
        let result = super::create_agent_with_identity(tx, id, prompt, record, created, validate)?;
        if let Some(task_id) = &task_id {
            let actor = studio_runtime_store::AgentRepository::get(
                tx,
                &studio_runtime_store::AgentId::from(parent_id),
            )?
            .ok_or(OperationError::Internal)?;
            let now = crate::event_write::unix_time().map_err(|_| OperationError::Internal)?;
            crate::work_assignment_runtime::assign_in_transaction(tx, task_id, id, &actor, now)
                .map_err(|_| OperationError::Invalid("Work item assignment was rejected"))?;
        }
        Ok(result)
    })?;
    #[cfg(target_os = "macos")]
    if workspace == "image" {
        provision_image_workspace(&result, id)?;
    }
    Ok(result)
}

/// Match Runtime.create's child defaults: implementers use the selected
/// workspace, while reviewer children default to a shared, non-worktree cwd.
/// Both roles still inherit the root's execution settings; provider permission
/// preparation applies the reviewer's read-only policy when YOLO is disabled.
fn child_role_workspace<'a>(
    role: Option<&'a str>,
    requested_workspace: Option<&'a str>,
) -> Result<(&'a str, bool), OperationError> {
    let role = role.unwrap_or("implementer");
    if !matches!(role, "implementer" | "reviewer") {
        return Err(OperationError::Invalid("Invalid agent role"));
    }
    let workspace = requested_workspace.unwrap_or(if role == "reviewer" {
        "shared"
    } else {
        "worktree"
    });
    let worktree = match workspace {
        "worktree" => true,
        "shared" => false,
        "image" => {
            return Err(OperationError::Unavailable(
                "workspace-state/image-workspace",
            ));
        }
        "layr" => return Err(OperationError::Unavailable("workspace-state/vm-workspace")),
        _ => return Err(OperationError::Invalid("Unknown workspace mode")),
    };
    Ok((role, worktree))
}

/// Record image intent so readiness can remount the durable ASIF state. On macOS,
/// provisioning is queued like Python's `codex_runtime.py:5757` base-build callback;
/// Python provisions with `create_workspace` at line 5878 after the base is ready.
#[cfg(target_os = "macos")]
fn set_image_workspace_fields(record: &mut Value, root: &str) {
    record["workspaceMode"] = json!("image");
    record["imageWorkspace"] = json!(true);
    record["imageWorkspaceReady"] = json!(false);
    record["imageWorkspacePhase"] = json!("creating");
    record["imageWorkspaceRepo"] = json!(root);
    record["workspaceBackend"] = json!("asif");
}

#[cfg(target_os = "macos")]
fn provision_image_workspace(result: &Value, fallback_id: &str) -> Result<(), OperationError> {
    let id = result
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(fallback_id);
    let root = result
        .get("imageWorkspaceRepo")
        .or_else(|| result.get("cwd"))
        .and_then(Value::as_str)
        .ok_or(OperationError::Internal)?;
    crate::image_workspace::start_workspace_provision(PathBuf::from(root).as_path(), id)
        .map(|_| ())
        .map_err(|error| OperationError::Workspace(error.to_string()))
}

pub fn configure(context: &Context<'_>, request: Configuration) -> Result<Value, OperationError> {
    let validate = |schema: &str, value: &Value| (context.validate)(schema, value);
    if request
        .max_agents
        .is_some_and(|max_agents| !(1..=1024).contains(&max_agents))
        || request
            .token_budget
            .is_some_and(|budget| budget.is_some_and(|budget| budget <= 0))
    {
        return Err(OperationError::Invalid(
            "Team size must be 1 to 1024 and token budget must be positive or null",
        ));
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
            ))?;
        if !(0..=512).contains(&limit) {
            return Err(OperationError::Invalid(
                "Subagent concurrency must be an integer from 0 to 512",
            ));
        }
        let signature = crate::event_write::request_signature(&json!({
            "operation":"subagent_concurrency","agent":request.id,
            "concurrency":limit,"expectedRevision":revision
        }));
        return mutate(context, |tx| {
            let mut root =
                load_agent_value(tx, &request.id, &validate)?.ok_or(MutationError::UnknownAgent)?;
            if root.get("isLead").and_then(Value::as_bool) != Some(true)
                || root.get("rootId").and_then(Value::as_str) != Some(request.id.as_str())
            {
                return Err(MutationError::LeadRequired.into());
            }
            if let Some(receipt) = EventWriteRepository::receipt(tx, request_id)? {
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
            let prior_max = root.get("maxAgents").and_then(Value::as_i64).unwrap_or(64);
            if current != limit {
                root["concurrency"] = json!(limit);
                root["agentModeRevision"] = json!(revision + 1);
                root["agentModeChangedAt"] =
                    json!(crate::event_write::unix_time().map_err(|_| MutationError::Internal)?);
                root["agentModeChangedBy"] = json!("user");
            }
            if root.get("maxAgentsExplicit").and_then(Value::as_bool) != Some(true) {
                root["maxAgents"] = json!(prior_max.max(limit + 2));
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
                request_id,
                &signature,
                &serde_json::to_string(&result).map_err(StoreError::from)?,
            )?;
            Ok(result)
        });
    }
    if request.expected_mode_revision.is_some() || request.request_id.is_some() {
        return Err(OperationError::Invalid(
            "Configure only team storage and token limits",
        ));
    }
    let max_agents = request.max_agents;
    mutate(context, |tx| {
        let result = configure_limits(
            tx,
            &request.id,
            max_agents,
            request.token_budget,
            |schema, value| (context.validate)(schema, value),
        )?;
        if !(context.validate)("ConfigureResponse", &result) {
            return Err(MutationError::InvalidStoredObject {
                table: "runtime_agents",
                id: request.id.clone(),
            }
            .into());
        }
        Ok(result)
    })
}
pub fn conversation(context: &Context<'_>, request: Conversation) -> Result<Value, OperationError> {
    let validate = |schema: &str, value: &Value| (context.validate)(schema, value);
    if (request.model_present && request.model.is_none())
        || (request.fast_mode_present && request.fast_mode.is_none())
    {
        return Err(OperationError::Invalid(
            "Model must be available and fast_mode must be a boolean",
        ));
    }
    if request.worker_settings {
        let initial = {
            let db = context.store.read()?;
            load_agent_value(&db, &request.id, &validate)?.ok_or(MutationError::UnknownAgent)?
        };
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
        let identity = account_identity(context, account_key)?;
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
            )?,
            None => initial
                .get("workerDefaults")
                .cloned()
                .unwrap_or_else(|| json!({"model":"gpt-6-luna","effort":"high","fastMode":false,"daybreakEnabled":false,"delegationDepth":1})),
        };
        let review_defaults = match request.review_defaults.as_ref() {
            Some(value) => resolve_review_defaults(context, Some(value), account_key, &identity)?,
            None => initial
                .get("reviewDefaults")
                .cloned()
                .unwrap_or(json!({"model":null,"effort":null})),
        };
        return mutate(context, |tx| {
            let mut target =
                load_agent_value(tx, &request.id, &validate)?.ok_or(MutationError::UnknownAgent)?;
            if target.get("accountKey").and_then(Value::as_str) != Some(account_key) {
                return Err(MutationError::IdentityChanged.into());
            }
            target["workerDefaults"] = worker_defaults;
            target["reviewDefaults"] = review_defaults;
            Ok(save_agent(tx, &target, &validate)?)
        });
    }
    if request.mode_settings {
        let expected = request.mode_revision.ok_or(OperationError::Invalid(
            "A nonnegative expected mode revision is required",
        ))?;
        if let Some(expected_account) = request.expected_account.as_deref() {
            let connection = context.store.read()?;
            let record = load_agent_value(&connection, &request.id, &validate)?
                .ok_or(MutationError::UnknownAgent)?;
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
        )?;
        let connection = context.store.read()?;
        let record = load_agent_value(&connection, &request.id, &validate)?
            .ok_or(MutationError::UnknownAgent)?;
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
        return mutate(context, |tx| {
            Ok(update_exclusions(
                tx,
                &request.id,
                request.expected_account.as_deref(),
                request.excluded.as_deref(),
                request.non_defaults,
                |schema, value| (context.validate)(schema, value),
            )?)
        });
    }
    if request.excluded.is_some() && request.non_defaults {
        return Err(OperationError::Invalid(
            "Save chat account exclusions separately from agent mode settings",
        ));
    }
    let initial = {
        let db = context.store.read()?;
        load_agent_value(&db, &request.id, &validate)?.ok_or(MutationError::UnknownAgent)?
    };
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
        account_key(context, None, &cwd)?
    } else {
        old_account.clone()
    };
    let identity = account_identity(context, &account)?;
    let provider = identity.provider.as_deref().unwrap_or("codex");
    let provider_changed = initial.get("provider").and_then(Value::as_str) != Some(provider);
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
        .unwrap_or(if provider == "claude" {
            "default"
        } else {
            DEFAULT_LEAD_MODEL
        });
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
                .ok_or(OperationError::Unavailable("provider-session/model-list"))?
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
            .ok_or(OperationError::Invalid("A settings request id is required"))?;
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
        return mutate(context, |tx| {
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
            if let Some(receipt) = EventWriteRepository::receipt(tx, request_id)? {
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
            EventWriteRepository::put_receipt(tx, request_id, &signature, "{\"applied\":true}")?;
            Ok(target)
        });
    }
    mutate(context, |tx| {
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

pub fn rename_operation(
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
        let validate = |schema: &str, value: &Value| (context.validate)(schema, value);
        let read = context.store.read()?;
        load_agent_value(&read, id, &validate)?.ok_or(MutationError::UnknownAgent)?;
        if let Some(receipt) = EventWriteRepository::receipt(&read, request_id)? {
            if receipt.signature != signature {
                return Err(MutationError::Conflict.into());
            }
            let response = serde_json::from_str(&receipt.result).map_err(|_| {
                OperationError::Create(MutationError::InvalidStoredObject {
                    table: "runtime_operation_receipts",
                    id: request_id.to_owned(),
                })
            })?;
            return Ok(RenameOperationResult {
                response,
                replayed: true,
            });
        }
    }
    let response = mutate(context, |tx| {
        Ok(rename(
            tx,
            id,
            name,
            request_id,
            &signature,
            |schema, value| (context.validate)(schema, value),
        )?)
    })?;
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

fn account_key(
    context: &Context<'_>,
    requested: Option<&str>,
    cwd: &str,
) -> Result<String, OperationError> {
    if let Some(account) = requested {
        return Ok(account.to_owned());
    }
    let default = context
        .accounts
        .as_ref()
        .and_then(|policy| {
            policy
                .accounts()
                .default_account_key
                .as_str()
                .map(str::to_owned)
        })
        .ok_or_else(|| OperationError::Internal)?;
    let connection = context.store.read()?;
    let mut statement = connection
        .prepare("SELECT id,record FROM runtime_projects")
        .map_err(|_| OperationError::Internal)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|_| OperationError::Internal)?;
    let mut selected: Option<(usize, String)> = None;
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
        let project_account = match record.get("accountKey") {
            None | Some(Value::Null) => None,
            Some(Value::String(account)) => Some(account.as_str()),
            Some(_) => return Err(invalid().into()),
        };
        let path = PathBuf::from(project_path);
        if PathBuf::from(cwd).starts_with(&path)
            && let Some(account) = project_account
        {
            let depth = path.components().count();
            if selected
                .as_ref()
                .is_none_or(|(old_depth, _)| depth > *old_depth)
            {
                selected = Some((depth, account.to_owned()));
            }
        }
    }
    Ok(selected.map_or(default, |(_, account)| account))
}

fn account_identity(
    context: &Context<'_>,
    account_key: &str,
) -> Result<AccountIdentity, OperationError> {
    let Some(driver) = &context.driver else {
        return Err(OperationError::Internal);
    };
    if driver.get(account_key).map_err(|error| match error {
        AccountDriverError::UnknownAccount => OperationError::Invalid("Unknown account"),
        _ => OperationError::Internal,
    })? == AccountStatus::Unavailable
    {
        return Err(OperationError::Invalid("Account is unavailable"));
    }
    let snapshot = driver.snapshot().map_err(|_| OperationError::Internal)?;
    let row = snapshot
        .accounts
        .into_iter()
        .find(|row| row.get("id").and_then(Value::as_str) == Some(account_key))
        .ok_or(OperationError::Invalid("Unknown account"))?;
    if row.get("deleted").and_then(Value::as_bool) == Some(true) {
        return Err(OperationError::Invalid(
            "This account was deleted. Select another account for new chats",
        ));
    }
    if row.get("disconnected").and_then(Value::as_bool) == Some(true) {
        return Err(OperationError::Invalid(
            "Reconnect this account before creating a chat",
        ));
    }
    let string_field = |key: &str| row.get(key).and_then(Value::as_str).map(str::to_owned);
    Ok(AccountIdentity {
        provider: Some(string_field("provider").unwrap_or_else(|| "codex".to_owned())),
        home: string_field("home"),
        account_id: string_field("accountId"),
        email: string_field("email"),
    })
}

fn resolve_project_binding(
    context: &Context<'_>,
    cwd: &str,
    project_id: Option<&str>,
    home_server: Option<&str>,
) -> Result<Value, OperationError> {
    let local = local_server_id(context)?.ok_or(OperationError::Unavailable(
        "cross-server-driver/local-identity",
    ))?;
    let connection = context.store.read()?;
    resolve_project_binding_from(&connection, cwd, project_id, home_server, &local)
}

fn resolve_worker_defaults(
    context: &Context<'_>,
    value: Option<&Value>,
    account_key: &str,
    account: &AccountIdentity,
    root_model: &str,
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
    let worker_account_key = object
        .get("account_key")
        .and_then(Value::as_str)
        .unwrap_or(account_key);
    let worker_account = if worker_account_key == account_key {
        account.clone()
    } else {
        account_identity(context, worker_account_key)?
    };
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.trim().is_empty())
        .unwrap_or(root_model);
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
    let settings = context
        .catalog
        .ok_or(OperationError::Unavailable("provider-session/model-list"))?
        .resolve(
            worker_account_key,
            &worker_account,
            model,
            effort,
            fast_mode,
            daybreak,
        )?;
    let mut defaults = json!({"model":object.get("model").cloned().unwrap_or(Value::Null),
        "effort":settings.effort,"fastMode":settings.fast_mode,"daybreakEnabled":settings.daybreak_enabled,
        "cyberAccessProgram":settings.cyber_access_program,"delegationDepth":depth});
    if object.contains_key("account_key") {
        defaults["accountKey"] = json!(worker_account_key);
    }
    Ok(defaults)
}

fn resolve_review_defaults(
    context: &Context<'_>,
    value: Option<&Value>,
    account_key: &str,
    account: &AccountIdentity,
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
    let settings = context
        .catalog
        .ok_or(OperationError::Unavailable("provider-session/model-list"))?
        .resolve(account_key, account, model, effort, false, false)?;
    Ok(json!({"model":settings.model,"effort":settings.effort}))
}

fn resolve_project_binding_from(
    connection: &Connection,
    cwd: &str,
    project_id: Option<&str>,
    home_server: Option<&str>,
    local: &str,
) -> Result<Value, OperationError> {
    let project_id = project_id
        .filter(|value| !value.is_empty())
        .ok_or(OperationError::Invalid("A project ID is required"))?;
    let home_server = home_server
        .filter(|value| !value.is_empty())
        .unwrap_or(local);
    if home_server != local {
        return Err(OperationError::Unavailable(
            "cross-server-driver/project-binding",
        ));
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
            return Ok(
                json!({"cwd":cwd,"projectId":project_id,"projectServerId":home_server,"localServerId":local}),
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
    super::canonical_project_directory(&path).map_err(OperationError::Invalid)
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_agent_record(
    id: &str,
    name: &str,
    prompt: &str,
    cwd: &str,
    account: &str,
    provider: &str,
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
          "provider":provider,"yoloMode":yolo,"name":name,"prompt":prompt,"cwd":cwd,
          "role":role,"isLead":is_lead,"needsTitle":draft,"parentId":null,"rootId":id,
          "model":if provider == "claude" {"default"} else {DEFAULT_LEAD_MODEL},"effort":"medium",
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

fn mutate(
    context: &Context<'_>,
    operation: impl FnOnce(&Transaction<'_>) -> Result<Value, OperationError>,
) -> Result<Value, OperationError> {
    context.store.transaction_with_error(|tx| {
        let before = SyncDocumentsRepository::entity_sequence(tx)?;
        let result = operation(tx)?;
        Ok(crate::sync_documents::with_entity_delta(
            tx, before, result,
        )?)
    })
}

fn local_server_id(context: &Context<'_>) -> Result<Option<String>, OperationError> {
    Ok(context.store.local_server_id()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[test]
    fn reviewer_child_defaults_to_shared_workspace() {
        assert_eq!(
            child_role_workspace(Some("reviewer"), None).unwrap(),
            ("reviewer", false),
        );
        assert_eq!(
            child_role_workspace(None, None).unwrap(),
            ("implementer", true),
        );
        assert!(matches!(
            child_role_workspace(Some("orchestrator"), None),
            Err(OperationError::Invalid("Invalid agent role"))
        ));
    }

    fn snapshot(store: &RuntimeStore) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
        let connection = store.read().expect("snapshot connection");
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

    #[test]
    fn creation_rechecks_replay_and_previous_eligibility_after_controlled_interleaving() {
        let root = std::env::temp_dir().join(format!("creation-recheck-{}", new_id().expect("id")));
        std::fs::create_dir_all(&root).expect("temporary state");
        let schema: Value = serde_json::from_str(include_str!(
            "../../../../apps/server/tests/fixtures/runtime-state-schema.json"
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
        let store = RuntimeStore::open(Some(&root)).expect("runtime store");
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
        let driver = crate::account_driver::AccountDriver::open(&registry_path)
            .expect("open fixture account driver");
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
            store
                .transaction(|tx| {
                    tx.execute(
                        "INSERT INTO runtime_agents VALUES(?1,?2)",
                        rusqlite::params![scenario, record.to_string()],
                    )?;
                    Ok(())
                })
                .expect("seed existing agent");
            let interleaved = Cell::new(false);
            let after_interleaving = RefCell::new(None);
            let validate = |schema: &str, _: &Value| {
                if schema == "PersistedAgentRecord" && !interleaved.replace(true) {
                    let mut changed = record.clone();
                    match scenario {
                        "agent" => changed["creationIntent"]["prompt"] = json!("Another request"),
                        "lead" => changed["quickCreateRequest"] = json!("{}"),
                        _ => changed["deletedAt"] = json!(1),
                    }
                    store
                        .transaction(|tx| {
                            tx.execute(
                                "UPDATE runtime_agents SET record=?1 WHERE id=?2",
                                rusqlite::params![changed.to_string(), scenario],
                            )?;
                            Ok(())
                        })
                        .expect("concurrent change after initial read");
                    *after_interleaving.borrow_mut() = Some(snapshot(&store));
                }
                true
            };
            let context = Context {
                store: &store,
                accounts: None,
                driver: Some(&driver),
                catalog: None,
                validate: &validate,
            };
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
                account: Some("default".to_owned()),
                prompt: if scenario == "agent" { "Task" } else { "" }.to_owned(),
                previous: (scenario == "previous").then(|| scenario.to_owned()),
                reuse_empty: Some(false),
                intent,
                ..Creation::default()
            };
            let result = if scenario == "agent" {
                agent(&context, request)
            } else {
                lead(&context, request)
            };
            assert!(
                matches!(
                    result,
                    Err(OperationError::Create(
                        MutationError::Conflict | MutationError::Deleted
                    ))
                ),
                "{scenario}: {result:?}"
            );
            assert!(interleaved.get());
            assert_eq!(
                Some(snapshot(&store)),
                *after_interleaving.borrow(),
                "refusal writes nothing after {scenario} interleaving"
            );
        }
        drop(store);
        std::fs::remove_dir_all(root).expect("remove temporary state");
    }
}
