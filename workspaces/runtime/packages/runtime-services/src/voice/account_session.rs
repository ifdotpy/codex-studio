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
    VoiceConnectionScope, VoiceFuture, VoiceReply, VoiceRequestPort, VoiceRequestReceipt,
    VoiceRpcId, VoiceWriteCertainty,
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
    ) -> VoiceFuture<'a, Result<VoiceConnectionScope, VoiceWriteCertainty>> {
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
                                && agent.extra.get("threadId").and_then(Value::as_str)
                                    == Some(thread_id_for_check.as_str())
                                && agent
                                    .extra
                                    .get("accountKey")
                                    .and_then(Value::as_str)
                                    .unwrap_or("default")
                                    == account_key_for_check,
                        )
                    },
                )
                .await
                .map_err(|_| VoiceWriteCertainty::NotSent)?;
            if !current {
                return Err(VoiceWriteCertainty::NotSent);
            }

            let operation = format!("voice:prepare:{session_id}");
            let receipt = session
                .provider_request(ProviderRequest {
                    account: account_key.clone(),
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
                Ok(_) => return Err(VoiceWriteCertainty::Unknown),
                Err(certainty) => return Err(certainty),
            };
            match receipt.result {
                Some(ProviderResponse::Reply(value))
                    if receipt.certainty == RequestCertainty::Written
                        && value.pointer("/account/type").and_then(Value::as_str)
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
                Some(ProviderResponse::Rejected(_)) => Err(VoiceWriteCertainty::Rejected),
                Some(ProviderResponse::Reply(_))
                    if receipt.certainty == RequestCertainty::Written =>
                {
                    Err(VoiceWriteCertainty::Rejected)
                }
                _ => Err(VoiceWriteCertainty::Unknown),
            }
        })
    }

    fn request<'a>(
        &'a self,
        scope: VoiceConnectionScope,
        operation_id: String,
        method: super::runtime::Method,
        params: Value,
        deadline: Instant,
    ) -> VoiceFuture<'a, VoiceRequestReceipt> {
        let session = Arc::clone(&self.session);
        Box::pin(async move {
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
                        certainty: account_error(error),
                    };
                }
            };
            let certainty =
                validate_receipt(&receipt, &operation_id, method, Some(&expected_scope))
                    .err()
                    .unwrap_or(match receipt.result {
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
                if !callback_request_matches(&store, request_id, &scope, &reply.rpc_id).await {
                    return Err(VoiceWriteCertainty::NotSent);
                }
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
                Err(CallbackError::Unknown | CallbackError::Store(_)) => {
                    VoiceWriteCertainty::Unknown
                }
                Err(_) => VoiceWriteCertainty::NotSent,
            };
            VoiceRequestReceipt { certainty }
        })
    }
}

async fn callback_request_matches(
    store: &RuntimeStore,
    id: &str,
    scope: &VoiceConnectionScope,
    rpc_id: &VoiceRpcId,
) -> bool {
    let id = id.to_owned();
    let raw = store
        .read_async(
            "runtime-services/voice/account_session",
            move |connection| {
                connection
                    .query_row(
                        "SELECT record FROM runtime_requests WHERE id=?1",
                        [id],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(studio_runtime_store::StoreError::from)
            },
        )
        .await;
    let Ok(raw) = raw else {
        return false;
    };
    let Ok(request) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    request.get("agent").and_then(Value::as_str) == Some(scope.agent_id.as_str())
        && request.get("accountKey").and_then(Value::as_str) == Some(scope.account_key.as_str())
        && request.get("connectionId").and_then(Value::as_str) == Some(scope.connection_id.as_str())
        && request.get("epoch").and_then(Value::as_i64) == Some(scope.epoch)
        && request
            .pointer("/supervisor/generation")
            .and_then(Value::as_u64)
            == Some(scope.generation)
        && request.pointer("/params/threadId").and_then(Value::as_str)
            == Some(scope.thread_id.as_str())
        && request.get("status").and_then(Value::as_str) == Some("pending")
        && match (request.get("rpcId"), rpc_id) {
            (Some(Value::Number(actual)), VoiceRpcId::Number(expected)) => {
                actual.as_i64() == Some(*expected)
            }
            (Some(Value::String(actual)), VoiceRpcId::String(expected)) => actual == expected,
            _ => false,
        }
}

fn account_error(error: AccountSessionError) -> VoiceWriteCertainty {
    match error {
        AccountSessionError::Requires(_) | AccountSessionError::Refused(_) => {
            VoiceWriteCertainty::NotSent
        }
        AccountSessionError::InvalidResponse(_)
        | AccountSessionError::Persistence
        | AccountSessionError::Unknown => VoiceWriteCertainty::Unknown,
    }
}

fn map_certainty(certainty: RequestCertainty) -> VoiceWriteCertainty {
    match certainty {
        RequestCertainty::Written => VoiceWriteCertainty::Written,
        RequestCertainty::NotSent => VoiceWriteCertainty::NotSent,
        RequestCertainty::Unknown => VoiceWriteCertainty::Unknown,
    }
}

fn validate_receipt(
    receipt: &SessionReceipt,
    operation: &str,
    method: super::runtime::Method,
    expected_scope: Option<&ConnectionScope>,
) -> Result<ConnectionScope, VoiceWriteCertainty> {
    let mismatch = receipt.operation != operation
        || expected_scope.is_some_and(|expected| receipt.scope != *expected)
        || receipt
            .ticket
            .as_ref()
            .is_some_and(|ticket| !ticket_matches(ticket, method, receipt.scope.generation));
    if mismatch {
        return Err(match receipt.certainty {
            RequestCertainty::NotSent => VoiceWriteCertainty::NotSent,
            RequestCertainty::Written | RequestCertainty::Unknown => VoiceWriteCertainty::Unknown,
        });
    }
    if receipt.certainty == RequestCertainty::Written
        && !receipt
            .ticket
            .as_ref()
            .is_some_and(|ticket| ticket_matches(ticket, method, receipt.scope.generation))
    {
        return Err(VoiceWriteCertainty::Unknown);
    }
    if receipt.certainty != RequestCertainty::Written {
        return Err(map_certainty(receipt.certainty));
    }
    Ok(receipt.scope.clone())
}

fn ticket_matches(ticket: &RequestTicket, method: super::runtime::Method, generation: u64) -> bool {
    ticket.method == method && ticket.generation == generation
}
