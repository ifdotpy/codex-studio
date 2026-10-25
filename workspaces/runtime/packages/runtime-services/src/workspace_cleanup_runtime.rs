//! Durable cleanup effects for agent workspaces.
//!
//! A cleanup intent is committed before its filesystem or guest effect. A
//! process restart turns an unfinished intent into `unknown`; callers must
//! inspect the external workspace and must never resend that effect.
use std::{path::PathBuf, sync::Arc, time::Duration};
use studio_runtime_store::path_identity::CanonicalPath;

use crate::linux_vm_guest::GuestRpc;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore, StoreError, Transaction};
use thiserror::Error;

#[path = "workspace_cleanup_submodules.rs"]
mod submodules;

const INTENT_FIELD: &str = "workspaceCleanupIntent";
const GUEST_TIMEOUT: Duration = Duration::from_secs(3_600);

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CleanupAction {
    Delete,
    Archive,
}

/// Lifecycle writers persist this before calling the cleanup service. No
/// external effect has started while an operation remains `queued`.
pub fn queued_cleanup_intent(agent_id: &str, epoch: i64, action: CleanupAction) -> Value {
    let operation_id = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        json!([agent_id, epoch, action]).to_string().as_bytes(),
    );
    json!({"operationId":operation_id.to_string(),"action":action,"phase":"queued"})
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct WorkspaceCleanupResult {
    pub agent_id: String,
    pub action: CleanupAction,
    pub workspace: Option<Value>,
    pub worktree: Option<Value>,
    pub held: bool,
}

#[derive(Debug, Error)]
pub enum WorkspaceCleanupError {
    #[error("Invalid agent workspace cleanup record")]
    InvalidRecord,
    #[error("Workspace cleanup is held for inspection")]
    Held,
    #[error("Workspace cleanup effect was not sent")]
    NotSent,
    #[error("Worktree retained: {0}")]
    UnsafeSubmodule(String),
    #[error("Workspace cleanup worker failed")]
    Worker,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Effects are split into one durable intent each, so later effects cannot be
/// sent until the earlier receipt has been committed.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CleanupEffect {
    ArchiveImage,
    RemoveImage,
    SaveNativeTurn,
    ReleaseNativeLine,
    SaveVmTurn { turn_id: String },
    ReleaseVmLine { method: String },
    SaveWorktreeRef { identity: Value },
    RemoveWorktree { identity: Value },
}

#[derive(Clone, Debug, PartialEq)]
pub enum EffectFailure {
    NotSent,
    UnsafeSubmodule(String),
    Unknown,
}

/// Injectable boundary for deterministic corpus and fail-point tests.
pub trait CleanupPorts: Send + Sync {
    fn perform(
        &self,
        agent: &Value,
        effect: &CleanupEffect,
        request_id: &str,
    ) -> Result<Value, EffectFailure>;
}

/// Run the cleanup behavior for an already authorized stop, delete or archive
/// action. The caller commits the lifecycle decision first; this service owns
/// durable intent and workspace effects only.
pub async fn cleanup_agent_workspace(
    store: Arc<RuntimeStore>,
    agent_id: String,
    action: CleanupAction,
) -> Result<WorkspaceCleanupResult, WorkspaceCleanupError> {
    cleanup_agent_workspace_with_ports(
        Arc::clone(&store),
        agent_id,
        action,
        Arc::new(ProductionPorts {
            state_dir: store.root().to_path_buf(),
        }),
    )
    .await
}

pub async fn cleanup_agent_workspace_with_ports(
    store: Arc<RuntimeStore>,
    agent_id: String,
    action: CleanupAction,
    ports: Arc<dyn CleanupPorts>,
) -> Result<WorkspaceCleanupResult, WorkspaceCleanupError> {
    let agent = read_agent(&store, agent_id.clone()).await?;
    if agent.get("deletedAt").is_none() {
        return Err(WorkspaceCleanupError::InvalidRecord);
    }
    if action == CleanupAction::Archive
        && agent
            .get("agentArchive")
            .and_then(Value::as_object)
            .is_none()
    {
        return Err(WorkspaceCleanupError::InvalidRecord);
    }
    let effects = plan(&agent, action, store.state_dir())?;
    if effects.is_empty() {
        complete_without_effect(&store, &agent_id, action, &agent).await?;
        return Ok(result(&agent_id, action, None, None, false));
    }
    let mut workspace = None;
    let mut worktree = None;
    for effect in effects {
        let (request_id, replay) = begin_effect(&store, &agent_id, action, &effect, &agent).await?;
        if let Some(receipt) = replay {
            finalize_effect(&store, &agent_id, &request_id, &effect, &receipt).await?;
            if matches!(effect, CleanupEffect::RemoveWorktree { .. }) {
                worktree = Some(receipt);
            } else {
                workspace = Some(receipt);
            }
            continue;
        }
        let current = read_agent(&store, agent_id.clone()).await?;
        if current
            .pointer(&format!("/{INTENT_FIELD}/requestId"))
            .and_then(Value::as_str)
            != Some(request_id.as_str())
            || current
                .pointer(&format!("/{INTENT_FIELD}/phase"))
                .and_then(Value::as_str)
                != Some("pending")
            || lifecycle_identity(&current) != lifecycle_identity(&agent)
        {
            return Err(WorkspaceCleanupError::Held);
        }
        let effect_ports = Arc::clone(&ports);
        let effect_agent = current.clone();
        let effect_request_id = request_id.clone();
        let effect_for_worker = effect.clone();
        let outcome = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/workspace_cleanup",
            move || effect_ports.perform(&effect_agent, &effect_for_worker, &effect_request_id),
        )
        .await
        .unwrap_or(Err(EffectFailure::Unknown));
        match outcome {
            Ok(receipt) => {
                complete_effect(&store, &agent_id, &request_id, &effect, &receipt).await?;
                if matches!(effect, CleanupEffect::RemoveWorktree { .. }) {
                    worktree = Some(receipt);
                } else {
                    workspace = Some(receipt);
                }
            }
            Err(EffectFailure::NotSent) => {
                mark_effect(&store, &agent_id, &request_id, "not_sent", Value::Null).await?;
                return Err(WorkspaceCleanupError::NotSent);
            }
            Err(EffectFailure::UnsafeSubmodule(reason)) => {
                mark_effect(
                    &store,
                    &agent_id,
                    &request_id,
                    "not_sent",
                    json!({"reason":reason}),
                )
                .await?;
                return Err(WorkspaceCleanupError::UnsafeSubmodule(reason));
            }
            Err(EffectFailure::Unknown) => {
                mark_effect(&store, &agent_id, &request_id, "unknown", Value::Null).await?;
                return Err(WorkspaceCleanupError::Held);
            }
        }
    }
    finalize_cleanup(&store, &agent_id, action).await?;
    Ok(result(&agent_id, action, workspace, worktree, false))
}

async fn complete_without_effect(
    store: &RuntimeStore,
    agent_id: &str,
    action: CleanupAction,
    expected_agent: &Value,
) -> Result<(), WorkspaceCleanupError> {
    let agent_id = agent_id.to_owned();
    let expected_agent = expected_agent.clone();
    store
        .transaction_with_error(move |tx| {
            let mut current = load_agent(tx, &agent_id)?;
            let expected_identity = lifecycle_identity(&expected_agent);
            let current_identity = lifecycle_identity(&current);
            if current_identity != expected_identity
                || current.get(INTENT_FIELD) != expected_agent.get(INTENT_FIELD)
            {
                return Err(WorkspaceCleanupError::Held);
            }
            if let Some(intent) = current.get(INTENT_FIELD).filter(|intent| !intent.is_null())
                && (intent.get("action")
                    != Some(&serde_json::to_value(action).map_err(StoreError::from)?)
                    || !matches!(
                        intent.get("phase").and_then(Value::as_str),
                        Some("queued" | "written" | "not_sent")
                    )
                    || intent
                        .get("identity")
                        .is_some_and(|identity| *identity != current_identity))
            {
                return Err(WorkspaceCleanupError::Held);
            }
            if action == CleanupAction::Archive {
                // The planner emits no effects for an already absent owned
                // worktree. Only settle metadata; never mutate Git or a parent.
                if current.get("worktreeReady").and_then(Value::as_bool) == Some(true) {
                    current["worktreeReady"] = json!(false);
                }
                if current.get("imageWorkspace").and_then(Value::as_bool) == Some(true)
                    && current.get("imageWorkspaceReady").and_then(Value::as_bool) != Some(true)
                    && current.get("imageWorkspacePhase").and_then(Value::as_str)
                        != Some("archived")
                {
                    current["imageWorkspacePhase"] = json!("archived");
                }
                if current
                    .pointer("/agentArchive/cleanupPending")
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    current["agentArchive"]["cleanupPending"] = json!(false);
                }
            }
            current
                .as_object_mut()
                .ok_or(WorkspaceCleanupError::InvalidRecord)?
                .remove(INTENT_FIELD);
            AgentRepository::put(
                tx,
                &AgentRecord::from_value(current).map_err(StoreError::from)?,
            )?;
            Ok::<(), WorkspaceCleanupError>(())
        })
        .await?;
    Ok(())
}

async fn finalize_cleanup(
    store: &RuntimeStore,
    agent_id: &str,
    action: CleanupAction,
) -> Result<(), WorkspaceCleanupError> {
    let agent_id = agent_id.to_owned();
    store
        .transaction_with_error(move |tx| {
            let mut agent = load_agent(tx, &agent_id)?;
            let intent = agent
                .get(INTENT_FIELD)
                .cloned()
                .ok_or(WorkspaceCleanupError::InvalidRecord)?;
            if intent.get("action")
                != Some(&serde_json::to_value(action).map_err(StoreError::from)?)
                || !matches!(intent.get("phase").and_then(Value::as_str), Some("written"))
            {
                return Err(WorkspaceCleanupError::Held);
            }
            if let (Some(original), Some(current)) = (
                intent.get("identity").and_then(Value::as_object),
                Some(lifecycle_identity(&agent)),
            ) {
                for key in ["id", "epoch", "deletedAt", "archiveAt", "archiveEpoch"] {
                    if original.get(key) != current.get(key) {
                        return Err(WorkspaceCleanupError::Held);
                    }
                }
            }
            if action == CleanupAction::Archive
                && agent
                    .pointer("/agentArchive/cleanupPending")
                    .and_then(Value::as_bool)
                    == Some(true)
            {
                agent["agentArchive"]["cleanupPending"] = json!(false);
            }
            agent
                .as_object_mut()
                .ok_or(WorkspaceCleanupError::InvalidRecord)?
                .remove(INTENT_FIELD);
            AgentRepository::put(
                tx,
                &AgentRecord::from_value(agent).map_err(StoreError::from)?,
            )?;
            Ok::<(), WorkspaceCleanupError>(())
        })
        .await?;
    Ok(())
}

/// Reconcile state left by a crash. Unfinished `pending` effects are changed
/// to `unknown` and held. A `written` receipt is safe to replay from the
/// store: it only finalizes that receipt and continues with the next effect.
pub async fn reconcile_pending_cleanup(
    store: Arc<RuntimeStore>,
) -> Result<Vec<WorkspaceCleanupResult>, WorkspaceCleanupError> {
    let rows = store
        .read_async("runtime-services/workspace_cleanup", |reader| {
            studio_runtime_store::StartupRecoveryRepository::agents_with_cleanup_intent(reader)
        })
        .await?;
    let mut held = Vec::new();
    for (id, raw) in rows {
        match reconcile_cleanup_intent(&store, &id, &raw).await {
            Ok(cleanup) => held.push(cleanup),
            Err(error) => eprintln!("Workspace cleanup remains pending for {id}: {error}"),
        }
    }
    Ok(held)
}

// This is a view of a persisted agent record, whose other fields belong to
// AgentRecord. The intent's receipt/effect fields remain owned by the effect runner.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CleanupRecord {
    workspace_cleanup_intent: CleanupIntent,
}

#[derive(Deserialize)]
struct CleanupIntent {
    action: CleanupAction,
    #[serde(flatten)]
    progress: CleanupProgress,
}

#[derive(Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum CleanupProgress {
    Queued,
    Written,
    NotSent,
    Pending {
        #[serde(rename = "requestId")]
        request_id: String,
    },
    Unknown,
}

async fn reconcile_cleanup_intent(
    store: &Arc<RuntimeStore>,
    id: &str,
    raw: &str,
) -> Result<WorkspaceCleanupResult, WorkspaceCleanupError> {
    let record: CleanupRecord =
        serde_json::from_str(raw).map_err(|_| WorkspaceCleanupError::InvalidRecord)?;
    let intent = record.workspace_cleanup_intent;
    match intent.progress {
        CleanupProgress::Queued | CleanupProgress::Written | CleanupProgress::NotSent => {
            match cleanup_agent_workspace(Arc::clone(store), id.to_owned(), intent.action).await {
                Ok(cleanup) => return Ok(cleanup),
                Err(
                    WorkspaceCleanupError::Held
                    | WorkspaceCleanupError::NotSent
                    | WorkspaceCleanupError::UnsafeSubmodule(_),
                ) => {}
                Err(error) => return Err(error),
            }
        }
        CleanupProgress::Pending { request_id } => {
            mark_effect(store, id, &request_id, "unknown", Value::Null).await?;
        }
        CleanupProgress::Unknown => {}
    }
    Ok(result(id, intent.action, None, None, true))
}

fn plan(
    agent: &Value,
    action: CleanupAction,
    state_dir: &std::path::Path,
) -> Result<Vec<CleanupEffect>, WorkspaceCleanupError> {
    if agent
        .get(INTENT_FIELD)
        .is_some_and(|intent| !intent.is_null())
    {
        let intent_action: CleanupAction = serde_json::from_value(
            agent[INTENT_FIELD]
                .get("action")
                .cloned()
                .ok_or(WorkspaceCleanupError::InvalidRecord)?,
        )
        .map_err(|_| WorkspaceCleanupError::InvalidRecord)?;
        if intent_action != action {
            return Err(WorkspaceCleanupError::Held);
        }
        let phase = agent[INTENT_FIELD].get("phase").and_then(Value::as_str);
        if !matches!(phase, Some("queued" | "not_sent" | "written")) {
            return Err(WorkspaceCleanupError::Held);
        }
    }
    let mut effects = Vec::new();
    let is_vm = agent.get("executionMode").and_then(Value::as_str) == Some("vm");

    let vm_dispose = is_vm
        && (agent.get("layrReady").and_then(Value::as_bool) == Some(true)
            || action == CleanupAction::Archive);
    if crate::layr_native::is_native(agent) && agent["layrReady"] == true {
        effects.push(CleanupEffect::SaveNativeTurn);
        effects.push(CleanupEffect::ReleaseNativeLine);
    } else if vm_dispose {
        if let Some(turns) = agent.get("layrTurnSaves").and_then(Value::as_object) {
            for (turn, state) in turns {
                if state.get("status").and_then(Value::as_str) != Some("saved") {
                    effects.push(CleanupEffect::SaveVmTurn {
                        turn_id: turn.clone(),
                    });
                }
            }
        }
        effects.push(CleanupEffect::ReleaseVmLine {
            method: if agent.get("isLead").and_then(Value::as_bool) == Some(true) {
                "agent.release".into()
            } else {
                "line.remove".into()
            },
        });
    } else if action == CleanupAction::Archive
        && agent.get("worktreeReady").and_then(Value::as_bool) == Some(true)
    {
        let Some(identity) = worktree_identity(agent, state_dir)? else {
            return Ok(effects);
        };
        effects.push(CleanupEffect::SaveWorktreeRef {
            identity: identity.clone(),
        });
        effects.push(CleanupEffect::RemoveWorktree { identity });
    }
    Ok(effects)
}

/// The batch archive preflight uses the same original-link, clean-tree and
/// nested-worktree checks as the effect owner. Ignored build output is removable.
pub(crate) fn check_archive_worktree(
    agent: &Value,
    state_dir: &std::path::Path,
) -> Result<(), WorkspaceCleanupError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Workspace {
        #[serde(default)]
        worktree_ready: bool,
    }
    let workspace: Workspace =
        serde_json::from_value(agent.clone()).map_err(|_| WorkspaceCleanupError::InvalidRecord)?;
    if workspace.worktree_ready
        && let Some(identity) = worktree_identity(agent, state_dir)?
    {
        checked_cleanup_worktree(&identity).map_err(|_| WorkspaceCleanupError::NotSent)?;
    }
    Ok(())
}

/// The owned worktree root of worker `id` that contains `cwd`
/// (`crate::worktree_root`). Layout only; the caller proves ownership.
fn owned_worktree_root<'a>(
    cwd: &'a std::path::Path,
    id: &str,
) -> Result<crate::worktree_root::OwnedRoot<'a>, WorkspaceCleanupError> {
    crate::worktree_root::owned_root(cwd, id).ok_or(WorkspaceCleanupError::InvalidRecord)
}

/// The main worktree of the repository that owns linked worktree `root`.
fn owning_repository(root: &std::path::Path) -> Result<PathBuf, WorkspaceCleanupError> {
    let common = String::from_utf8(
        git_output(
            root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            None,
        )
        .map_err(|_| WorkspaceCleanupError::InvalidRecord)?,
    )
    .map_err(|_| WorkspaceCleanupError::InvalidRecord)?;
    let common = CanonicalPath::existing(common.trim_end_matches('\n'))
        .map_err(|_| WorkspaceCleanupError::InvalidRecord)?;
    if common.file_name().and_then(|name| name.to_str()) != Some(".git") {
        return Err(WorkspaceCleanupError::InvalidRecord);
    }
    common
        .parent()
        .map(std::path::Path::to_path_buf)
        .ok_or(WorkspaceCleanupError::InvalidRecord)
}

fn worktree_identity(
    agent: &Value,
    _state_dir: &std::path::Path,
) -> Result<Option<Value>, WorkspaceCleanupError> {
    #[derive(Deserialize)]
    struct WorktreeOwner {
        id: String,
        cwd: PathBuf,
        /// The canonical Git common directory recorded when Studio provisioned
        /// the worktree.
        #[serde(default, rename = "worktreeCommonDir")]
        common_dir: Option<PathBuf>,
    }
    let owner: WorktreeOwner =
        serde_json::from_value(agent.clone()).map_err(|_| WorkspaceCleanupError::InvalidRecord)?;
    let lexical = studio_runtime_store::path_identity::LexicalPath::exact(&owner.cwd)
        .ok_or(WorkspaceCleanupError::InvalidRecord)?;
    let lexical_owned = owned_worktree_root(&lexical, &owner.id)?;
    let lexical_root = lexical_owned.root();
    // Inspect the exact owned root, not a canonicalized parent or a missing cwd
    // subdirectory. A dangling link is still present and must stay held. The
    // worker directory itself must never be a link: cleanup would act on its
    // target, which may belong to another repository. Ancestors may be links.
    match std::fs::symlink_metadata(lexical_root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(WorkspaceCleanupError::InvalidRecord),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(WorkspaceCleanupError::InvalidRecord);
        }
        Ok(_) => {}
    }
    let id = owner.id.as_str();
    let cwd =
        CanonicalPath::existing(&owner.cwd).map_err(|_| WorkspaceCleanupError::InvalidRecord)?;
    let owned = owned_worktree_root(&cwd, id)?;
    // The recorded spelling and the resolved directory must name the same
    // worker directory; otherwise a substituted path redirects cleanup.
    let resolved_root =
        CanonicalPath::existing(lexical_root).map_err(|_| WorkspaceCleanupError::InvalidRecord)?;
    if resolved_root.as_path() != owned.root() {
        return Err(WorkspaceCleanupError::InvalidRecord);
    }
    let root = owned.root();
    // Git registration proves repository membership, not Studio ownership: a
    // replaced ancestor can lead to another repository's registered worktree.
    // Require the Git common directory Studio provisioned from. Records from
    // before that field existed are accepted only in the `<repository>/.worktrees`
    // layout of their recorded path.
    let expected = match (&owner.common_dir, lexical_owned) {
        (Some(recorded), _) => CanonicalPath::existing(recorded),
        (None, crate::worktree_root::OwnedRoot::Repository { repository, .. }) => {
            CanonicalPath::existing(repository.join(".git"))
        }
        (None, crate::worktree_root::OwnedRoot::Linked(_)) => {
            return Err(WorkspaceCleanupError::InvalidRecord);
        }
    }
    .map_err(|_| WorkspaceCleanupError::InvalidRecord)?;
    let repo = owning_repository(root)?;
    if repo.join(".git").as_path() != expected.as_path() {
        return Err(WorkspaceCleanupError::InvalidRecord);
    }
    let repo = repo.as_path();
    let relative = cwd
        .strip_prefix(root)
        .map_err(|_| WorkspaceCleanupError::InvalidRecord)?;
    let head = String::from_utf8(
        git_output(&cwd, &["rev-parse", "HEAD"], None)
            .map_err(|_| WorkspaceCleanupError::InvalidRecord)?,
    )
    .map_err(|_| WorkspaceCleanupError::InvalidRecord)?
    .trim()
    .to_owned();
    if head.is_empty() {
        return Err(WorkspaceCleanupError::InvalidRecord);
    }
    let registered_branch = registered_worktree_branch(repo, root)?;
    Ok(Some(
        json!({"id":id,"cwd":cwd,"root":root,"repository":repo,"relative":relative,"head":head,
        "branch":registered_branch,"lifecycleIdentity":[agent.get("epoch"),agent.get("cwd"),agent.get("deletedAt")],
        "epoch":agent.get("epoch"),"deletedAt":agent.get("deletedAt"),
        "archive":agent.get("agentArchive")}),
    ))
}

fn registered_worktree_branch(
    repository: &std::path::Path,
    target_root: &std::path::Path,
) -> Result<Option<String>, WorkspaceCleanupError> {
    let listing = String::from_utf8(
        git_output(repository, &["worktree", "list", "--porcelain", "-z"], None)
            .map_err(|_| WorkspaceCleanupError::InvalidRecord)?,
    )
    .map_err(|_| WorkspaceCleanupError::InvalidRecord)?;
    let mut path: Option<PathBuf> = None;
    let mut branch = None;
    let mut detached = false;
    let finish_entry = |path: Option<&PathBuf>, branch: &Option<String>, detached: bool| {
        if path.is_some_and(|path| path == target_root) {
            if branch.is_some() || detached {
                Some(branch.clone())
            } else {
                None
            }
        } else {
            None
        }
    };
    for field in listing.split('\0').filter(|field| !field.is_empty()) {
        if let Some(value) = field.strip_prefix("worktree ") {
            if let Some(result) = finish_entry(path.as_ref(), &branch, detached) {
                return Ok(result);
            }
            path = CanonicalPath::existing(PathBuf::from(value))
                .ok()
                .map(CanonicalPath::into_path_buf);
            branch = None;
            detached = false;
        } else if field == "detached" {
            detached = true;
        } else if let Some(value) = field.strip_prefix("branch refs/heads/") {
            branch = Some(value.to_owned());
        }
    }
    finish_entry(path.as_ref(), &branch, detached).ok_or(WorkspaceCleanupError::InvalidRecord)
}

async fn read_agent(
    store: &RuntimeStore,
    agent_id: String,
) -> Result<Value, WorkspaceCleanupError> {
    store
        .read_async("runtime-services/workspace_cleanup", move |tx| {
            load_agent(tx, &agent_id)
        })
        .await
}

fn load_agent(tx: &Transaction<'_>, agent_id: &str) -> Result<Value, WorkspaceCleanupError> {
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [agent_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(StoreError::from)?;
    let raw = raw.ok_or(WorkspaceCleanupError::InvalidRecord)?;
    let mut agent = serde_json::from_str(&raw).map_err(|_| WorkspaceCleanupError::InvalidRecord)?;
    studio_runtime_store::agent_volatile::merge(tx, agent_id, &mut agent)?;
    Ok(agent)
}

async fn begin_effect(
    store: &RuntimeStore,
    agent_id: &str,
    action: CleanupAction,
    effect: &CleanupEffect,
    expected_agent: &Value,
) -> Result<(String, Option<Value>), WorkspaceCleanupError> {
    let agent_id = agent_id.to_owned();
    let effect = effect.clone();
    let expected_agent = expected_agent.clone();
    let result = store.transaction_with_error(move |tx| {
        let mut agent = load_agent(tx, &agent_id)?;
        let identity = lifecycle_identity(&expected_agent);
        if lifecycle_identity(&agent) != identity {
            return Err(WorkspaceCleanupError::Held);
        }
        let mut attempt = 0_u64;
        if let Some(intent) = agent.get(INTENT_FIELD).filter(|v| !v.is_null()) {
            if intent.get("action") == Some(&serde_json::to_value(action).map_err(StoreError::from)?)
                && intent.get("effect") == Some(&serde_json::to_value(&effect).map_err(StoreError::from)?)
                && intent.get("phase").and_then(Value::as_str) == Some("not_sent")
            {
                attempt = intent.get("attempt").and_then(Value::as_u64).unwrap_or(0) + 1;
            }
            let phase = intent.get("phase").and_then(Value::as_str);
            if intent.get("identity").is_some_and(|previous| *previous != identity) {
                return Err(WorkspaceCleanupError::Held);
            }
            let same_effect = intent.get("action") == Some(&serde_json::to_value(action).map_err(StoreError::from)?)
                && intent.get("effect") == Some(&serde_json::to_value(&effect).map_err(StoreError::from)?);
            if phase == Some("written") && same_effect {
                let request_id = intent
                    .get("requestId")
                    .and_then(Value::as_str)
                    .ok_or(WorkspaceCleanupError::InvalidRecord)?
                    .to_owned();
                return Ok((
                    request_id,
                    Some(intent.get("receipt").cloned().unwrap_or(Value::Null)),
                ));
            }
            if phase == Some("queued") {
                if intent.get("action") != Some(&serde_json::to_value(action).map_err(StoreError::from)?) {
                    return Err(WorkspaceCleanupError::Held);
                }
            } else if phase == Some("not_sent") {
                if !same_effect {
                    return Err(WorkspaceCleanupError::Held);
                }
                attempt = intent.get("attempt").and_then(Value::as_u64).unwrap_or(0) + 1;
            } else if phase == Some("written") {
                attempt = 0;
            } else {
                return Err(WorkspaceCleanupError::Held);
            }
        } else {
            attempt = 0;
        }
        let request_id = uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_OID,
            json!([agent_id, action, effect, attempt]).to_string().as_bytes(),
        )
        .to_string();
        agent[INTENT_FIELD] = json!({"requestId":request_id,"attempt":attempt,"action":action,"effect":effect,"identity":identity,"phase":"pending","receipt":null});
        AgentRepository::put(tx, &AgentRecord::from_value(agent).map_err(StoreError::from)?)?;
        Ok((request_id, None))
    }).await?;
    Ok(result)
}

fn lifecycle_identity(agent: &Value) -> Value {
    json!({
        "id":agent.get("id"),
        "epoch":agent.get("epoch"),
        "cwd":agent.get("cwd"),
        "deletedAt":agent.get("deletedAt"),
        "archiveAt":agent.pointer("/agentArchive/at"),
        "archiveEpoch":agent.pointer("/agentArchive/epoch")
    })
}

async fn complete_effect(
    store: &RuntimeStore,
    agent_id: &str,
    request_id: &str,
    effect: &CleanupEffect,
    receipt: &Value,
) -> Result<(), WorkspaceCleanupError> {
    let agent_id = agent_id.to_owned();
    let request_id = request_id.to_owned();
    let effect = effect.clone();
    let receipt = receipt.clone();
    let completed = store
        .transaction_with_error(move |tx| {
            let mut agent = load_agent(tx, &agent_id)?;
            let intent_value = agent
                .get(INTENT_FIELD)
                .cloned()
                .ok_or(WorkspaceCleanupError::InvalidRecord)?;
            if intent_value.get("requestId").and_then(Value::as_str) != Some(request_id.as_str()) {
                return Ok::<bool, WorkspaceCleanupError>(false);
            }
            if intent_value.get("identity") != Some(&lifecycle_identity(&agent)) {
                agent[INTENT_FIELD]["phase"] = json!("unknown");
                AgentRepository::put(
                    tx,
                    &AgentRecord::from_value(agent).map_err(StoreError::from)?,
                )?;
                return Ok(false);
            }
            if !valid_receipt(&effect, &receipt) {
                agent[INTENT_FIELD]["phase"] = json!("unknown");
                agent[INTENT_FIELD]["receipt"] = receipt.clone();
                AgentRepository::put(
                    tx,
                    &AgentRecord::from_value(agent).map_err(StoreError::from)?,
                )?;
                return Ok(false);
            }
            let intent = agent
                .get_mut(INTENT_FIELD)
                .and_then(Value::as_object_mut)
                .ok_or(WorkspaceCleanupError::InvalidRecord)?;
            intent.insert("phase".into(), json!("written"));
            intent.insert("receipt".into(), receipt.clone());
            apply_effect_result(&mut agent, &effect, &receipt)?;
            let updated_identity = lifecycle_identity(&agent);
            agent[INTENT_FIELD]["identity"] = updated_identity;
            AgentRepository::put(
                tx,
                &AgentRecord::from_value(agent).map_err(StoreError::from)?,
            )?;
            Ok(true)
        })
        .await?;
    if !completed {
        return Err(WorkspaceCleanupError::Held);
    }
    Ok(())
}

fn valid_receipt(effect: &CleanupEffect, receipt: &Value) -> bool {
    match effect {
        CleanupEffect::SaveNativeTurn | CleanupEffect::SaveVmTurn { .. } => receipt
            .get("stateId")
            .and_then(Value::as_str)
            .is_some_and(|state_id| !state_id.is_empty()),
        _ => true,
    }
}

async fn finalize_effect(
    store: &RuntimeStore,
    agent_id: &str,
    request_id: &str,
    effect: &CleanupEffect,
    receipt: &Value,
) -> Result<(), WorkspaceCleanupError> {
    let agent_id = agent_id.to_owned();
    let request_id = request_id.to_owned();
    let effect = effect.clone();
    let receipt = receipt.clone();
    store
        .transaction_with_error(move |tx| {
            let mut agent = load_agent(tx, &agent_id)?;
            let intent = agent
                .get(INTENT_FIELD)
                .cloned()
                .ok_or(WorkspaceCleanupError::InvalidRecord)?;
            if agent
                .pointer(&format!("/{INTENT_FIELD}/requestId"))
                .and_then(Value::as_str)
                != Some(request_id.as_str())
                || intent.get("identity") != Some(&lifecycle_identity(&agent))
            {
                return Err(WorkspaceCleanupError::Held);
            }
            apply_effect_result(&mut agent, &effect, &receipt)?;
            AgentRepository::put(
                tx,
                &AgentRecord::from_value(agent).map_err(StoreError::from)?,
            )?;
            Ok::<(), WorkspaceCleanupError>(())
        })
        .await?;
    Ok(())
}

fn apply_effect_result(
    agent: &mut Value,
    effect: &CleanupEffect,
    receipt: &Value,
) -> Result<(), WorkspaceCleanupError> {
    match effect {
        CleanupEffect::ArchiveImage | CleanupEffect::RemoveImage => {}
        CleanupEffect::ReleaseNativeLine | CleanupEffect::ReleaseVmLine { .. } => {
            agent["layrReady"] = json!(false);
        }
        CleanupEffect::RemoveWorktree { identity } => {
            let repo = identity
                .get("repository")
                .cloned()
                .ok_or(WorkspaceCleanupError::InvalidRecord)?;
            let relative = identity
                .get("relative")
                .cloned()
                .ok_or(WorkspaceCleanupError::InvalidRecord)?;
            agent["cwd"] = json!(
                std::path::PathBuf::from(
                    repo.as_str().ok_or(WorkspaceCleanupError::InvalidRecord)?
                )
                .join(
                    relative
                        .as_str()
                        .ok_or(WorkspaceCleanupError::InvalidRecord)?
                )
                .to_string_lossy()
            );
            agent["worktreeReady"] = json!(false);
            agent["cleanedWorktree"] = json!({
                "root": identity.get("root"),
                "repo": identity.get("repository"),
                "relative": identity.get("relative"),
                "branch": identity.get("branch").and_then(Value::as_str),
                "head": identity.get("head"),
                "identity": identity.get("lifecycleIdentity"),
                "bytes": receipt.get("bytes").cloned().unwrap_or(Value::Null)
            });
            agent
                .as_object_mut()
                .ok_or(WorkspaceCleanupError::InvalidRecord)?
                .remove("worktreeCleanup");
        }
        CleanupEffect::SaveNativeTurn => {
            agent["layrStateId"] = receipt["stateId"].clone();
            if let Some(turns) = agent["layrTurnSaves"].as_object_mut() {
                for save in turns.values_mut().filter(|save| save["status"] != "saved") {
                    *save = json!({"status":"saved","stateId":receipt["stateId"]});
                }
            }
        }
        CleanupEffect::SaveWorktreeRef { .. } => {}
        CleanupEffect::SaveVmTurn { turn_id } => {
            agent["layrTurnSaves"][turn_id] =
                json!({"status":"saved","stateId":receipt.get("stateId")});
            agent["layrStateId"] = receipt.get("stateId").cloned().unwrap_or(Value::Null);
        }
    }
    Ok(())
}

async fn mark_effect(
    store: &RuntimeStore,
    agent_id: &str,
    request_id: &str,
    phase: &str,
    receipt: Value,
) -> Result<(), WorkspaceCleanupError> {
    let agent_id = agent_id.to_owned();
    let request_id = request_id.to_owned();
    let phase = phase.to_owned();
    store
        .transaction_with_error(move |tx| {
            let mut agent = load_agent(tx, &agent_id)?;
            let intent = agent
                .get_mut(INTENT_FIELD)
                .and_then(Value::as_object_mut)
                .ok_or(WorkspaceCleanupError::InvalidRecord)?;
            if intent.get("requestId").and_then(Value::as_str) != Some(request_id.as_str()) {
                return Err(WorkspaceCleanupError::Held);
            }
            intent.insert("phase".into(), json!(phase));
            intent.insert("receipt".into(), receipt.clone());
            AgentRepository::put(
                tx,
                &AgentRecord::from_value(agent).map_err(StoreError::from)?,
            )?;
            Ok::<(), WorkspaceCleanupError>(())
        })
        .await?;
    Ok(())
}

fn result(
    agent_id: &str,
    action: CleanupAction,
    workspace: Option<Value>,
    worktree: Option<Value>,
    held: bool,
) -> WorkspaceCleanupResult {
    WorkspaceCleanupResult {
        agent_id: agent_id.to_owned(),
        action,
        workspace,
        worktree,
        held,
    }
}

fn guest_failure(error: crate::linux_vm_guest::GuestError) -> EffectFailure {
    use crate::linux_vm_guest::GuestError;

    match error {
        GuestError::InvalidRequest | GuestError::Identity => EffectFailure::NotSent,
        GuestError::Transport {
            uncertain: false, ..
        }
        | GuestError::Protocol {
            uncertain: false, ..
        } => EffectFailure::NotSent,
        _ => EffectFailure::Unknown,
    }
}

struct ProductionPorts {
    state_dir: PathBuf,
}
impl CleanupPorts for ProductionPorts {
    fn perform(
        &self,
        agent: &Value,
        effect: &CleanupEffect,
        request_id: &str,
    ) -> Result<Value, EffectFailure> {
        match effect {
            CleanupEffect::SaveNativeTurn => {
                crate::layr_native::save(&self.state_dir, agent, request_id)
                    .map_err(|_| EffectFailure::Unknown)
            }
            CleanupEffect::ReleaseNativeLine => {
                crate::layr_native::remove(&self.state_dir, agent, request_id)
                    .map_err(|_| EffectFailure::Unknown)
            }
            // Historical image records keep their files. New cleanup plans never emit these effects.
            CleanupEffect::ArchiveImage | CleanupEffect::RemoveImage => {
                Ok(json!({"state":"kept","reason":"legacy image workspace"}))
            }
            CleanupEffect::SaveVmTurn { turn_id } => {
                let client =
                    crate::linux_vm_guest::UnixGuestRpc::for_runtime_state(&self.state_dir);
                let id = agent["id"].as_str().ok_or(EffectFailure::NotSent)?;
                let value = client
                    .call(
                        "line.save",
                        &json!({"agentId":id,"turnId":turn_id}),
                        Some(request_id),
                        GUEST_TIMEOUT,
                    )
                    .map_err(guest_failure)?;
                let state_id = value
                    .get("stateId")
                    .and_then(Value::as_str)
                    .filter(|state_id| !state_id.is_empty())
                    .ok_or(EffectFailure::Unknown)?;
                Ok(json!({"stateId":state_id}))
            }
            CleanupEffect::ReleaseVmLine { method } => {
                let client =
                    crate::linux_vm_guest::UnixGuestRpc::for_runtime_state(&self.state_dir);
                client
                    .call(
                        method,
                        &json!({"agentId":agent["id"]}),
                        Some(request_id),
                        GUEST_TIMEOUT,
                    )
                    .map_err(guest_failure)
            }
            CleanupEffect::SaveWorktreeRef { identity } => {
                save_worktree_archive_ref(identity)?;
                Ok(json!({"state":"saved"}))
            }
            CleanupEffect::RemoveWorktree { identity } => {
                remove_worktree(identity)?;
                Ok(json!({"state":"removed","bytes":null}))
            }
        }
    }
}

fn git_output(
    repository: &std::path::Path,
    args: &[&str],
    input: Option<&[u8]>,
) -> Result<Vec<u8>, ()> {
    let environment = [
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_OPTIONAL_LOCKS", "0"),
        ("GIT_ALLOW_PROTOCOL", ""),
    ];
    let mut safe_args = vec!["-c", "protocol.allow=never"];
    safe_args.extend_from_slice(args);
    let output = crate::bounded_child::run_without_git_environment(crate::bounded_child::Request {
        program: std::ffi::OsStr::new("git"),
        git_directory: Some(repository.as_os_str()),
        args: &safe_args,
        environment: &environment,
        input,
        stdout_limit: 1024 * 1024,
        stderr_limit: 4096,
        timeout: Duration::from_secs(300),
    })
    .map_err(|_| ())?;
    if output.stdout.truncated {
        return Err(());
    }
    Ok(output.stdout.bytes)
}

fn identity_paths(identity: &Value) -> Result<(PathBuf, PathBuf, String, String), ()> {
    let repo = identity
        .get("repository")
        .and_then(Value::as_str)
        .ok_or(())?;
    let root = identity.get("root").and_then(Value::as_str).ok_or(())?;
    let id = identity.get("id").and_then(Value::as_str).ok_or(())?;
    let head = identity.get("head").and_then(Value::as_str).ok_or(())?;
    Ok((
        PathBuf::from(repo),
        PathBuf::from(root),
        id.to_owned(),
        head.to_owned(),
    ))
}

fn check_worktree(identity: &Value) -> Result<(PathBuf, PathBuf, String, String), ()> {
    let (repo, root, id, saved_head) = identity_paths(identity)?;
    let repository_root =
        String::from_utf8(git_output(&repo, &["rev-parse", "--show-toplevel"], None)?)
            .map_err(|_| ())?;
    if CanonicalPath::existing(PathBuf::from(repository_root.trim())).map_err(|_| ())?
        != CanonicalPath::existing(&repo).map_err(|_| ())?
    {
        return Err(());
    }
    if root.is_symlink() || !root.is_dir() || root.join(".git").is_symlink() {
        return Err(());
    }
    if !std::fs::symlink_metadata(root.join(".git"))
        .map_err(|_| ())?
        .file_type()
        .is_file()
    {
        return Err(());
    }
    let git_link = std::fs::read_to_string(root.join(".git")).map_err(|_| ())?;
    let linked_gitdir = git_link
        .strip_prefix("gitdir: ")
        .and_then(|path| path.strip_suffix('\n'))
        .filter(|path| !path.is_empty() && !path.contains('\n'))
        .map(PathBuf::from)
        .ok_or(())?;
    if !linked_gitdir.is_absolute() {
        return Err(());
    }
    let actual_gitdir = String::from_utf8(git_output(
        &root,
        &["rev-parse", "--absolute-git-dir"],
        None,
    )?)
    .map_err(|_| ())?;
    if CanonicalPath::existing(PathBuf::from(actual_gitdir.trim())).map_err(|_| ())?
        != CanonicalPath::existing(&linked_gitdir).map_err(|_| ())?
        || !CanonicalPath::existing(&linked_gitdir)
            .map_err(|_| ())?
            .starts_with(CanonicalPath::existing(repo.join(".git/worktrees")).map_err(|_| ())?)
    {
        return Err(());
    }
    let listing = String::from_utf8(git_output(
        &repo,
        &["worktree", "list", "--porcelain", "-z"],
        None,
    )?)
    .map_err(|_| ())?;
    let mut entry = std::collections::BTreeMap::new();
    let mut belongs = false;
    for field in listing.split('\0').filter(|field| !field.is_empty()) {
        if let Some((key, value)) = field.split_once(' ') {
            if key == "worktree" {
                if belongs {
                    break;
                }
                entry.clear();
                belongs = CanonicalPath::existing(PathBuf::from(value)).ok()
                    == CanonicalPath::existing(&root).ok();
            }
            if belongs {
                entry.insert(key, value);
            }
        } else if field == "bare" {
            continue;
        }
    }
    if !belongs || entry.get("HEAD").copied() != Some(saved_head.as_str()) {
        return Err(());
    }
    let branch = identity.get("branch").and_then(Value::as_str);
    if entry
        .get("branch")
        .and_then(|v| v.strip_prefix("refs/heads/"))
        != branch
    {
        return Err(());
    }
    for field in listing
        .split('\0')
        .filter(|field| field.starts_with("worktree "))
    {
        let registered_path = PathBuf::from(field.trim_start_matches("worktree "));
        if let Ok(registered_path) = CanonicalPath::existing(&registered_path)
            && registered_path != root
            && registered_path.starts_with(&root)
        {
            return Err(());
        }
    }
    let cwd = identity.get("cwd").and_then(Value::as_str).ok_or(())?;
    let relative = identity.get("relative").and_then(Value::as_str).ok_or(())?;
    if PathBuf::from(cwd)
        .strip_prefix(&root)
        .map_err(|_| ())?
        .to_string_lossy()
        != relative
    {
        return Err(());
    }
    Ok((repo, root, id, saved_head))
}

fn checked_cleanup_worktree(
    identity: &Value,
) -> Result<(PathBuf, PathBuf, String, String), EffectFailure> {
    let checked = check_worktree(identity).map_err(|_| EffectFailure::NotSent)?;
    submodules::inspect(&checked.1, &checked.0).map_err(EffectFailure::UnsafeSubmodule)?;
    let status = git_output(
        &checked.1,
        &[
            "status",
            "--porcelain",
            "--untracked-files=normal",
            "--ignore-submodules=none",
        ],
        None,
    )
    .map_err(|_| EffectFailure::NotSent)?;
    if !status.is_empty() {
        return Err(EffectFailure::NotSent);
    }
    Ok(checked)
}

fn save_worktree_archive_ref(identity: &Value) -> Result<(), EffectFailure> {
    let (repo, _root, id, head) = checked_cleanup_worktree(identity)?;
    let current = format!("refs/codex-agents/archive/{id}");
    let previous = String::from_utf8(
        git_output(
            &repo,
            &["for-each-ref", "--format=%(objectname)", &current],
            None,
        )
        .map_err(|_| EffectFailure::Unknown)?,
    )
    .map_err(|_| EffectFailure::Unknown)?;
    let previous = previous.trim();
    let zero = "0".repeat(head.len());
    let mut commands = String::from("start\n");
    if !previous.is_empty() && previous != head {
        let history = format!("refs/codex-agents/archive-history/{id}/{previous}");
        let existing = String::from_utf8(
            git_output(
                &repo,
                &["for-each-ref", "--format=%(objectname)", &history],
                None,
            )
            .map_err(|_| EffectFailure::Unknown)?,
        )
        .map_err(|_| EffectFailure::Unknown)?;
        if existing.trim().is_empty() {
            commands.push_str(&format!("create {history} {previous}\n"));
        } else if existing.trim() != previous {
            return Err(EffectFailure::Unknown);
        }
    }
    commands.push_str(&format!(
        "update {current} {head} {}\nprepare\ncommit\n",
        if previous.is_empty() {
            zero.as_str()
        } else {
            previous
        }
    ));
    git_output(&repo, &["update-ref", "--stdin"], Some(commands.as_bytes()))
        .map_err(|_| EffectFailure::Unknown)?;
    Ok(())
}

fn remove_worktree(identity: &Value) -> Result<(), EffectFailure> {
    let (repo, root, _, _) = checked_cleanup_worktree(identity)?;
    let root = root.to_str().ok_or(EffectFailure::NotSent)?;
    git_output(&repo, &["worktree", "remove", "--force", root], None)
        .map_err(|_| EffectFailure::Unknown)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};
    use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore};

    use super::{
        CleanupAction, CleanupEffect, CleanupPorts, EffectFailure, WorkspaceCleanupError,
        begin_effect, cleanup_agent_workspace_with_ports, complete_effect, queued_cleanup_intent,
        reconcile_pending_cleanup,
    };

    struct FakePorts {
        outcomes: Mutex<Vec<Result<Value, EffectFailure>>>,
        calls: Mutex<Vec<(CleanupEffect, String)>>,
    }

    impl FakePorts {
        fn new(outcomes: Vec<Result<Value, EffectFailure>>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl CleanupPorts for FakePorts {
        fn perform(
            &self,
            _agent: &Value,
            effect: &CleanupEffect,
            request_id: &str,
        ) -> Result<Value, EffectFailure> {
            self.calls
                .lock()
                .expect("calls mutex")
                .push((effect.clone(), request_id.to_owned()));
            self.outcomes.lock().expect("outcomes mutex").remove(0)
        }
    }

    async fn fixture() -> (tempfile::TempDir, Arc<RuntimeStore>) {
        let (directory, _root, store) =
            crate::remote_transport::tests::runtime_store_fixture().await;
        (directory, store)
    }

    async fn put_agent(store: &RuntimeStore, mut record: Value) {
        record["id"] = json!("cleanup-agent");
        record["rootId"] = json!("cleanup-agent");
        record["epoch"] = json!(7);
        record["status"] = json!("paused");
        record["deletedAt"] = json!(7);
        if record["agentArchive"].is_object() {
            record["agentArchive"]
                .as_object_mut()
                .expect("archive object")
                .entry("at")
                .or_insert(json!(7));
            record["agentArchive"]
                .as_object_mut()
                .expect("archive object")
                .entry("epoch")
                .or_insert(json!(7));
        }
        let agent = AgentRecord::from_value(record).expect("agent record parses");
        store
            .transaction(move |tx| AgentRepository::put(tx, &agent))
            .await
            .expect("agent stores");
    }

    fn agent(store: &RuntimeStore) -> Value {
        let record = store
            .test_reader()
            .expect("test reader")
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='cleanup-agent'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("agent reads");
        serde_json::from_str(&record).expect("stored agent parses")
    }

    #[test]
    fn shared_workspace_agent_never_plans_a_directory_removal() {
        let agent = json!({
            "id":"shared-child", "cwd":"/project", "workspaceMode":"shared",
            "worktree":false,
        });
        for action in [CleanupAction::Archive, CleanupAction::Delete] {
            assert!(
                super::plan(&agent, action, std::path::Path::new("/nonexistent-state"))
                    .expect("plan")
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn missing_archive_worktree_settles_without_external_effects() {
        let (directory, store) = fixture().await;
        let cwd = directory
            .path()
            .join("project/.worktrees/codex-agents/cleanup-agent");
        put_agent(&store, json!({
            "cwd":cwd, "worktreeReady":true,
            "agentArchive":{"cleanupPending":true},
            "workspaceCleanupIntent":super::queued_cleanup_intent("cleanup-agent", 7, CleanupAction::Archive)
        })).await;
        let ports = Arc::new(FakePorts::new(vec![]));
        let cleaned = cleanup_agent_workspace_with_ports(
            Arc::clone(&store),
            "cleanup-agent".into(),
            CleanupAction::Archive,
            ports.clone(),
        )
        .await
        .expect("missing workspace already cleaned");
        assert!(!cleaned.held);
        assert!(ports.calls.lock().expect("calls").is_empty());
        let saved = agent(&store);
        assert_eq!(saved["worktreeReady"], false);
        assert_eq!(saved["cwd"], json!(cwd));
        assert_eq!(saved["agentArchive"]["cleanupPending"], false);
        assert!(saved.get("workspaceCleanupIntent").is_none());
    }

    #[tokio::test]
    async fn missing_worktree_subdirectory_does_not_settle_a_live_root() {
        let (directory, store) = fixture().await;
        let root = directory
            .path()
            .join("project/.worktrees/codex-agents/cleanup-agent");
        std::fs::create_dir_all(&root).expect("live root");
        let marker = root.join("owned.txt");
        std::fs::write(&marker, "keep").expect("live marker");
        put_agent(&store, json!({
            "cwd":root.join("missing-subdirectory"), "worktreeReady":true,
            "agentArchive":{"cleanupPending":true},
            "workspaceCleanupIntent":super::queued_cleanup_intent("cleanup-agent", 7, CleanupAction::Archive)
        })).await;
        let ports = Arc::new(FakePorts::new(vec![]));
        assert!(
            cleanup_agent_workspace_with_ports(
                Arc::clone(&store),
                "cleanup-agent".into(),
                CleanupAction::Archive,
                ports.clone()
            )
            .await
            .is_err()
        );
        assert!(ports.calls.lock().expect("calls").is_empty());
        assert_eq!(
            std::fs::read_to_string(marker).expect("live marker"),
            "keep"
        );
        assert_eq!(agent(&store)["worktreeReady"], true);
        assert!(agent(&store).get("workspaceCleanupIntent").is_some());
    }

    #[tokio::test]
    async fn cleanup_failure_does_not_block_unrelated_queued_intents() {
        let (directory, store) = fixture().await;
        put_agent(&store, json!({
            "cwd":directory.path(), "worktreeReady":true,
            "agentArchive":{"cleanupPending":true},
            "workspaceCleanupIntent":super::queued_cleanup_intent("cleanup-agent", 7, CleanupAction::Archive)
        })).await;
        let other = AgentRecord::from_value(json!({
            "id":"z-unrelated", "rootId":"z-unrelated", "epoch":1, "status":"paused", "deletedAt":1,
            "workspaceCleanupIntent":super::queued_cleanup_intent("z-unrelated", 1, CleanupAction::Delete)
        })).expect("unrelated record");
        store
            .transaction(move |tx| AgentRepository::put(tx, &other))
            .await
            .expect("seed unrelated");
        super::reconcile_pending_cleanup(Arc::clone(&store))
            .await
            .expect("isolate failure");
        assert!(
            agent(&store).get("workspaceCleanupIntent").is_some(),
            "failed intent is retained"
        );
        let raw: String = store
            .test_reader()
            .expect("reader")
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='z-unrelated'",
                [],
                |row| row.get(0),
            )
            .expect("unrelated row");
        let saved: Value = serde_json::from_str(&raw).expect("record");
        assert!(
            saved.get("workspaceCleanupIntent").is_none(),
            "unrelated intent drains"
        );
    }

    #[tokio::test]
    async fn archive_orders_workspace_effects_and_clears_cleanup_pending() {
        let (_directory, store) = fixture().await;
        put_agent(
            &store,
            json!({
                "executionMode":"vm", "layrReady":true,
                "layrTurnSaves":{"turn-1":{"status":"pending"}},
                "agentArchive":{"cleanupPending":true}
            }),
        )
        .await;
        let ports = Arc::new(FakePorts::new(vec![
            Ok(json!({"stateId":"state-1"})),
            Ok(json!({"stateId":"state-2"})),
        ]));
        let result = cleanup_agent_workspace_with_ports(
            Arc::clone(&store),
            "cleanup-agent".to_owned(),
            CleanupAction::Archive,
            ports.clone(),
        )
        .await
        .expect("archive cleanup completes");
        assert!(!result.held);
        let calls = ports.calls.lock().expect("calls mutex");
        assert!(matches!(calls[0].0, CleanupEffect::SaveVmTurn { .. }));
        assert!(matches!(calls[1].0, CleanupEffect::ReleaseVmLine { .. }));
        assert_ne!(calls[0].1, calls[1].1);
        let saved = agent(&store);
        assert_eq!(saved.pointer("/layrReady"), Some(&json!(false)));
        assert_eq!(
            saved.pointer("/agentArchive/cleanupPending"),
            Some(&json!(false))
        );
        assert_eq!(
            saved.pointer("/layrTurnSaves/turn-1/status"),
            Some(&json!("saved"))
        );
    }

    #[tokio::test]
    async fn unknown_effect_is_held_after_restart_and_never_resent() {
        let (_directory, store) = fixture().await;
        put_agent(
            &store,
            json!({
                "executionMode":"vm", "layrReady":true,
                "layrTurnSaves":{"turn-1":{"status":"pending"}},
                "agentArchive":{"cleanupPending":true}
            }),
        )
        .await;
        let ports = Arc::new(FakePorts::new(vec![Err(EffectFailure::Unknown)]));
        assert!(matches!(
            cleanup_agent_workspace_with_ports(
                Arc::clone(&store),
                "cleanup-agent".to_owned(),
                CleanupAction::Archive,
                ports.clone()
            )
            .await,
            Err(WorkspaceCleanupError::Held)
        ));
        drop(store);
        let store = Arc::new(
            RuntimeStore::open(Some(_directory.path()))
                .await
                .expect("store restarts"),
        );
        let held = reconcile_pending_cleanup(Arc::clone(&store))
            .await
            .expect("unknown intent reconciles");
        assert_eq!(held.len(), 1);
        assert!(held[0].held);
        assert_eq!(
            cleanup_agent_workspace_with_ports(
                Arc::clone(&store),
                "cleanup-agent".to_owned(),
                CleanupAction::Archive,
                ports.clone()
            )
            .await
            .expect_err("unknown blocks replay")
            .to_string(),
            "Workspace cleanup is held for inspection"
        );
        assert_eq!(ports.calls.lock().expect("calls mutex").len(), 1);
        assert_eq!(
            agent(&store).pointer("/workspaceCleanupIntent/phase"),
            Some(&json!("unknown"))
        );
    }

    #[tokio::test]
    async fn panicking_effect_is_marked_unknown_and_stays_held_after_reopen() {
        struct CrashPort;
        impl CleanupPorts for CrashPort {
            fn perform(
                &self,
                _agent: &Value,
                _effect: &CleanupEffect,
                _request_id: &str,
            ) -> Result<Value, EffectFailure> {
                panic!("simulated response loss after the durable pending write");
            }
        }

        let (directory, store) = fixture().await;
        put_agent(
            &store,
            json!({"executionMode":"vm", "layrReady":true,
                "layrTurnSaves":{"turn-1":{"status":"pending"}},
                "agentArchive":{"cleanupPending":true}}),
        )
        .await;
        assert!(matches!(
            cleanup_agent_workspace_with_ports(
                Arc::clone(&store),
                "cleanup-agent".to_owned(),
                CleanupAction::Archive,
                Arc::new(CrashPort),
            )
            .await,
            Err(WorkspaceCleanupError::Held)
        ));
        assert_eq!(
            agent(&store).pointer("/workspaceCleanupIntent/phase"),
            Some(&json!("unknown"))
        );
        drop(store);
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("state reopens"),
        );
        let held = reconcile_pending_cleanup(Arc::clone(&store))
            .await
            .expect("pending effect reconciles");
        assert_eq!(held.len(), 1);
        assert!(held[0].held);
        assert_eq!(
            agent(&store).pointer("/workspaceCleanupIntent/phase"),
            Some(&json!("unknown"))
        );
    }

    #[tokio::test]
    async fn legacy_image_cleanup_preserves_files_and_record_fields() {
        let (_directory, store) = fixture().await;
        put_agent(
            &store,
            json!({"imageWorkspace":true,"imageWorkspaceReady":true}),
        )
        .await;
        let ports = Arc::new(FakePorts::new(vec![]));
        cleanup_agent_workspace_with_ports(
            Arc::clone(&store),
            "cleanup-agent".into(),
            CleanupAction::Delete,
            ports.clone(),
        )
        .await
        .expect("metadata cleanup");
        assert!(
            ports
                .calls
                .lock()
                .expect("fixture operation succeeds")
                .is_empty()
        );
        assert_eq!(agent(&store)["imageWorkspaceReady"], true);
    }

    #[tokio::test]
    async fn written_retry_replays_the_persisted_attempt_without_resending() {
        let (_directory, store) = fixture().await;
        put_agent(
            &store,
            json!({
                "executionMode":"vm", "layrReady":true,
                "agentArchive":{"cleanupPending":true}
            }),
        )
        .await;
        let action = CleanupAction::Archive;
        let effect = CleanupEffect::ReleaseVmLine {
            method: "line.remove".into(),
        };
        let request_id = uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_OID,
            json!(["cleanup-agent", action, effect, 1_u64])
                .to_string()
                .as_bytes(),
        )
        .to_string();
        let mut record = agent(&store);
        record["workspaceCleanupIntent"] = json!({
            "requestId":request_id,
            "attempt":1,
            "action":action,
            "effect":effect,
            "identity":super::lifecycle_identity(&record),
            "phase":"written",
            "receipt":{"state":"released"}
        });
        store
            .transaction(move |tx| {
                AgentRepository::put(tx, &AgentRecord::from_value(record).expect("record parses"))
            })
            .await
            .expect("written effect stages");

        let ports = Arc::new(FakePorts::new(vec![]));
        cleanup_agent_workspace_with_ports(
            Arc::clone(&store),
            "cleanup-agent".to_owned(),
            action,
            ports.clone(),
        )
        .await
        .expect("written release replays from its receipt");
        assert!(ports.calls.lock().expect("calls mutex").is_empty());
        let saved = agent(&store);
        assert_eq!(saved["agentArchive"]["cleanupPending"], false);
        assert!(saved.get("workspaceCleanupIntent").is_none());
    }

    #[tokio::test]
    async fn late_receipt_after_lifecycle_change_is_held_without_applying_it() {
        let (_directory, store) = fixture().await;
        put_agent(
            &store,
            json!({"imageWorkspace":true,"imageWorkspaceReady":true}),
        )
        .await;
        let expected = agent(&store);
        let effect = CleanupEffect::RemoveImage;
        let (request_id, replay) = begin_effect(
            &store,
            "cleanup-agent",
            CleanupAction::Delete,
            &effect,
            &expected,
        )
        .await
        .expect("intent commits");
        assert!(replay.is_none());
        let mut changed = agent(&store);
        changed["epoch"] = json!(8);
        store
            .transaction(move |tx| {
                AgentRepository::put(
                    tx,
                    &AgentRecord::from_value(changed).expect("record parses"),
                )
            })
            .await
            .expect("new lifecycle commits");
        assert!(matches!(
            complete_effect(
                &store,
                "cleanup-agent",
                &request_id,
                &effect,
                &json!({"state":"removed"})
            )
            .await,
            Err(WorkspaceCleanupError::Held)
        ));
        let saved = agent(&store);
        assert_eq!(saved["epoch"], 8);
        assert_eq!(saved["imageWorkspaceReady"], true);
        assert_eq!(saved["workspaceCleanupIntent"]["phase"], "unknown");
    }

    #[tokio::test]
    async fn malformed_vm_save_receipt_holds_before_release() {
        let (_directory, store) = fixture().await;
        put_agent(
            &store,
            json!({
                "executionMode":"vm", "layrReady":true,
                "layrTurnSaves":{"turn-1":{"status":"pending"}},
                "agentArchive":{"cleanupPending":true}
            }),
        )
        .await;
        let ports = Arc::new(FakePorts::new(vec![Ok(json!({"stateId":""}))]));
        assert!(matches!(
            cleanup_agent_workspace_with_ports(
                Arc::clone(&store),
                "cleanup-agent".to_owned(),
                CleanupAction::Archive,
                ports.clone()
            )
            .await,
            Err(WorkspaceCleanupError::Held)
        ));
        assert_eq!(ports.calls.lock().expect("calls mutex").len(), 1);
        let saved = agent(&store);
        assert_eq!(saved["layrReady"], true);
        assert_eq!(saved["workspaceCleanupIntent"]["phase"], "unknown");
    }

    #[tokio::test]
    async fn no_effect_completion_preserves_a_replacement_lifecycle_and_intent() {
        let (_directory, store) = fixture().await;
        put_agent(&store, json!({"agentArchive":{"cleanupPending":true}})).await;
        let mut queued = agent(&store);
        queued["workspaceCleanupIntent"] =
            queued_cleanup_intent("cleanup-agent", 7, CleanupAction::Archive);
        store
            .transaction(move |tx| {
                AgentRepository::put(tx, &AgentRecord::from_value(queued).expect("queued record"))
            })
            .await
            .expect("intent stages");
        let expected = agent(&store);
        let mut replacement = expected.clone();
        replacement["epoch"] = json!(8);
        replacement["workspaceCleanupIntent"]["operationId"] = json!("replacement-operation");
        store
            .transaction(move |tx| {
                AgentRepository::put(
                    tx,
                    &AgentRecord::from_value(replacement).expect("replacement record"),
                )
            })
            .await
            .expect("replacement lifecycle commits");

        assert!(matches!(
            super::complete_without_effect(
                &store,
                "cleanup-agent",
                CleanupAction::Archive,
                &expected
            )
            .await,
            Err(WorkspaceCleanupError::Held)
        ));
        let saved = agent(&store);
        assert_eq!(saved["epoch"], 8);
        assert_eq!(saved["agentArchive"]["cleanupPending"], true);
        assert_eq!(
            saved["workspaceCleanupIntent"]["operationId"],
            "replacement-operation"
        );
    }

    #[tokio::test]
    async fn guest_failures_are_retryable_only_when_proven_pre_send() {
        use crate::linux_vm_guest::GuestError;

        assert_eq!(
            super::guest_failure(GuestError::Transport {
                message: "guest socket is unavailable".into(),
                uncertain: false,
            }),
            EffectFailure::NotSent
        );
        assert_eq!(
            super::guest_failure(GuestError::Protocol {
                message: "request exceeds the frame limit",
                uncertain: false,
            }),
            EffectFailure::NotSent
        );
        assert_eq!(
            super::guest_failure(GuestError::Rejected {
                code: Some("line_release_failed".into()),
                detail: None,
                uncertain: false,
            }),
            EffectFailure::Unknown
        );
        assert_eq!(
            super::guest_failure(GuestError::Transport {
                message: "response lost".into(),
                uncertain: true,
            }),
            EffectFailure::Unknown
        );
    }

    #[tokio::test]
    async fn definite_not_sent_retry_uses_a_new_guest_request_identity() {
        let (_directory, store) = fixture().await;
        put_agent(
            &store,
            json!({
                "executionMode":"vm", "layrReady":true,
                "layrTurnSaves":{"turn-1":{"status":"pending"}},
                "agentArchive":{"cleanupPending":true}
            }),
        )
        .await;
        let ports = Arc::new(FakePorts::new(vec![
            Err(EffectFailure::NotSent),
            Ok(json!({"stateId":"state-1"})),
            Ok(json!({"stateId":"state-2"})),
        ]));
        assert!(matches!(
            cleanup_agent_workspace_with_ports(
                Arc::clone(&store),
                "cleanup-agent".to_owned(),
                CleanupAction::Archive,
                ports.clone()
            )
            .await,
            Err(WorkspaceCleanupError::NotSent)
        ));
        cleanup_agent_workspace_with_ports(
            Arc::clone(&store),
            "cleanup-agent".to_owned(),
            CleanupAction::Archive,
            ports.clone(),
        )
        .await
        .expect("definitely unsent effect retries");
        let calls = ports.calls.lock().expect("calls mutex");
        assert_ne!(calls[0].1, calls[1].1);
        assert!(matches!(calls[2].0, CleanupEffect::ReleaseVmLine { .. }));
    }

    #[tokio::test]
    async fn archive_saves_a_fixture_repository_ref_before_removing_its_worktree() {
        let (directory, store) = fixture().await;
        // Git reports the repository in its resolved form.
        let repository = studio_runtime_store::path_identity::resolve(directory.path())
            .expect("resolved fixture folder")
            .join("project");
        let root = repository.join(".worktrees/codex-agents/cleanup-agent");
        std::fs::create_dir_all(root.parent().expect("worktree parent creates"))
            .expect("fixture parent creates");
        fixture_git(
            directory.path(),
            &["init", "-q", repository.to_str().expect("repository path")],
        );
        let git = |args: &[&str]| fixture_git(&repository, args);
        std::fs::write(repository.join("README.md"), "fixture\n").expect("fixture file writes");
        git(&["add", "README.md"]);
        git(&["commit", "-q", "-m", "fixture"]);
        let previous = git(&["rev-parse", "HEAD"]);
        std::fs::write(repository.join("README.md"), "second fixture\n")
            .expect("second fixture file writes");
        git(&["commit", "-q", "-am", "second fixture"]);
        git(&[
            "update-ref",
            "refs/codex-agents/archive/cleanup-agent",
            &previous,
        ]);
        git(&[
            "update-ref",
            &format!("refs/codex-agents/archive-history/cleanup-agent/{previous}"),
            &previous,
        ]);
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "cleanup-agent",
            root.to_str().expect("fixture path is UTF-8"),
        ]);
        let head = git(&["rev-parse", "cleanup-agent"]);
        put_agent(
            &store,
            json!({
                "cwd":root, "branch":"cleanup-agent", "worktreeReady":true,
                "agentArchive":{"at":7,"epoch":7,"cleanupPending":true}
            }),
        )
        .await;
        super::cleanup_agent_workspace(
            Arc::clone(&store),
            "cleanup-agent".to_owned(),
            CleanupAction::Archive,
        )
        .await
        .expect("fixture worktree archives");
        assert!(!root.exists(), "target worktree is removed");
        assert_eq!(
            git(&["rev-parse", "refs/codex-agents/archive/cleanup-agent"]),
            head
        );
        let saved = agent(&store);
        assert_eq!(saved["worktreeReady"], false);
        assert_eq!(
            saved["cleanedWorktree"]["repo"],
            repository.to_string_lossy().to_string()
        );
        assert!(saved["cleanedWorktree"]["identity"].is_array());
        assert_eq!(
            saved.pointer("/agentArchive/cleanupPending"),
            Some(&json!(false))
        );
    }

    /// Archive a clean fixture worktree at `root` while `configured` names the
    /// worktree root in `worktree-root.json`.
    async fn archive_fixture_worktree_at(
        directory: &tempfile::TempDir,
        store: &Arc<RuntimeStore>,
        repository: &std::path::Path,
        root: &std::path::Path,
        configured: &std::path::Path,
    ) {
        std::fs::create_dir_all(configured).expect("configured root creates");
        std::fs::write(
            store
                .state_dir()
                .join(crate::worktree_root::WORKTREE_ROOT_FILE),
            json!({ "root": configured }).to_string(),
        )
        .expect("worktree root configuration writes");
        std::fs::create_dir_all(root.parent().expect("worktree parent"))
            .expect("fixture parent creates");
        fixture_git(
            directory.path(),
            &["init", "-q", repository.to_str().expect("repository path")],
        );
        let git = |args: &[&str]| fixture_git(repository, args);
        std::fs::write(repository.join("README.md"), "fixture\n").expect("fixture file writes");
        git(&["add", "README.md"]);
        git(&["commit", "-q", "-m", "fixture"]);
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "cleanup-agent",
            root.to_str().expect("fixture path is UTF-8"),
        ]);
        let head = git(&["rev-parse", "cleanup-agent"]);
        put_agent(
            store,
            json!({
                "cwd":root, "branch":"cleanup-agent", "worktreeReady":true,
                "worktreeCommonDir":repository.join(".git"),
                "agentArchive":{"at":7,"epoch":7,"cleanupPending":true}
            }),
        )
        .await;
        super::cleanup_agent_workspace(
            Arc::clone(store),
            "cleanup-agent".to_owned(),
            CleanupAction::Archive,
        )
        .await
        .expect("fixture worktree archives");
        assert!(!root.exists(), "target worktree is removed");
        assert_eq!(
            git(&["rev-parse", "refs/codex-agents/archive/cleanup-agent"]),
            head
        );
        let saved = agent(store);
        assert_eq!(saved["worktreeReady"], false);
        assert_eq!(
            saved["cleanedWorktree"]["repo"],
            repository.to_string_lossy().to_string()
        );
    }

    #[tokio::test]
    async fn archive_removes_a_worktree_under_the_configured_root() {
        let (directory, store) = fixture().await;
        let base = studio_runtime_store::path_identity::resolve(directory.path())
            .expect("resolved fixture folder");
        let repository = base.join("project");
        let configured = base.join("other-disk/worktrees");
        let root = configured.join("codex-agents/cleanup-agent");
        archive_fixture_worktree_at(&directory, &store, &repository, &root, &configured).await;
    }

    #[tokio::test]
    async fn archive_keeps_repository_default_worktrees_manageable_after_configuration() {
        let (directory, store) = fixture().await;
        let base = studio_runtime_store::path_identity::resolve(directory.path())
            .expect("resolved fixture folder");
        let repository = base.join("project");
        let root = repository.join(".worktrees/codex-agents/cleanup-agent");
        let configured = base.join("other-disk/worktrees");
        archive_fixture_worktree_at(&directory, &store, &repository, &root, &configured).await;
    }

    #[tokio::test]
    async fn archive_manages_a_configured_root_named_like_the_default() {
        let (directory, store) = fixture().await;
        let base = studio_runtime_store::path_identity::resolve(directory.path())
            .expect("resolved fixture folder");
        let repository = base.join("project");
        let configured = base.join("disk/.worktrees");
        let root = configured.join("codex-agents/cleanup-agent");
        archive_fixture_worktree_at(&directory, &store, &repository, &root, &configured).await;
    }

    fn fixture_repository(base: &std::path::Path, name: &str) -> std::path::PathBuf {
        let repository = base.join(name);
        fixture_git(base, &["init", "-q", repository.to_str().expect("path")]);
        std::fs::write(repository.join("README.md"), "fixture\n").expect("file writes");
        fixture_git(&repository, &["add", "README.md"]);
        fixture_git(&repository, &["commit", "-q", "-m", "fixture"]);
        repository
    }

    /// A worker provisioned from a lead's linked worktree records the shared
    /// common directory, which cleanup must accept.
    #[test]
    fn worker_provisioned_from_a_linked_source_stays_owned() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let base = studio_runtime_store::path_identity::resolve(directory.path())
            .expect("resolved fixture folder");
        let repository = fixture_repository(&base, "project");
        let lead = base.join("lead-checkout");
        fixture_git(
            &repository,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                lead.to_str().expect("lead path"),
            ],
        );
        let root = base.join("configured/codex-agents/cleanup-agent");
        std::fs::create_dir_all(root.parent().expect("workers")).expect("workers create");
        fixture_git(
            &lead,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "cleanup-agent",
                root.to_str().expect("root path"),
            ],
        );
        let common = fixture_git(
            &lead,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        );
        let agent = json!({"id": "cleanup-agent", "cwd": root, "worktreeCommonDir": common.trim()});
        let identity = super::worktree_identity(&agent, &base)
            .expect("linked-source worker is owned")
            .expect("worker worktree identity");
        assert_eq!(identity["repository"], json!(repository));
    }

    /// A legacy worker reached through a `.worktrees` link to another disk,
    /// recorded before the common directory existed, stays owned.
    #[test]
    fn legacy_worker_under_a_linked_default_root_stays_owned() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let base = studio_runtime_store::path_identity::resolve(directory.path())
            .expect("resolved fixture folder");
        let repository = fixture_repository(&base, "project");
        let disk = base.join("other-disk");
        std::fs::create_dir_all(disk.join("codex-agents")).expect("disk root creates");
        std::os::unix::fs::symlink(&disk, repository.join(".worktrees")).expect("root link");
        let root = repository.join(".worktrees/codex-agents/cleanup-agent");
        fixture_git(
            &repository,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "cleanup-agent",
                root.to_str().expect("root path"),
            ],
        );
        let agent = json!({"id": "cleanup-agent", "cwd": root});
        let identity = super::worktree_identity(&agent, &base)
            .expect("legacy linked-root worker is owned")
            .expect("worker worktree identity");
        assert_eq!(identity["repository"], json!(repository));
    }

    /// A replaced `codex-agents` ancestor that leads to another repository's
    /// registered worktree must not pass ownership, with or without a record.
    #[test]
    fn substituted_worker_ancestor_is_refused_and_target_survives() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let base = studio_runtime_store::path_identity::resolve(directory.path())
            .expect("resolved fixture folder");
        let mut repositories = Vec::new();
        for name in ["project", "foreign"] {
            let repository = base.join(name);
            fixture_git(&base, &["init", "-q", repository.to_str().expect("path")]);
            std::fs::write(repository.join("README.md"), "fixture\n").expect("file writes");
            fixture_git(&repository, &["add", "README.md"]);
            fixture_git(&repository, &["commit", "-q", "-m", "fixture"]);
            repositories.push(repository);
        }
        let target = repositories[1].join(".worktrees/codex-agents/cleanup-agent");
        fixture_git(
            &repositories[1],
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                target.to_str().expect("target path"),
            ],
        );
        let configured = base.join("configured");
        std::fs::create_dir_all(&configured).expect("configured root creates");
        std::os::unix::fs::symlink(
            target.parent().expect("foreign workers"),
            configured.join("codex-agents"),
        )
        .expect("substitute ancestor link creates");
        let cwd = configured.join("codex-agents/cleanup-agent");
        for agent in [
            json!({"id": "cleanup-agent", "cwd": cwd, "worktreeCommonDir": repositories[0].join(".git")}),
            json!({"id": "cleanup-agent", "cwd": cwd}),
        ] {
            assert!(matches!(
                super::worktree_identity(&agent, &base),
                Err(WorkspaceCleanupError::InvalidRecord)
            ));
        }
        assert!(target.join("README.md").exists());
    }

    #[test]
    fn worktree_outside_a_worker_directory_is_not_owned() {
        for path in [
            "/elsewhere/cleanup-agent",
            "/elsewhere/codex-agents/other-agent",
        ] {
            assert!(matches!(
                super::owned_worktree_root(std::path::Path::new(path), "cleanup-agent"),
                Err(WorkspaceCleanupError::InvalidRecord)
            ));
        }
    }

    /// A worker directory replaced by a link to another repository's worktree
    /// must never become the cleanup target.
    #[test]
    fn substituted_worker_link_is_refused_and_target_survives() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let base = studio_runtime_store::path_identity::resolve(directory.path())
            .expect("resolved fixture folder");
        let foreign = base.join("foreign");
        fixture_git(
            &base,
            &["init", "-q", foreign.to_str().expect("foreign path")],
        );
        std::fs::write(foreign.join("README.md"), "foreign\n").expect("file writes");
        fixture_git(&foreign, &["add", "README.md"]);
        fixture_git(&foreign, &["commit", "-q", "-m", "foreign"]);
        let target = foreign.join(".worktrees/codex-agents/cleanup-agent");
        fixture_git(
            &foreign,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                target.to_str().expect("target path"),
            ],
        );
        let workers = base.join("configured/codex-agents");
        std::fs::create_dir_all(&workers).expect("workers directory creates");
        let link = workers.join("cleanup-agent");
        std::os::unix::fs::symlink(&target, &link).expect("substitute link creates");
        let agent = json!({"id": "cleanup-agent", "cwd": link, "worktreeReady": true});
        assert!(matches!(
            super::worktree_identity(&agent, &base),
            Err(WorkspaceCleanupError::InvalidRecord)
        ));
        assert!(target.join("README.md").exists());
    }

    #[tokio::test]
    async fn archive_allows_a_verified_detached_worktree() {
        let (directory, store) = fixture().await;
        let repository = directory.path().join("detached-project");
        let root = repository.join(".worktrees/codex-agents/cleanup-agent");
        std::fs::create_dir_all(root.parent().expect("worktree parent creates"))
            .expect("fixture parent creates");
        fixture_git(
            directory.path(),
            &["init", "-q", repository.to_str().expect("repository path")],
        );
        let git = |args: &[&str]| fixture_git(&repository, args);
        std::fs::write(repository.join("README.md"), "fixture\n").expect("fixture file writes");
        git(&["add", "README.md"]);
        git(&["commit", "-q", "-m", "fixture"]);
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "codex-agent/cleanup-agent",
            root.to_str().expect("fixture path is UTF-8"),
        ]);
        fixture_git(&root, &["switch", "--detach", "HEAD"]);
        put_agent(
            &store,
            json!({
                "cwd":root, "branch":"codex-agent/cleanup-agent", "worktreeReady":true,
                "agentArchive":{"at":7,"epoch":7,"cleanupPending":true}
            }),
        )
        .await;
        super::cleanup_agent_workspace(
            Arc::clone(&store),
            "cleanup-agent".to_owned(),
            CleanupAction::Archive,
        )
        .await
        .expect("detached worktree archives");
        assert!(!root.exists(), "detached worktree is removed");
        assert_eq!(agent(&store)["cleanedWorktree"]["branch"], Value::Null);
    }

    #[tokio::test]
    async fn archive_keeps_parent_when_a_registered_nested_worktree_exists() {
        let (directory, store) = fixture().await;
        let repository = directory.path().join("nested-project");
        let root = repository.join(".worktrees/codex-agents/cleanup-agent");
        let nested = root.join(".worktrees/nested");
        std::fs::create_dir_all(root.parent().expect("worktree parent creates"))
            .expect("fixture parent creates");
        fixture_git(
            directory.path(),
            &["init", "-q", repository.to_str().expect("repository path")],
        );
        let git = |args: &[&str]| fixture_git(&repository, args);
        std::fs::write(repository.join("README.md"), "fixture\n").expect("fixture file writes");
        git(&["add", "README.md"]);
        git(&["commit", "-q", "-m", "fixture"]);
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "cleanup-agent",
            root.to_str().expect("fixture path is UTF-8"),
        ]);
        std::fs::write(repository.join(".git/info/exclude"), ".worktrees/\n")
            .expect("nested worktree path ignored");
        git(&[
            "worktree",
            "add",
            "-q",
            "--detach",
            nested.to_str().expect("nested path is UTF-8"),
        ]);
        put_agent(
            &store,
            json!({
                "cwd":root, "branch":"cleanup-agent", "worktreeReady":true,
                "agentArchive":{"at":7,"epoch":7,"cleanupPending":true}
            }),
        )
        .await;

        let error = super::cleanup_agent_workspace(
            Arc::clone(&store),
            "cleanup-agent".to_owned(),
            CleanupAction::Archive,
        )
        .await
        .expect_err("nested registration is unsafe to remove");
        assert!(matches!(error, WorkspaceCleanupError::NotSent));
        assert!(root.exists(), "parent worktree remains on disk");
        assert!(nested.exists(), "nested worktree remains on disk");
    }

    fn fixture_git(directory: &std::path::Path, args: &[&str]) -> String {
        let mut command = std::process::Command::new("git");
        for (key, _) in
            std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("GIT_"))
        {
            command.env_remove(key);
        }
        let output = command
            .arg("-C")
            .arg(directory)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Cleanup Fixture")
            .env("GIT_AUTHOR_EMAIL", "cleanup@example.invalid")
            .env("GIT_COMMITTER_NAME", "Cleanup Fixture")
            .env("GIT_COMMITTER_EMAIL", "cleanup@example.invalid")
            .output()
            .expect("fixture Git starts");
        assert!(
            output.status.success(),
            "git {args:?}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("fixture Git output is UTF-8")
            .trim()
            .to_owned()
    }

    async fn submodule_fixture() -> (
        tempfile::TempDir,
        Arc<RuntimeStore>,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        submodule_fixture_with_nested(false).await
    }

    async fn submodule_fixture_with_nested(
        nested: bool,
    ) -> (
        tempfile::TempDir,
        Arc<RuntimeStore>,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let (directory, store) = fixture().await;
        let base = std::fs::canonicalize(directory.path()).expect("fixture path resolves");
        let source = base.join("submodule-source");
        std::fs::create_dir(&source).expect("submodule source creates");
        fixture_git(&source, &["init", "-q"]);
        std::fs::write(source.join("file.txt"), "original\n").expect("source writes");
        if nested {
            let nested_source = base.join("nested-source");
            std::fs::create_dir(&nested_source).expect("nested source creates");
            fixture_git(&nested_source, &["init", "-q"]);
            std::fs::write(nested_source.join("file.txt"), "nested original\n")
                .expect("nested source writes");
            fixture_git(&nested_source, &["add", "."]);
            fixture_git(&nested_source, &["commit", "-qm", "nested base"]);
            fixture_git(
                &source,
                &[
                    "-c",
                    "protocol.file.allow=always",
                    "submodule",
                    "add",
                    nested_source.to_str().expect("nested source path is UTF-8"),
                    "nested",
                ],
            );
            fixture_git(
                &source,
                &[
                    "config",
                    "-f",
                    ".gitmodules",
                    "submodule.nested.ignore",
                    "all",
                ],
            );
        }
        fixture_git(&source, &["add", "."]);
        fixture_git(&source, &["commit", "-qm", "submodule base"]);
        let repository = base.join("project");
        std::fs::create_dir(&repository).expect("repository creates");
        fixture_git(&repository, &["init", "-q"]);
        fixture_git(
            &repository,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                source.to_str().expect("source path is UTF-8"),
                "module",
            ],
        );
        fixture_git(
            &repository,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.module.ignore",
                "all",
            ],
        );
        fixture_git(&repository, &["add", "."]);
        fixture_git(&repository, &["commit", "-qm", "parent base"]);
        let root = repository.join(".worktrees/codex-agents/cleanup-agent");
        fixture_git(
            &repository,
            &[
                "worktree",
                "add",
                "-qb",
                "cleanup-agent",
                root.to_str().expect("worktree path is UTF-8"),
            ],
        );
        fixture_git(
            &root,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "--recursive",
            ],
        );
        put_agent(
            &store,
            json!({
                "cwd":root, "branch":"cleanup-agent", "worktreeReady":true,
                "agentArchive":{"at":7,"epoch":7,"cleanupPending":true}
            }),
        )
        .await;
        (directory, store, repository, root)
    }

    async fn assert_submodule_edit_survives_archive(change: &str) {
        let (_directory, store, repository, root) = submodule_fixture().await;
        let module = root.join("module");
        let saved_gitdir = fixture_git(&module, &["rev-parse", "--absolute-git-dir"]);
        let file = module.join(if change == "untracked" {
            "untracked.txt"
        } else {
            "file.txt"
        });
        std::fs::write(&file, "worker data\n").expect("worker edit writes");
        let commit = if change == "commit" {
            fixture_git(&module, &["commit", "-qam", "worker commit"]);
            Some(fixture_git(&module, &["rev-parse", "HEAD"]))
        } else {
            None
        };
        assert_eq!(
            fixture_git(&root, &["status", "--porcelain"]),
            "",
            "ignore=all hides submodule changes"
        );
        let outcome = super::cleanup_agent_workspace(
            Arc::clone(&store),
            "cleanup-agent".into(),
            CleanupAction::Archive,
        )
        .await;
        let archive = fixture_git(
            &repository,
            &[
                "for-each-ref",
                "--format=%(objectname)",
                "refs/codex-agents/archive/cleanup-agent",
            ],
        );
        eprintln!(
            "submodule reproduction: change={change}, outcome={outcome:?}, file_exists={}, module_gitdir_exists={}, archive_ref={archive}, worker_commit={commit:?}",
            file.exists(),
            std::path::Path::new(&saved_gitdir).exists()
        );
        assert!(
            root.exists(),
            "archive must retain the worktree with {change} submodule data"
        );
        assert_eq!(
            std::fs::read_to_string(&file).expect("worker file remains"),
            "worker data\n"
        );
        let error = outcome.expect_err("unsafe submodule prevents archive removal");
        assert!(
            matches!(error, WorkspaceCleanupError::UnsafeSubmodule(_)),
            "{error}"
        );
        assert_eq!(agent(&store)["worktreeReady"], true);
        assert!(
            agent(&store)["workspaceCleanupIntent"]["receipt"]["reason"]
                .as_str()
                .expect("cleanup refusal is text")
                .contains("submodule")
        );
    }

    #[tokio::test]
    async fn archive_keeps_submodule_modified_file_despite_ignore_all() {
        assert_submodule_edit_survives_archive("modified").await;
    }

    #[tokio::test]
    async fn archive_keeps_submodule_untracked_file_despite_ignore_all() {
        assert_submodule_edit_survives_archive("untracked").await;
    }

    #[tokio::test]
    async fn archive_keeps_submodule_unpushed_commit_despite_ignore_all() {
        assert_submodule_edit_survives_archive("commit").await;
    }

    async fn assert_submodule_held(
        store: &Arc<RuntimeStore>,
        root: &std::path::Path,
        reason: &str,
    ) {
        let error = super::cleanup_agent_workspace(
            Arc::clone(store),
            "cleanup-agent".into(),
            CleanupAction::Archive,
        )
        .await
        .expect_err("unsafe submodule retains worktree");
        assert!(
            matches!(error, WorkspaceCleanupError::UnsafeSubmodule(_)),
            "{error}"
        );
        assert!(error.to_string().contains(reason), "{error}");
        assert!(root.exists());
        let saved = agent(store);
        assert_eq!(saved["worktreeReady"], true);
        assert_eq!(saved["workspaceCleanupIntent"]["phase"], "not_sent");
        assert!(
            saved["workspaceCleanupIntent"]["receipt"]["reason"]
                .as_str()
                .expect("saved refusal is text")
                .contains(reason)
        );
    }

    #[tokio::test]
    async fn archive_cleans_recursive_submodules_and_preserves_archive_ref() {
        let (_directory, store, repository, root) = submodule_fixture_with_nested(true).await;
        assert!(root.join("module/nested/file.txt").exists());
        let head = fixture_git(&root, &["rev-parse", "HEAD"]);
        super::cleanup_agent_workspace(
            Arc::clone(&store),
            "cleanup-agent".into(),
            CleanupAction::Archive,
        )
        .await
        .expect("clean recursive submodules archive");
        assert!(!root.exists());
        assert_eq!(
            fixture_git(
                &repository,
                &["rev-parse", "refs/codex-agents/archive/cleanup-agent"]
            ),
            head
        );
    }

    #[tokio::test]
    async fn archive_cleans_uninitialized_submodules() {
        let (_directory, store, _repository, root) = submodule_fixture().await;
        fixture_git(&root, &["submodule", "deinit", "module"]);
        super::cleanup_agent_workspace(
            Arc::clone(&store),
            "cleanup-agent".into(),
            CleanupAction::Archive,
        )
        .await
        .expect("uninitialized submodule archives");
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn archive_keeps_submodule_local_branch_with_recorded_head() {
        let (_directory, store, _repository, root) = submodule_fixture().await;
        let module = root.join("module");
        let recorded = fixture_git(&module, &["rev-parse", "HEAD"]);
        fixture_git(&module, &["checkout", "-qb", "worker"]);
        std::fs::write(module.join("file.txt"), "local branch\n")
            .expect("local branch file writes");
        fixture_git(&module, &["commit", "-qam", "local branch"]);
        fixture_git(&module, &["checkout", "--detach", &recorded]);
        assert_submodule_held(&store, &root, "local commits").await;
        let held = reconcile_pending_cleanup(Arc::clone(&store))
            .await
            .expect("recovery retains unsafe worktree");
        assert_eq!(held.len(), 1);
        assert!(held[0].held);
        assert!(root.exists());
    }

    #[tokio::test]
    async fn archive_keeps_submodule_recursive_local_commit_with_clean_status() {
        let (_directory, store, _repository, root) = submodule_fixture_with_nested(true).await;
        let nested = root.join("module/nested");
        let recorded = fixture_git(&nested, &["rev-parse", "HEAD"]);
        std::fs::write(nested.join("file.txt"), "nested worker data\n")
            .expect("nested worker file writes");
        fixture_git(&nested, &["commit", "-qam", "nested detached commit"]);
        let local = fixture_git(&nested, &["rev-parse", "HEAD"]);
        fixture_git(&nested, &["checkout", "--detach", &recorded]);
        assert_eq!(
            fixture_git(
                &root,
                &["status", "--porcelain", "--ignore-submodules=none"]
            ),
            ""
        );
        assert_submodule_held(&store, &root, "module/nested has local commits").await;
        assert_eq!(fixture_git(&nested, &["rev-parse", "HEAD@{1}"]), local);
    }

    #[tokio::test]
    async fn archive_keeps_submodule_recursive_modified_file() {
        let (_directory, store, _repository, root) = submodule_fixture_with_nested(true).await;
        let file = root.join("module/nested/file.txt");
        std::fs::write(&file, "nested edit\n").expect("nested edit writes");
        assert_eq!(fixture_git(&root, &["status", "--porcelain"]), "");
        assert_submodule_held(&store, &root, "modified or untracked").await;
        assert_eq!(
            std::fs::read_to_string(file).expect("nested edit remains readable"),
            "nested edit\n"
        );
    }

    #[tokio::test]
    async fn archive_keeps_submodule_unreadable_head() {
        let (_directory, store, _repository, root) = submodule_fixture().await;
        let gitdir = fixture_git(&root.join("module"), &["rev-parse", "--absolute-git-dir"]);
        std::fs::write(std::path::Path::new(&gitdir).join("HEAD"), "invalid head\n")
            .expect("invalid HEAD writes");
        assert_submodule_held(&store, &root, "cannot be read").await;
    }

    #[tokio::test]
    async fn archive_keeps_submodule_foreign_git_directory() {
        let (directory, store, _repository, root) = submodule_fixture().await;
        let foreign = std::fs::canonicalize(directory.path().join("submodule-source/.git"))
            .expect("foreign Git directory resolves");
        std::fs::write(
            root.join("module/.git"),
            format!("gitdir: {}\n", foreign.display()),
        )
        .expect("foreign Git link writes");
        assert_submodule_held(&store, &root, "foreign Git directory").await;
    }

    #[tokio::test]
    async fn archive_keeps_submodule_without_git_directory_and_with_files() {
        let (_directory, store, _repository, root) = submodule_fixture().await;
        std::fs::remove_file(root.join("module/.git")).expect("submodule Git link removes");
        assert_submodule_held(&store, &root, "contains files but has no Git directory").await;
    }

    #[tokio::test]
    async fn archive_rechecks_submodule_after_saving_parent_ref() {
        let (_directory, store, repository, root) = submodule_fixture().await;
        let identity = super::worktree_identity(&agent(&store), store.state_dir())
            .expect("worktree identity reads")
            .expect("owned worktree identity");
        super::save_worktree_archive_ref(&identity).expect("clean parent ref saves");
        std::fs::write(root.join("module/file.txt"), "late edit\n").expect("late edit writes");
        let error =
            super::remove_worktree(&identity).expect_err("new submodule edit blocks removal");
        assert!(matches!(error, EffectFailure::UnsafeSubmodule(_)));
        assert!(root.exists());
        assert_eq!(
            fixture_git(
                &repository,
                &["rev-parse", "refs/codex-agents/archive/cleanup-agent"]
            ),
            identity["head"].as_str().expect("archive HEAD is text")
        );
    }

    fn record_local_submodule_commit(root: &std::path::Path) -> String {
        let module = root.join("module");
        std::fs::write(module.join("file.txt"), "recorded local commit\n")
            .expect("local commit file writes");
        fixture_git(&module, &["commit", "-qam", "local submodule commit"]);
        let head = fixture_git(&module, &["rev-parse", "HEAD"]);
        fixture_git(
            root,
            &[
                "update-index",
                "--cacheinfo",
                &format!("160000,{head},module"),
            ],
        );
        fixture_git(
            root,
            &[
                "-c",
                "submodule.module.ignore=none",
                "commit",
                "-qm",
                "record local submodule commit",
            ],
        );
        assert_eq!(fixture_git(root, &["rev-parse", "HEAD:module"]), head);
        head
    }

    #[tokio::test]
    async fn archive_keeps_submodule_recorded_unpushed_commit() {
        let (_directory, store, _repository, root) = submodule_fixture().await;
        let local = record_local_submodule_commit(&root);
        assert_submodule_held(&store, &root, "recorded commit without a remote ref").await;
        assert_eq!(
            fixture_git(&root.join("module"), &["rev-parse", "HEAD"]),
            local
        );
    }

    #[tokio::test]
    async fn archive_keeps_submodule_without_remote_or_main_copy() {
        let (_directory, store, _repository, root) = submodule_fixture().await;
        record_local_submodule_commit(&root);
        fixture_git(&root.join("module"), &["remote", "remove", "origin"]);
        assert_submodule_held(&store, &root, "recorded commit without a remote ref").await;
    }

    #[tokio::test]
    async fn archive_cleans_submodule_without_remote_with_main_copy() {
        let (_directory, store, _repository, root) = submodule_fixture().await;
        fixture_git(&root.join("module"), &["remote", "remove", "origin"]);
        super::cleanup_agent_workspace(
            Arc::clone(&store),
            "cleanup-agent".into(),
            CleanupAction::Archive,
        )
        .await
        .expect("main submodule object store preserves the recorded commit");
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn archive_cleans_recorded_submodule_commit_copied_to_main() {
        let (_directory, store, repository, root) = submodule_fixture().await;
        let local = record_local_submodule_commit(&root);
        fixture_git(
            &repository.join("module"),
            &[
                "-c",
                "protocol.file.allow=always",
                "fetch",
                root.join("module").to_str().expect("module path is UTF-8"),
                &local,
            ],
        );
        fixture_git(&root.join("module"), &["remote", "remove", "origin"]);
        super::cleanup_agent_workspace(
            Arc::clone(&store),
            "cleanup-agent".into(),
            CleanupAction::Archive,
        )
        .await
        .expect("main copy preserves the otherwise unpublished commit");
        assert!(!root.exists());
        assert_eq!(
            fixture_git(&repository.join("module"), &["cat-file", "-t", &local]),
            "commit"
        );
    }

    #[tokio::test]
    async fn archive_keeps_submodule_with_foreign_object_alternates() {
        let (_directory, store, repository, root) = submodule_fixture().await;
        let gitdir = fixture_git(&root.join("module"), &["rev-parse", "--absolute-git-dir"]);
        let foreign = fixture_git(
            &repository.join("module"),
            &["rev-parse", "--absolute-git-dir"],
        );
        std::fs::write(
            std::path::Path::new(&gitdir).join("objects/info/alternates"),
            format!("{foreign}/objects\n"),
        )
        .expect("foreign object alternate writes");
        assert_submodule_held(&store, &root, "foreign Git directory or configuration").await;
    }

    #[tokio::test]
    async fn archive_keeps_submodule_with_unreadable_configuration() {
        let (_directory, store, _repository, root) = submodule_fixture().await;
        std::fs::write(root.join(".gitmodules"), "[broken\n")
            .expect("invalid Git configuration writes");
        assert_submodule_held(&store, &root, "cannot be read").await;
    }

    #[tokio::test]
    async fn lifecycle_intent_has_stable_operation_identity() {
        let first = queued_cleanup_intent("cleanup-agent", 7, CleanupAction::Delete);
        let replay = queued_cleanup_intent("cleanup-agent", 7, CleanupAction::Delete);
        assert_eq!(first, replay);
        assert_eq!(first["phase"], "queued");
        assert_eq!(first["action"], "delete");
    }
}
