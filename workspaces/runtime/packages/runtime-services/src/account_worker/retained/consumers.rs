//! Leaf adapters share the account mailbox. Synchronous recovery calls belong
//! only on the runtime's separate bounded blocking executor.
use super::*;
use crate::{
    claude_controls::{
        ControlError,
        service::{ControlBinding, ControlFuture},
    },
    claude_runtime::{ClaudeControlCall, ClaudeControlReceipt, ClaudeSessionPort},
    recovery_history::{HistoryError, HistoryMethod, HistoryReceipt},
};
use std::sync::Arc;

pub struct ClaudePort {
    port: RetainedSessionPort,
    binding: ControlBinding,
    closed: ControlBinding,
}
impl ClaudePort {
    pub fn new(port: RetainedSessionPort) -> Result<Self, ControlError> {
        let scope = port.scope();
        Ok(Self {
            binding: ControlBinding::new(
                scope.account.clone(),
                scope.connection.clone(),
                scope.generation,
            )?,
            closed: ControlBinding::new(
                scope.account.clone(),
                format!("closed:{}", scope.connection),
                scope.generation,
            )?,
            port,
        })
    }
}
impl ClaudeSessionPort for ClaudePort {
    fn binding(&self) -> ControlBinding {
        if self.port.is_live() {
            self.binding.clone()
        } else {
            self.closed.clone()
        }
    }
    fn request(&self, call: ClaudeControlCall) -> ControlFuture<'_, ClaudeControlReceipt> {
        Box::pin(async move {
            if call.identity.binding != self.binding() {
                return Err(ControlError::Changed);
            }
            if call.identity.signature
                != crate::event_write::request_signature(
                    &json!({"method":call.identity.method.as_str(),"params":call.params}),
                )
            {
                return Err(ControlError::Conflict);
            }
            let mut nonce = [0u8; 16];
            getrandom::fill(&mut nonce).map_err(|_| ControlError::Unavailable)?;
            let request = call
                .identity
                .scope
                .request_id
                .clone()
                .unwrap_or_else(|| studio_runtime_store::sha256_hex(&nonce));
            let operation = format!(
                "claude-control:{}",
                crate::event_write::request_signature(&json!([
                    call.identity.scope.agent,
                    call.identity.scope.epoch,
                    request
                ]))
            );
            let receipt = self
                .port
                .request(SessionRequest {
                    scope: self.port.scope().clone(),
                    operation,
                    actor: RequestActor {
                        agent: call.identity.scope.agent.clone(),
                        epoch: i64::try_from(call.identity.scope.epoch)
                            .map_err(|_| ControlError::Changed)?,
                    },
                    method: call.identity.method,
                    params: call.params,
                    deadline: (tokio::time::Instant::now() + std::time::Duration::from_secs(20))
                        .into_std(),
                })
                .await
                .map_err(|e| match e {
                    PortError::Identity => ControlError::Changed,
                    PortError::Invalid => ControlError::Request,
                    PortError::NotSent | PortError::Capacity => ControlError::NotSent,
                    _ => ControlError::Unknown,
                })?;
            if self.binding() != call.identity.binding {
                return Err(ControlError::Changed);
            }
            match receipt.result {
                RequestResult::Reply(result) => Ok(ClaudeControlReceipt {
                    identity: call.identity,
                    result,
                }),
                RequestResult::Rejected(_) => Err(ControlError::Request),
                RequestResult::NotSent => Err(ControlError::NotSent),
                _ => Err(ControlError::Unknown),
            }
        })
    }
}
impl AccountHandle {
    pub fn claude_session(
        &self,
    ) -> Result<crate::claude_runtime::RetainedClaudeSession, ControlError> {
        Ok(crate::claude_runtime::RetainedClaudeSession::new(Arc::new(
            ClaudePort::new(self.retained_session())?,
        )))
    }
}

pub struct HistoryPort {
    port: RetainedSessionPort,
    actor: RequestActor,
    runtime: tokio::runtime::Handle,
}
impl HistoryPort {
    pub fn new(
        port: RetainedSessionPort,
        actor: RequestActor,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            port,
            actor,
            runtime,
        }
    }
}
fn history_error(error: PortError) -> HistoryError {
    match error {
        PortError::Identity => HistoryError::Identity,
        PortError::Deadline => HistoryError::Deadline,
        PortError::Capacity => HistoryError::Capacity,
        _ => HistoryError::Unavailable,
    }
}
impl crate::recovery_runtime::RetainedHistory for HistoryPort {
    fn request(
        &mut self,
        scope: &ConnectionScope,
        method: HistoryMethod,
        params: Value,
        deadline: Instant,
    ) -> Result<HistoryReceipt, HistoryError> {
        if scope != self.port.scope() {
            return Err(HistoryError::Identity);
        }
        let operation =
            crate::event_write::request_signature(&json!([scope, method.as_str(), &params]));
        let receipt = self
            .runtime
            .block_on(self.port.request(SessionRequest {
                scope: scope.clone(),
                actor: self.actor.clone(),
                operation: format!("recovery-history:{operation}"),
                method: Method::from_wire(method.as_str()).ok_or(HistoryError::Unsupported)?,
                params,
                deadline,
            }))
            .map_err(history_error)?;
        match receipt.result {
            RequestResult::Reply(body) => Ok(HistoryReceipt {
                scope: receipt.scope,
                body,
            }),
            _ => Err(HistoryError::Unavailable),
        }
    }
    fn barrier(&mut self, scope: &ConnectionScope, deadline: Instant) -> Result<(), HistoryError> {
        self.runtime
            .block_on(self.port.barrier(scope, deadline))
            .map_err(history_error)
    }
}
