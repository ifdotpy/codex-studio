//! Local managed Spawn and native Review child creation.
//!
//! Python parity sources: `codex_runtime.py::Runtime.spawn_agents` (8666–8862)
//! and `codex_agent_review.py::request` (114–260). Preparation and catalog
//! access happen before the writer; child admission, records, first-turn intents
//! and task assignment commit together. The caller releases returned wake
//! signals only after this transaction commits.

use crate::event_write;
use rusqlite::{OptionalExtension, Transaction};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, RuntimeStore, StoreError};
use thiserror::Error;

use crate::managed_host_tools::MAX_SPAWN_BATCH_AGENTS;
use studio_runtime_schema::agent_limits::DEFAULT_MAX_TEAM_AGENTS;
const MAX_NAME_CHARS: usize = 100;
const MAX_PROMPT_CHARS: usize = 32_000;
/// Python's Linux VM review state save timeout, in seconds.
const VM_REVIEW_STATE_TIMEOUT_SECONDS: u64 = 130;

fn vm_review_guest_ids(
    request_id: &str,
) -> Result<(String, String), crate::managed_host_tools::Error> {
    let digest = studio_runtime_store::sha256_hex(request_id.as_bytes());
    let guest_request_id = format!("layr-review-state:{digest}");
    let turn_id = format!("review:{digest}");
    let limit = crate::linux_vm_guest::GUEST_REQUEST_ID_MAX_CHARS;
    if guest_request_id.chars().count() > limit || turn_id.chars().count() > limit {
        return Err(crate::managed_host_tools::Error::Invalid(
            "The VM review request identity is too long",
        ));
    }
    Ok((guest_request_id, turn_id))
}

fn validate_vm_parent_workspace(workspace: &str, vm_parent: bool) -> Result<(), Error> {
    if matches!(workspace, "layr" | "linux") && !vm_parent {
        return Err(crate::managed_host_tools::Error::Invalid(
            "A VM workspace requires a VM parent",
        )
        .into());
    }
    Ok(())
}

fn validate_image_workspace_platform(workspace: &str, is_macos: bool) -> Result<(), Error> {
    if workspace == "image" && !is_macos {
        return Err(Error::Unavailable("workspace-state/image-provisioning"));
    }
    Ok(())
}

fn host_project_cwd<'a>(vm_workspace: bool, root: &'a Value, parent_cwd: &'a str) -> &'a str {
    if vm_workspace {
        root.get("hostProjectPath")
            .or_else(|| root.get("cwd"))
            .and_then(Value::as_str)
            .unwrap_or(parent_cwd)
    } else {
        parent_cwd
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedCreationEnvironmentError {
    Invalid(&'static str),
    Unavailable(&'static str),
}

/// Host account facts and model admission. Production implementations resolve
/// model settings through the retained `AccountSessionPort` broker.
pub trait ManagedCreationEnvironment: Send + Sync {
    /// Whether this environment can provision image workspaces. Kept injectable
    /// so platform-independent tests can exercise the macOS provisioning path.
    fn supports_image_workspace(&self) -> bool {
        cfg!(target_os = "macos")
    }
    /// Guest RPC boundary used by VM review state capture. Production uses the
    /// runtime's Linux VM socket; tests inject a fake guest.
    fn guest_rpc(
        &self,
        store: &RuntimeStore,
    ) -> Result<std::sync::Arc<dyn crate::linux_vm_guest::GuestRpc>, ManagedCreationEnvironmentError>
    {
        #[cfg(unix)]
        {
            Ok(std::sync::Arc::new(
                crate::linux_vm_guest::UnixGuestRpc::for_runtime_state(store.state_dir()),
            ))
        }
        #[cfg(not(unix))]
        {
            let _ = store;
            Err(ManagedCreationEnvironmentError::Unavailable(
                "linux-vm/guest-rpc",
            ))
        }
    }
    /// Wait for the actor and worker-candidate model catalogs at the callback's async
    /// boundary. Implementations keep this bounded and never run it at worker
    /// startup or inside the synchronous store callback.
    fn wait_for_catalog<'a>(
        &'a self,
        _account_key: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), ManagedCreationEnvironmentError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async { Ok(()) })
    }
    /// Resolve the account and model as `codex_worker_accounts.resolve` does.
    /// `requested_account_key` is the per-worker `account_key`; the default is
    /// kept separate so an explicit value can be checked against chat settings.
    /// `excluded_account_keys` comes from the root agent. Project keys are the
    /// resolved project settings allowlist, when the root has a project binding.
    /// Implicit selection picks the ready account with the fewest active agents
    /// (`active_agents`, ties by account key), filtering disabled/unusable
    /// accounts, project keys, provider, and models (including aliases);
    /// explicit selection never falls back.
    fn select_worker_account(
        &self,
        request: crate::provider_callbacks::WorkerAccountRequest<'_>,
    ) -> Result<(String, crate::host_worker_create::AccountIdentity), ManagedCreationEnvironmentError>;
    fn validate_worker_choice(
        &self,
        store: &RuntimeStore,
        account_key: &str,
        account: &crate::host_worker_create::AccountIdentity,
        requested: &str,
        default: &str,
    ) -> Result<(), ManagedCreationEnvironmentError>;
    fn catalog(&self) -> &dyn crate::host_worker_create::CreationCatalog;
    fn prepare_workspace(
        &self,
        store: &RuntimeStore,
        actor: &AgentRecord,
        args: &Value,
        request_id: &str,
    ) -> Result<
        Box<dyn crate::monitor_runtime::WorkspaceAdmission + Send + Sync>,
        ManagedCreationEnvironmentError,
    >;

    /// Start image workspace provisioning after child rows commit. Tests override this
    /// boundary so ordinary workspace tests never create real disk images.
    fn provision_image_workspace(
        &self,
        _store: &RuntimeStore,
        root: &Path,
        agent_id: &str,
    ) -> Result<Value, ManagedCreationEnvironmentError> {
        #[cfg(target_os = "macos")]
        {
            // Complete before returning: the caller releases the queued wake
            // signals only after this hook and the readiness update commit.
            let workspace =
                crate::image_workspace::create_workspace(root, agent_id).map_err(|_| {
                    ManagedCreationEnvironmentError::Unavailable(
                        "workspace-state/image-provisioning",
                    )
                })?;
            Some(workspace)
                .filter(|workspace| {
                    workspace.get("path").and_then(Value::as_str).is_some()
                        && workspace.get("mount").and_then(Value::as_str).is_some()
                })
                .ok_or(ManagedCreationEnvironmentError::Unavailable(
                    "workspace-state/image-provisioning",
                ))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (root, agent_id);
            Err(ManagedCreationEnvironmentError::Unavailable(
                "workspace-state/image-provisioning",
            ))
        }
    }
}

fn expand_review_home(path: &str, home: Option<&str>) -> PathBuf {
    if path == "~" {
        return home
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = home
    {
        return Path::new(home).join(rest);
    }
    PathBuf::from(path)
}

/// Python `codex_worker_workspace.select` note for a default worktree outside Git.
const NO_GIT_WORKTREE_NOTE: &str =
    "This folder has no Git repository; the worker uses the shared folder";

/// Python `codex_runtime.no_worktree_warning`.
fn no_worktree_warning(directory: &str) -> String {
    format!(
        "{directory} is not in a git repository. This agent works directly in the folder, \
         without an isolated worktree. Other agents in this folder can change the same files."
    )
}

/// Maximum characters of a requested worker folder, after trimming.
const MAX_SPAWN_DIRECTORY_CHARS: usize = 4096;

/// Python `codex_runtime.spawn_directory` for a host worker: `None` keeps the
/// parent's folder; a relative path starts at the parent's folder; the
/// resolved path must be an existing folder.
fn spawn_directory(parent: &str, requested: Option<&Value>) -> Result<Option<String>, Error> {
    let requested = match requested {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(value)) => value,
        Some(_) => {
            return Err(
                crate::managed_host_tools::Error::Invalid("cwd must be a folder path").into(),
            );
        }
    };
    let trimmed = event_write::python_strip(requested);
    if trimmed.is_empty() || trimmed.chars().count() > MAX_SPAWN_DIRECTORY_CHARS {
        return Err(crate::managed_host_tools::Error::Invalid("cwd must be a folder path").into());
    }
    let home = std::env::var("HOME").ok();
    let expanded = expand_review_home(trimmed, home.as_deref());
    let path = if expanded.is_absolute() {
        expanded
    } else {
        Path::new(parent).join(expanded)
    };
    // `Path.resolve()` of an existing folder is its canonical path.
    match std::fs::canonicalize(&path) {
        Ok(path) if path.is_dir() => Ok(Some(path.to_string_lossy().into_owned())),
        _ => {
            Err(crate::managed_host_tools::Error::Invalid("cwd must be an existing folder").into())
        }
    }
}

fn vm_child_directory(parent: &str, requested: Option<&str>) -> Result<(String, String), Error> {
    let root = Path::new(parent);
    let raw = requested.map(Path::new).unwrap_or(root);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.join(raw)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                return Err(crate::managed_host_tools::Error::Invalid(
                    "A worker directory must stay in its parent layr line",
                )
                .into());
            }
            value => normalized.push(value.as_os_str()),
        }
    }
    let subpath = normalized.strip_prefix(root).map_err(|_| {
        Error::Managed(crate::managed_host_tools::Error::Invalid(
            "A worker directory must stay in its parent layr line",
        ))
    })?;
    Ok((
        normalized.to_string_lossy().into_owned(),
        subpath.to_string_lossy().into_owned(),
    ))
}

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error(transparent)]
    Managed(#[from] crate::managed_host_tools::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("Host child creation was refused")]
    Refused,
    #[error("Not implemented: requires {0}")]
    Unavailable(&'static str),
}

impl From<ManagedCreationEnvironmentError> for Error {
    fn from(error: ManagedCreationEnvironmentError) -> Self {
        match error {
            ManagedCreationEnvironmentError::Invalid(message) => {
                crate::managed_host_tools::Error::Invalid(message).into()
            }
            ManagedCreationEnvironmentError::Unavailable(name) => Error::Unavailable(name),
        }
    }
}

struct Draft {
    typed: AgentRecord,
    record: Value,
    id: String,
    prompt: String,
    task_id: Option<String>,
    intent: Value,
    worktree: bool,
    workspace: String,
    worker_base_main_ref: Option<String>,
    worker_base_behind_main: Option<i64>,
    epoch: i64,
    account: crate::host_worker_create::AccountIdentity,
    workspace_admission: Box<dyn crate::monitor_runtime::WorkspaceAdmission + Send + Sync>,
}

/// Create a local host batch using the P02 typed draft and recheck contract.
/// `request_id` is the callback's durable operation identity, so child ids and
/// initial-event ids remain stable across retries.
pub(crate) async fn spawn_agents(
    store: &RuntimeStore,
    actor: &crate::managed_host_tools::Actor,
    args: &Value,
    request_id: &str,
    now: f64,
    environment: &dyn ManagedCreationEnvironment,
) -> Result<crate::managed_host_tools::OperationResult, Error> {
    let actor_record = actor.record().clone();
    if actor_record.extra.get("role").and_then(Value::as_str) == Some("reviewer") {
        return Err(
            crate::managed_host_tools::Error::Invalid("Reviewers cannot create agents").into(),
        );
    }
    if !now.is_finite() || now < 0.0 || request_id.is_empty() {
        return Err(crate::managed_host_tools::Error::Invalid("Invalid spawn identity").into());
    }
    if args
        .get("server")
        .is_some_and(|value| event_write::truthy(Some(value)))
    {
        return Err(Error::Unavailable("multi-server/worker-call"));
    }
    let specs = args
        .get("agents")
        .and_then(Value::as_array)
        .filter(|items| (1..=MAX_SPAWN_BATCH_AGENTS).contains(&items.len()))
        .ok_or(crate::managed_host_tools::Error::Invalid(
            crate::managed_host_tools::SPAWN_BATCH_SIZE_REFUSAL,
        ))?;
    let parent_value = actor_record.value();
    let root_id = actor_record.root_id.0.as_str();
    let root = stored_root(store, AgentId::from(root_id)).await?;
    event_write::assert_delegation(&root)
        .map_err(|_| crate::managed_host_tools::Error::Invalid("Delegation is disabled"))?;
    let root_value = root.value();
    let defaults = root_value
        .get("workerDefaults")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let parent_cwd = parent_value.get("cwd").and_then(Value::as_str).ok_or(
        crate::managed_host_tools::Error::Invalid("Select a project directory"),
    )?;
    let parent_account = parent_value
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let mut drafts = Vec::with_capacity(specs.len());
    let mut task_ids = std::collections::BTreeSet::new();

    // Workers without an explicit account go to the least loaded one; the count
    // grows with each pick so a batch spreads across accounts as well.
    let mut account_load = store
        .read_async("runtime-services/managed_child_create", |read| {
            studio_runtime_store::AgentIndexedReads::slots_by_account(read)
        })
        .await?;
    for (index, spec) in specs.iter().enumerate() {
        if !spec.is_object() {
            return Err(crate::managed_host_tools::Error::Invalid(
                "Every worker needs a name, task and valid role",
            )
            .into());
        }
        let profile = if let Some(profile_id) = spec.get("profile_id").filter(|value| {
            !value.is_null() && value.as_str().is_none_or(|value| !value.is_empty())
        }) {
            let profile_id =
                profile_id
                    .as_str()
                    .ok_or(crate::managed_host_tools::Error::Invalid(
                        "Unknown worker profile",
                    ))?;
            let profile_id = profile_id.to_owned();
            Some(
                store
                    .read_async("runtime-services/managed_child_create", move |read| {
                        studio_runtime_store::Table::<studio_runtime_store::ProfileRecord>::new(
                            read,
                        )
                        .get(&profile_id)
                    })
                    .await?
                    .ok_or(crate::managed_host_tools::Error::Invalid(
                        "Unknown worker profile",
                    ))?,
            )
        } else {
            None
        };
        let name = spec
            .get("name")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty() && text.trim().chars().count() <= MAX_NAME_CHARS)
            .ok_or(crate::managed_host_tools::Error::Invalid(
                "Every worker needs a name, task and valid role",
            ))?;
        let prompt = spec
            .get("prompt")
            .and_then(Value::as_str)
            .filter(|text| {
                !text.trim().is_empty() && text.trim().chars().count() <= MAX_PROMPT_CHARS
            })
            .ok_or(crate::managed_host_tools::Error::Invalid(
                "Every worker needs a name, task and valid role",
            ))?;
        let role = spec.get("role").and_then(Value::as_str).unwrap_or_else(|| {
            profile
                .as_ref()
                .map(|profile| match profile.role {
                    studio_runtime_store::ProfileRole::Implementer => "implementer",
                    studio_runtime_store::ProfileRole::Reviewer => "reviewer",
                })
                .unwrap_or("implementer")
        });
        if !matches!(role, "implementer" | "reviewer") {
            return Err(crate::managed_host_tools::Error::Invalid(
                "Every worker needs a name, task and valid role",
            )
            .into());
        }
        if let Some(task) = spec.get("task_id") {
            let task = task.as_str().filter(|value| !value.is_empty()).ok_or(
                crate::managed_host_tools::Error::Invalid("task_id must be a task id"),
            )?;
            if actor_record.extra.get("isLead").and_then(Value::as_bool) != Some(true) {
                return Err(crate::managed_host_tools::Error::Invalid(
                    "Only the lead can delegate an existing task; ask the lead to assign it",
                )
                .into());
            }
            if !task_ids.insert(task.to_owned()) {
                return Err(crate::managed_host_tools::Error::Invalid(
                    "Assign each task to one worker in a batch",
                )
                .into());
            }
        }
        let requested_base_ref = if let Some(base_ref) = spec.get("base_ref") {
            let value = base_ref
                .as_str()
                .filter(|value| !value.trim().is_empty() && value.chars().count() <= 1024)
                .ok_or(crate::managed_host_tools::Error::Invalid(
                    "base_ref must be a branch, tag, or commit",
                ))?;
            Some(value.trim())
        } else {
            None
        };
        if let Some(base_ref) = requested_base_ref
            && (base_ref.is_empty() || base_ref.chars().count() > 1024)
        {
            return Err(crate::managed_host_tools::Error::Invalid(
                "base_ref must be a branch, tag, or commit",
            )
            .into());
        }
        if spec
            .get("server")
            .is_some_and(|value| event_write::truthy(Some(value)))
        {
            return Err(Error::Unavailable("multi-server/worker-call"));
        }
        let vm_actor = parent_value.get("executionMode").and_then(Value::as_str) == Some("vm");
        if vm_actor
            && (spec.get("workspace").is_some_and(|value| value != "layr")
                || spec
                    .get("environment")
                    .is_some_and(|value| value != "linux"))
        {
            return Err(crate::managed_host_tools::Error::Invalid(
                "All agents in a VM chat use layr in the VM",
            )
            .into());
        }
        if vm_actor && role == "reviewer" {
            return Err(crate::managed_host_tools::Error::Unavailable(
                "workspace-state/vm-workspace",
            )
            .into());
        }
        let vm_directory = if vm_actor {
            Some(vm_child_directory(
                parent_cwd,
                spec.get("cwd").and_then(Value::as_str),
            )?)
        } else {
            None
        };
        // Python `spawn_directory`: a host child may work in another folder;
        // a relative path starts at the parent's folder.
        let alternate_directory = if vm_actor {
            None
        } else {
            spawn_directory(parent_cwd, spec.get("cwd"))?
                .filter(|directory| directory.as_str() != parent_cwd)
        };
        let child_directory = alternate_directory.as_deref().unwrap_or(parent_cwd);
        if role == "reviewer" && spec.get("workspace").is_some() {
            return Err(crate::managed_host_tools::Error::Invalid(
                "workspace applies to implementers; reviewers use the shared folder with read-only access",
            )
            .into());
        }
        let workspace = spec
            .get("workspace")
            .and_then(Value::as_str)
            .or_else(|| {
                if role == "reviewer" && !vm_actor {
                    Some("shared")
                } else if vm_actor {
                    Some("layr")
                } else {
                    parent_value.get("workspaceMode").and_then(Value::as_str)
                }
            })
            .unwrap_or("worktree");
        validate_vm_parent_workspace(workspace, vm_actor)?;
        // Python resolves image support and starts its base before the writer
        // (`codex_runtime.py:8895-8915`, `codex_workspace_images.py:834`); refuse
        // before drafting or committing children when this provider is absent.
        validate_image_workspace_platform(workspace, environment.supports_image_workspace())?;
        let worktree = match workspace {
            "worktree" => true,
            "shared" => false,
            "image" | "layr" | "linux" => false,
            _ => {
                return Err(crate::managed_host_tools::Error::Invalid(
                    "workspace must be \"image\", \"worktree\", or \"shared\"",
                )
                .into());
            }
        };
        // Python `git_toplevel` decides Git support for an implementer's
        // folder; a reviewer has no repository there (`spawn_agents`).
        let in_git = !vm_actor
            && role == "implementer"
            && crate::host_worker_create::inside_git_work_tree(Path::new(child_directory));
        let explicit_workspace = spec.get("workspace").and_then(Value::as_str);
        if !vm_actor && explicit_workspace == Some("worktree") && !in_git {
            return Err(crate::managed_host_tools::Error::Invalid(
                "workspace \"worktree\" requires a Git repository",
            )
            .into());
        }
        if !vm_actor && requested_base_ref.is_some() && !in_git {
            return Err(crate::managed_host_tools::Error::Invalid(
                "base_ref requires an implementer in a Git repository",
            )
            .into());
        }
        if workspace == "shared" && spec.get("base_ref").is_some() {
            return Err(crate::managed_host_tools::Error::Invalid(
                "base_ref cannot be used with workspace \"shared\"",
            )
            .into());
        }
        // Python `select_workspace`: the parent's default worktree mode uses
        // the shared folder, with a note, where the folder has no Git.
        let workspace_note = (!vm_actor
            && worktree
            && !in_git
            && explicit_workspace.is_none()
            && parent_value.get("workspaceMode").and_then(Value::as_str) == Some("worktree"))
        .then_some(NO_GIT_WORKTREE_NOTE);
        let worktree = worktree && in_git;
        let id = child_id(request_id, index);
        let requested_account_key = spec.get("account_key").and_then(Value::as_str);
        let default_account_key = defaults.get("accountKey").and_then(Value::as_str);
        let workspace_admission =
            environment.prepare_workspace(store, &actor_record, spec, request_id)?;
        let profile_model = profile.as_ref().map(|profile| profile.model.as_str());
        let requested_model = spec
            .get("model")
            .and_then(Value::as_str)
            .or(profile_model)
            .or_else(|| defaults.get("model").and_then(Value::as_str))
            .or_else(|| root_value.get("model").and_then(Value::as_str))
            .unwrap_or("gpt-6-astra");
        let excluded_account_keys = root_value
            .get("excludedAccountKeys")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let project_account_keys = match root_value
            .get("projectId")
            .and_then(Value::as_str)
            .zip(root_value.get("projectServerId").and_then(Value::as_str))
        {
            Some((project_id, home_server)) => {
                let project_id = project_id.to_owned();
                store
                    .read_async("runtime-services/managed_child_create", move |read| {
                        studio_runtime_store::ProjectsRepository::get(read, &project_id)
                    })
                    .await
                    .ok()
                    .flatten()
                    .map(|(_, record)| (record, home_server))
            }
            None => None,
        }
        .and_then(|(record, home_server)| {
            (record["homeServerId"] == home_server)
                .then(|| {
                    record["accountKeys"].as_array().map(|keys| {
                        keys.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect::<Vec<_>>()
                    })
                })
                .flatten()
        });
        let (account_key, account) =
            environment.select_worker_account(crate::provider_callbacks::WorkerAccountRequest {
                parent_account_key: parent_account,
                requested_account_key,
                default_account_key,
                model: requested_model,
                excluded_account_keys: &excluded_account_keys,
                project_account_keys: project_account_keys.as_deref(),
                active_agents: &account_load,
            })?;
        *account_load.entry(account_key.clone()).or_default() += 1;
        if spec.get("model").and_then(Value::as_str).is_some() || profile_model.is_some() {
            let default_model = defaults
                .get("model")
                .and_then(Value::as_str)
                .filter(|model| !model.is_empty())
                .or_else(|| root_value.get("model").and_then(Value::as_str))
                .unwrap_or(requested_model);
            environment.validate_worker_choice(
                store,
                &account_key,
                &account,
                requested_model,
                default_model,
            )?;
        }
        let provider = account.provider.key();
        let account_adapter = crate::provider_adapter::adapter(account.provider);
        // An unselected lead default that the account's provider cannot run
        // becomes that provider's own default.
        let model = if requested_model == crate::agents_create::DEFAULT_LEAD_MODEL
            && root_value.get("model").is_none()
            && defaults.get("model").is_none()
            && profile_model.is_none()
            && spec.get("model").is_none()
            && !account_adapter.owns_model(requested_model)
        {
            account_adapter.default_model()
        } else {
            requested_model
        };
        let effort = if spec.get("effort").is_some() {
            spec.get("effort").and_then(Value::as_str)
        } else if let Some(effort) = profile.as_ref().and_then(|profile| profile.effort) {
            Some(match effort {
                studio_runtime_store::ReasoningEffort::Low => "low",
                studio_runtime_store::ReasoningEffort::Medium => "medium",
                studio_runtime_store::ReasoningEffort::High => "high",
                studio_runtime_store::ReasoningEffort::Xhigh => "xhigh",
                studio_runtime_store::ReasoningEffort::Max => "max",
                studio_runtime_store::ReasoningEffort::Ultra => "ultra",
            })
        } else {
            defaults.get("effort").and_then(Value::as_str)
        };
        let fast_mode = spec
            .get("fast_mode")
            .and_then(Value::as_bool)
            .or_else(|| defaults.get("fastMode").and_then(Value::as_bool))
            .unwrap_or(false);
        let daybreak = spec
            .get("daybreak_enabled")
            .and_then(Value::as_bool)
            .or_else(|| defaults.get("daybreakEnabled").and_then(Value::as_bool))
            .unwrap_or(false);
        let vm_workspace = matches!(workspace, "layr" | "linux");
        let host_cwd = host_project_cwd(vm_workspace, &root_value, child_directory);
        // The parent's project binding names the parent's folder. Another
        // folder uses its own path settings, as Python `settings_key` does.
        let parent_project_id = parent_value
            .get("projectId")
            .filter(|_| alternate_directory.is_none());
        let project = stored_project(
            store,
            parent_project_id
                .and_then(Value::as_str)
                .unwrap_or(host_cwd),
            host_cwd,
        )
        .await?;
        let project_worker_base = if requested_base_ref.is_none() && in_git {
            worker_base_for_directory(store, parent_project_id, host_cwd).await?
        } else {
            None
        };
        let project_worker_base = project_worker_base.as_deref();
        let base_ref_request = requested_base_ref.or(project_worker_base);
        let (worktree, (base_ref, base_commit, main_ref, behind_main)) = resolve_child_base(
            worktree,
            workspace == "image",
            Path::new(host_cwd),
            base_ref_request,
        )?;
        let (workspace_in_git, image_workspace_has_git) = match vm_actor {
            false => (in_git, in_git),
            true => {
                let in_git = parent_value
                    .get("workspaceInGit")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                (
                    in_git,
                    in_git
                        || parent_value
                            .get("imageWorkspaceHasGit")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                )
            }
        };
        let body = json!({"account_key":account_key,"name":name,"model":model,"effort":effort,
            "prompt":prompt,"workspace_explicit":spec.get("workspace").is_some(),
            "_workspaceInGit":workspace_in_git,
            "_imageWorkspaceHasGit":image_workspace_has_git});
        let draft = crate::host_worker_create::prepare_work_item(
            &body,
            Path::new(host_cwd),
            AgentId::from(id.as_str()),
            actor_record.root_id.clone(),
            actor_record.id.clone(),
            &base_ref,
            &base_commit,
            actor_record.epoch,
            parent_cwd,
            host_cwd,
            parent_account,
            parent_value.get("projectId"),
            parent_value.get("projectServerId"),
            project.as_ref(),
            &account,
            environment.catalog(),
            role,
            worktree,
            fast_mode,
            daybreak,
        )
        .map_err(map_create_error)?;
        let mut record = crate::host_worker_create::persisted_record(&draft)
            .map_err(map_create_error)?
            .value();
        if workspace == "image" {
            crate::agents_create::set_image_workspace_fields(&mut record, child_directory);
            if base_ref_request.is_some() {
                record["imageWorkspaceBaseRef"] = json!(base_ref);
                record["workerBaseRef"] = json!(base_ref);
                record["workerBaseCommit"] = json!(base_commit);
            }
        } else if vm_workspace {
            record["workspaceMode"] = json!("layr");
            let (child_cwd, subpath) = vm_directory.as_ref().ok_or(Error::Refused)?;
            crate::agents_create::set_vm_workspace_fields(&mut record, host_cwd);
            record["cwd"] = json!(child_cwd);
            record["layrSubpath"] = json!(subpath);
            if let Some(base_state) = spec.get("base_ref") {
                record["layrBaseState"] = base_state.clone();
            }
        }
        if let Some(note) = workspace_note {
            record["imageWorkspaceError"] = json!(note);
        }
        // Python `create`: an implementer outside Git and outside an image
        // works in the folder itself, and the record says so.
        if !vm_actor && role == "implementer" && !worktree && workspace != "image" && !in_git {
            record["worktreeWarning"] = json!(no_worktree_warning(
                record["cwd"].as_str().unwrap_or(child_directory)
            ));
        }
        record["provider"] = json!(provider);
        record["workerBaseMainRef"] = json!(main_ref.clone());
        record["workerBaseBehindMain"] = json!(behind_main);
        // Python's AgentFactory persists these keys on every row (including
        // rows without a selected profile): codex_runtime.py:5014-5015.
        record["profileId"] = spec.get("profile_id").cloned().unwrap_or(Value::Null);
        record["profileInstructions"] = json!("");
        if let Some(profile) = &profile {
            record["profileInstructions"] = json!(profile.instructions);
        }
        record["name"] = json!(name.trim());
        record["yoloMode"] = root_value.get("yoloMode").cloned().unwrap_or(json!(true));
        record["maxAgents"] = root_value
            .get("maxAgents")
            .cloned()
            .unwrap_or(json!(DEFAULT_MAX_TEAM_AGENTS));
        record["tokenBudget"] = root_value
            .get("tokenBudget")
            .cloned()
            .unwrap_or(Value::Null);
        drafts.push(Draft {
            typed: draft,
            record,
            id,
            prompt: prompt.trim().to_owned(),
            task_id: spec
                .get("task_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            intent: spec.clone(),
            worktree,
            workspace: if vm_workspace { "layr" } else { workspace }.to_owned(),
            worker_base_main_ref: main_ref,
            worker_base_behind_main: behind_main,
            epoch: actor_record.epoch,
            account,
            workspace_admission,
        });
    }

    let image_provisions = drafts
        .iter()
        .filter(|draft| draft.workspace == "image")
        .map(|draft| {
            (
                draft.id.clone(),
                draft.record["imageWorkspaceRepo"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                draft.record["epoch"].as_i64().unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    let request_id = request_id.to_owned();
    let mut result = store.transaction_with_error(move |tx| -> Result<crate::managed_host_tools::OperationResult, crate::managed_host_tools::Error> {
        recheck_actor(tx, &actor_record)?;
        let current_root = AgentRepository::get(tx, &actor_record.root_id)?
            .ok_or(crate::managed_host_tools::Error::Stale)?;
        event_write::assert_delegation(&current_root)
            .map_err(|_| crate::managed_host_tools::Error::Invalid("Delegation is disabled"))?;
        let max = current_root.extra.get("maxAgents").and_then(Value::as_i64).unwrap_or(DEFAULT_MAX_TEAM_AGENTS);
        let existing = drafts.iter().map(|draft| AgentRepository::get(tx, &AgentId::from(draft.id.as_str())))
            .collect::<Result<Vec<_>, _>>()?;
        let new_count = existing.iter().filter(|item| item.is_none()).count() as i64;
        if new_count != 0 && new_count != drafts.len() as i64 {
            return Err(crate::managed_host_tools::Error::Invalid("This request id has different content"));
        }
        if active_agents(tx, &actor_record.root_id.0)? + new_count > max {
            return Err(crate::managed_host_tools::Error::Invalid("This batch exceeds the active agent limit; no workers were created"));
        }
        for draft in &drafts {
            draft.workspace_admission.check(tx, &actor_record)
                .map_err(|_| crate::managed_host_tools::Error::Stale)?;
            crate::host_worker_create::recheck_worker(tx, &draft.typed, draft.epoch, &draft.account)
                .map_err(map_recheck_error)?;
        }
        if !can_delegate(tx, &actor_record, &current_root)? {
            return Err(crate::managed_host_tools::Error::Invalid("Delegation depth reached"));
        }
        let mut children = Vec::with_capacity(drafts.len());
        let mut signals = Vec::new();
        for (draft, saved) in drafts.iter().zip(existing) {
            if let Some(saved) = saved {
                children.push(project_child(&saved.value(), draft.task_id.as_deref(), draft.worktree));
                continue;
            }
            let mut text = draft.prompt.clone();
            if draft.workspace != "image"
                && let Some(error) = draft
                    .record
                    .get("imageWorkspaceError")
                    .and_then(Value::as_str)
                    .filter(|error| !error.is_empty())
            {
                text.push_str(&format!(
                    "\n\n[Studio workspace fallback] {error} {}",
                    if draft.worktree {
                        "Studio will use a Git worktree."
                    } else {
                        "Studio will use the selected folder."
                    }
                ));
            }
            if draft.worktree {
                text.push_str(&format!("\n\n[Studio worker base] Commit {} from ref {}.", draft.record["workerBaseCommit"].as_str().unwrap_or_default(), draft.record["workerBaseRef"].as_str().unwrap_or("HEAD")));
                if let Some(behind) = draft.worker_base_behind_main.filter(|behind| *behind != 0) {
                    text.push_str(&format!(" This base is {behind} commits behind {}. Check for newer project instructions before starting.", draft.worker_base_main_ref.as_deref().unwrap_or("main")));
                }
            }
            if draft.workspace == "image"
                && let Some(base_ref) = draft
                    .record
                    .get("imageWorkspaceBaseRef")
                    .and_then(Value::as_str)
            {
                text.push_str(&format!(
                    "\n\n[Studio requested base] Ref {base_ref} resolves to commit {}. Check out this commit yourself before editing.",
                    draft.record["workerBaseCommit"].as_str().unwrap_or_default()
                ));
            }
            if let Some(task) = &draft.task_id {
                let work = studio_runtime_store::WorkItemsRepository::work_record(tx, task)?
                    .ok_or(crate::managed_host_tools::Error::Invalid("Unknown task_id in this team; no workers were created"))?;
                let title = work.get("title").and_then(Value::as_str).unwrap_or("Assigned task");
                text.push_str(&format!("\n\n[Studio task {task}] {title}\nYou own this task. Submit the result with the work item tools."));
            }
            let record = AgentRecord::from_value(draft.record.clone())
                .map_err(|_| crate::managed_host_tools::Error::Invalid("Invalid child agent record"))?;
            AgentRepository::put(tx, &record)?;
            record_spawn_effect(tx, &actor_record, &draft.record, &request_id);
            let mut record_value = draft.record.clone();
            record_value["workerBaseMainRef"] = json!(draft.worker_base_main_ref);
            record_value["workerBaseBehindMain"] = json!(draft.worker_base_behind_main);
            crate::agents_create::publish_agent(tx, &record_value)?;
            if let Some(task_id) = &draft.task_id {
                crate::work_assignment_runtime::assign_in_transaction(tx, task_id, &draft.id, &actor_record, now)
                    .map_err(|_| crate::managed_host_tools::Error::Invalid("Work item assignment was rejected"))?;
            }
            let inserted = event_write::enqueue_event(tx, &record, format!("{}:initial", draft.id), "user", text, now)
                .map_err(|_| crate::managed_host_tools::Error::Invalid("Initial turn intent was rejected"))?;
            if !inserted {
                return Err(crate::managed_host_tools::Error::Invalid("This request id has different content"));
            }
            children.push(project_child(&draft.record, draft.task_id.as_deref(), draft.worktree));
            signals.push(crate::managed_host_tools::WakeSignal::new("worker-driver"));
        }
        Ok(crate::managed_host_tools::OperationResult {
            value: json!({"requestId":request_id,"agents":children,"delivery":"Results wake you automatically. Finish your turn while waiting."}),
            signals,
        })
    }).await.map_err(Error::from)?;
    for (id, root, epoch) in image_provisions {
        let workspace = match environment.provision_image_workspace(store, Path::new(&root), &id) {
            Ok(workspace) => workspace,
            Err(error) => {
                if let Err(cleanup_error) = crate::image_workspace::remove_workspace(&id, true) {
                    eprintln!("Could not remove failed image workspace: {cleanup_error}");
                }
                crate::agents_create::persist_image_provision_failure(
                    store,
                    &id,
                    epoch,
                    &format!("{error:?}"),
                )
                .await
                .map_err(|_| Error::Unavailable("workspace-state/image-provisioning"))?;
                return Err(Error::from(error));
            }
        };
        let id_for_update = id.clone();
        let committed = store
            .transaction(move |tx| {
                let Some(record) =
                    AgentRepository::get(tx, &AgentId::from(id_for_update.as_str()))?
                else {
                    return Ok(None);
                };
                let value = record.into_value();
                if !crate::agents_create::image_provision_is_current(&value, epoch) {
                    return Ok(None);
                }
                let invalid = || studio_runtime_store::StoreError::InvalidStoredObject {
                    table: "runtime_agents",
                    id: id_for_update.clone(),
                };
                let now = crate::event_write::unix_time().map_err(|_| invalid())?;
                crate::image_workspace_agents::record_ready(tx, value, &workspace, now)
                    .map(Some)
                    .map_err(|error| match error {
                        crate::event_write::EventWriteError::Store(error) => error,
                        _ => invalid(),
                    })
            })
            .await
            .map_err(Error::from)?;
        let Some(committed) = committed else {
            if let Err(cleanup_error) = crate::image_workspace::remove_workspace(&id, true) {
                eprintln!("Could not remove stale image workspace: {cleanup_error}");
            }
            return Err(crate::managed_host_tools::Error::Stale.into());
        };
        if let Some(children) = result.value["agents"].as_array_mut() {
            for child in children {
                if child.get("id").and_then(Value::as_str) == Some(id.as_str()) {
                    child["cwd"] = committed["cwd"].clone();
                    child["imageWorkspaceReady"] = json!(true);
                    child["imageWorkspacePhase"] = json!("ready");
                    break;
                }
            }
        }
    }
    Ok(result)
}

pub(crate) fn child_id(request_id: &str, index: usize) -> String {
    uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("{request_id}:{index}").as_bytes(),
    )
    .to_string()
}

/// The stored root agent; a missing root makes the request stale.
async fn stored_root(store: &RuntimeStore, root_id: AgentId) -> Result<AgentRecord, Error> {
    store
        .read_async("runtime-services/managed_child_create", move |read| {
            AgentRepository::get(read, &root_id)?
                .ok_or_else(|| crate::managed_host_tools::Error::Stale.into())
        })
        .await
}

/// The project stored under `project_id`, else the one stored under `fallback`.
async fn stored_project(
    store: &RuntimeStore,
    project_id: &str,
    fallback: &str,
) -> Result<Option<Value>, Error> {
    let (project_id, fallback) = (project_id.to_owned(), fallback.to_owned());
    store
        .read_async("runtime-services/managed_child_create", move |read| {
            Ok::<_, Error>(
                studio_runtime_store::ProjectsRepository::get(read, &project_id)?
                    .or(studio_runtime_store::ProjectsRepository::get(
                        read, &fallback,
                    )?)
                    .map(|(_, project)| project),
            )
        })
        .await
}

/// Match Python `Runtime.project_worker_base`: project handles are exact ids;
/// filesystem directories inherit the setting from their deepest ancestor.
async fn worker_base_for_directory(
    store: &RuntimeStore,
    project_id: Option<&Value>,
    cwd: &str,
) -> Result<Option<String>, Error> {
    if let Some(handle) = project_id
        .and_then(Value::as_str)
        .filter(|value| value.starts_with("project:"))
    {
        let handle = handle.to_owned();
        let (_, project) = store
            .read_async("runtime-services/managed_child_create", move |read| {
                studio_runtime_store::ProjectsRepository::get(read, &handle)
            })
            .await?
            .ok_or(crate::managed_host_tools::Error::Invalid(
                "Select an existing project",
            ))?;
        return Ok(project
            .get("workerBaseRef")
            .and_then(Value::as_str)
            .map(str::to_owned));
    }
    let projects = store
        .read_async("runtime-services/managed_child_create", |read| {
            studio_runtime_store::ProjectsRepository::list(read)
        })
        .await?;
    let directory = canonical_or_expanded(cwd);
    Ok(projects
        .into_iter()
        .filter_map(|(_, project)| {
            let worker_base = project.get("workerBaseRef")?.as_str()?;
            let path = project.get("path")?.as_str()?;
            let path = canonical_or_expanded(path);
            directory
                .starts_with(&path)
                .then_some((path.components().count(), worker_base.to_owned()))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, worker_base)| worker_base))
}

fn canonical_or_expanded(path: &str) -> PathBuf {
    studio_runtime_store::debug_assert_outside_sql();
    let expanded = if path == "~" {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(path))
    } else if let Some(suffix) = path.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(suffix))
            .unwrap_or_else(|| PathBuf::from(path))
    } else {
        PathBuf::from(path)
    };
    std::fs::canonicalize(&expanded).unwrap_or(expanded)
}

fn recheck_actor(
    tx: &Transaction<'_>,
    actor: &AgentRecord,
) -> Result<(), crate::managed_host_tools::Error> {
    let current =
        AgentRepository::get(tx, &actor.id)?.ok_or(crate::managed_host_tools::Error::Stale)?;
    if current.epoch != actor.epoch
        || current.root_id != actor.root_id
        || current.extra.get("accountKey") != actor.extra.get("accountKey")
        || current.extra.get("provider") != actor.extra.get("provider")
        || event_write::truthy(current.extra.get("deletedAt"))
        || !event_write::truthy(current.extra.get("autoWake"))
    {
        return Err(crate::managed_host_tools::Error::Stale);
    }
    Ok(())
}

fn active_agents(
    tx: &Transaction<'_>,
    root_id: &str,
) -> Result<i64, crate::managed_host_tools::Error> {
    Ok(studio_runtime_store::AgentIndexedReads::active_team_members(tx, root_id)?)
}

/// Preserve Python's best-effort execution history for a spawned child. The
/// child row itself is authoritative; a broken optional history table must not
/// turn successful child creation into a refusal.
fn record_spawn_effect(tx: &Transaction<'_>, actor: &AgentRecord, child: &Value, request_id: &str) {
    let result = (|| -> Result<(), rusqlite::Error> {
        tx.execute_batch("SAVEPOINT managed_spawn_execution_effect")?;
        let write = (|| -> Result<(), rusqlite::Error> {
            let has_runs: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_execution_runs')",
                [],
                |row| row.get(0),
            )?;
            if !has_runs {
                return Ok(());
            }
            let actor_value = actor.value();
            let account = actor_value
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default");
            let thread = actor_value.get("threadId").and_then(Value::as_str);
            let turn = actor_value.get("turnId").and_then(Value::as_str);
            let active_turn = turn.filter(|turn| !turn.is_empty());
            let run = if let Some(turn) = active_turn {
                let mut statement = tx.prepare_cached(
                    "SELECT id,record FROM runtime_execution_runs WHERE agent=?1 AND account=?2 AND thread IS ?3 AND turn=?4 LIMIT 2",
                )?;
                let rows = statement
                    .query_map(
                        rusqlite::params![actor.id.0, account, thread, turn],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                (rows.len() == 1).then(|| rows[0].clone())
            } else {
                tx.query_row(
                    "SELECT id,record FROM runtime_execution_runs WHERE agent=?1 AND account=?2 AND epoch=?3 AND turn IS NULL ORDER BY created DESC LIMIT 1",
                    rusqlite::params![actor.id.0, account, actor.epoch],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?
            };
            let Some((run_id, raw_run)) = run else {
                return Ok(());
            };
            let Ok(run) = serde_json::from_str::<Value>(&raw_run) else {
                return Ok(());
            };
            if active_turn.is_none()
                && matches!(
                    run.get("status").and_then(Value::as_str),
                    Some("completed" | "failed" | "interrupted" | "cancelled")
                )
            {
                return Ok(());
            }
            let Some(child_id) = child.get("id").and_then(Value::as_str) else {
                return Ok(());
            };
            let effect_id = format!("spawn:{child_id}");
            let previous: Option<String> = tx
                .query_row(
                    "SELECT record FROM runtime_execution_effects WHERE id=?1",
                    [&effect_id],
                    |row| row.get(0),
                )
                .optional()?;
            let previous: Option<Value> = previous.and_then(|raw| serde_json::from_str(&raw).ok());
            let effect_run = previous
                .as_ref()
                .and_then(|value| value.get("runId"))
                .and_then(Value::as_str)
                .unwrap_or(&run_id);
            let attempt_id = previous
                .as_ref()
                .and_then(|value| value.get("attemptId"))
                .cloned()
                .or_else(|| run.get("latestAttemptId").cloned())
                .unwrap_or(Value::Null);
            let effect = json!({"id":effect_id,"runId":effect_run,"kind":"spawn",
                "referenceId":child_id,"attemptId":attempt_id,"requestId":request_id,
                "status":child.get("status").cloned().unwrap_or(Value::Null)});
            tx.execute(
                "INSERT INTO runtime_execution_effects(id,run,kind,reference,record) VALUES(?1,?2,'spawn',?3,?4) ON CONFLICT(id) DO UPDATE SET run=excluded.run,kind=excluded.kind,reference=excluded.reference,record=excluded.record",
                rusqlite::params![effect_id, effect_run, child_id, effect.to_string()],
            )?;
            let node_id = format!("worker:{child_id}");
            let node = json!({"id":node_id,"runId":run_id,"kind":"managed_worker",
                "agentId":child_id,"status":child.get("status").cloned().unwrap_or(Value::Null)});
            tx.execute(
                "INSERT INTO runtime_execution_nodes(id,run,record) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET run=excluded.run,record=excluded.record",
                rusqlite::params![node_id, run_id, node.to_string()],
            )?;
            Ok(())
        })();
        if write.is_err()
            && let Err(error) = tx.execute_batch("ROLLBACK TO managed_spawn_execution_effect")
        {
            eprintln!("managed child execution history rollback failed: {error}");
        }
        tx.execute_batch("RELEASE managed_spawn_execution_effect")?;
        write
    })();
    if result.is_err() {
        eprintln!("managed child execution history could not be recorded");
    }
}

fn can_delegate(
    tx: &Transaction<'_>,
    actor: &AgentRecord,
    root: &AgentRecord,
) -> Result<bool, crate::managed_host_tools::Error> {
    if actor.extra.get("role").and_then(Value::as_str) == Some("reviewer") {
        return Ok(false);
    }
    let limit = root
        .extra
        .get("workerDefaults")
        .and_then(|value| value.get("delegationDepth"))
        .and_then(Value::as_u64)
        .filter(|value| (1..=3).contains(value))
        .unwrap_or(1);
    let mut cursor = actor.clone();
    let mut depth = 0_u64;
    let mut visited = std::collections::BTreeSet::new();
    while cursor.id != root.id {
        if !visited.insert(cursor.id.0.clone()) {
            return Ok(false);
        }
        let Some(parent_id) = cursor.extra.get("parentId").and_then(Value::as_str) else {
            return Ok(false);
        };
        cursor = AgentRepository::get(tx, &AgentId::from(parent_id))?
            .ok_or(crate::managed_host_tools::Error::Stale)?;
        if cursor.root_id != root.id {
            return Ok(false);
        }
        depth += 1;
        if depth >= limit {
            return Ok(false);
        }
    }
    Ok(true)
}

fn project_child(record: &Value, task_id: Option<&str>, worktree: bool) -> Value {
    let mut output = serde_json::Map::new();
    for field in [
        "id",
        "name",
        "status",
        "model",
        "effort",
        "fastMode",
        "accountKey",
        "provider",
        "cwd",
    ] {
        output.insert(
            field.to_owned(),
            record.get(field).cloned().unwrap_or(Value::Null),
        );
    }
    output.insert("worktree".to_owned(), json!(worktree));
    output.insert("environment".to_owned(), json!("host"));
    output.insert(
        "workspace".to_owned(),
        record
            .get("workspaceMode")
            .cloned()
            .unwrap_or_else(|| json!(if worktree { "worktree" } else { "shared" })),
    );
    output.insert("workspaceBackend".to_owned(), Value::Null);
    for (key, field) in [
        ("workspaceError", "imageWorkspaceError"),
        ("warning", "worktreeWarning"),
    ] {
        if let Some(value) = record
            .get(field)
            .filter(|value| event_write::truthy(Some(value)))
        {
            output.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(task) = task_id {
        output.insert("taskId".to_owned(), json!(task));
    }
    if worktree {
        output.insert(
            "baseRef".to_owned(),
            record.get("workerBaseRef").cloned().unwrap_or(Value::Null),
        );
        output.insert(
            "baseCommit".to_owned(),
            record
                .get("workerBaseCommit")
                .cloned()
                .unwrap_or(Value::Null),
        );
        if event_write::truthy(record.get("workerBaseBehindMain")) {
            let behind = &record["workerBaseBehindMain"];
            let main_ref = record
                .get("workerBaseMainRef")
                .cloned()
                .unwrap_or_else(|| json!("main"));
            output.insert("baseBehindMain".to_owned(), behind.clone());
            output.insert("baseMainRef".to_owned(), main_ref.clone());
            output.insert(
                "baseWarning".to_owned(),
                json!(format!(
                    "Base commit {} is {} commits behind {}",
                    record["workerBaseCommit"].as_str().unwrap_or(""),
                    behind,
                    main_ref.as_str().unwrap_or("main"),
                )),
            );
        }
    }
    Value::Object(output)
}

/// Base commit, ref, main ref and main lag of a child, as resolved by the host.
type WorkerBase = (String, String, Option<String>, Option<i64>);

/// Resolve the child's base commit. A worktree request in a project without a
/// repository or commits (and no explicit base ref) falls back to the host
/// folder, as lead creation does; the returned flag is the workspace used.
fn resolve_child_base(
    worktree: bool,
    image: bool,
    host_cwd: &Path,
    base_ref_request: Option<&str>,
) -> Result<(bool, WorkerBase), Error> {
    if !(worktree || (image && base_ref_request.is_some())) {
        return Ok((worktree, Default::default()));
    }
    match crate::host_worker_create::resolve_worker_base(host_cwd, base_ref_request) {
        Ok(base) => Ok((worktree, base)),
        Err(crate::agents_create::CreateError::BaseUnresolved)
            if worktree && base_ref_request.is_none() =>
        {
            Ok((false, Default::default()))
        }
        Err(error) => Err(map_create_error(error)),
    }
}

fn map_create_error(error: crate::agents_create::CreateError) -> Error {
    match error {
        crate::agents_create::CreateError::CapabilityUnavailable(name) => Error::Unavailable(name),
        crate::agents_create::CreateError::Store(error) => Error::Store(error),
        crate::agents_create::CreateError::BaseUnresolved => Error::Managed(
            crate::managed_host_tools::Error::Invalid("base commit could not be resolved"),
        ),
        crate::agents_create::CreateError::InvalidCreation(message) => {
            Error::Managed(crate::managed_host_tools::Error::Invalid(message))
        }
        _ => Error::Refused,
    }
}

fn map_recheck_error(
    error: crate::agents_create::MutationError,
) -> crate::managed_host_tools::Error {
    match error {
        crate::agents_create::MutationError::Store(error) => {
            crate::managed_host_tools::Error::Store(error)
        }
        _ => crate::managed_host_tools::Error::Stale,
    }
}

struct ReviewDraft {
    typed: AgentRecord,
    record: Value,
    epoch: i64,
    account: crate::host_worker_create::AccountIdentity,
    workspace_admission: Box<dyn crate::monitor_runtime::WorkspaceAdmission + Send + Sync>,
}

/// Reserve a native Codex review child. `nativeReview` is its durable first-turn
/// intent; the account worker claims it after the callback commits and wakes it.
pub(crate) async fn request_review(
    store: &RuntimeStore,
    actor: &crate::managed_host_tools::Actor,
    args: &Value,
    request_id: &str,
    now: f64,
    environment: &dyn ManagedCreationEnvironment,
) -> Result<crate::managed_host_tools::OperationResult, Error> {
    let actor_record = actor.record().clone();
    if actor_record
        .extra
        .get("executionMode")
        .and_then(Value::as_str)
        == Some("vm")
    {
        if actor_record.extra.get("isLead").and_then(Value::as_bool) != Some(true) {
            return Err(crate::managed_host_tools::Error::Invalid(
                "Only the lead can create a layr reviewer",
            )
            .into());
        }
        // VM reviewer preparation is not implemented by the production worker path yet.
        // Refuse before line.save so a request that cannot be fulfilled does not mutate
        // the lead's guest workspace.
        return Err(
            crate::managed_host_tools::Error::Unavailable("workspace-state/vm-workspace").into(),
        );
    }
    if studio_runtime_providers::ProviderKind::of_field(actor_record.extra.get("provider"))
        != Ok(studio_runtime_providers::ProviderKind::Codex)
    {
        return Err(crate::managed_host_tools::Error::Invalid(
            "Native review requires a Codex agent",
        )
        .into());
    }
    if actor_record
        .extra
        .get("nativeReview")
        .is_some_and(|value| event_write::truthy(Some(value)))
    {
        return Err(crate::managed_host_tools::Error::Invalid(
            "A native reviewer cannot create another review",
        )
        .into());
    }
    if event_write::truthy(actor_record.extra.get("daybreakEnabled")) {
        return Err(crate::managed_host_tools::Error::Invalid(
            "Native review cannot select Daybreak. Delegate a review task to a subagent instead",
        )
        .into());
    }
    if !now.is_finite() || now < 0.0 || request_id.is_empty() {
        return Err(crate::managed_host_tools::Error::Invalid("Invalid review identity").into());
    }
    let parent_value = actor_record.value();
    let root = stored_root(store, actor_record.root_id.clone()).await?;
    let root_value = root.value();
    let parent_cwd = parent_value.get("cwd").and_then(Value::as_str).ok_or(
        crate::managed_host_tools::Error::Invalid("Select a project directory"),
    )?;
    let mut directory_path = args
        .get("cwd")
        .and_then(Value::as_str)
        .map(|path| expand_review_home(path, std::env::var("HOME").ok().as_deref()))
        .unwrap_or_else(|| PathBuf::from(parent_cwd));
    if !directory_path.is_absolute() {
        directory_path = Path::new(parent_cwd).join(directory_path);
    }
    let directory = crate::agents_create::canonical_project_directory(&directory_path)
        .map_err(crate::managed_host_tools::Error::Invalid)?;
    let plan = crate::native_review::ReviewPlan::from_records(
        parent_value.clone(),
        root_value.clone(),
        args.clone(),
        request_id.to_owned(),
        directory.clone(),
    )
    .map_err(|_| crate::managed_host_tools::Error::Invalid("Invalid native review request"))?;
    let child_id = plan.spec().get("id").and_then(Value::as_str).ok_or(
        crate::managed_host_tools::Error::Invalid("Invalid native review request"),
    )?;
    let saved_id = child_id.to_owned();
    if let Some(saved) = store
        .read_async("runtime-services/managed_child_create", move |read| {
            studio_runtime_store::ContextRepairRepository::agent(read, &saved_id)
        })
        .await?
    {
        let value = plan.existing(&saved).map_err(|_| {
            crate::managed_host_tools::Error::Invalid("This request id has different content")
        })?;
        return Ok(crate::managed_host_tools::OperationResult {
            value,
            signals: Vec::new(),
        });
    }
    let account_key = parent_value
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let spec = plan.spec();
    let model = spec.get("model").and_then(Value::as_str).ok_or(
        crate::managed_host_tools::Error::Invalid("Invalid native review model"),
    )?;
    let (account_key, account) =
        environment.select_worker_account(crate::provider_callbacks::WorkerAccountRequest {
            parent_account_key: account_key,
            requested_account_key: Some(account_key),
            default_account_key: None,
            model,
            excluded_account_keys: &[],
            project_account_keys: None,
            active_agents: &std::collections::HashMap::new(),
        })?;
    // Review is read-only in the parent's managed workspace. Its requested
    // repository is validated separately and may differ from the actor's cwd.
    let workspace_admission = environment.prepare_workspace(
        store,
        &actor_record,
        &json!({"workspace":"shared"}),
        request_id,
    )?;
    let effort = spec.get("effort").and_then(Value::as_str);
    let fast_mode = spec
        .get("fast_mode")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (base_ref, base_commit, _, _) =
        crate::host_worker_create::resolve_worker_base(Path::new(&directory), None)
            .map_err(map_create_error)?;
    let project = stored_project(
        store,
        parent_value
            .get("projectId")
            .and_then(Value::as_str)
            .unwrap_or(parent_cwd),
        parent_cwd,
    )
    .await?;
    let body = json!({"account_key":account_key,"name":"Review","model":model,
        "effort":effort,"prompt":spec.get("prompt").and_then(Value::as_str).unwrap_or(""),
        "_workspaceInGit":parent_value.get("workspaceInGit").and_then(Value::as_bool).unwrap_or(false),
        "_imageWorkspaceHasGit":parent_value.get("workspaceInGit").and_then(Value::as_bool).unwrap_or(false)
            || parent_value.get("imageWorkspaceHasGit").and_then(Value::as_bool).unwrap_or(false)});
    let prepared = crate::host_worker_create::prepare_work_item(
        &body,
        Path::new(&directory),
        AgentId::from(child_id),
        actor_record.root_id.clone(),
        actor_record.id.clone(),
        &base_ref,
        &base_commit,
        actor_record.epoch,
        parent_cwd,
        parent_cwd,
        &account_key,
        parent_value.get("projectId"),
        parent_value.get("projectServerId"),
        project.as_ref(),
        &account,
        environment.catalog(),
        "reviewer",
        false,
        fast_mode,
        false,
    )
    .map_err(map_create_error)?;
    let mut record = crate::host_worker_create::persisted_record(&prepared)
        .map_err(map_create_error)?
        .value();
    record["provider"] = json!("codex");
    record["yoloMode"] = json!(false);
    record["worktree"] = json!(false);
    record["workspaceMode"] = json!("shared");
    record["workspaceInGit"] = json!(false);
    record["imageWorkspaceHasGit"] = json!(false);
    record["workerBaseRef"] = Value::Null;
    record["workerBaseCommit"] = Value::Null;
    record["maxAgents"] = root_value
        .get("maxAgents")
        .cloned()
        .unwrap_or(json!(DEFAULT_MAX_TEAM_AGENTS));
    record["tokenBudget"] = root_value
        .get("tokenBudget")
        .cloned()
        .unwrap_or(Value::Null);
    let review = ReviewDraft {
        typed: prepared,
        record,
        epoch: actor_record.epoch,
        account,
        workspace_admission,
    };
    let child_id = child_id.to_owned();
    let value = store
        .transaction_with_error(move |tx| -> Result<Value, crate::managed_host_tools::Error> {
            recheck_actor(tx, &actor_record)?;
            if AgentRepository::get(tx, &AgentId::from(child_id.as_str()))?.is_none() {
                let maximum = root_value
                    .get("maxAgents")
                    .and_then(Value::as_i64)
                    .unwrap_or(64);
                if active_agents(tx, &actor_record.root_id.0)? >= maximum {
                    return Err(crate::managed_host_tools::Error::Invalid(
                        "Team active agent limit reached; free a finished agent before creating a review",
                    ));
                }
            }
            let creator = ReviewCreator {
                actor: &actor_record,
                draft: &review,
            };
            let response =
                crate::native_review::reserve(tx, &plan, &creator).map_err(
                    |error| match error {
                        crate::native_actions::NativeError::Store(error) => {
                            crate::managed_host_tools::Error::Store(error)
                        }
                        crate::native_actions::NativeError::Unavailable(name) => {
                            crate::managed_host_tools::Error::Unavailable(name)
                        }
                        _ => crate::managed_host_tools::Error::Invalid(
                            "Native review creation was refused",
                        ),
                    },
                )?;
            let child = AgentRepository::get(tx, &AgentId::from(child_id.as_str()))?
                .ok_or(crate::managed_host_tools::Error::Stale)?;
            crate::agents_create::publish_agent(tx, &child.value())?;
            Ok(response)
        }).await
        .map_err(Error::from)?;
    Ok(crate::managed_host_tools::OperationResult {
        value,
        signals: vec![crate::managed_host_tools::WakeSignal::new("worker-driver")],
    })
}

struct ReviewCreator<'a> {
    actor: &'a AgentRecord,
    draft: &'a ReviewDraft,
}
impl crate::native_review::ReviewCreator for ReviewCreator<'_> {
    fn child(
        &self,
        tx: &Transaction<'_>,
        actor: &Value,
        spec: &Value,
    ) -> crate::native_actions::Result<Value> {
        if actor.get("id") != Some(&Value::String(self.actor.id.0.clone()))
            || spec.get("id") != Some(&Value::String(self.draft.typed.id.0.clone()))
        {
            return Err(crate::native_actions::NativeError::Changed);
        }
        self.draft
            .workspace_admission
            .check(tx, self.actor)
            .map_err(|_| crate::native_actions::NativeError::Changed)?;
        crate::host_worker_create::recheck_worker(
            tx,
            &self.draft.typed,
            self.draft.epoch,
            &self.draft.account,
        )
        .map_err(|_| crate::native_actions::NativeError::Changed)?;
        Ok(self.draft.record.clone())
    }
    fn receipt(
        &self,
        _: &Transaction<'_>,
        _: &str,
        _: &Value,
    ) -> crate::native_actions::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod vm_review_identity_tests {
    use super::{
        Error, WorkerBase, host_project_cwd, resolve_child_base, validate_image_workspace_platform,
        validate_vm_parent_workspace, vm_review_guest_ids,
    };
    use serde_json::json;

    #[test]
    fn guest_review_ids_are_bounded_by_the_named_limit() {
        let rust_guest = include_str!("linux_vm_guest.rs");
        assert!(rust_guest.contains("chars().count() > GUEST_REQUEST_ID_MAX_CHARS"));
        let long = "r".repeat(4096);
        for supplied in ["short-request", long.as_str()] {
            let (request_id, turn_id) = vm_review_guest_ids(supplied).unwrap();
            assert!(
                request_id.chars().count() <= crate::linux_vm_guest::GUEST_REQUEST_ID_MAX_CHARS
            );
            assert!(turn_id.chars().count() <= crate::linux_vm_guest::GUEST_REQUEST_ID_MAX_CHARS);
            assert!(request_id.starts_with("layr-review-state:"));
            assert!(turn_id.starts_with("review:"));
            assert_eq!(
                (request_id, turn_id),
                vm_review_guest_ids(supplied).unwrap(),
                "operation IDs are stable for the same source request"
            );
        }
    }

    #[test]
    fn native_parent_cannot_create_layr_child_without_panicking() {
        assert!(matches!(
            validate_vm_parent_workspace("layr", false),
            Err(Error::Managed(crate::managed_host_tools::Error::Invalid(
                "A VM workspace requires a VM parent"
            )))
        ));
        assert!(validate_vm_parent_workspace("layr", true).is_ok());
    }

    #[test]
    fn vm_child_rechecks_parent_project_with_host_cwd() {
        let root = json!({"cwd":"/guest/project","hostProjectPath":"/host/project"});
        assert_eq!(
            host_project_cwd(true, &root, "/guest/project"),
            "/host/project"
        );
        assert_eq!(
            host_project_cwd(false, &root, "/host/project"),
            "/host/project"
        );
    }

    #[test]
    fn image_child_refuses_on_unsupported_platform_before_creation() {
        assert!(matches!(
            validate_image_workspace_platform("image", false),
            Err(Error::Unavailable("workspace-state/image-provisioning"))
        ));
        assert!(validate_image_workspace_platform("image", true).is_ok());
        assert!(validate_image_workspace_platform("worktree", false).is_ok());
    }

    #[test]
    fn worktree_without_repository_falls_back_to_host() {
        let directory = tempfile::tempdir().unwrap();
        let (worktree, base) = resolve_child_base(true, false, directory.path(), None).unwrap();
        assert!(!worktree);
        assert_eq!(base, WorkerBase::default());
        assert!(resolve_child_base(true, false, directory.path(), Some("HEAD")).is_err());
    }
}
