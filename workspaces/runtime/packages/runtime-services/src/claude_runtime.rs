//! Claude controls use the account owner's retained session. This adapter never
//! opens a provider or consumes its journal. The owner commits intent before I/O
//! and settles the receipt before returning it; an unresolved hand-off is Unknown.
use crate::claude_controls::{
    ControlError, Result,
    service::{ControlBinding, ControlFuture, ControlScope, ControlSession},
};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_providers::Method;

const MAX_CONTROL_BYTES: usize = 2 * 1024 * 1024;

/// Immutable identity accompanying both the request and its committed receipt.
/// The worker additionally binds its native RequestTicket to this identity.
#[derive(Clone, PartialEq, Eq)]
pub struct ClaudeControlIdentity {
    pub binding: ControlBinding,
    pub scope: ControlScope,
    pub method: Method,
    pub signature: String,
}
#[derive(Clone)]
pub struct ClaudeControlCall {
    pub identity: ClaudeControlIdentity,
    pub params: Value,
}
pub struct ClaudeControlReceipt {
    pub identity: ClaudeControlIdentity,
    pub result: Value,
}

/// The retained account owner supplies this port, with its existing bounded
/// command lane and receipt store. No auxiliary account actor or receipt ledger.
/// Mutating methods must commit intent before submitting, and never resubmit an
/// intent without positive NotSent evidence. Dropped replies remain Unknown.
pub trait ClaudeSessionPort: Send + Sync {
    fn binding(&self) -> ControlBinding;
    fn request(&self, call: ClaudeControlCall) -> ControlFuture<'_, ClaudeControlReceipt>;
}

pub struct RetainedClaudeSession {
    port: Arc<dyn ClaudeSessionPort>,
    binding: ControlBinding,
}
impl RetainedClaudeSession {
    pub fn new(port: Arc<dyn ClaudeSessionPort>) -> Self {
        Self {
            binding: port.binding(),
            port,
        }
    }
}
impl ControlSession for RetainedClaudeSession {
    fn binding(&self) -> ControlBinding {
        self.port.binding()
    }
    fn call(&self, _method: &str, _params: Value) -> ControlFuture<'_, Value> {
        Box::pin(async { Err(ControlError::Request) })
    }
    fn call_scoped(
        &self,
        scope: ControlScope,
        method: &str,
        params: Value,
    ) -> ControlFuture<'_, Value> {
        let method = Method::from_wire(method).filter(|method| {
            matches!(
                method,
                Method::ClaudeState
                    | Method::ClaudeCommands
                    | Method::ClaudeSettings
                    | Method::ClaudeStopTask
                    | Method::ThreadRollback
                    | Method::ThreadQueueList
            )
        });
        Box::pin(async move {
            let method = method.ok_or(ControlError::Request)?;
            if scope.agent.is_empty()
                || scope.agent.len() > 1024
                || scope
                    .request_id
                    .as_ref()
                    .is_some_and(|id| id.is_empty() || id.len() > 4096)
                || (matches!(
                    method,
                    Method::ClaudeSettings | Method::ClaudeStopTask | Method::ThreadRollback
                ) && scope.request_id.is_none())
            {
                return Err(ControlError::Request);
            }
            if !params.is_object()
                || serde_json::to_vec(&params)
                    .map_err(|_| ControlError::Request)?
                    .len()
                    > MAX_CONTROL_BYTES
            {
                return Err(ControlError::Request);
            }
            if self.port.binding() != self.binding {
                return Err(ControlError::Changed);
            }
            let identity = ClaudeControlIdentity {
                binding: self.binding.clone(),
                scope,
                method,
                signature: crate::event_write::request_signature(
                    &json!({"method":method.as_str(),"params":params}),
                ),
            };
            let receipt = self
                .port
                .request(ClaudeControlCall {
                    identity: identity.clone(),
                    params,
                })
                .await?;
            validate_receipt(&identity, &receipt, &self.port.binding())?;
            Ok(receipt.result)
        })
    }
}
fn validate_receipt(
    expected: &ClaudeControlIdentity,
    receipt: &ClaudeControlReceipt,
    current: &ControlBinding,
) -> Result<()> {
    if receipt.identity != *expected || current != &expected.binding {
        return Err(ControlError::Unknown);
    }
    if serde_json::to_vec(&receipt.result)
        .map_err(|_| ControlError::Unknown)?
        .len()
        > MAX_CONTROL_BYTES
    {
        return Err(ControlError::Unknown);
    }
    Ok(())
}
