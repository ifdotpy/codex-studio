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
        let saved: NativeRequest =
            decode(operation["nativeRequest"].clone(), "fork.nativeRequest")?;
        let retry = NativeRequest {
            ticket: ticket.clone(),
            method: "thread/fork".into(),
            params: params.clone(),
        };
        return if saved == retry {
            Err(NativeError::Unknown)
        } else {
            Err(NativeError::Conflict)
        };
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

/// Dispatch a fork whose durable intent is owned by another workflow (for
/// example context repair). The caller must persist the exact NativeRequest
/// before invoking this function; this function never claims or retries it.
/// A custom portable-history path is accepted only with turns excluded and goal
/// continuation deferred, and its source thread may differ from the managed
/// agent's current thread.
pub async fn dispatch_reserved(
    request: &NativeRequest,
    session: &impl RetainedSession,
    deadline: tokio::time::Instant,
) -> Result<TransportReceipt> {
    validate_request(request)?;
    send(session, request, deadline).await
}

/// Submit a reserved fork through HttpState's shared retained-account broker.
/// The caller persists the exact NativeRequest before calling this function;
/// the broker commits its provider intent and checks the connection scope.
/// Call from a blocking worker because AccountSessionPort is synchronous.
pub fn dispatch_reserved_via_broker(
    request: &NativeRequest,
    port: &dyn crate::account_session_runtime::AccountSessionPort,
    deadline: std::time::Instant,
) -> Result<crate::account_session_runtime::SessionReceipt> {
    use crate::account_session_runtime::{AccountSessionError, ConnectionScope, ProviderRequest};
    use crate::account_worker::retained::RequestActor;
    use studio_runtime_providers::Method;

    validate_request(request)?;
    let epoch = i64::try_from(request.ticket.epoch).map_err(|_| NativeError::Changed)?;
    let method = Method::from_wire(&request.method).ok_or(NativeError::Invalid("fork.method"))?;
    let scope = ConnectionScope {
        account: request.ticket.account.clone(),
        connection: request.ticket.connection.clone(),
        generation: request.ticket.generation,
    };
    let receipt = port
        .provider_request(ProviderRequest {
            account: request.ticket.account.clone(),
            operation: request.ticket.request.clone(),
            actor: Some(RequestActor {
                agent: request.ticket.agent.clone(),
                epoch,
            }),
            expected_scope: Some(scope.clone()),
            method,
            params: request.params.clone(),
            deadline,
        })
        .map_err(|error| match error {
            AccountSessionError::Requires(name) => NativeError::Unavailable(name),
            AccountSessionError::Refused(_) => NativeError::Changed,
            AccountSessionError::Unknown => NativeError::Unknown,
            _ => NativeError::Unknown,
        })?;
    if receipt.operation != request.ticket.request
        || receipt.scope != scope
        || receipt
            .ticket
            .as_ref()
            .is_some_and(|ticket| ticket.method != method || ticket.generation != scope.generation)
    {
        return Err(NativeError::Changed);
    }
    Ok(receipt)
}

fn validate_request(request: &NativeRequest) -> Result<()> {
    bounded(&request.params, "fork.params")?;
    if request.method != "thread/fork"
        || request.ticket.request.is_empty()
        || request.ticket.connection.is_empty()
        || request.ticket.generation == 0
        || request
            .params
            .get("threadId")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || request.params.get("dynamicTools").is_some()
    {
        return Err(NativeError::Invalid("fork.params"));
    }
    if let Some(path) = request.params.get("path") {
        let Some(path) = path.as_str() else {
            return Err(NativeError::Invalid("fork.params.path"));
        };
        if !std::path::Path::new(path).is_absolute()
            || request.params.get("excludeTurns") != Some(&json!(true))
            || request.params.get("deferGoalContinuation") != Some(&json!(true))
        {
            return Err(NativeError::Invalid("fork.params.portableHistory"));
        }
    } else if request.params.get("threadId").and_then(Value::as_str)
        != request.ticket.thread.as_deref()
    {
        return Err(NativeError::Invalid("fork.params.threadId"));
    }
    Ok(())
}
