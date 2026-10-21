//! Git-backed checkpoint capture and preview for local workspaces.

use std::fs::{File, FileTimes, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::bounded_child::{self, ChildError};
use crate::event_write::truthy;
use serde_json::{Value, json};
use studio_runtime_store::{
    CaptureKind, CheckpointsRepository, RuntimeStore, StoreError, StoredCheckpointObject,
    WorkspaceSnapshot,
};
use thiserror::Error;

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PATCH_CHARS: usize = 300_000;
const MAX_PATCH_BYTES: usize = MAX_PATCH_CHARS * 4 + 4;
const ACTIVE_OPERATION_PHASES: &[&str] = &[
    "capture_pending",
    "capture_running",
    "local_mutation",
    "provider_pending",
    "provider_ready",
    "recovery_required",
];

#[derive(Debug, Error)]
pub enum CheckpointsError {
    #[error("Unknown managed agent")]
    UnknownAgent,
    #[error("Unknown checkpoint")]
    UnknownCheckpoint,
    #[error("Checkpoint belongs to another agent")]
    WrongAgent,
    #[error("This agent was deleted")]
    DeletedAgent,
    #[error("Studio Git checkpoints are unavailable for image workspaces")]
    ExecutionWorkspace,
    #[error("Studio Git tools are unavailable for image workspaces")]
    GitToolsUnavailable,
    #[error("Workspace activity changed before checkpoint capture")]
    ActivityChanged,
    #[error(
        "Checkpoint capture failed ({capture_error}); reservation settlement failed ({settlement_error})"
    )]
    SettlementFailed {
        capture_error: String,
        settlement_error: String,
    },
    #[error("Workspace recovery is required before using this workspace")]
    RecoveryRequired,
    #[error("An agent is using this workspace")]
    AgentBusy,
    #[error("A monitor is using this workspace")]
    MonitorBusy,
    #[error("A command or tool is still active")]
    TaskBusy,
    #[error("Git operation failed")]
    GitFailed,
    #[error("Git operation timed out")]
    GitTimeout,
    #[error("The workspace cannot be accessed")]
    WorkspaceUnavailable,
    #[error("Checkpoint worker failed")]
    Worker,
    #[error("Invalid stored object {object}/{id}")]
    InvalidStoredObject { object: &'static str, id: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreviewContext {
    pub agent: StoredCheckpointObject,
    pub checkpoint: StoredCheckpointObject,
    pub recovery_expected_tree: Option<String>,
}

pub async fn capture_context(
    store: &RuntimeStore,
    agent_id: &str,
) -> Result<WorkspaceSnapshot, CheckpointsError> {
    let agent_id = agent_id.to_owned();
    store
        .read_async(
            "runtime-services/checkpoints/capture_context",
            move |connection| {
                CheckpointsRepository::workspace_snapshot(connection, &agent_id)?
                    .ok_or(CheckpointsError::UnknownAgent)
            },
        )
        .await
}

pub async fn preview_context(
    store: &RuntimeStore,
    agent_id: &str,
    checkpoint_id: &str,
) -> Result<PreviewContext, CheckpointsError> {
    let (agent_id, checkpoint_id) = (agent_id.to_owned(), checkpoint_id.to_owned());
    store
        .read_async(
            "runtime-services/checkpoints/preview_context",
            move |connection| preview_context_in(connection, &agent_id, &checkpoint_id),
        )
        .await
}

fn preview_context_in(
    connection: &rusqlite::Connection,
    agent_id: &str,
    checkpoint_id: &str,
) -> Result<PreviewContext, CheckpointsError> {
    let agent = CheckpointsRepository::agent(connection, agent_id)?
        .ok_or(CheckpointsError::UnknownAgent)?;
    let checkpoint = CheckpointsRepository::checkpoint(connection, checkpoint_id)?
        .ok_or(CheckpointsError::UnknownCheckpoint)?;
    if checkpoint.record.get("agent").and_then(Value::as_str) != Some(agent_id) {
        return Err(CheckpointsError::WrongAgent);
    }
    let recovery_expected_tree =
        CheckpointsRepository::restore_expected_tree(connection, agent_id, checkpoint_id)?;
    Ok(PreviewContext {
        agent,
        checkpoint,
        recovery_expected_tree,
    })
}

pub async fn capture(
    store: Arc<RuntimeStore>,
    agent_id: String,
    label: String,
) -> Result<Value, CheckpointsError> {
    let snapshot = capture_context(&store, &agent_id).await?;
    let snapshot = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/checkpoints",
        move || {
            ensure_agent_available(&snapshot.agent)?;
            ensure_workspace_idle(&snapshot, None)?;
            Ok::<_, CheckpointsError>(snapshot)
        },
    )
    .await
    .map_err(|_| CheckpointsError::Worker)??;
    let checkpoint_id = new_uuid()?;
    let operation_id = format!("capture:{checkpoint_id}");
    let now = now_seconds();
    let transaction_operation_id = operation_id.clone();
    let (reserved, snapshot) = store
        .transaction_with(move |transaction| {
            let reserved = CheckpointsRepository::reserve_capture(
                transaction,
                &snapshot,
                &transaction_operation_id,
                now,
            )?;
            Ok::<_, CheckpointsError>((reserved, snapshot))
        })
        .await?;
    if !reserved {
        return Err(CheckpointsError::ActivityChanged);
    }
    finish_reserved(
        store,
        snapshot,
        label,
        checkpoint_id,
        operation_id,
        CaptureKind::Manual,
    )
    .await
}

/// Capture the pre-restore tree while the durable restore reservation owns the
/// workspace. The operation records the checkpoint identity before returning,
/// so a retry never creates a second before-restore checkpoint.
pub async fn capture_before_restore(
    store: Arc<RuntimeStore>,
    agent_id: String,
    operation_id: String,
    expected_tree: String,
) -> Result<Value, CheckpointsError> {
    capture_before_restore_inner(store, agent_id, operation_id, expected_tree).await
}

async fn capture_before_restore_inner(
    store: Arc<RuntimeStore>,
    agent_id: String,
    operation_id: String,
    expected_tree: String,
) -> Result<Value, CheckpointsError> {
    let read_store = Arc::clone(&store);
    let read_operation = operation_id.clone();
    let read_expected_tree = expected_tree.clone();
    let snapshot = capture_context(&store, &agent_id).await?;
    let snapshot = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/checkpoints",
        move || {
            if snapshot
                .agent
                .record
                .get("workspaceOperation")
                .and_then(Value::as_str)
                != Some("restore")
                || snapshot
                    .agent
                    .record
                    .get("workspaceReservationId")
                    .and_then(Value::as_str)
                    != Some(read_operation.as_str())
            {
                return Err(CheckpointsError::ActivityChanged);
            }
            ensure_workspace_idle(&snapshot, Some(&read_operation))?;
            let cwd = agent_string(&snapshot, "cwd")?.to_owned();
            let tree = snapshot_tree(&read_store, &cwd)?;
            if tree != read_expected_tree {
                return Err(CheckpointsError::ActivityChanged);
            }
            let checkpoint_id =
                uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, read_operation.as_bytes())
                    .to_string();
            Ok::<_, CheckpointsError>((snapshot, cwd, tree, checkpoint_id))
        },
    )
    .await
    .map_err(|_| CheckpointsError::Worker)??;
    let (snapshot, cwd, tree, checkpoint_id) = snapshot;
    let read_checkpoint_id = checkpoint_id.clone();
    let existing = store
        .read_async("runtime-services/checkpoints", move |db| {
            studio_runtime_store::CheckpointsRepository::checkpoint(db, &read_checkpoint_id)
        })
        .await?;
    if let Some(existing) = existing {
        return Ok(checkpoint_summary(existing.record));
    }
    let git_agent_id = agent_id.clone();
    let git_checkpoint_id = checkpoint_id.clone();
    let git_cwd = cwd.clone();
    let git_tree = tree.clone();
    let commit_stage = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/checkpoints",
        move || {
            let commit_output = git(
                &git_cwd,
                &["commit-tree", git_tree.as_str(), "-p", "HEAD"],
                &[
                    ("GIT_AUTHOR_NAME", "Codex Agents"),
                    ("GIT_AUTHOR_EMAIL", "local@codex-agents.invalid"),
                    ("GIT_COMMITTER_NAME", "Codex Agents"),
                    ("GIT_COMMITTER_EMAIL", "local@codex-agents.invalid"),
                ],
                Some(b"Codex Agents workspace checkpoint\n"),
                128,
            )?;
            let commit = String::from_utf8_lossy(&commit_output).trim().to_owned();
            let reference =
                format!("refs/codex-agents/checkpoints/{git_agent_id}/{git_checkpoint_id}");
            git(&git_cwd, &["update-ref", &reference, &commit], &[], None, 0)?;
            Ok::<_, CheckpointsError>((commit, reference))
        },
    )
    .await
    .map_err(|_| CheckpointsError::Worker)??;
    let (commit, reference) = commit_stage;
    let parent_id = snapshot
        .agent
        .record
        .get("checkpointHistoryHead")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let parent_boundary = snapshot
        .parent_checkpoint
        .as_ref()
        .and_then(|parent| parent.record.get("historyBoundary"))
        .and_then(Value::as_i64);
    let root_id = snapshot
        .agent
        .record
        .get("rootId")
        .cloned()
        .unwrap_or(Value::Null);
    let thread_id = snapshot
        .agent
        .record
        .get("threadId")
        .cloned()
        .unwrap_or(Value::Null);
    let turn_id = snapshot
        .agent
        .record
        .get("lastCompletedTurn")
        .cloned()
        .unwrap_or(Value::Null);
    let transaction_operation_id = operation_id;
    let transaction_agent_id = agent_id;
    let transaction_checkpoint_id = checkpoint_id;
    let transaction_cwd = cwd;
    let transaction_tree = tree;
    let transaction_commit = commit;
    let transaction_reference = reference;
    store.transaction_with(move |tx| {
        let Some(mut operation) = studio_runtime_store::workspace_operations::WorkspaceOperationsRepository::operation(tx, &transaction_operation_id)? else {
            return Err(CheckpointsError::ActivityChanged);
        };
        if operation.get("kind").and_then(Value::as_str) != Some("restore")
            || operation.get("phase").and_then(Value::as_str) != Some("local_mutation")
            || operation.get("beforeCheckpoint").is_some_and(|value| !value.is_null())
            || !studio_runtime_store::CheckpointsRepository::workspace_unchanged(tx, &snapshot)?
        {
            return Err(CheckpointsError::ActivityChanged);
        }
        let (history_delta, history_boundary) = studio_runtime_store::CheckpointsRepository::history_delta(tx, &transaction_agent_id, parent_boundary)?;
        let record = json!({
            "historyParent":parent_id,"historyDelta":history_delta,"historyBoundary":history_boundary,
            "id":&transaction_checkpoint_id,"agent":&transaction_agent_id,"rootId":root_id,
            "label":"Before restore","tree":&transaction_tree,"commit":&transaction_commit,"ref":&transaction_reference,
            "threadId":thread_id,
            "turnId":turn_id,"created":now_seconds(),"cwd":transaction_cwd
        });
        tx.execute("INSERT INTO runtime_checkpoints(id,record) VALUES(?1,?2)", rusqlite::params![&transaction_checkpoint_id, record.to_string()])
            .map_err(StoreError::from)?;
        let Some(mut agent) = studio_runtime_store::CheckpointsRepository::agent(tx, &transaction_agent_id)? else {
            return Err(CheckpointsError::UnknownAgent);
        };
        agent.record["checkpointHistoryHead"] = json!(transaction_checkpoint_id);
        let agent = studio_runtime_store::AgentRecord::from_value(agent.record)
            .map_err(StoreError::from)?;
        studio_runtime_store::AgentRepository::put(tx, &agent)?;
        operation["beforeCheckpoint"] = json!(transaction_checkpoint_id);
        studio_runtime_store::workspace_operations::WorkspaceOperationsRepository::put_operation(tx, &operation)?;
        Ok::<_, CheckpointsError>(record)
    }).await
}

/// Restore a previously previewed tree in the caller's already-reserved,
/// isolated worktree. The expected tree is rechecked immediately before Git
/// mutates the index and files.
pub fn restore_files(
    store: &RuntimeStore,
    cwd: &str,
    expected_tree: &str,
    target_tree: &str,
) -> Result<(), CheckpointsError> {
    if !is_full_object_id(expected_tree) || !is_full_object_id(target_tree) {
        return Err(CheckpointsError::GitFailed);
    }
    if snapshot_tree(store, cwd)? != expected_tree {
        return Err(CheckpointsError::ActivityChanged);
    }
    git(cwd, &["read-tree", expected_tree], &[], None, 0)?;
    git(
        cwd,
        &["read-tree", "--reset", "-u", target_tree],
        &[],
        None,
        0,
    )?;
    if snapshot_tree(store, cwd)? != target_tree {
        return Err(CheckpointsError::GitFailed);
    }
    Ok(())
}

/// Validate a checkpoint tree before a restore creates its provider thread.
pub fn validate_tree(cwd: &str, tree: &str) -> Result<(), CheckpointsError> {
    if !is_full_object_id(tree) {
        return Err(CheckpointsError::GitFailed);
    }
    git(
        cwd,
        &["cat-file", "-e", &format!("{tree}^{{tree}}")],
        &[],
        None,
        0,
    )?;
    Ok(())
}

pub(crate) async fn finish_reserved(
    store: Arc<RuntimeStore>,
    snapshot: WorkspaceSnapshot,
    label: String,
    checkpoint_id: String,
    operation_id: String,
    kind: CaptureKind,
) -> Result<Value, CheckpointsError> {
    let result = capture_reserved(
        Arc::clone(&store),
        snapshot,
        label,
        checkpoint_id,
        operation_id.clone(),
        kind,
    )
    .await;
    match result {
        Ok(checkpoint) => Ok(checkpoint_summary(checkpoint)),
        Err(error) => {
            let message = error.to_string();
            let settlement_operation_id = operation_id;
            let transaction_operation_id = settlement_operation_id.clone();
            let settlement = store
                .transaction_with(move |transaction| {
                    CheckpointsRepository::settle_capture(
                        transaction,
                        &transaction_operation_id,
                        Some(&message),
                        now_seconds(),
                    )?;
                    Ok::<_, CheckpointsError>(())
                })
                .await;
            match settlement {
                Ok(()) => Err(error),
                Err(settlement_error) => Err(CheckpointsError::SettlementFailed {
                    capture_error: error.to_string(),
                    settlement_error: settlement_error.to_string(),
                }),
            }
        }
    }
}

async fn capture_reserved(
    store: Arc<RuntimeStore>,
    snapshot: WorkspaceSnapshot,
    label: String,
    checkpoint_id: String,
    operation_id: String,
    kind: CaptureKind,
) -> Result<Value, CheckpointsError> {
    let transaction_operation_id = operation_id.clone();
    let preflight_operation_id = transaction_operation_id.clone();
    let snapshot = capture_context(&store, &snapshot.agent.id).await?;
    let snapshot = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/checkpoints",
        move || {
            if snapshot
                .agent
                .record
                .get("workspaceReservationId")
                .and_then(Value::as_str)
                != Some(preflight_operation_id.as_str())
                || snapshot
                    .agent
                    .record
                    .get("workspaceOperation")
                    .and_then(Value::as_str)
                    != Some(kind.as_str())
            {
                return Err(CheckpointsError::ActivityChanged);
            }
            ensure_workspace_idle(&snapshot, Some(&preflight_operation_id))?;
            Ok::<_, CheckpointsError>(snapshot)
        },
    )
    .await
    .map_err(|_| CheckpointsError::Worker)??;
    let (started, snapshot) = store
        .transaction_with(move |transaction| {
            let started = CheckpointsRepository::set_capture_running(
                transaction,
                &snapshot,
                &transaction_operation_id,
                now_seconds(),
            )
            .map_err(CheckpointsError::from)?;
            Ok::<_, CheckpointsError>((started, snapshot))
        })
        .await?;
    if !started {
        return Err(CheckpointsError::ActivityChanged);
    }
    let git_store = Arc::clone(&store);
    let transaction_agent_id = snapshot.agent.id.clone();
    let transaction_operation_id = operation_id;
    let transaction_checkpoint_id = checkpoint_id;
    let transaction_label = label;
    let git_agent_id = transaction_agent_id.clone();
    let git_checkpoint_id = transaction_checkpoint_id.clone();
    let (parent_id, parent_boundary, root_id, thread_id, turn_id, cwd, tree, commit, reference) =
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/checkpoints",
            move || {
                let cwd = agent_string(&snapshot, "cwd")?.to_owned();
                let tree = snapshot_tree(&git_store, &cwd)?;
                let commit_output = git(
                    &cwd,
                    &["commit-tree", tree.as_str(), "-p", "HEAD"],
                    &[
                        ("GIT_AUTHOR_NAME", "Codex Agents"),
                        ("GIT_AUTHOR_EMAIL", "local@codex-agents.invalid"),
                        ("GIT_COMMITTER_NAME", "Codex Agents"),
                        ("GIT_COMMITTER_EMAIL", "local@codex-agents.invalid"),
                    ],
                    Some(b"Codex Agents workspace checkpoint\n"),
                    128,
                )?;
                let commit = String::from_utf8_lossy(&commit_output).trim().to_owned();
                let reference =
                    format!("refs/codex-agents/checkpoints/{git_agent_id}/{git_checkpoint_id}");
                git(&cwd, &["update-ref", &reference, &commit], &[], None, 0)?;
                let parent_id = snapshot
                    .agent
                    .record
                    .get("checkpointHistoryHead")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let parent_boundary = snapshot
                    .parent_checkpoint
                    .as_ref()
                    .and_then(|parent| parent.record.get("historyBoundary"))
                    .and_then(Value::as_i64);
                let root_id = snapshot
                    .agent
                    .record
                    .get("rootId")
                    .cloned()
                    .unwrap_or(Value::Null);
                let thread_id = snapshot
                    .agent
                    .record
                    .get("threadId")
                    .cloned()
                    .unwrap_or(Value::Null);
                let turn_id = snapshot
                    .agent
                    .record
                    .get("lastCompletedTurn")
                    .cloned()
                    .unwrap_or(Value::Null);
                Ok::<_, CheckpointsError>((
                    parent_id,
                    parent_boundary,
                    root_id,
                    thread_id,
                    turn_id,
                    cwd,
                    tree,
                    commit,
                    reference,
                ))
            },
        )
        .await
        .map_err(|_| CheckpointsError::Worker)??;
    store
        .transaction_with(move |transaction| {
            let (history_delta, history_boundary) = CheckpointsRepository::history_delta(
                transaction,
                &transaction_agent_id,
                parent_boundary,
            )?;
            let record = json!({
                "historyParent": parent_id,
                "historyDelta": history_delta,
                "historyBoundary": history_boundary,
                "id": &transaction_checkpoint_id,
                "agent": &transaction_agent_id,
                "rootId": root_id,
                "label": &transaction_label,
                "tree": &tree,
                "commit": &commit,
                "ref": &reference,
                "threadId": thread_id,
                "turnId": turn_id,
                "created": now_seconds(),
                "cwd": cwd,
            });
            if !CheckpointsRepository::complete_capture(
                transaction,
                &transaction_agent_id,
                &transaction_operation_id,
                parent_id.as_deref(),
                &record,
                now_seconds(),
            )? {
                return Err(CheckpointsError::ActivityChanged);
            }
            Ok::<_, CheckpointsError>(record)
        })
        .await
}

pub fn preview(store: &RuntimeStore, context: PreviewContext) -> Result<Value, CheckpointsError> {
    ensure_agent_identity(&context.agent)?;
    if is_image_workspace(&context.agent) {
        return Err(CheckpointsError::GitToolsUnavailable);
    }
    ensure_agent_available(&context.agent)?;
    let cwd = context
        .agent
        .record
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| invalid_agent(&context.agent.id))?;
    let checkpoint = &context.checkpoint.record;
    let tree = checkpoint
        .get("tree")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_checkpoint(&context.checkpoint.id))?;
    if !is_full_object_id(tree) {
        return Err(invalid_checkpoint(&context.checkpoint.id));
    }
    let current_tree = snapshot_tree(store, cwd)?;
    let expected_tree = context
        .recovery_expected_tree
        .as_deref()
        .unwrap_or(&current_tree);
    if !is_full_object_id(expected_tree) {
        return Err(invalid_checkpoint(&context.checkpoint.id));
    }
    let (bytes, output_truncated) = git_limited(
        cwd,
        &[
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--end-of-options",
            expected_tree,
            tree,
            "--",
        ],
        &[],
        None,
        MAX_PATCH_BYTES,
    )?;
    let raw_patch = String::from_utf8_lossy(&bytes).into_owned();
    let truncated = output_truncated || raw_patch.chars().count() > MAX_PATCH_CHARS;
    let patch = raw_patch.chars().take(MAX_PATCH_CHARS).collect::<String>();
    let can_restore = false;
    let mut summary = checkpoint.clone();
    remove_summary_payload(&mut summary);
    Ok(json!({
        "checkpoint": summary,
        "expectedTree": expected_tree,
        "diff": patch,
        "patch": patch,
        "truncated": truncated,
        "canRestore": can_restore,
    }))
}

pub fn ensure_agent_available(agent: &StoredCheckpointObject) -> Result<(), CheckpointsError> {
    ensure_agent_identity(agent)?;
    if is_image_workspace(agent) {
        return Err(CheckpointsError::ExecutionWorkspace);
    }
    Ok(())
}

fn ensure_agent_identity(agent: &StoredCheckpointObject) -> Result<(), CheckpointsError> {
    if agent.record.get("id").and_then(Value::as_str) != Some(agent.id.as_str()) {
        return Err(invalid_agent(&agent.id));
    }
    if truthy(agent.record.get("deletedAt")) {
        return Err(CheckpointsError::DeletedAgent);
    }
    Ok(())
}

fn is_image_workspace(agent: &StoredCheckpointObject) -> bool {
    agent.record.get("imageWorkspace").and_then(Value::as_bool) == Some(true)
}

pub fn ensure_workspace_idle(
    snapshot: &WorkspaceSnapshot,
    reservation_id: Option<&str>,
) -> Result<(), CheckpointsError> {
    let cwd = agent_string(snapshot, "cwd")?;
    let target = Path::new(cwd)
        .canonicalize()
        .map_err(|_| CheckpointsError::WorkspaceUnavailable)?;
    let agents = snapshot
        .agents
        .iter()
        .map(|agent| {
            let record = agent
                .record
                .as_object()
                .ok_or_else(|| invalid_agent(&agent.id))?;
            if record.get("id").and_then(Value::as_str) != Some(agent.id.as_str()) {
                return Err(invalid_agent(&agent.id));
            }
            Ok((agent.id.as_str(), record))
        })
        .collect::<Result<Vec<_>, CheckpointsError>>()?;
    let by_id = agents
        .iter()
        .map(|(id, record)| (*id, *record))
        .collect::<std::collections::BTreeMap<_, _>>();
    for operation in &snapshot.operations {
        validate_internal_record(operation, "workspace_operations")?;
        if reservation_id == Some(operation.id.as_str()) {
            continue;
        }
        if !operation
            .record
            .get("phase")
            .and_then(Value::as_str)
            .is_some_and(|phase| ACTIVE_OPERATION_PHASES.contains(&phase))
        {
            continue;
        }
        let Some(operation_agent) = operation.record.get("agent").and_then(Value::as_str) else {
            continue;
        };
        if let Some(record) = by_id.get(operation_agent)
            && canonical_cwd(record).is_ok_and(|path| path == target)
        {
            return Err(CheckpointsError::RecoveryRequired);
        }
    }
    for (id, record) in &agents {
        let active = truthy(record.get("inFlight")) || truthy(record.get("workspaceOperation"));
        let own_reservation = reservation_id.is_some_and(|reservation| {
            *id == snapshot.agent.id
                && record.get("workspaceReservationId").and_then(Value::as_str) == Some(reservation)
                && matches!(
                    record.get("workspaceOperation").and_then(Value::as_str),
                    Some("capture" | "checkpoint" | "restore")
                )
        });
        if active
            && canonical_cwd(record).is_ok_and(|path| path == target)
            && (truthy(record.get("inFlight")) || !own_reservation)
        {
            return Err(CheckpointsError::AgentBusy);
        }
    }
    for monitor in &snapshot.monitors {
        validate_internal_record(monitor, "monitors")?;
        if matches!(
            monitor.record.get("status").and_then(Value::as_str),
            Some("running" | "starting" | "approval")
        ) {
            let path = monitor
                .record
                .get("cwd")
                .and_then(Value::as_str)
                .ok_or_else(|| CheckpointsError::InvalidStoredObject {
                    object: "monitors",
                    id: monitor.id.clone(),
                })?;
            if Path::new(path)
                .canonicalize()
                .is_ok_and(|path| path == target)
            {
                return Err(CheckpointsError::MonitorBusy);
            }
        }
    }
    for task in &snapshot.tasks {
        validate_internal_record(task, "tasks")?;
        if task.record.get("status").and_then(Value::as_str) == Some("running")
            && task
                .record
                .get("agent")
                .and_then(Value::as_str)
                .and_then(|id| by_id.get(id))
                .is_some_and(|record| canonical_cwd(record).is_ok_and(|path| path == target))
        {
            return Err(CheckpointsError::TaskBusy);
        }
    }
    Ok(())
}

fn validate_internal_record(
    record: &StoredCheckpointObject,
    object: &'static str,
) -> Result<(), CheckpointsError> {
    let Some(value) = record.record.as_object() else {
        return Err(CheckpointsError::InvalidStoredObject {
            object,
            id: record.id.clone(),
        });
    };
    if value.get("id").and_then(Value::as_str) != Some(record.id.as_str()) {
        return Err(CheckpointsError::InvalidStoredObject {
            object,
            id: record.id.clone(),
        });
    }
    let required = match object {
        "workspace_operations" => &["kind", "agent", "phase"][..],
        "tasks" => &["agent", "status"][..],
        "monitors" => &["status"][..],
        _ => &[][..],
    };
    if required.iter().any(|field| {
        value
            .get(*field)
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    }) {
        return Err(CheckpointsError::InvalidStoredObject {
            object,
            id: record.id.clone(),
        });
    }
    Ok(())
}

fn canonical_cwd(record: &serde_json::Map<String, Value>) -> Result<PathBuf, CheckpointsError> {
    studio_runtime_store::debug_assert_outside_sql();
    let path = record
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .ok_or(CheckpointsError::WorkspaceUnavailable)?;
    Path::new(path)
        .canonicalize()
        .map_err(|_| CheckpointsError::WorkspaceUnavailable)
}

pub fn snapshot_tree(store: &RuntimeStore, cwd: &str) -> Result<String, CheckpointsError> {
    snapshot_tree_at(cwd, store.root())
}

fn snapshot_tree_at(cwd: &str, root: &Path) -> Result<String, CheckpointsError> {
    let temporary = tempfile::Builder::new()
        .prefix("checkpoint-")
        .tempdir_in(root)
        .map_err(|_| CheckpointsError::WorkspaceUnavailable)?;
    let index = temporary.path().join("index");
    let raw_path = git(cwd, &["rev-parse", "--git-path", "index"], &[], None, 4096)?;
    let git_index = PathBuf::from(String::from_utf8_lossy(&raw_path).trim());
    let source = if git_index.is_absolute() {
        git_index
    } else {
        Path::new(cwd).join(git_index)
    };
    let index_value = index.to_string_lossy().into_owned();
    if source.is_file() {
        let source_file =
            File::open(&source).map_err(|_| CheckpointsError::WorkspaceUnavailable)?;
        let metadata = source_file
            .metadata()
            .map_err(|_| CheckpointsError::WorkspaceUnavailable)?;
        let mut target = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&index)
            .map_err(|_| CheckpointsError::WorkspaceUnavailable)?;
        let mut source_file = source_file;
        std::io::copy(&mut source_file, &mut target)
            .map_err(|_| CheckpointsError::WorkspaceUnavailable)?;
        let mut times = FileTimes::new();
        if let Ok(accessed) = metadata.accessed() {
            times = times.set_accessed(accessed);
        }
        if let Ok(modified) = metadata.modified() {
            times = times.set_modified(modified);
        }
        target
            .set_times(times)
            .map_err(|_| CheckpointsError::WorkspaceUnavailable)?;
    } else {
        git(
            cwd,
            &["read-tree", "HEAD"],
            &[("GIT_INDEX_FILE", index_value.as_str())],
            None,
            0,
        )?;
    }
    git(
        cwd,
        &["add", "-A", "--", "."],
        &[("GIT_INDEX_FILE", index_value.as_str())],
        None,
        0,
    )?;
    let tree = git(
        cwd,
        &["write-tree"],
        &[("GIT_INDEX_FILE", index_value.as_str())],
        None,
        128,
    )?;
    Ok(String::from_utf8_lossy(&tree).trim().to_owned())
}

pub(crate) fn git(
    cwd: &str,
    args: &[&str],
    environment: &[(&str, &str)],
    input: Option<&[u8]>,
    stdout_limit: usize,
) -> Result<Vec<u8>, CheckpointsError> {
    let (stdout, truncated) = git_with_output_limit(cwd, args, environment, input, stdout_limit)?;
    if truncated {
        return Err(CheckpointsError::GitFailed);
    }
    Ok(stdout)
}

fn git_limited(
    cwd: &str,
    args: &[&str],
    environment: &[(&str, &str)],
    input: Option<&[u8]>,
    max_output_bytes: usize,
) -> Result<(Vec<u8>, bool), CheckpointsError> {
    git_with_output_limit(cwd, args, environment, input, max_output_bytes)
}

fn git_with_output_limit(
    cwd: &str,
    args: &[&str],
    environment: &[(&str, &str)],
    input: Option<&[u8]>,
    max_output_bytes: usize,
) -> Result<(Vec<u8>, bool), CheckpointsError> {
    let output = bounded_child::run(bounded_child::Request {
        program: std::ffi::OsStr::new("git"),
        cwd: std::ffi::OsStr::new(cwd),
        args,
        environment,
        input,
        stdout_limit: max_output_bytes,
        stderr_limit: 4800,
        timeout: GIT_TIMEOUT,
    })
    .map_err(|error| match error {
        ChildError::Failed(_) => CheckpointsError::GitFailed,
        ChildError::TimedOut => CheckpointsError::GitTimeout,
        ChildError::Launch(detail) => {
            eprintln!("checkpoint Git process could not run: {detail}");
            CheckpointsError::GitFailed
        }
    })?;
    Ok((output.stdout.bytes, output.stdout.truncated))
}

fn is_full_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn checkpoint_summary(mut checkpoint: Value) -> Value {
    remove_summary_payload(&mut checkpoint);
    checkpoint
}

fn remove_summary_payload(checkpoint: &mut Value) {
    if let Some(object) = checkpoint.as_object_mut() {
        object.remove("items");
        object.remove("historyDelta");
        object.remove("_payloadBlobs");
    }
}

fn agent_string<'a>(
    snapshot: &'a WorkspaceSnapshot,
    name: &str,
) -> Result<&'a str, CheckpointsError> {
    snapshot
        .agent
        .record
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid_agent(&snapshot.agent.id))
}

fn invalid_agent(id: &str) -> CheckpointsError {
    CheckpointsError::InvalidStoredObject {
        object: "agents",
        id: id.to_owned(),
    }
}

fn invalid_checkpoint(id: &str) -> CheckpointsError {
    CheckpointsError::InvalidStoredObject {
        object: "checkpoints",
        id: id.to_owned(),
    }
}

fn new_uuid() -> Result<String, CheckpointsError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| CheckpointsError::WorkspaceUnavailable)?;
    Ok(uuid::Builder::from_random_bytes(bytes)
        .into_uuid()
        .to_string())
}

pub(crate) fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{CheckpointsError, ensure_workspace_idle};
    use serde_json::json;
    use studio_runtime_store::{StoredCheckpointObject, WorkspaceSnapshot};

    #[test]
    fn capture_reservation_allows_its_owner_but_keeps_other_activity_blocked() -> std::io::Result<()>
    {
        let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
        let agent = StoredCheckpointObject {
            id: "checkpoint-agent".to_owned(),
            record: json!({
                "id": "checkpoint-agent",
                "cwd": cwd,
                "inFlight": false,
                "workspaceOperation": "capture",
                "workspaceReservationId": "capture:checkpoint",
            }),
        };
        let operation = StoredCheckpointObject {
            id: "capture:checkpoint".to_owned(),
            record: json!({
                "id": "capture:checkpoint",
                "kind": "capture",
                "agent": "checkpoint-agent",
                "phase": "capture_pending",
            }),
        };
        let missing_cwd = std::env::temp_dir()
            .join(format!("codex-checkpoint-missing-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let unavailable_agent = StoredCheckpointObject {
            id: "unavailable-agent".to_owned(),
            record: json!({
                "id": "unavailable-agent",
                "cwd": missing_cwd,
                "workspaceOperation": "checkpoint",
                "inFlight": false,
            }),
        };
        let unavailable_operation = StoredCheckpointObject {
            id: "checkpoint:unavailable".to_owned(),
            record: json!({
                "id": "checkpoint:unavailable",
                "kind": "checkpoint",
                "agent": "unavailable-agent",
                "phase": "capture_running",
            }),
        };
        let snapshot = WorkspaceSnapshot {
            agent: agent.clone(),
            parent_checkpoint: None,
            agents: vec![agent, unavailable_agent],
            operations: vec![operation, unavailable_operation],
            monitors: Vec::new(),
            tasks: Vec::new(),
        };

        assert!(ensure_workspace_idle(&snapshot, Some("capture:checkpoint")).is_ok());
        assert!(matches!(
            ensure_workspace_idle(&snapshot, None),
            Err(CheckpointsError::RecoveryRequired)
        ));

        let mut busy_snapshot = snapshot;
        busy_snapshot.monitors.push(StoredCheckpointObject {
            id: "active-monitor".to_owned(),
            record: json!({
                "id": "active-monitor",
                "cwd": cwd,
                "status": "running",
            }),
        });
        assert!(matches!(
            ensure_workspace_idle(&busy_snapshot, Some("capture:checkpoint")),
            Err(CheckpointsError::MonitorBusy)
        ));
        Ok(())
    }
}
