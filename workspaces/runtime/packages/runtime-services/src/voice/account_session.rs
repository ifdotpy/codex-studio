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
        let session_id = session_id.to_owned();
        let account_key = account_key.to_owned();
        let thread_id = thread_id.to_owned();
        Box::pin(async move {
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/voice/account_session",
                move || {
                    let connection = store.read().map_err(|_| VoiceWriteCertainty::NotSent)?;
                    let agent = super::checked_agent(&connection, &agent_id)
                        .map_err(|_| VoiceWriteCertainty::NotSent)?;
                    if agent.epoch != epoch
                        || agent.extra.get("threadId").and_then(Value::as_str)
                            != Some(thread_id.as_str())
                        || agent
                            .extra
                            .get("accountKey")
                            .and_then(Value::as_str)
                            .unwrap_or("default")
                            != account_key
                    {
                        return Err(VoiceWriteCertainty::NotSent);
                    }
                    drop(connection);

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
                },
            )
            .await
            .unwrap_or(Err(VoiceWriteCertainty::Unknown))
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
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/voice/account_session",
                move || {
                    let expected_scope = ConnectionScope {
                        account: scope.account_key.clone(),
                        connection: scope.connection_id.clone(),
                        generation: scope.generation,
                    };
                    let deleted_actor_session = (method
                        == super::runtime::Method::ThreadRealtimeStop)
                        .then(|| operation_id.strip_prefix("voice:delete-stop:"))
                        .flatten()
                        .filter(|session_id| !session_id.is_empty())
                        .map(str::to_owned);
                    let receipt = match session.provider_request(ProviderRequest {
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
                    }) {
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
                                Some(ProviderResponse::Rejected(_)) => {
                                    VoiceWriteCertainty::Rejected
                                }
                                None => VoiceWriteCertainty::Unknown,
                            });
                    VoiceRequestReceipt { certainty }
                },
            )
            .await
            .unwrap_or(VoiceRequestReceipt {
                certainty: VoiceWriteCertainty::Unknown,
            })
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
        let callbacks = Arc::clone(&self.callbacks);
        Box::pin(async move {
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/voice/account_session",
                move || {
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
                    if request_id.is_empty()
                        || !callback_request_matches(&store, request_id, &scope, &reply.rpc_id)
                    {
                        return VoiceRequestReceipt {
                            certainty: VoiceWriteCertainty::NotSent,
                        };
                    }
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0.0, |duration| duration.as_secs_f64());
                    let certainty = match callbacks.answer(request_id, &reply.result, now) {
                        Ok(_) => VoiceWriteCertainty::Written,
                        Err(CallbackError::Unknown | CallbackError::Store(_)) => {
                            VoiceWriteCertainty::Unknown
                        }
                        Err(_) => VoiceWriteCertainty::NotSent,
                    };
                    VoiceRequestReceipt { certainty }
                },
            )
            .await
            .unwrap_or(VoiceRequestReceipt {
                certainty: VoiceWriteCertainty::Unknown,
            })
        })
    }
}

fn callback_request_matches(
    store: &RuntimeStore,
    id: &str,
    scope: &VoiceConnectionScope,
    rpc_id: &VoiceRpcId,
) -> bool {
    let Ok(connection) = store.read() else {
        return false;
    };
    let raw = connection.query_row(
        "SELECT record FROM runtime_requests WHERE id=?1",
        [id],
        |row| row.get::<_, String>(0),
    );
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
