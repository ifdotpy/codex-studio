//! Fork hand-off over the existing workspace journal. Local branch/restore
//! mutation is a separate consumer, permitted only after provider_ready commits.
use crate::native_actions::{
    Delivery, NativeError, NativeRequest, Result, RetainedSession, TransportReceipt, bounded,
    decode, send,
};
use crate::workspace_operations::{OperationSnapshot, ProviderTicket, accept_provider_result};
use rusqlite::Transaction;
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_store::workspace_operations::WorkspaceOperationsRepository as Repository;
use studio_runtime_store::{CheckpointsRepository, RuntimeStore};

/// The checkpoint owner has already reserved the workspace, frozen source and
/// prepared thread params outside the transaction. Recheck both before hand-off.
pub fn claim(
    tx: &Transaction<'_>,
    ticket: &ProviderTicket,
    params: Value,
) -> Result<NativeRequest> {
    bounded(&params, "fork.params")?;
    let mut operation =
        Repository::operation(tx, &ticket.operation)?.ok_or(NativeError::Changed)?;
    let actor = CheckpointsRepository::agent(tx, &ticket.agent)?.ok_or(NativeError::Changed)?;
    check(&operation, &actor.record, ticket)?;
    if operation.get("providerTicket").is_some() {
        return Err(NativeError::Unknown);
    }
    if operation["phase"] != "provider_pending"
        || !matches!(operation["kind"].as_str(), Some("branch" | "restore"))
    {
        return Err(NativeError::Changed);
    }
    if params.get("threadId").and_then(Value::as_str) != ticket.thread.as_deref()
        || params.get("dynamicTools").is_some()
    {
        return Err(NativeError::Invalid("fork.params.threadId"));
    }
    let request = NativeRequest {
        ticket: ticket.clone(),
        method: "thread/fork".into(),
        params,
    };
    operation["providerTicket"] = json!(ticket);
    operation["nativeRequest"] = json!(request);
    Repository::put_operation(tx, &operation)?;
    Ok(request)
}
fn check(operation: &Value, actor: &Value, ticket: &ProviderTicket) -> Result<()> {
    let signature = operation["signature"]
        .as_str()
        .ok_or(NativeError::Invalid("fork.signature"))?;
    OperationSnapshot::from_records(operation, actor)
        .and_then(|s| s.retry(signature))
        .map_err(|_| NativeError::Changed)?;
    if operation["id"] != ticket.operation
        || operation["agent"] != ticket.agent
        || actor["epoch"].as_u64() != Some(ticket.epoch)
        || actor
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            != ticket.account
        || actor["threadId"].as_str() != ticket.thread.as_deref()
        || actor["workspaceReservationId"] != ticket.operation
        || ticket.connection.is_empty()
        || ticket.generation == 0
        || ticket.request.is_empty()
    {
        return Err(NativeError::Changed);
    }
    Ok(())
}
pub fn settle(tx: &Transaction<'_>, receipt: &TransportReceipt) -> Result<Value> {
    let ticket = &receipt.ticket;
    let mut operation =
        Repository::operation(tx, &ticket.operation)?.ok_or(NativeError::Changed)?;
    let actor = CheckpointsRepository::agent(tx, &ticket.agent)?.ok_or(NativeError::Changed)?;
    check(&operation, &actor.record, ticket)?;
    let expected: ProviderTicket =
        decode(operation["providerTicket"].clone(), "fork.providerTicket")?;
    if expected != *ticket {
        return Err(NativeError::Changed);
    }
    let Delivery::Written(response) = &receipt.delivery else {
        // NotSent also requires the workspace owner to release/reprepare its
        // reservation; this executor never turns a failure into a second fork.
        return Err(NativeError::Unknown);
    };
    bounded(response, "fork.response")?;
    let result =
        accept_provider_result(&expected, ticket, response).map_err(|_| NativeError::Changed)?;
    if operation["phase"] == "provider_ready" && operation["provider"] == result {
        return Ok(result);
    }
    if !matches!(
        operation["phase"].as_str(),
        Some("provider_pending" | "recovery_required")
    ) {
        return Err(NativeError::Unknown);
    }
    operation["provider"] = result.clone();
    operation["phase"] = json!("provider_ready");
    Repository::put_operation(tx, &operation)?;
    Ok(result)
}
pub async fn execute(
    store: Arc<RuntimeStore>,
    ticket: ProviderTicket,
    params: Value,
    session: &impl RetainedSession,
    deadline: tokio::time::Instant,
) -> Result<Value> {
    let claiming = store.clone();
    let request = tokio::task::spawn_blocking(move || {
        claiming.transaction_with(|tx| claim(tx, &ticket, params))
    })
    .await
    .map_err(|_| NativeError::Unknown)??;
    let receipt = send(session, &request, deadline).await?;
    tokio::task::spawn_blocking(move || store.transaction_with(|tx| settle(tx, &receipt)))
        .await
        .map_err(|_| NativeError::Unknown)?
}
