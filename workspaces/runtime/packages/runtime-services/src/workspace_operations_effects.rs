//! Synchronous caller for turn completion; Git effects run only between transactions.
use crate::checkpoints::{self, CheckpointsError};
use crate::workspace_operations::{
    CheckpointHistory, CompletedTurn, CompletionSnapshot, OperationSnapshot,
    WorkspaceOperationError,
};
use serde_json::Value;
use std::collections::BTreeSet;
use studio_runtime_store::workspace_operations::WorkspaceOperationsRepository as Repository;
use studio_runtime_store::{CaptureKind, CheckpointsRepository, RuntimeStore, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceEffectsError {
    #[error(transparent)]
    Decision(#[from] WorkspaceOperationError),
    #[error(transparent)]
    Checkpoint(#[from] CheckpointsError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Call after the completed-turn transaction commits, using its original agent record.
/// A stale callback is a no-op. Replaying a committed capture returns its original summary.
pub fn checkpoint_after_turn(
    store: &RuntimeStore,
    request: &CompletedTurn,
) -> Result<Option<Value>, WorkspaceEffectsError> {
    let checkpoint_id = request.checkpoint_id();
    let operation_id = format!("checkpoint:{checkpoint_id}");
    let snapshot = checkpoints::capture_context(store, request.agent_id())?;
    let committed = Repository::completed(&store.read()?, request.agent_id(), request.turn_id())?;
    if !CompletionSnapshot::from_record(&snapshot.agent.record, committed)?.eligible(request) {
        return Ok(None);
    }
    if let Some(checkpoint) = CheckpointsRepository::checkpoint(&store.read()?, &checkpoint_id)? {
        let mut summary = checkpoint.record;
        if let Some(object) = summary.as_object_mut() {
            for field in ["items", "historyDelta", "_payloadBlobs"] {
                object.remove(field);
            }
        }
        return Ok(Some(summary));
    }
    checkpoints::ensure_agent_available(&snapshot.agent)?;
    // Resolving paths can block and must precede the write transaction.
    let idle = checkpoints::ensure_workspace_idle(&snapshot, None);
    let reserved = store.transaction_with(|tx| {
        let Some(current) = CheckpointsRepository::agent(tx, request.agent_id())? else {
            return Ok(false);
        };
        let committed = Repository::completed(tx, request.agent_id(), request.turn_id())?;
        if !CompletionSnapshot::from_record(&current.record, committed)?.eligible(request) {
            return Ok(false);
        }
        if Repository::operation(tx, &operation_id)?.is_some() {
            return Ok(false);
        }
        if let Err(error) = &idle {
            Repository::checkpoint_error(
                tx,
                request.agent_id(),
                &format!("Checkpoint skipped: {error}"),
            )?;
            return Ok(false);
        }
        Ok::<_, WorkspaceEffectsError>(CheckpointsRepository::reserve_checkpoint(
            tx,
            &snapshot,
            &operation_id,
            CaptureKind::Turn,
            request.turn_id(),
            checkpoints::now_seconds(),
        )?)
    })?;
    if !reserved {
        return Ok(None);
    }
    // The shared capture adapter persists settlement even if the child fails.
    Ok(Some(checkpoints::finish_reserved(
        store,
        &snapshot,
        "After turn",
        &checkpoint_id,
        &operation_id,
        CaptureKind::Turn,
    )?))
}

/// The server calls this once after opening the store and before accepting requests.
pub fn startup_recovery(store: &RuntimeStore) -> Result<usize, WorkspaceEffectsError> {
    recover_operations(store, checkpoints::now_seconds())
}

/// Startup runs once, before turn admission. Unknown writes stay held; no provider is called.
pub fn recover_operations(store: &RuntimeStore, now: f64) -> Result<usize, WorkspaceEffectsError> {
    store.transaction_with(|tx| {
        let operations = Repository::active_operations(tx)?;
        let mut count = 0;
        for operation in operations {
            // Provisioning is owned by workspace_ready and has its own Git evidence protocol.
            if operation.get("kind").and_then(Value::as_str) == Some("provision") {
                continue;
            }
            let id = operation
                .get("agent")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    StoreError::SchemaMismatch("Invalid workspace operation agent".into())
                })?;
            let actor =
                CheckpointsRepository::agent(tx, id)?.ok_or(CheckpointsError::UnknownAgent)?;
            let writes = OperationSnapshot::from_records(&operation, &actor.record)?.recover(now);
            Repository::apply_recovery(tx, &writes.operation, &writes.agent)?;
            count += 1;
        }
        Ok(count)
    })
}

/// Restore's local adapter calls this before any history visibility write.
pub fn checkpoint_history(
    store: &RuntimeStore,
    agent: &str,
    checkpoint: &str,
) -> Result<BTreeSet<String>, WorkspaceEffectsError> {
    let db = store.read()?;
    let selected = CheckpointsRepository::checkpoint(&db, checkpoint)?
        .ok_or(CheckpointsError::UnknownCheckpoint)?;
    if selected.record.get("agent").and_then(Value::as_str) != Some(agent) {
        return Err(CheckpointsError::WrongAgent.into());
    }
    let mut history = Repository::history(&db, agent, checkpoint)?;
    history
        .checkpoints
        .retain(|v| v.get("id").and_then(Value::as_str) != Some(checkpoint));
    Ok(
        CheckpointHistory::from_records(&selected.record, &history.checkpoints, history.items)?
            .visible_ids()?,
    )
}

/// Inspect an exact restore/branch retry without dispatching or modifying its journal.
pub fn inspect_retry(
    store: &RuntimeStore,
    operation_id: &str,
    signature: &str,
) -> Result<crate::workspace_operations::RetryDecision, WorkspaceEffectsError> {
    let db = store.read()?;
    let operation = Repository::operation(&db, operation_id)?
        .ok_or(WorkspaceOperationError::RecoveryRequired)?;
    let agent = operation
        .get("agent")
        .and_then(Value::as_str)
        .ok_or(WorkspaceOperationError::SourceChanged)?;
    let actor = CheckpointsRepository::agent(&db, agent)?.ok_or(CheckpointsError::UnknownAgent)?;
    Ok(OperationSnapshot::from_records(&operation, &actor.record)?.retry(signature)?)
}

/// The account worker delivers a known response using the ticket stored before dispatch.
/// A missing, foreign or stale ticket cannot make the local restore/branch phase ready.
pub fn record_provider_response(
    store: &RuntimeStore,
    receipt: &crate::workspace_operations::ProviderTicket,
    response: &Value,
) -> Result<Value, WorkspaceEffectsError> {
    use crate::workspace_operations::{ProviderTicket, accept_provider_result};
    store.transaction_with(|tx| {
        let mut operation = Repository::operation(tx, &receipt.operation)?
            .ok_or(WorkspaceOperationError::ForeignReceipt)?;
        let expected: ProviderTicket = serde_json::from_value(
            operation
                .get("providerTicket")
                .cloned()
                .ok_or(WorkspaceOperationError::ForeignReceipt)?,
        )
        .map_err(|_| WorkspaceOperationError::ForeignReceipt)?;
        let actor = CheckpointsRepository::agent(tx, &expected.agent)?
            .ok_or(CheckpointsError::UnknownAgent)?;
        if operation.get("agent").and_then(Value::as_str) != Some(expected.agent.as_str())
            || actor.record.get("epoch").and_then(Value::as_u64) != Some(expected.epoch)
            || actor
                .record
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
                != expected.account
            || actor.record.get("threadId").and_then(Value::as_str) != expected.thread.as_deref()
        {
            return Err(WorkspaceOperationError::ForeignReceipt.into());
        }
        let signature = operation
            .get("signature")
            .and_then(Value::as_str)
            .ok_or(WorkspaceOperationError::RequestChanged)?;
        OperationSnapshot::from_records(&operation, &actor.record)?.retry(signature)?;
        let result = accept_provider_result(&expected, receipt, response)?;
        if operation.get("phase").and_then(Value::as_str) == Some("provider_ready")
            && operation.get("provider") == Some(&result)
        {
            return Ok(result);
        }
        if operation.get("phase").and_then(Value::as_str) != Some("provider_pending") {
            return Err(WorkspaceOperationError::RecoveryRequired.into());
        }
        operation["provider"] = result.clone();
        operation["phase"] = Value::String("provider_ready".into());
        operation["updated"] = serde_json::json!(checkpoints::now_seconds());
        Repository::put_operation(tx, &operation)?;
        Ok(result)
    })
}
