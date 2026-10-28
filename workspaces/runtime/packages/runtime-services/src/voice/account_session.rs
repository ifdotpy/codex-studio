//! Production voice adapter over the account's retained provider session.
//!
//! Provider writes use the account worker's durable request ledger. Approval
//! replies use the shared callback lane, which retains the original RPC ID and
//! commits its intent before handing the reply to the worker.

use std::{sync::Arc, time::Instant};

use serde_json::{Value, json};
use studio_runtime_providers::RequestTicket;
use studio_runtime_store::RuntimeStore;

use crate::{
    account_session_runtime::{
        AccountSessionError, AccountSessionPort, ConnectionScope, ProviderRequest,
        ProviderResponse, RequestCertainty, SessionReceipt,
    },
    account_worker::retained::RequestActor,
    callback_runtime::CallbackRuntime,
    provider_callbacks::CallbackError,
};

use super::runtime::{
    VoiceConnectionScope, VoiceFuture, VoicePrepareFailure, VoiceReply, VoiceRequestPort,
    VoiceRequestReceipt, VoiceRpcId, VoiceWriteCertainty,
};

const ACCOUNT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub struct AccountSessionVoicePort {
    store: Arc<RuntimeStore>,
    session: Arc<dyn AccountSessionPort>,
    callbacks: Arc<CallbackRuntime>,
}

impl AccountSessionVoicePort {
    pub fn new(
        store: Arc<RuntimeStore>,
        session: Arc<dyn AccountSessionPort>,
        callbacks: Arc<CallbackRuntime>,
    ) -> Self {
        Self {
            store,
            session,
            callbacks,
        }
    }
}

impl VoiceRequestPort for AccountSessionVoicePort {
    fn prepare<'a>(
        &'a self,
        agent_id: &'a str,
        session_id: &'a str,
        account_key: &'a str,
        thread_id: &'a str,
        epoch: i64,
    ) -> VoiceFuture<'a, Result<VoiceConnectionScope, VoicePrepareFailure>> {
        let store = Arc::clone(&self.store);
        let session = Arc::clone(&self.session);
        let agent_id = agent_id.to_owned();
        let agent_id_for_check = agent_id.clone();
        let session_id = session_id.to_owned();
        let account_key = account_key.to_owned();
        let account_key_for_check = account_key.clone();
        let thread_id = thread_id.to_owned();
        let thread_id_for_check = thread_id.clone();
        Box::pin(async move {
            let current = store
                .read_async(
                    "runtime-services/voice/account_session",
                    move |connection| {
                        let Ok(agent) = super::checked_agent(connection, &agent_id_for_check)
                        else {
                            return Ok(false);
                        };
                        Ok::<_, studio_runtime_store::StoreError>(
                            agent.epoch == epoch
                                && agent.thread_id.as_deref()
                                    == Some(thread_id_for_check.as_str())
                                && agent.account_key.as_str()
                                    == account_key_for_check,
                        )
                    },
                )
                .await
                .map_err(|_| VoicePrepareFailure::NotSent)?;
            if !current {
                return Err(VoicePrepareFailure::NotSent);
            }

            let operation = format!("voice:prepare:{session_id}");
            let receipt = session
                .provider_request(ProviderRequest {
                    account: account_key.clone(),
                    root_request_id: session_id.clone(),
                    admitted_request: None,
                    operation: operation.clone(),
                    actor: None,
                    expected_scope: None,
                    deleted_actor_session: None,
                    method: super::runtime::Method::AccountRead,
                    params: json!({"refreshToken": false}),
                    deadline: Instant::now() + ACCOUNT_READ_TIMEOUT,
                })
                .await
                .map_err(account_error)?;
            let scope = match validate_receipt(
                &receipt,
                &operation,
                super::runtime::Method::AccountRead,
                None,
            ) {
                Ok(scope) if scope.account == account_key => scope,
                Ok(_) => return Err(VoicePrepareFailure::Unknown),
                Err(certainty) => return Err(certainty),
            };
            match receipt.response() {
                Some(ProviderResponse::Reply(value))
                    if value.pointer("/account/type").and_then(Value::as_str)
                        == Some("chatgpt") =>
                {
                    Ok(VoiceConnectionScope {
                        agent_id,
                        epoch,
                        account_key,
                        thread_id,
                        connection_id: scope.connection,
                        generation: scope.generation,
                    })
                }
                Some(ProviderResponse::Rejected(_)) => Err(VoicePrepareFailure::Rejected),
                Some(ProviderResponse::Reply(_)) => Err(VoicePrepareFailure::Rejected),
                _ => Err(VoicePrepareFailure::Unknown),
            }
        })
    }

    fn request<'a>(
        &'a self,
        scope: VoiceConnectionScope,
        operation: super::runtime::VoiceOperation,
        method: super::runtime::Method,
        params: Value,
        deadline: Instant,
    ) -> VoiceFuture<'a, VoiceRequestReceipt> {
        let session = Arc::clone(&self.session);
        Box::pin(async move {
            let operation_id = operation.id;
            let expected_scope = ConnectionScope {
                account: scope.account_key.clone(),
                connection: scope.connection_id.clone(),
                generation: scope.generation,
            };
            let deleted_actor_session = (method == super::runtime::Method::ThreadRealtimeStop)
                .then(|| operation_id.strip_prefix("voice:delete-stop:"))
                .flatten()
                .filter(|session_id| !session_id.is_empty())
                .map(str::to_owned);
            let receipt = match session
                .provider_request(ProviderRequest {
                    account: scope.account_key,
                    root_request_id: operation.root_request_id,
                    admitted_request: operation.admitted_request,
                    operation: operation_id.clone(),
                    actor: Some(RequestActor {
                        agent: scope.agent_id,
                        epoch: scope.epoch,
                    }),
                    expected_scope: Some(expected_scope.clone()),
                    deleted_actor_session,
                    method,
                    params,
                    deadline,
                })
                .await
            {
                Ok(receipt) => receipt,
                Err(error) => {
                    return VoiceRequestReceipt {
                        certainty: account_error(error).into(),
                    };
                }
            };
            let certainty =
                validate_receipt(&receipt, &operation_id, method, Some(&expected_scope))
                    .err()
                    .map(VoiceWriteCertainty::from)
                    .unwrap_or(match receipt.response() {
                        Some(ProviderResponse::Reply(_)) => VoiceWriteCertainty::Written,
                        Some(ProviderResponse::Rejected(_)) => VoiceWriteCertainty::Rejected,
                        None => VoiceWriteCertainty::Unknown,
                    });
            VoiceRequestReceipt { certainty }
        })
    }

    fn reply<'a>(
        &'a self,
        scope: VoiceConnectionScope,
        operation_id: String,
        reply: VoiceReply,
        deadline: Instant,
    ) -> VoiceFuture<'a, VoiceRequestReceipt> {
        let store = Arc::clone(&self.store);
        let session = Arc::clone(&self.session);
        let callbacks = Arc::clone(&self.callbacks);
        Box::pin(async move {
            if deadline <= Instant::now() {
                return VoiceRequestReceipt {
                    certainty: VoiceWriteCertainty::NotSent,
                };
            }
            let Some(request_id) = operation_id.strip_prefix("voice:approve:") else {
                return VoiceRequestReceipt {
                    certainty: VoiceWriteCertainty::NotSent,
                };
            };
            if request_id.is_empty() {
                return VoiceRequestReceipt {
                    certainty: VoiceWriteCertainty::NotSent,
                };
            }
            let preflight = async {
                callback_request_matches(&store, request_id, &scope, &reply.rpc_id).await?;
                let expected_session = ConnectionScope {
                    account: scope.account_key.clone(),
                    connection: scope.connection_id.clone(),
                    generation: scope.generation,
                };
                if session.current_scope(&scope.account_key).as_ref() != Some(&expected_session) {
                    return Err(VoiceWriteCertainty::NotSent);
                }
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0.0, |duration| duration.as_secs_f64());
                match crate::provider_callbacks::pending_native_delete_context(&store, request_id)
                    .await
                {
                    Ok(Some(context))
                        if context.agent == scope.agent_id
                            && context.epoch == scope.epoch as u64
                            && context.account == scope.account_key
                            && context.connection == scope.connection_id
                            && context.generation == scope.generation
                            && context.thread == scope.thread_id =>
                    {
                        Ok((now, context))
                    }
                    _ => Err(VoiceWriteCertainty::NotSent),
                }
            }
            .await;
            let (now, context) = match preflight {
                Ok(value) => value,
                Err(certainty) => return VoiceRequestReceipt { certainty },
            };
            let certainty = match callbacks
                .answer_on_live_session(request_id, &reply.result, now, &context)
                .await
            {
                Ok(_) => VoiceWriteCertainty::Written,
                Err(CallbackError::Expired) => VoiceWriteCertainty::Expired,
                Err(CallbackError::Unknown | CallbackError::Store(_)) => {
                    VoiceWriteCertainty::Unknown
                }
                Err(_) => VoiceWriteCertainty::NotSent,
            };
            VoiceRequestReceipt { certainty }
        })
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct VoiceCallbackAuthority {
    agent: String,
    account_key: String,
    connection_id: String,
    epoch: i64,
    supervisor: VoiceCallbackGeneration,
    params: VoiceCallbackThread,
    status: studio_runtime_schema::persistence::RequestCardStatus,
    rpc_id: VoiceRpcId,
}
#[derive(serde::Deserialize)]
struct VoiceCallbackGeneration {
    generation: u64,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct VoiceCallbackThread {
    thread_id: String,
}

async fn callback_request_matches(
    store: &RuntimeStore,
    id: &str,
    scope: &VoiceConnectionScope,
    rpc_id: &VoiceRpcId,
) -> Result<(), VoiceWriteCertainty> {
    use studio_runtime_schema::persistence::RequestCardStatus;
    let id = id.to_owned();
    let request: VoiceCallbackAuthority = store
        .read_async(
            "runtime-services/voice/account_session",
            move |connection| {
                let raw: String = connection.query_row(
                    "SELECT record FROM runtime_requests WHERE id=?1",
                    [id],
                    |row| row.get(0),
                )?;
                serde_json::from_str(&raw).map_err(studio_runtime_store::StoreError::from)
            },
        )
        .await
        .map_err(|_| VoiceWriteCertainty::NotSent)?;
    if request.agent != scope.agent_id
        || request.account_key != scope.account_key
        || request.connection_id != scope.connection_id
        || request.epoch != scope.epoch
        || request.supervisor.generation != scope.generation
        || request.params.thread_id != scope.thread_id
        || request.rpc_id != *rpc_id
    {
        return Err(VoiceWriteCertainty::NotSent);
    }
    if request.status == RequestCardStatus::Expired {
        Err(VoiceWriteCertainty::Expired)
    } else if request.status == RequestCardStatus::Pending {
        Ok(())
    } else {
        Err(VoiceWriteCertainty::NotSent)
    }
}

fn account_error(error: AccountSessionError) -> VoicePrepareFailure {
    match error {
        AccountSessionError::Expired => VoicePrepareFailure::Expired,
        AccountSessionError::Requires(_) | AccountSessionError::Refused(_) => {
            VoicePrepareFailure::NotSent
        }
        AccountSessionError::InvalidResponse(_)
        | AccountSessionError::Persistence
        | AccountSessionError::Unknown => VoicePrepareFailure::Unknown,
    }
}

fn validate_receipt(
    receipt: &SessionReceipt,
    operation: &str,
    method: super::runtime::Method,
    expected_scope: Option<&ConnectionScope>,
) -> Result<ConnectionScope, VoicePrepareFailure> {
    let mismatch = receipt.operation != operation
        || expected_scope.is_some_and(|expected| receipt.scope != *expected)
        || receipt
            .ticket()
            .is_some_and(|ticket| !ticket_matches(ticket, method, receipt.scope.generation));
    if mismatch {
        return Err(match receipt.certainty() {
            RequestCertainty::NotSent => VoicePrepareFailure::NotSent,
            RequestCertainty::Written | RequestCertainty::Unknown => VoicePrepareFailure::Unknown,
        });
    }
    match receipt.certainty() {
        RequestCertainty::Written => Ok(receipt.scope.clone()),
        RequestCertainty::NotSent => Err(VoicePrepareFailure::NotSent),
        RequestCertainty::Unknown => Err(VoicePrepareFailure::Unknown),
    }
}

fn ticket_matches(ticket: &RequestTicket, method: super::runtime::Method, generation: u64) -> bool {
    ticket.method == method && ticket.generation == generation
}
