//! Durable cleanup effects for agent workspaces.
//!
//! A cleanup intent is committed before its filesystem or guest effect. A
//! process restart turns an unfinished intent into `unknown`; callers must
//! inspect the external workspace and must never resend that effect.
use std::{path::PathBuf, time::Duration};

use crate::linux_vm_guest::GuestRpc;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore, StoreError, Transaction};
use thiserror::Error;

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
    SaveVmTurn { turn_id: String },
    ReleaseVmLine { method: String },
    SaveWorktreeRef { identity: Value },
    RemoveWorktree { identity: Value },
}

#[derive(Clone, Debug, PartialEq)]
pub enum EffectFailure {
    NotSent,
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
pub fn cleanup_agent_workspace(
    store: &RuntimeStore,
    agent_id: &str,
    action: CleanupAction,
) -> Result<WorkspaceCleanupResult, WorkspaceCleanupError> {
    cleanup_agent_workspace_with_ports(
        store,
        agent_id,
        action,
        &ProductionPorts {
            state_dir: store.root().to_path_buf(),
        },
    )
}

pub fn cleanup_agent_workspace_with_ports(
    store: &RuntimeStore,
    agent_id: &str,
    action: CleanupAction,
    ports: &dyn CleanupPorts,
) -> Result<WorkspaceCleanupResult, WorkspaceCleanupError> {
    let agent = read_agent(store, agent_id)?;
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
    let effects = plan(&agent, action)?;
    if effects.is_empty() {
        if action == CleanupAction::Archive {
            store.transaction_with_error(|tx| {
                let mut current = load_agent(tx, agent_id)?;
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
                AgentRepository::put(
                    tx,
                    &AgentRecord::from_value(current).map_err(StoreError::from)?,
                )?;
                Ok::<(), WorkspaceCleanupError>(())
            })?;
        }
        let phase = agent
            .pointer(&format!("/{INTENT_FIELD}/phase"))
            .and_then(Value::as_str);
        if matches!(phase, Some("queued" | "written" | "not_sent")) {
            clear_intent(store, agent_id)?;
        }
        return Ok(result(agent_id, action, None, None, false));
    }
    let mut workspace = None;
    let mut worktree = None;
    for effect in effects {
        let (request_id, replay) = begin_effect(store, agent_id, action, &effect, &agent)?;
        if let Some(receipt) = replay {
            finalize_effect(store, agent_id, &request_id, &effect, &receipt)?;
            if matches!(effect, CleanupEffect::RemoveWorktree { .. }) {
                worktree = Some(receipt);
            } else {
                workspace = Some(receipt);
            }
            continue;
        }
        let current = read_agent(store, agent_id)?;
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
        match ports.perform(&current, &effect, &request_id) {
            Ok(receipt) => {
                complete_effect(store, agent_id, &request_id, &effect, &receipt)?;
                if matches!(effect, CleanupEffect::RemoveWorktree { .. }) {
                    worktree = Some(receipt);
                } else {
                    workspace = Some(receipt);
                }
            }
            Err(EffectFailure::NotSent) => {
                mark_effect(store, agent_id, &request_id, "not_sent", Value::Null)?;
                return Err(WorkspaceCleanupError::NotSent);
            }
            Err(EffectFailure::Unknown) => {
                mark_effect(store, agent_id, &request_id, "unknown", Value::Null)?;
                return Err(WorkspaceCleanupError::Held);
            }
        }
    }
    finalize_cleanup(store, agent_id, action)?;
    Ok(result(agent_id, action, workspace, worktree, false))
}

fn finalize_cleanup(
    store: &RuntimeStore,
    agent_id: &str,
    action: CleanupAction,
) -> Result<(), WorkspaceCleanupError> {
    store.transaction_with_error(|tx| {
        let mut agent = load_agent(tx, agent_id)?;
        let intent = agent
            .get(INTENT_FIELD)
            .cloned()
            .ok_or(WorkspaceCleanupError::InvalidRecord)?;
        if intent.get("action") != Some(&serde_json::to_value(action).map_err(StoreError::from)?)
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
    })?;
    Ok(())
}

/// Reconcile state left by a crash. Unfinished `pending` effects are changed
/// to `unknown` and held. A `written` receipt is safe to replay from the
/// store: it only finalizes that receipt and continues with the next effect.
pub fn reconcile_pending_cleanup(
    store: &RuntimeStore,
) -> Result<Vec<WorkspaceCleanupResult>, WorkspaceCleanupError> {
    let reader = store.read()?;
    let mut statement = reader.prepare(
        "SELECT id,record FROM runtime_agents WHERE json_type(record, '$.workspaceCleanupIntent')='object' ORDER BY id",
    ).map_err(StoreError::from)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(StoreError::from)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)?;
    drop(statement);
    drop(reader);
    let mut held = Vec::new();
    for (id, raw) in rows {
        let record: Value =
            serde_json::from_str(&raw).map_err(|_| WorkspaceCleanupError::InvalidRecord)?;
        let intent = record
            .get(INTENT_FIELD)
            .ok_or(WorkspaceCleanupError::InvalidRecord)?;
        let phase = intent.get("phase").and_then(Value::as_str);
        if phase == Some("pending") {
            let request_id = intent
                .get("requestId")
                .and_then(Value::as_str)
                .ok_or(WorkspaceCleanupError::InvalidRecord)?
                .to_owned();
            mark_effect(store, &id, &request_id, "unknown", Value::Null)?;
        }
        if matches!(phase, Some("queued" | "written" | "not_sent")) {
            let action: CleanupAction = serde_json::from_value(
                intent
                    .get("action")
                    .cloned()
                    .ok_or(WorkspaceCleanupError::InvalidRecord)?,
            )
            .map_err(|_| WorkspaceCleanupError::InvalidRecord)?;
            match cleanup_agent_workspace(store, &id, action) {
                Ok(cleanup) => held.push(cleanup),
                Err(WorkspaceCleanupError::Held | WorkspaceCleanupError::NotSent) => {
                    held.push(result(&id, action, None, None, true));
                }
                Err(error) => return Err(error),
            }
            continue;
        }
        if !matches!(phase, Some("pending" | "unknown")) {
            continue;
        }
        let action: CleanupAction = serde_json::from_value(
            intent
                .get("action")
                .cloned()
                .ok_or(WorkspaceCleanupError::InvalidRecord)?,
        )
        .map_err(|_| WorkspaceCleanupError::InvalidRecord)?;
        held.push(result(&id, action, None, None, true));
    }
    Ok(held)
}

fn plan(agent: &Value, action: CleanupAction) -> Result<Vec<CleanupEffect>, WorkspaceCleanupError> {
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
    let image = agent.get("imageWorkspace").and_then(Value::as_bool) == Some(true);
    let vm_dispose = is_vm
        && (agent.get("layrReady").and_then(Value::as_bool) == Some(true)
            || action == CleanupAction::Archive);
    if vm_dispose {
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
    } else if image && action == CleanupAction::Archive {
        if agent.get("imageWorkspaceReady").and_then(Value::as_bool) == Some(true)
            || agent
                .pointer("/cleanedImageWorkspace/phase")
                .and_then(Value::as_str)
                == Some("archiving")
        {
            effects.push(CleanupEffect::ArchiveImage);
        }
    } else if image && action == CleanupAction::Delete {
        if agent.get("imageWorkspacePhase").and_then(Value::as_str) != Some("removed") {
            effects.push(CleanupEffect::RemoveImage);
        }
    } else if action == CleanupAction::Archive
        && agent.get("worktreeReady").and_then(Value::as_bool) == Some(true)
    {
        let identity = worktree_identity(agent)?;
        effects.push(CleanupEffect::SaveWorktreeRef {
            identity: identity.clone(),
        });
        effects.push(CleanupEffect::RemoveWorktree { identity });
    }
    Ok(effects)
}

fn worktree_identity(agent: &Value) -> Result<Value, WorkspaceCleanupError> {
    let id = agent
        .get("id")
        .and_then(Value::as_str)
        .ok_or(WorkspaceCleanupError::InvalidRecord)?;
    let cwd = agent
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or(WorkspaceCleanupError::InvalidRecord)?;
    let cwd = std::path::Path::new(cwd)
        .canonicalize()
        .map_err(|_| WorkspaceCleanupError::InvalidRecord)?;
    let root = cwd
        .ancestors()
        .find(|path| {
            path.file_name().and_then(|name| name.to_str()) == Some(id)
                && path
                    .parent()
                    .and_then(std::path::Path::file_name)
                    .and_then(|name| name.to_str())
                    == Some("codex-agents")
                && path
                    .parent()
                    .and_then(std::path::Path::parent)
                    .and_then(std::path::Path::file_name)
                    .and_then(|name| name.to_str())
                    == Some(".worktrees")
        })
        .ok_or(WorkspaceCleanupError::InvalidRecord)?;
    let repo = root
        .parent()
        .and_then(std::path::Path::parent)
        .and_then(std::path::Path::parent)
        .ok_or(WorkspaceCleanupError::InvalidRecord)?;
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
    Ok(
        json!({"id":id,"cwd":cwd,"root":root,"repository":repo,"relative":relative,"head":head,
        "branch":agent.get("branch"),"lifecycleIdentity":[agent.get("epoch"),agent.get("cwd"),agent.get("deletedAt")],
        "epoch":agent.get("epoch"),"deletedAt":agent.get("deletedAt"),
        "archive":agent.get("agentArchive")}),
    )
}

fn read_agent(store: &RuntimeStore, agent_id: &str) -> Result<Value, WorkspaceCleanupError> {
    let record = store.read_transaction_with_error(|tx| load_agent(tx, agent_id))?;
    Ok(record)
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
    serde_json::from_str(&raw).map_err(|_| WorkspaceCleanupError::InvalidRecord)
}

fn begin_effect(
    store: &RuntimeStore,
    agent_id: &str,
    action: CleanupAction,
    effect: &CleanupEffect,
    expected_agent: &Value,
) -> Result<(String, Option<Value>), WorkspaceCleanupError> {
    let result = store.transaction_with_error(|tx| {
        let mut agent = load_agent(tx, agent_id)?;
        let identity = lifecycle_identity(expected_agent);
        if lifecycle_identity(&agent) != identity {
            return Err(WorkspaceCleanupError::Held);
        }
        let mut attempt = 0_u64;
        if let Some(intent) = agent.get(INTENT_FIELD).filter(|v| !v.is_null()) {
            if intent.get("action") == Some(&serde_json::to_value(action).map_err(StoreError::from)?)
                && intent.get("effect") == Some(&serde_json::to_value(effect).map_err(StoreError::from)?)
                && intent.get("phase").and_then(Value::as_str) == Some("not_sent")
            {
                attempt = intent.get("attempt").and_then(Value::as_u64).unwrap_or(0) + 1;
            }
            let phase = intent.get("phase").and_then(Value::as_str);
            if intent.get("identity").is_some_and(|previous| *previous != identity) {
                return Err(WorkspaceCleanupError::Held);
            }
            let same_effect = intent.get("action") == Some(&serde_json::to_value(action).map_err(StoreError::from)?)
                && intent.get("effect") == Some(&serde_json::to_value(effect).map_err(StoreError::from)?);
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
    })?;
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

fn complete_effect(
    store: &RuntimeStore,
    agent_id: &str,
    request_id: &str,
    effect: &CleanupEffect,
    receipt: &Value,
) -> Result<(), WorkspaceCleanupError> {
    let completed = store.transaction_with_error(|tx| {
        let mut agent = load_agent(tx, agent_id)?;
        let intent_value = agent
            .get(INTENT_FIELD)
            .cloned()
            .ok_or(WorkspaceCleanupError::InvalidRecord)?;
        if intent_value.get("requestId").and_then(Value::as_str) != Some(request_id) {
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
        if !valid_receipt(effect, receipt) {
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
        apply_effect_result(&mut agent, effect, receipt)?;
        AgentRepository::put(
            tx,
            &AgentRecord::from_value(agent).map_err(StoreError::from)?,
        )?;
        Ok(true)
    })?;
    if !completed {
        return Err(WorkspaceCleanupError::Held);
    }
    Ok(())
}

fn valid_receipt(effect: &CleanupEffect, receipt: &Value) -> bool {
    match effect {
        CleanupEffect::SaveVmTurn { .. } => receipt
            .get("stateId")
            .and_then(Value::as_str)
            .is_some_and(|state_id| !state_id.is_empty()),
        _ => true,
    }
}

fn finalize_effect(
    store: &RuntimeStore,
    agent_id: &str,
    request_id: &str,
    effect: &CleanupEffect,
    receipt: &Value,
) -> Result<(), WorkspaceCleanupError> {
    store.transaction_with_error(|tx| {
        let mut agent = load_agent(tx, agent_id)?;
        let intent = agent
            .get(INTENT_FIELD)
            .cloned()
            .ok_or(WorkspaceCleanupError::InvalidRecord)?;
        if agent
            .pointer(&format!("/{INTENT_FIELD}/requestId"))
            .and_then(Value::as_str)
            != Some(request_id)
            || intent.get("identity") != Some(&lifecycle_identity(&agent))
        {
            return Err(WorkspaceCleanupError::Held);
        }
        apply_effect_result(&mut agent, effect, receipt)?;
        AgentRepository::put(
            tx,
            &AgentRecord::from_value(agent).map_err(StoreError::from)?,
        )?;
        Ok::<(), WorkspaceCleanupError>(())
    })?;
    Ok(())
}

fn apply_effect_result(
    agent: &mut Value,
    effect: &CleanupEffect,
    receipt: &Value,
) -> Result<(), WorkspaceCleanupError> {
    match effect {
        CleanupEffect::ArchiveImage => {
            agent["imageWorkspaceReady"] = json!(false);
            agent["imageWorkspacePhase"] = json!("archived");
            agent["cleanedImageWorkspace"] = json!({"phase":"archived","freedBytes":receipt.get("freedBytes").cloned().unwrap_or(json!(0))});
        }
        CleanupEffect::RemoveImage => {
            agent["imageWorkspaceReady"] = json!(false);
            agent["imageWorkspacePhase"] = json!("removed");
        }
        CleanupEffect::ReleaseVmLine { .. } => {
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
        CleanupEffect::SaveWorktreeRef { .. } => {}
        CleanupEffect::SaveVmTurn { turn_id } => {
            agent["layrTurnSaves"][turn_id] =
                json!({"status":"saved","stateId":receipt.get("stateId")});
            agent["layrStateId"] = receipt.get("stateId").cloned().unwrap_or(Value::Null);
        }
    }
    Ok(())
}

fn mark_effect(
    store: &RuntimeStore,
    agent_id: &str,
    request_id: &str,
    phase: &str,
    receipt: Value,
) -> Result<(), WorkspaceCleanupError> {
    store.transaction_with_error(|tx| {
        let mut agent = load_agent(tx, agent_id)?;
        let intent = agent
            .get_mut(INTENT_FIELD)
            .and_then(Value::as_object_mut)
            .ok_or(WorkspaceCleanupError::InvalidRecord)?;
        if intent.get("requestId").and_then(Value::as_str) != Some(request_id) {
            return Err(WorkspaceCleanupError::Held);
        }
        intent.insert("phase".into(), json!(phase));
        intent.insert("receipt".into(), receipt.clone());
        AgentRepository::put(
            tx,
            &AgentRecord::from_value(agent).map_err(StoreError::from)?,
        )?;
        Ok::<(), WorkspaceCleanupError>(())
    })?;
    Ok(())
}

fn clear_intent(store: &RuntimeStore, agent_id: &str) -> Result<(), WorkspaceCleanupError> {
    store.transaction_with_error(|tx| {
        let mut agent = load_agent(tx, agent_id)?;
        let object = agent
            .as_object_mut()
            .ok_or(WorkspaceCleanupError::InvalidRecord)?;
        object.remove(INTENT_FIELD);
        AgentRepository::put(
            tx,
            &AgentRecord::from_value(agent).map_err(StoreError::from)?,
        )?;
        Ok::<(), WorkspaceCleanupError>(())
    })?;
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
            CleanupEffect::ArchiveImage => crate::image_workspace::archive_workspace(
                agent["id"].as_str().ok_or(EffectFailure::NotSent)?,
            )
            .map_err(|_| EffectFailure::Unknown),
            CleanupEffect::RemoveImage => crate::image_workspace::remove_workspace(
                agent["id"].as_str().ok_or(EffectFailure::NotSent)?,
                false,
            )
            .map_err(|_| EffectFailure::Unknown),
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
                    .map_err(|error| match error {
                        crate::linux_vm_guest::GuestError::InvalidRequest
                        | crate::linux_vm_guest::GuestError::Identity => EffectFailure::NotSent,
                        _ => EffectFailure::Unknown,
                    })?;
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
                    .map_err(|error| match error {
                        crate::linux_vm_guest::GuestError::InvalidRequest
                        | crate::linux_vm_guest::GuestError::Identity => EffectFailure::NotSent,
                        _ => EffectFailure::Unknown,
                    })
            }
            CleanupEffect::SaveWorktreeRef { identity } => {
                check_worktree(identity).map_err(|_| EffectFailure::NotSent)?;
                save_worktree_archive_ref(identity).map_err(|_| EffectFailure::Unknown)?;
                Ok(json!({"state":"saved"}))
            }
            CleanupEffect::RemoveWorktree { identity } => {
                check_worktree(identity).map_err(|_| EffectFailure::NotSent)?;
                remove_worktree(identity).map_err(|_| EffectFailure::Unknown)?;
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
        cwd: repository.as_os_str(),
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
    if PathBuf::from(repository_root.trim())
        .canonicalize()
        .map_err(|_| ())?
        != repo.canonicalize().map_err(|_| ())?
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
    if PathBuf::from(actual_gitdir.trim())
        .canonicalize()
        .map_err(|_| ())?
        != linked_gitdir.canonicalize().map_err(|_| ())?
        || !linked_gitdir
            .canonicalize()
            .map_err(|_| ())?
            .starts_with(repo.join(".git/worktrees").canonicalize().map_err(|_| ())?)
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
                belongs = PathBuf::from(value).canonicalize().ok() == root.canonicalize().ok();
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
        if let Ok(registered_path) = registered_path.canonicalize() {
            if registered_path != root && registered_path.starts_with(&root) {
                return Err(());
            }
        }
    }
    let status = git_output(
        &root,
        &["status", "--porcelain", "--untracked-files=normal"],
        None,
    )?;
    if !status.is_empty() {
        return Err(());
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

fn save_worktree_archive_ref(identity: &Value) -> Result<(), ()> {
    let (repo, _root, id, head) = check_worktree(identity)?;
    let current = format!("refs/codex-agents/archive/{id}");
    let previous = String::from_utf8(git_output(
        &repo,
        &["for-each-ref", "--format=%(objectname)", &current],
        None,
    )?)
    .map_err(|_| ())?;
    let previous = previous.trim();
    let zero = "0".repeat(head.len());
    let mut commands = String::from("start\n");
    if !previous.is_empty() && previous != head {
        let history = format!("refs/codex-agents/archive-history/{id}/{previous}");
        let existing = String::from_utf8(git_output(
            &repo,
            &["for-each-ref", "--format=%(objectname)", &history],
            None,
        )?)
        .map_err(|_| ())?;
        if existing.trim().is_empty() {
            commands.push_str(&format!("create {history} {previous}\n"));
        } else if existing.trim() != previous {
            return Err(());
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
    git_output(&repo, &["update-ref", "--stdin"], Some(commands.as_bytes()))?;
    Ok(())
}

fn remove_worktree(identity: &Value) -> Result<(), ()> {
    let (repo, root, _, _) = check_worktree(identity)?;
    let root = root.to_str().ok_or(())?;
    git_output(&repo, &["worktree", "remove", "--force", root], None)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::{Value, json};
    use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore};

    use super::{
        CleanupAction, CleanupEffect, CleanupPorts, EffectFailure, WorkspaceCleanupError,
        cleanup_agent_workspace_with_ports, queued_cleanup_intent, reconcile_pending_cleanup,
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

    fn fixture() -> (tempfile::TempDir, RuntimeStore) {
        let tmpfs = std::env::var_os("CS_W9_CLEANUP_TEST_TMPFS")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/dev/shm/cs-w9-cleanup-tmp"));
        std::fs::create_dir_all(&tmpfs).expect("tmpfs test parent exists");
        let directory = tempfile::Builder::new()
            .prefix("workspace-cleanup-")
            .tempdir_in(tmpfs)
            .expect("tmpfs state root creates");
        let root = directory.path();
        let schema: Value = serde_json::from_str(include_str!(
            "../../../apps/server/tests/fixtures/runtime-state-schema.json"
        ))
        .expect("runtime schema fixture parses");
        for name in ["canvas.sqlite3", "analytics.sqlite3"] {
            let path = root.join(name);
            let connection = rusqlite::Connection::open(path).expect("fixture DB opens");
            connection
                .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
                .expect("fixture pragmas apply");
            let mut statements = schema[name]["sqlite_master"]
                .as_array()
                .expect("schema entries")
                .iter()
                .filter_map(|entry| {
                    Some((
                        entry["type"].as_str()?,
                        entry["name"].as_str()?,
                        entry["sql"].as_str()?,
                    ))
                })
                .collect::<Vec<_>>();
            statements.sort_by_key(|(kind, _, _)| match *kind {
                "table" => 0,
                "index" => 1,
                "view" => 2,
                _ => 3,
            });
            for (_, object_name, sql) in statements {
                let exists: bool = connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
                        [object_name],
                        |row| row.get(0),
                    )
                    .expect("schema entry lookup");
                if !exists {
                    connection.execute_batch(sql).expect("schema installs");
                }
            }
        }
        let store = RuntimeStore::open(Some(root)).expect("fixture store opens");
        (directory, store)
    }

    fn put_agent(store: &RuntimeStore, mut record: Value) {
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
            .transaction(|tx| AgentRepository::put(tx, &agent))
            .expect("agent stores");
    }

    fn agent(store: &RuntimeStore) -> Value {
        store
            .read_transaction_with_error(|tx| {
                tx.query_row(
                    "SELECT record FROM runtime_agents WHERE id='cleanup-agent'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .map_err(studio_runtime_store::StoreError::from)
            })
            .map(|record| serde_json::from_str(&record).expect("stored agent parses"))
            .expect("agent reads")
    }

    #[test]
    fn archive_orders_workspace_effects_and_clears_cleanup_pending() {
        let (_directory, store) = fixture();
        put_agent(
            &store,
            json!({
                "executionMode":"vm", "layrReady":true,
                "layrTurnSaves":{"turn-1":{"status":"pending"}},
                "agentArchive":{"cleanupPending":true}
            }),
        );
        let ports = FakePorts::new(vec![
            Ok(json!({"stateId":"state-1"})),
            Ok(json!({"stateId":"state-2"})),
        ]);
        let result = cleanup_agent_workspace_with_ports(
            &store,
            "cleanup-agent",
            CleanupAction::Archive,
            &ports,
        )
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

    #[test]
    fn unknown_effect_is_held_after_restart_and_never_resent() {
        let (_directory, store) = fixture();
        put_agent(
            &store,
            json!({
                "imageWorkspace":true, "imageWorkspaceReady":true,
                "agentArchive":{"cleanupPending":true}
            }),
        );
        let ports = FakePorts::new(vec![Err(EffectFailure::Unknown)]);
        assert!(matches!(
            cleanup_agent_workspace_with_ports(
                &store,
                "cleanup-agent",
                CleanupAction::Archive,
                &ports
            ),
            Err(WorkspaceCleanupError::Held)
        ));
        drop(store);
        let store = RuntimeStore::open(Some(_directory.path())).expect("store restarts");
        let held = reconcile_pending_cleanup(&store).expect("unknown intent reconciles");
        assert_eq!(held.len(), 1);
        assert!(held[0].held);
        assert_eq!(
            cleanup_agent_workspace_with_ports(
                &store,
                "cleanup-agent",
                CleanupAction::Archive,
                &ports
            )
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

    #[test]
    fn crash_with_pending_intent_becomes_unknown_after_reopen() {
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

        let (directory, store) = fixture();
        put_agent(
            &store,
            json!({"imageWorkspace":true,"imageWorkspaceReady":true,
                "agentArchive":{"cleanupPending":true}}),
        );
        let crash = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = cleanup_agent_workspace_with_ports(
                &store,
                "cleanup-agent",
                CleanupAction::Archive,
                &CrashPort,
            );
        }));
        assert!(crash.is_err(), "fake effect loses its response");
        drop(store);
        let store = RuntimeStore::open(Some(directory.path())).expect("state reopens");
        let held = reconcile_pending_cleanup(&store).expect("pending effect reconciles");
        assert_eq!(held.len(), 1);
        assert!(held[0].held);
        assert_eq!(
            agent(&store).pointer("/workspaceCleanupIntent/phase"),
            Some(&json!("unknown"))
        );
    }

    #[test]
    fn delete_removes_image_workspace_through_the_effect_port() {
        let (_directory, store) = fixture();
        put_agent(
            &store,
            json!({"imageWorkspace":true,"imageWorkspaceReady":true}),
        );
        let ports = FakePorts::new(vec![Ok(json!({"state":"removed","freedBytes":44}))]);
        cleanup_agent_workspace_with_ports(&store, "cleanup-agent", CleanupAction::Delete, &ports)
            .expect("delete removes image workspace");
        assert!(matches!(
            ports.calls.lock().expect("calls mutex")[0].0,
            CleanupEffect::RemoveImage
        ));
        let saved = agent(&store);
        assert_eq!(saved["imageWorkspacePhase"], "removed");
        assert_eq!(saved["imageWorkspaceReady"], false);
    }

    #[test]
    fn written_retry_replays_the_persisted_attempt_without_resending() {
        let (_directory, store) = fixture();
        put_agent(
            &store,
            json!({
                "executionMode":"vm", "layrReady":true,
                "agentArchive":{"cleanupPending":true}
            }),
        );
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
            .transaction(|tx| {
                AgentRepository::put(tx, &AgentRecord::from_value(record).expect("record parses"))
            })
            .expect("written effect stages");

        let ports = FakePorts::new(vec![]);
        cleanup_agent_workspace_with_ports(&store, "cleanup-agent", action, &ports)
            .expect("written release replays from its receipt");
        assert!(ports.calls.lock().expect("calls mutex").is_empty());
        let saved = agent(&store);
        assert_eq!(saved["agentArchive"]["cleanupPending"], false);
        assert!(saved.get("workspaceCleanupIntent").is_none());
    }

    #[test]
    fn late_receipt_after_lifecycle_change_is_held_without_applying_it() {
        struct MutateLifecyclePort<'a>(&'a RuntimeStore);
        impl CleanupPorts for MutateLifecyclePort<'_> {
            fn perform(
                &self,
                _agent: &Value,
                _effect: &CleanupEffect,
                _request_id: &str,
            ) -> Result<Value, EffectFailure> {
                let mut changed = agent(self.0);
                changed["epoch"] = json!(8);
                self.0
                    .transaction(|tx| {
                        AgentRepository::put(
                            tx,
                            &AgentRecord::from_value(changed).expect("record parses"),
                        )
                    })
                    .expect("new lifecycle commits");
                Ok(json!({"state":"removed"}))
            }
        }

        let (_directory, store) = fixture();
        put_agent(
            &store,
            json!({"imageWorkspace":true,"imageWorkspaceReady":true}),
        );
        assert!(matches!(
            cleanup_agent_workspace_with_ports(
                &store,
                "cleanup-agent",
                CleanupAction::Delete,
                &MutateLifecyclePort(&store)
            ),
            Err(WorkspaceCleanupError::Held)
        ));
        let saved = agent(&store);
        assert_eq!(saved["epoch"], 8);
        assert_eq!(saved["imageWorkspaceReady"], true);
        assert_eq!(saved["workspaceCleanupIntent"]["phase"], "unknown");
    }

    #[test]
    fn malformed_vm_save_receipt_holds_before_release() {
        let (_directory, store) = fixture();
        put_agent(
            &store,
            json!({
                "executionMode":"vm", "layrReady":true,
                "layrTurnSaves":{"turn-1":{"status":"pending"}},
                "agentArchive":{"cleanupPending":true}
            }),
        );
        let ports = FakePorts::new(vec![Ok(json!({"stateId":""}))]);
        assert!(matches!(
            cleanup_agent_workspace_with_ports(
                &store,
                "cleanup-agent",
                CleanupAction::Archive,
                &ports
            ),
            Err(WorkspaceCleanupError::Held)
        ));
        assert_eq!(ports.calls.lock().expect("calls mutex").len(), 1);
        let saved = agent(&store);
        assert_eq!(saved["layrReady"], true);
        assert_eq!(saved["workspaceCleanupIntent"]["phase"], "unknown");
    }

    #[test]
    fn definite_not_sent_retry_uses_a_new_guest_request_identity() {
        let (_directory, store) = fixture();
        put_agent(
            &store,
            json!({
                "executionMode":"vm", "layrReady":true,
                "layrTurnSaves":{"turn-1":{"status":"pending"}},
                "agentArchive":{"cleanupPending":true}
            }),
        );
        let ports = FakePorts::new(vec![
            Err(EffectFailure::NotSent),
            Ok(json!({"stateId":"state-1"})),
            Ok(json!({"stateId":"state-2"})),
        ]);
        assert!(matches!(
            cleanup_agent_workspace_with_ports(
                &store,
                "cleanup-agent",
                CleanupAction::Archive,
                &ports
            ),
            Err(WorkspaceCleanupError::NotSent)
        ));
        cleanup_agent_workspace_with_ports(&store, "cleanup-agent", CleanupAction::Archive, &ports)
            .expect("definitely unsent effect retries");
        let calls = ports.calls.lock().expect("calls mutex");
        assert_ne!(calls[0].1, calls[1].1);
        assert!(matches!(calls[2].0, CleanupEffect::ReleaseVmLine { .. }));
    }

    #[test]
    fn archive_saves_a_fixture_repository_ref_before_removing_its_worktree() {
        use std::process::Command;

        let (directory, store) = fixture();
        let repository = directory.path().join("project");
        let root = repository.join(".worktrees/codex-agents/cleanup-agent");
        std::fs::create_dir_all(root.parent().expect("worktree parent creates"))
            .expect("fixture parent creates");
        let init = Command::new("git")
            .args(["init", "-q"])
            .arg(&repository)
            .output()
            .expect("fixture git init starts");
        assert!(init.status.success(), "fixture repository initializes");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&repository)
                .args(args)
                .output()
                .expect("fixture Git starts");
            assert!(output.status.success(), "fixture Git succeeds");
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        git(&["config", "user.name", "Cleanup Fixture"]);
        git(&["config", "user.email", "cleanup@example.invalid"]);
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
        );
        super::cleanup_agent_workspace(&store, "cleanup-agent", CleanupAction::Archive)
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

    #[test]
    fn archive_allows_a_verified_detached_worktree() {
        use std::process::Command;

        let (directory, store) = fixture();
        let repository = directory.path().join("detached-project");
        let root = repository.join(".worktrees/codex-agents/cleanup-agent");
        std::fs::create_dir_all(root.parent().expect("worktree parent creates"))
            .expect("fixture parent creates");
        let init = Command::new("git")
            .args(["init", "-q"])
            .arg(&repository)
            .output()
            .expect("fixture git init starts");
        assert!(init.status.success(), "fixture repository initializes");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&repository)
                .args(args)
                .output()
                .expect("fixture Git starts");
            assert!(output.status.success(), "fixture Git succeeds");
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        git(&["config", "user.name", "Cleanup Fixture"]);
        git(&["config", "user.email", "cleanup@example.invalid"]);
        std::fs::write(repository.join("README.md"), "fixture\n").expect("fixture file writes");
        git(&["add", "README.md"]);
        git(&["commit", "-q", "-m", "fixture"]);
        git(&[
            "worktree",
            "add",
            "-q",
            "--detach",
            root.to_str().expect("fixture path is UTF-8"),
        ]);
        put_agent(
            &store,
            json!({
                "cwd":root, "branch":null, "worktreeReady":true,
                "agentArchive":{"at":7,"epoch":7,"cleanupPending":true}
            }),
        );
        super::cleanup_agent_workspace(&store, "cleanup-agent", CleanupAction::Archive)
            .expect("detached worktree archives");
        assert!(!root.exists(), "detached worktree is removed");
        assert_eq!(agent(&store)["cleanedWorktree"]["branch"], Value::Null);
    }

    #[test]
    fn archive_keeps_parent_when_a_registered_nested_worktree_exists() {
        use std::process::Command;

        let (directory, store) = fixture();
        let repository = directory.path().join("nested-project");
        let root = repository.join(".worktrees/codex-agents/cleanup-agent");
        let nested = root.join(".worktrees/nested");
        std::fs::create_dir_all(root.parent().expect("worktree parent creates"))
            .expect("fixture parent creates");
        let init = Command::new("git")
            .args(["init", "-q"])
            .arg(&repository)
            .output()
            .expect("fixture git init starts");
        assert!(init.status.success(), "fixture repository initializes");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&repository)
                .args(args)
                .output()
                .expect("fixture Git starts");
            assert!(output.status.success(), "fixture Git succeeds");
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        git(&["config", "user.name", "Cleanup Fixture"]);
        git(&["config", "user.email", "cleanup@example.invalid"]);
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
        );

        let error = super::cleanup_agent_workspace(&store, "cleanup-agent", CleanupAction::Archive)
            .expect_err("nested registration is unsafe to remove");
        assert!(matches!(error, WorkspaceCleanupError::NotSent));
        assert!(root.exists(), "parent worktree remains on disk");
        assert!(nested.exists(), "nested worktree remains on disk");
    }

    #[test]
    fn lifecycle_intent_has_stable_operation_identity() {
        let first = queued_cleanup_intent("cleanup-agent", 7, CleanupAction::Delete);
        let replay = queued_cleanup_intent("cleanup-agent", 7, CleanupAction::Delete);
        assert_eq!(first, replay);
        assert_eq!(first["phase"], "queued");
        assert_eq!(first["action"], "delete");
    }
}
