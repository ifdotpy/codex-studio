//! Git-backed checkpoint capture, preview, and restore for local workspaces.
//!
//! Restore follows Python `WorkspaceRuntime.restore_checkpoint` and
//! `checkpoint_preview` in `codex_workspace.py`. The deliberate safety
//! difference is `unknown_restore_outcome_is_held`: an ambiguous provider
//! receipt is persisted as recovery-required and is never resent.

use std::fs::{File, FileTimes, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::account_session_runtime::{
    AccountSessionError, AccountSessionPort, ConnectionScope, ProviderRequest, ProviderResponse,
    RequestCertainty,
};
use crate::account_worker::retained::RequestActor;
use crate::bounded_child::{self, ChildError};
use crate::event_write::truthy;
use crate::native_actions::NativeRequest;
use crate::workspace_operations::{CheckpointHistory, ProviderTicket};
use serde_json::{Value, json};
use studio_runtime_store::workspace_operations::WorkspaceOperationsRepository;
use studio_runtime_store::{
    AgentRecord, AgentRepository, CaptureKind, CheckpointsRepository, RestoreReservation,
    RuntimeStore, StoreError, StoredCheckpointObject, TranscriptRepository, WorkspaceSnapshot,
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
    #[error("Workspace checkpoint state is not initialized")]
    MissingSchema,
    #[error("Studio Git checkpoints are unavailable for image workspaces")]
    ExecutionWorkspace,
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
    #[error("Provider request outcome is unknown; inspect restore recovery before retrying")]
    ProviderUnknown,
    #[error("Not implemented: requires provider-session/native-request")]
    ProviderUnavailable,
    #[error("Provider rejected the restore request")]
    ProviderRejected,
    #[error("Native thread preparation failed: {0}")]
    Preparation(String),
    #[error(transparent)]
    WorkspaceOperation(#[from] crate::workspace_operations::WorkspaceOperationError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
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

struct RestoreContext {
    snapshot: WorkspaceSnapshot,
    agent: StoredCheckpointObject,
    checkpoint: StoredCheckpointObject,
}

/// Keep thread/start params behind a small adapter while the shared account
/// preparation module owns their construction and its exact Python parity.
trait RestoreStartParams {
    fn build(
        &self,
        store: &RuntimeStore,
        resources: &crate::account_worker::preparation::PreparationResources,
        agent_id: &str,
        attempt_id: &str,
        generation: u64,
    ) -> Result<Value, CheckpointsError>;
}

struct SharedRestoreStartParams;

impl RestoreStartParams for SharedRestoreStartParams {
    fn build(
        &self,
        store: &RuntimeStore,
        resources: &crate::account_worker::preparation::PreparationResources,
        agent_id: &str,
        attempt_id: &str,
        generation: u64,
    ) -> Result<Value, CheckpointsError> {
        let attempt_id = format!(
            "checkpoint-restore:{}",
            &studio_runtime_store::sha256_hex(attempt_id.as_bytes())[..32]
        );
        let prepared = crate::account_worker::preparation::prepare_native_thread(
            store,
            resources,
            agent_id,
            &attempt_id,
            generation,
        )
        .map_err(|error| CheckpointsError::Preparation(error.to_string()))?;
        Ok(prepared.params)
    }
}

pub fn capture_context(
    store: &RuntimeStore,
    agent_id: &str,
) -> Result<WorkspaceSnapshot, CheckpointsError> {
    let connection = store.read()?;
    if !CheckpointsRepository::tables_available(&connection)? {
        return Err(CheckpointsError::MissingSchema);
    }
    CheckpointsRepository::workspace_snapshot(&connection, agent_id)?
        .ok_or(CheckpointsError::UnknownAgent)
}

pub fn preview_context(
    store: &RuntimeStore,
    agent_id: &str,
    checkpoint_id: &str,
) -> Result<PreviewContext, CheckpointsError> {
    let connection = store.read()?;
    if !CheckpointsRepository::checkpoint_table_available(&connection)? {
        return Err(CheckpointsError::MissingSchema);
    }
    let agent = CheckpointsRepository::agent(&connection, agent_id)?
        .ok_or(CheckpointsError::UnknownAgent)?;
    let checkpoint = CheckpointsRepository::checkpoint(&connection, checkpoint_id)?
        .ok_or(CheckpointsError::UnknownCheckpoint)?;
    if let Some(owner) = checkpoint.record.get("agent").and_then(Value::as_str)
        && owner != agent_id
    {
        return Err(CheckpointsError::WrongAgent);
    }
    let recovery_expected_tree =
        CheckpointsRepository::restore_expected_tree(&connection, agent_id, checkpoint_id)?;
    Ok(PreviewContext {
        agent,
        checkpoint,
        recovery_expected_tree,
    })
}

pub fn capture(
    store: &RuntimeStore,
    snapshot: WorkspaceSnapshot,
    label: &str,
) -> Result<Value, CheckpointsError> {
    ensure_agent_available(&snapshot.agent)?;
    ensure_workspace_idle(&snapshot, None)?;
    let checkpoint_id = new_uuid()?;
    let operation_id = format!("capture:{checkpoint_id}");
    let now = now_seconds();
    store.transaction_with(|transaction| {
        if !CheckpointsRepository::reserve_capture(transaction, &snapshot, &operation_id, now)? {
            return Err(CheckpointsError::ActivityChanged);
        }
        Ok::<_, CheckpointsError>(())
    })?;

    finish_reserved(
        store,
        &snapshot,
        label,
        &checkpoint_id,
        &operation_id,
        CaptureKind::Manual,
    )
}

pub(crate) fn finish_reserved(
    store: &RuntimeStore,
    snapshot: &WorkspaceSnapshot,
    label: &str,
    checkpoint_id: &str,
    operation_id: &str,
    kind: CaptureKind,
) -> Result<Value, CheckpointsError> {
    let result = capture_reserved(store, snapshot, label, checkpoint_id, operation_id, kind);
    match result {
        Ok(checkpoint) => Ok(checkpoint_summary(checkpoint)),
        Err(error) => {
            let message = error.to_string();
            let settlement = store.transaction_with(|transaction| {
                CheckpointsRepository::settle_capture(
                    transaction,
                    operation_id,
                    Some(&message),
                    now_seconds(),
                )?;
                Ok::<_, CheckpointsError>(())
            });
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

fn capture_reserved(
    store: &RuntimeStore,
    snapshot: &WorkspaceSnapshot,
    label: &str,
    checkpoint_id: &str,
    operation_id: &str,
    kind: CaptureKind,
) -> Result<Value, CheckpointsError> {
    let snapshot = capture_context(store, &snapshot.agent.id)?;
    if snapshot
        .agent
        .record
        .get("workspaceReservationId")
        .and_then(Value::as_str)
        != Some(operation_id)
        || snapshot
            .agent
            .record
            .get("workspaceOperation")
            .and_then(Value::as_str)
            != Some(kind.as_str())
    {
        return Err(CheckpointsError::ActivityChanged);
    }
    ensure_workspace_idle(&snapshot, Some(operation_id))?;
    let started = store.transaction_with(|transaction| {
        CheckpointsRepository::set_capture_running(
            transaction,
            &snapshot,
            operation_id,
            now_seconds(),
        )
        .map_err(CheckpointsError::from)
    })?;
    if !started {
        return Err(CheckpointsError::ActivityChanged);
    }
    let agent_id = &snapshot.agent.id;
    let cwd = agent_string(&snapshot, "cwd")?;
    let tree = snapshot_tree(store, cwd)?;
    let commit_output = git(
        cwd,
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
    let reference = format!("refs/codex-agents/checkpoints/{agent_id}/{checkpoint_id}");
    git(cwd, &["update-ref", &reference, &commit], &[], None, 0)?;

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
    store.transaction_with(|transaction| {
        let (history_delta, history_boundary) =
            CheckpointsRepository::history_delta(transaction, agent_id, parent_boundary)?;
        let record = json!({
            "historyParent": parent_id,
            "historyDelta": history_delta,
            "historyBoundary": history_boundary,
            "id": checkpoint_id,
            "agent": agent_id,
            "rootId": snapshot.agent.record.get("rootId"),
            "label": label,
            "tree": tree,
            "commit": commit,
            "ref": reference,
            "threadId": snapshot.agent.record.get("threadId"),
            "turnId": snapshot.agent.record.get("lastCompletedTurn"),
            "created": now_seconds(),
            "cwd": cwd,
        });
        if !CheckpointsRepository::complete_capture(
            transaction,
            agent_id,
            operation_id,
            parent_id.as_deref(),
            &record,
            now_seconds(),
        )? {
            return Err(CheckpointsError::ActivityChanged);
        }
        Ok::<_, CheckpointsError>(record)
    })
}

pub fn preview(store: &RuntimeStore, context: PreviewContext) -> Result<Value, CheckpointsError> {
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
    let mut can_restore = truthy(context.agent.record.get("worktreeReady"))
        && !truthy(context.agent.record.get("inFlight"));
    if context.recovery_expected_tree.is_some() {
        can_restore &= current_tree == expected_tree || current_tree == tree;
    }
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

/// Restore a saved conversation and its isolated workspace. The provider intent
/// is saved in the workspace journal before the retained account broker is called.
pub fn restore(
    store: &RuntimeStore,
    agent_id: &str,
    checkpoint_id: &str,
    expected_tree: &str,
    preparation: Option<&crate::account_worker::preparation::PreparationResources>,
    account: &dyn AccountSessionPort,
) -> Result<Value, CheckpointsError> {
    if !is_full_object_id(expected_tree) {
        return Err(CheckpointsError::UnknownCheckpoint);
    }
    let operation_id = restore_operation_id(agent_id, checkpoint_id, expected_tree);
    let signature = restore_signature(agent_id, checkpoint_id, expected_tree);
    let initial = restore_context(store, agent_id, checkpoint_id)?;
    ensure_agent_available(&initial.agent)?;
    if initial.agent.record.get("worktreeReady") != Some(&Value::Bool(true)) {
        return Err(CheckpointsError::ExecutionWorkspace);
    }
    let operation_record = WorkspaceOperationsRepository::operation(&store.read()?, &operation_id)?;
    if operation_record.is_none() {
        ensure_workspace_idle(&initial.snapshot, None)?;
    }
    let cwd = agent_string(&initial.snapshot, "cwd")?;
    let checkpoint_tree = stored_tree(&initial.checkpoint)?;
    let current_tree = snapshot_tree(store, cwd)?;

    if let Some(operation) = operation_record.as_ref() {
        if operation.get("signature").and_then(Value::as_str) != Some(signature.as_str()) {
            return Err(CheckpointsError::ActivityChanged);
        }
        if operation.get("delivery").and_then(Value::as_str) == Some("rejected") {
            return Err(CheckpointsError::ProviderRejected);
        }
        if operation.get("phase").and_then(Value::as_str) == Some("completed") {
            if current_tree != checkpoint_tree {
                return Err(CheckpointsError::ActivityChanged);
            }
            return Ok(operation
                .get("result")
                .cloned()
                .unwrap_or_else(|| json!({"status":"restored","checkpoint":checkpoint_id})));
        }
    }

    let stored_request = operation_record
        .as_ref()
        .and_then(|op| op.get("nativeRequest"))
        .cloned()
        .map(serde_json::from_value::<NativeRequest>)
        .transpose()?;
    let stored_scope = stored_request.as_ref().map(|request| ConnectionScope {
        account: request.ticket.account.clone(),
        connection: request.ticket.connection.clone(),
        generation: request.ticket.generation,
    });
    if operation_record.is_some() && stored_request.is_none() {
        // Every provider handoff is preceded by an exact durable NativeRequest.
        // An older or damaged row cannot safely be reconstructed and resent.
        return Err(CheckpointsError::ProviderUnknown);
    }
    let scope = stored_scope
        .clone()
        .or_else(|| {
            account.current_scope(
                initial
                    .agent
                    .record
                    .get("accountKey")
                    .and_then(Value::as_str)
                    .unwrap_or("default"),
            )
        })
        .ok_or(CheckpointsError::ProviderUnavailable)?;
    let request = if let Some(request) = stored_request {
        request
    } else {
        let epoch = initial
            .agent
            .record
            .get("epoch")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid_agent(agent_id))?;
        let method = if initial
            .checkpoint
            .record
            .get("turnId")
            .and_then(Value::as_str)
            .is_some()
        {
            "thread/fork"
        } else {
            "thread/start"
        };
        let ticket = ProviderTicket {
            operation: operation_id.clone(),
            agent: agent_id.to_owned(),
            epoch,
            account: scope.account.clone(),
            thread: (method == "thread/fork")
                .then(|| {
                    initial
                        .checkpoint
                        .record
                        .get("threadId")
                        .and_then(Value::as_str)
                })
                .flatten()
                .map(str::to_owned),
            connection: scope.connection.clone(),
            generation: scope.generation,
            request: operation_id.clone(),
        };
        let resources = preparation.ok_or(CheckpointsError::ProviderUnavailable)?;
        let start_params = SharedRestoreStartParams.build(
            store,
            resources,
            agent_id,
            &operation_id,
            scope.generation,
        )?;
        let params = if method == "thread/fork" {
            restore_fork_params(
                &initial.agent.record,
                &initial.checkpoint,
                cwd,
                resources,
                &start_params,
            )?
        } else {
            start_params
        };
        NativeRequest {
            ticket,
            method: method.into(),
            params,
        }
    };
    let ticket = request.ticket.clone();

    let saved_provider = operation_record
        .as_ref()
        .and_then(|operation| operation.get("provider"))
        .filter(|_| {
            matches!(
                operation_record
                    .as_ref()
                    .and_then(|op| op.get("phase"))
                    .and_then(Value::as_str),
                Some("provider_ready" | "local_mutation" | "recovery_required")
            )
        })
        .cloned();
    let needs_dispatch = if saved_provider.is_some() {
        false
    } else if let Some(operation) = operation_record.as_ref() {
        if operation.get("phase").and_then(Value::as_str) == Some("recovery_required")
            || operation.get("providerTicket").is_some()
                && operation.get("delivery").and_then(Value::as_str) != Some("not_sent")
        {
            return Err(CheckpointsError::ProviderUnknown);
        }
        operation.get("delivery").and_then(Value::as_str) == Some("not_sent")
    } else {
        true
    };

    let retry_not_sent = operation_record.as_ref().is_some_and(|operation| {
        operation.get("phase").and_then(Value::as_str) == Some("failed")
            && operation.get("delivery").and_then(Value::as_str) == Some("not_sent")
    });
    if operation_record.is_none() || retry_not_sent {
        let ticket_value = serde_json::to_value(&ticket)?;
        let request_value = serde_json::to_value(&request)?;
        let reserved = store.transaction_with(|tx| {
            CheckpointsRepository::reserve_restore(
                tx,
                &initial.snapshot,
                RestoreReservation {
                    operation_id: &operation_id,
                    checkpoint_id,
                    expected_tree,
                    signature: &signature,
                    ticket: &ticket_value,
                    native_request: &request_value,
                    now: now_seconds(),
                },
            )
        })?;
        if !reserved {
            return Err(CheckpointsError::ActivityChanged);
        }
        if operation_record.is_none() && current_tree != expected_tree {
            store.transaction_with(|tx| {
                CheckpointsRepository::set_restore_phase(
                    tx,
                    &operation_id,
                    "failed",
                    now_seconds(),
                    &[("delivery", json!("not_sent"))],
                )?;
                CheckpointsRepository::fail_restore(
                    tx,
                    &operation_id,
                    "Files changed after the preview. Preview again",
                    now_seconds(),
                )
            })?;
            return Err(CheckpointsError::ActivityChanged);
        }
    }

    let provider = if let Some(provider) = saved_provider {
        provider
    } else {
        if needs_dispatch {
            let receipt = match dispatch_restore_provider(&request, account, &scope) {
                Ok(receipt) => receipt,
                Err(error @ CheckpointsError::ProviderUnknown) => {
                    store.transaction_with(|tx| {
                        CheckpointsRepository::set_restore_phase(
                            tx,
                            &operation_id,
                            "recovery_required",
                            now_seconds(),
                            &[("error", json!(error.to_string()))],
                        )
                    })?;
                    return Err(error);
                }
                Err(error @ CheckpointsError::ProviderUnavailable) => {
                    store.transaction_with(|tx| {
                        CheckpointsRepository::set_restore_phase(
                            tx,
                            &operation_id,
                            "failed",
                            now_seconds(),
                            &[("delivery", json!("not_sent"))],
                        )?;
                        CheckpointsRepository::fail_restore(
                            tx,
                            &operation_id,
                            &error.to_string(),
                            now_seconds(),
                        )
                    })?;
                    return Err(error);
                }
                Err(error) => {
                    store.transaction_with(|tx| {
                        CheckpointsRepository::fail_restore(
                            tx,
                            &operation_id,
                            &error.to_string(),
                            now_seconds(),
                        )
                    })?;
                    return Err(error);
                }
            };
            match receipt.certainty {
                RequestCertainty::Written => match receipt.result {
                    Some(ProviderResponse::Reply(response)) => {
                        let provider = match crate::workspace_operations::accept_provider_result(
                            &ticket, &ticket, &response,
                        ) {
                            Ok(provider) => provider,
                            Err(_) => {
                                store.transaction_with(|tx| {
                                    CheckpointsRepository::set_restore_phase(
                                        tx,
                                        &operation_id,
                                        "failed",
                                        now_seconds(),
                                        &[("delivery", json!("rejected"))],
                                    )?;
                                    CheckpointsRepository::fail_restore(
                                        tx,
                                        &operation_id,
                                        "Provider returned an invalid restore response",
                                        now_seconds(),
                                    )
                                })?;
                                return Err(CheckpointsError::ProviderRejected);
                            }
                        };
                        store.transaction_with(|tx| {
                            CheckpointsRepository::set_restore_phase(
                                tx,
                                &operation_id,
                                "provider_ready",
                                now_seconds(),
                                &[
                                    ("provider", provider.clone()),
                                    ("delivery", json!("written")),
                                ],
                            )
                        })?;
                        provider
                    }
                    Some(ProviderResponse::Rejected(_)) => {
                        store.transaction_with(|tx| {
                            CheckpointsRepository::set_restore_phase(
                                tx,
                                &operation_id,
                                "failed",
                                now_seconds(),
                                &[("delivery", json!("rejected"))],
                            )?;
                            CheckpointsRepository::fail_restore(
                                tx,
                                &operation_id,
                                "Provider rejected the restore request",
                                now_seconds(),
                            )
                        })?;
                        return Err(CheckpointsError::ProviderRejected);
                    }
                    None => {
                        store.transaction_with(|tx| {
                            CheckpointsRepository::set_restore_phase(
                                tx,
                                &operation_id,
                                "recovery_required",
                                now_seconds(),
                                &[("error", json!("Provider receipt has no result"))],
                            )
                        })?;
                        return Err(CheckpointsError::ProviderUnknown);
                    }
                },
                RequestCertainty::NotSent => {
                    store.transaction_with(|tx| {
                        CheckpointsRepository::set_restore_phase(
                            tx,
                            &operation_id,
                            "failed",
                            now_seconds(),
                            &[("delivery", json!("not_sent"))],
                        )?;
                        CheckpointsRepository::fail_restore(
                            tx,
                            &operation_id,
                            "Provider request was not sent",
                            now_seconds(),
                        )
                    })?;
                    return Err(CheckpointsError::ProviderRejected);
                }
                RequestCertainty::Unknown => {
                    store.transaction_with(|tx| {
                        CheckpointsRepository::set_restore_phase(
                            tx,
                            &operation_id,
                            "recovery_required",
                            now_seconds(),
                            &[("error", json!("Provider request outcome is unknown"))],
                        )
                    })?;
                    return Err(CheckpointsError::ProviderUnknown);
                }
            }
        } else {
            return Err(CheckpointsError::ProviderUnknown);
        }
    };

    restore_local(
        store,
        agent_id,
        checkpoint_id,
        expected_tree,
        &operation_id,
        &signature,
        &provider,
    )
}

fn restore_context(
    store: &RuntimeStore,
    agent_id: &str,
    checkpoint_id: &str,
) -> Result<RestoreContext, CheckpointsError> {
    let connection = store.read()?;
    if !CheckpointsRepository::tables_available(&connection)? {
        return Err(CheckpointsError::MissingSchema);
    }
    let snapshot = CheckpointsRepository::workspace_snapshot(&connection, agent_id)?
        .ok_or(CheckpointsError::UnknownAgent)?;
    let agent = snapshot.agent.clone();
    let checkpoint = CheckpointsRepository::checkpoint(&connection, checkpoint_id)?
        .ok_or(CheckpointsError::UnknownCheckpoint)?;
    if checkpoint.record.get("agent").and_then(Value::as_str) != Some(agent_id) {
        return Err(CheckpointsError::WrongAgent);
    }
    Ok(RestoreContext {
        snapshot,
        agent,
        checkpoint,
    })
}

fn stored_tree(checkpoint: &StoredCheckpointObject) -> Result<&str, CheckpointsError> {
    let tree = checkpoint
        .record
        .get("tree")
        .and_then(Value::as_str)
        .filter(|value| is_full_object_id(value))
        .ok_or_else(|| invalid_checkpoint(&checkpoint.id))?;
    Ok(tree)
}

fn restore_operation_id(agent: &str, checkpoint: &str, expected_tree: &str) -> String {
    let digest = studio_runtime_store::sha256_hex(expected_tree.as_bytes());
    format!("restore:{agent}:{checkpoint}:{}", &digest[..32])
}

fn restore_signature(agent: &str, checkpoint: &str, expected_tree: &str) -> String {
    crate::event_write::request_signature(&json!({
        "agent": agent,
        "checkpoint": checkpoint,
        "expectedTree": expected_tree,
    }))
}

fn restore_fork_params(
    agent: &Value,
    checkpoint: &StoredCheckpointObject,
    cwd: &str,
    resources: &crate::account_worker::preparation::PreparationResources,
    start_params: &Value,
) -> Result<Value, CheckpointsError> {
    let thread = checkpoint
        .record
        .get("threadId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| invalid_checkpoint(&checkpoint.id))?;
    let mut params = json!({
        "threadId":thread,
        "cwd":cwd,
        "config":resources.thread_config,
    });
    if let Some(turn) = checkpoint.record.get("turnId").and_then(Value::as_str) {
        params["lastTurnId"] = json!(turn);
    }
    if let Some(model) = agent.get("model") {
        params["model"] = model.clone();
    }
    if let Some(policy) = start_params.get("approvalPolicy") {
        params["approvalPolicy"] = policy.clone();
    }
    if let Some(sandbox) = start_params.get("sandbox") {
        params["sandbox"] = sandbox.clone();
    }
    Ok(params)
}

fn dispatch_restore_provider(
    request: &NativeRequest,
    account: &dyn AccountSessionPort,
    scope: &ConnectionScope,
) -> Result<crate::account_session_runtime::SessionReceipt, CheckpointsError> {
    let deadline = std::time::Instant::now() + GIT_TIMEOUT;
    if request.method == "thread/fork" {
        return crate::thread_fork_runtime::dispatch_reserved_via_broker(
            request, account, deadline,
        )
        .map_err(map_fork_error);
    }
    let method = studio_runtime_providers::Method::from_wire(&request.method)
        .ok_or(CheckpointsError::ProviderRejected)?;
    account
        .provider_request(ProviderRequest {
            account: request.ticket.account.clone(),
            operation: request.ticket.request.clone(),
            actor: Some(RequestActor {
                agent: request.ticket.agent.clone(),
                epoch: request.ticket.epoch as i64,
            }),
            expected_scope: Some(scope.clone()),
            method,
            params: request.params.clone(),
            deadline,
        })
        .map_err(map_account_error)
}

fn map_fork_error(error: crate::native_actions::NativeError) -> CheckpointsError {
    match error {
        crate::native_actions::NativeError::Unavailable(_) => CheckpointsError::ProviderUnavailable,
        crate::native_actions::NativeError::Unknown => CheckpointsError::ProviderUnknown,
        _ => CheckpointsError::ProviderRejected,
    }
}

fn map_account_error(error: AccountSessionError) -> CheckpointsError {
    match error {
        AccountSessionError::Requires(_) => CheckpointsError::ProviderUnavailable,
        AccountSessionError::Unknown => CheckpointsError::ProviderUnknown,
        AccountSessionError::Refused(_) | AccountSessionError::InvalidResponse(_) => {
            CheckpointsError::ProviderRejected
        }
        AccountSessionError::Persistence => CheckpointsError::ProviderUnknown,
    }
}

fn restore_local(
    store: &RuntimeStore,
    agent_id: &str,
    checkpoint_id: &str,
    expected_tree: &str,
    operation_id: &str,
    signature: &str,
    provider: &Value,
) -> Result<Value, CheckpointsError> {
    let connection = store.read()?;
    let agent = CheckpointsRepository::agent(&connection, agent_id)?
        .ok_or(CheckpointsError::UnknownAgent)?;
    let operation = WorkspaceOperationsRepository::operation(&connection, operation_id)?
        .ok_or(CheckpointsError::RecoveryRequired)?;
    if operation.get("signature").and_then(Value::as_str) != Some(signature)
        || operation.get("source") != Some(&CheckpointsRepository::workspace_source(&agent.record))
        || operation.get("provider") != Some(provider)
    {
        return Err(CheckpointsError::ActivityChanged);
    }
    let checkpoint = CheckpointsRepository::checkpoint(&connection, checkpoint_id)?
        .ok_or(CheckpointsError::UnknownCheckpoint)?;
    let tree = stored_tree(&checkpoint)?.to_owned();
    let cwd = agent
        .record
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_agent(agent_id))?
        .to_owned();
    let current_tree = snapshot_tree(store, &cwd)?;
    if current_tree != expected_tree && current_tree != tree {
        return Err(CheckpointsError::RecoveryRequired);
    }
    let needs_before_checkpoint = current_tree == expected_tree
        && !operation
            .get("beforeCheckpoint")
            .is_some_and(|value| !value.is_null());
    let backup_id = begin_restore_local_mutation(
        store,
        agent_id,
        operation_id,
        signature,
        provider,
        needs_before_checkpoint,
    )?;
    if current_tree == expected_tree {
        if let Some(backup_id) = backup_id {
            write_before_restore_checkpoint(store, agent_id, operation_id, &cwd, &backup_id)?;
        }
        git(&cwd, &["read-tree", expected_tree], &[], None, 0)?;
        git(&cwd, &["read-tree", "--reset", "-u", &tree], &[], None, 0)?;
    }
    if snapshot_tree(store, &cwd)? != tree {
        return Err(CheckpointsError::RecoveryRequired);
    }
    complete_restore(
        store,
        agent_id,
        checkpoint_id,
        operation_id,
        signature,
        provider,
    )
}

fn begin_restore_local_mutation(
    store: &RuntimeStore,
    agent_id: &str,
    operation_id: &str,
    signature: &str,
    provider: &Value,
    needs_before_checkpoint: bool,
) -> Result<Option<String>, CheckpointsError> {
    let candidate = needs_before_checkpoint.then(new_uuid).transpose()?;
    store.transaction_with(|tx| {
        let agent =
            CheckpointsRepository::agent(tx, agent_id)?.ok_or(CheckpointsError::UnknownAgent)?;
        let mut operation = WorkspaceOperationsRepository::operation(tx, operation_id)?
            .ok_or_else(|| {
                StoreError::SchemaMismatch(format!(
                    "Invalid stored object runtime_workspace_operations/{operation_id}"
                ))
            })?;
        if operation.get("signature").and_then(Value::as_str) != Some(signature)
            || operation.get("source")
                != Some(&CheckpointsRepository::workspace_source(&agent.record))
            || operation.get("provider") != Some(provider)
        {
            return Err(CheckpointsError::ActivityChanged);
        }
        let backup_id = if needs_before_checkpoint
            && operation.get("beforeCheckpoint").is_none_or(Value::is_null)
        {
            Some(
                operation
                    .get("beforeCheckpointId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or(candidate)
                    .ok_or_else(|| {
                        StoreError::SchemaMismatch("Restore backup identity is missing".into())
                    })?,
            )
        } else {
            None
        };
        operation["phase"] = json!("local_mutation");
        operation["updated"] = json!(now_seconds());
        if let Some(id) = backup_id.as_ref() {
            operation["beforeCheckpointId"] = json!(id);
        }
        WorkspaceOperationsRepository::put_operation(tx, &operation)?;
        Ok(backup_id)
    })
}

fn write_before_restore_checkpoint(
    store: &RuntimeStore,
    agent_id: &str,
    operation_id: &str,
    cwd: &str,
    checkpoint_id: &str,
) -> Result<(), CheckpointsError> {
    let snapshot = capture_context(store, agent_id)?;
    let tree = snapshot_tree(store, cwd)?;
    let reference = format!("refs/codex-agents/checkpoints/{agent_id}/{checkpoint_id}");
    let existing_commit = git(
        cwd,
        &["for-each-ref", "--format=%(objectname)", "--", &reference],
        &[],
        None,
        128,
    )?;
    let existing_commit = String::from_utf8_lossy(&existing_commit)
        .lines()
        .next()
        .filter(|commit| is_full_object_id(commit))
        .map(str::to_owned);
    let existing_matches_tree = if existing_commit.is_some() {
        let treeish = format!("{reference}^{{tree}}");
        let existing_tree = git(cwd, &["rev-parse", "--verify", &treeish], &[], None, 128)?;
        String::from_utf8_lossy(&existing_tree).trim() == tree
    } else {
        false
    };
    let commit = if existing_matches_tree {
        existing_commit.ok_or(CheckpointsError::GitFailed)?
    } else {
        let commit_bytes = git(
            cwd,
            &["commit-tree", &tree, "-p", "HEAD"],
            &[
                ("GIT_AUTHOR_NAME", "Codex Agents"),
                ("GIT_AUTHOR_EMAIL", "local@codex-agents.invalid"),
                ("GIT_COMMITTER_NAME", "Codex Agents"),
                ("GIT_COMMITTER_EMAIL", "local@codex-agents.invalid"),
            ],
            Some(b"Codex Agents workspace checkpoint\n"),
            128,
        )?;
        let commit = String::from_utf8_lossy(&commit_bytes).trim().to_owned();
        git(cwd, &["update-ref", &reference, &commit], &[], None, 0)?;
        commit
    };
    store.transaction_with(|tx| {
        let current = CheckpointsRepository::agent(tx, agent_id)?
            .ok_or_else(|| StoreError::SchemaMismatch(format!("Invalid stored object agents/{agent_id}")))?;
        let operation = WorkspaceOperationsRepository::operation(tx, operation_id)?
            .ok_or_else(|| StoreError::SchemaMismatch(format!("Invalid stored object runtime_workspace_operations/{operation_id}")))?;
        if operation.get("phase").and_then(Value::as_str) != Some("local_mutation")
            || operation.get("source") != Some(&CheckpointsRepository::workspace_source(&current.record))
        { return Err(StoreError::SchemaMismatch("Restore reservation changed".into())); }
        let parent_id = current.record.get("checkpointHistoryHead").and_then(Value::as_str).map(str::to_owned);
        let parent_boundary = snapshot.parent_checkpoint.as_ref()
            .and_then(|parent| parent.record.get("historyBoundary"))
            .and_then(Value::as_i64);
        let (history_delta, history_boundary) = CheckpointsRepository::history_delta(tx, agent_id, parent_boundary)?;
        let record = json!({
            "id":checkpoint_id,"agent":agent_id,"rootId":current.record.get("rootId"),
            "label":"Before restore","tree":tree,"commit":commit,"ref":reference,
            "historyParent":parent_id,"historyDelta":history_delta,"historyBoundary":history_boundary,
            "threadId":current.record.get("threadId"),"turnId":current.record.get("lastCompletedTurn"),
            "created":now_seconds(),"cwd":cwd,
        });
        tx.execute("INSERT INTO runtime_checkpoints(id,record) VALUES(?1,?2)", rusqlite::params![checkpoint_id, record.to_string()])?;
        let mut current_record = current.record;
        current_record["checkpointHistoryHead"] = json!(checkpoint_id);
        AgentRepository::put(tx, &AgentRecord::from_value(current_record)?)?;
        let mut operation = operation;
        operation["beforeCheckpoint"] = json!(checkpoint_id);
        operation
            .as_object_mut()
            .ok_or_else(|| StoreError::SchemaMismatch("Invalid restore operation".into()))?
            .remove("beforeCheckpointId");
        WorkspaceOperationsRepository::put_operation(tx, &operation)?;
        Ok::<_, StoreError>(())
    })?;
    Ok(())
}

fn complete_restore(
    store: &RuntimeStore,
    agent_id: &str,
    checkpoint_id: &str,
    operation_id: &str,
    signature: &str,
    provider: &Value,
) -> Result<Value, CheckpointsError> {
    store.transaction_with(|tx| {
        let mut agent = CheckpointsRepository::agent(tx, agent_id)?
            .ok_or(CheckpointsError::UnknownAgent)?;
        let mut operation = WorkspaceOperationsRepository::operation(tx, operation_id)?
            .ok_or(CheckpointsError::RecoveryRequired)?;
        let checkpoint = CheckpointsRepository::checkpoint(tx, checkpoint_id)?
            .ok_or(CheckpointsError::UnknownCheckpoint)?;
        if operation.get("signature").and_then(Value::as_str) != Some(signature)
            || operation.get("source") != Some(&CheckpointsRepository::workspace_source(&agent.record))
            || operation.get("provider") != Some(provider)
            || operation.get("phase").and_then(Value::as_str) != Some("local_mutation")
            || agent.record.get("workspaceReservationId").and_then(Value::as_str) != Some(operation_id)
            || !matches!(agent.record.get("workspaceOperation").and_then(Value::as_str), Some("restore" | "restore_recovery"))
        {
            return Err(CheckpointsError::ActivityChanged);
        }
        let history = WorkspaceOperationsRepository::history(tx, agent_id, checkpoint_id)?;
        let Some(selected) = history.checkpoints.first() else {
            return Err(CheckpointsError::UnknownCheckpoint);
        };
        let visible = CheckpointHistory::from_records(
            selected,
            &history.checkpoints[1..],
            history.items,
        )?.visible_ids()?;
        let mut items = TranscriptRepository::items_for_agent_including_restored(tx, agent_id)
            .map_err(|_| CheckpointsError::Store(StoreError::SchemaMismatch("Invalid stored object runtime_items".into())))?;
        let search_index_available: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_search_indexed')",
            [],
            |row| row.get(0),
        ).map_err(StoreError::from)?;
        for item in &mut items {
            let should_be_visible = visible.contains(&item.id);
            let was_visible = item.record.get("afterRestore").is_none();
            if should_be_visible == was_visible { continue; }
            if should_be_visible {
                if let Some(object) = item.record.as_object_mut() { object.remove("afterRestore"); }
                let indexed: bool = if search_index_available {
                    tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM runtime_search_indexed WHERE id=?1)",
                        [&item.id], |row| row.get(0),
                    ).map_err(StoreError::from)?
                } else {
                    false
                };
                if !indexed {
                    let text = TranscriptRepository::full_text(tx, item)
                        .map_err(|_| CheckpointsError::Store(StoreError::SchemaMismatch("Invalid stored object runtime_item_fulltext".into())))?
                        .unwrap_or_default();
                    TranscriptRepository::put_notification_item(
                        tx, &item.id, agent_id, &item.record,
                        item.record.get("created").and_then(Value::as_f64).unwrap_or_default(),
                        &text, true,
                    ).map_err(|_| CheckpointsError::Store(StoreError::SchemaMismatch("Invalid stored object runtime_items".into())))?;
                } else {
                    tx.execute("UPDATE runtime_items SET record=?1 WHERE id=?2", rusqlite::params![item.record.to_string(), item.id]).map_err(StoreError::from)?;
                }
            } else {
                item.record["afterRestore"] = json!(checkpoint_id);
                tx.execute("UPDATE runtime_items SET record=?1 WHERE id=?2", rusqlite::params![item.record.to_string(), item.id]).map_err(StoreError::from)?;
            }
        }
        tx.execute("UPDATE runtime_events SET status='cancelled' WHERE agent=?1 AND status='pending'", [agent_id]).map_err(StoreError::from)?;
        let result = json!({"status":"restored","checkpoint":checkpoint_id});
        agent.record["threadId"] = provider["thread"]["id"].clone();
        for (source, target) in [("sandbox", "sandbox"), ("approvalPolicy", "approvalPolicy"), ("activePermissionProfile", "profile")] {
            agent.record[target] = provider.get(source).cloned().unwrap_or(Value::Null);
        }
        agent.record["turnId"] = Value::Null;
        agent.record["inFlight"] = json!(false);
        agent.record["status"] = json!("paused");
        agent.record["autoWake"] = json!(false);
        agent.record["workspaceOperation"] = Value::Null;
        agent.record.as_object_mut().ok_or_else(|| invalid_agent(agent_id))?.remove("workspaceReservationId");
        agent.record["restoredCheckpoint"] = json!(checkpoint_id);
        agent.record["lastCompletedTurn"] = checkpoint.record.get("turnId").cloned().unwrap_or(Value::Null);
        agent.record["checkpointHistoryHead"] = json!(checkpoint_id);
        AgentRepository::put(tx, &AgentRecord::from_value(agent.record)?)?;
        operation["phase"] = json!("completed");
        operation["result"] = result.clone();
        operation["updated"] = json!(now_seconds());
        if let Some(object) = operation.as_object_mut() {
            object.remove("restartHold");
        }
        WorkspaceOperationsRepository::put_operation(tx, &operation)?;
        let item_id = format!("{agent_id}:restore:{checkpoint_id}");
        let label = checkpoint.record.get("label").and_then(Value::as_str).unwrap_or("checkpoint");
        let item = json!({"id":item_id,"role":"system","title":"Checkpoint","text":format!("Restored workspace and conversation to {label}")});
        TranscriptRepository::put_notification_item(tx, &item_id, agent_id, &item, now_seconds(), item["text"].as_str().unwrap_or_default(), false)
            .map_err(|_| CheckpointsError::Store(StoreError::SchemaMismatch("Invalid stored object runtime_items".into())))?;
        Ok::<_, CheckpointsError>(result)
    })
}

pub(crate) fn ensure_agent_available(
    agent: &StoredCheckpointObject,
) -> Result<(), CheckpointsError> {
    if agent.record.get("id").and_then(Value::as_str) != Some(agent.id.as_str()) {
        return Err(invalid_agent(&agent.id));
    }
    if truthy(agent.record.get("deletedAt")) {
        return Err(CheckpointsError::DeletedAgent);
    }
    if agent.record.get("imageWorkspace").and_then(Value::as_bool) == Some(true)
        || agent.record.get("executionMode").and_then(Value::as_str) == Some("vm")
        || matches!(
            agent.record.get("workspaceMode").and_then(Value::as_str),
            Some("image" | "layr")
        )
    {
        return Err(CheckpointsError::ExecutionWorkspace);
    }
    Ok(())
}

pub(crate) fn ensure_workspace_idle(
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
                    Some("capture" | "checkpoint")
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
    let path = record
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .ok_or(CheckpointsError::WorkspaceUnavailable)?;
    Path::new(path)
        .canonicalize()
        .map_err(|_| CheckpointsError::WorkspaceUnavailable)
}

pub(crate) fn snapshot_tree(store: &RuntimeStore, cwd: &str) -> Result<String, CheckpointsError> {
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
        ChildError::Other => CheckpointsError::GitFailed,
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
