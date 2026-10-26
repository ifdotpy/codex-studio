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

/// Maximum provider control error excerpt, in Unicode characters.
const MAX_CONTROL_ERROR_CHARACTERS: usize = 2048;

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
            let restoring_files = call.identity.method == Method::ThreadRollback
                && call.params["restoreFiles"] == true;
            let rewind_request = call.params.get("requestId").cloned();
            let rewind_turn = call.params.get("turnId").cloned();
            let timeout_seconds = if restoring_files {
                crate::claude_controls::FILE_RESTORE_TIMEOUT_SECONDS
            } else {
                20
            };
            let receipt = self
                .port
                .request(SessionRequest {
                    scope: self.port.scope().clone(),
                    expected_scope: None,
                    operation,
                    actor: Some(RequestActor {
                        agent: call.identity.scope.agent.clone(),
                        epoch: i64::try_from(call.identity.scope.epoch)
                            .map_err(|_| ControlError::Changed)?,
                    }),
                    transfer: None,
                    deleted_actor_session: None,
                    method: call.identity.method,
                    params: call.params,
                    deadline: (tokio::time::Instant::now()
                        + std::time::Duration::from_secs(timeout_seconds))
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
                RequestResult::Rejected(error) if restoring_files => {
                    let reason = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("The bridge rejected the file restore");
                    let reason = reason.chars().take(MAX_CONTROL_ERROR_CHARACTERS).collect();
                    let data = &error["data"];
                    if data["fileRestoreOutcome"] == "not_applied"
                        && rewind_request
                            .as_ref()
                            .is_some_and(|id| id.is_string() && data.get("requestId") == Some(id))
                        && rewind_turn
                            .as_ref()
                            .is_some_and(|id| id.is_string() && data.get("turnId") == Some(id))
                    {
                        Err(ControlError::RewindNotApplied(reason))
                    } else {
                        Err(ControlError::RewindRejected(reason))
                    }
                }
                RequestResult::Rejected(error)
                    if call.identity.method == studio_runtime_providers::Method::ClaudeSettings
                        && error
                            .pointer("/data/settingsOutcome")
                            .and_then(Value::as_str)
                            == Some("not_applied") =>
                {
                    Err(ControlError::SettingsNotApplied(
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("The permission change was not applied")
                            .chars()
                            .take(MAX_CONTROL_ERROR_CHARACTERS)
                            .collect(),
                    ))
                }
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
    recovery_id: String,
    read_step: u64,
}
impl HistoryPort {
    pub fn new(port: RetainedSessionPort, actor: RequestActor, recovery_id: String) -> Self {
        Self {
            port,
            actor,
            recovery_id,
            read_step: 0,
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
    fn request<'a>(
        &'a mut self,
        scope: &ConnectionScope,
        method: HistoryMethod,
        params: Value,
        deadline: Instant,
    ) -> futures_util::future::BoxFuture<'a, Result<HistoryReceipt, HistoryError>> {
        if scope != self.port.scope() {
            return Box::pin(async { Err(HistoryError::Identity) });
        }
        let operation = history_operation(&self.recovery_id, self.read_step, method, &params);
        self.read_step = self.read_step.saturating_add(1);
        let Some(method) = Method::from_wire(method.as_str()) else {
            return Box::pin(async { Err(HistoryError::Unsupported) });
        };
        let request = self.port.request(SessionRequest {
            scope: scope.clone(),
            expected_scope: None,
            actor: Some(self.actor.clone()),
            transfer: None,
            deleted_actor_session: None,
            operation,
            method,
            params,
            deadline,
        });
        Box::pin(async move {
            let receipt = request.await.map_err(history_error)?;
            match receipt.result {
                RequestResult::Reply(body) => Ok(HistoryReceipt {
                    scope: receipt.scope,
                    body,
                }),
                _ => Err(HistoryError::Unavailable),
            }
        })
    }
    fn barrier<'a>(
        &'a mut self,
        scope: &ConnectionScope,
        deadline: Instant,
    ) -> futures_util::future::BoxFuture<'a, Result<(), HistoryError>> {
        let scope = scope.clone();
        Box::pin(async move {
            self.port
                .barrier(&scope, deadline)
                .await
                .map_err(history_error)
        })
    }
}

fn history_operation(
    recovery_id: &str,
    step: u64,
    method: HistoryMethod,
    params: &Value,
) -> String {
    let query = crate::event_write::request_signature(&json!([method.as_str(), params]));
    format!("recovery-history:{recovery_id}:step:{step}:{query}")
}

#[cfg(test)]
mod history_identity_tests {
    use super::*;

    #[test]
    fn recovery_history_identity_is_stable_per_command_and_query() {
        let params = json!({"threadId":"thread-1","includeTurns":false});
        let first = history_operation("recovery-1", 0, HistoryMethod::Thread, &params);
        assert_eq!(
            first,
            history_operation("recovery-1", 0, HistoryMethod::Thread, &params)
        );
        assert_ne!(
            first,
            history_operation("recovery-1", 1, HistoryMethod::Thread, &params)
        );
        assert_ne!(
            first,
            history_operation("recovery-2", 0, HistoryMethod::Thread, &params)
        );
        assert_ne!(
            first,
            history_operation("recovery-1", 0, HistoryMethod::Turns, &params)
        );
        assert_ne!(
            first,
            history_operation(
                "recovery-1",
                0,
                HistoryMethod::Thread,
                &json!({"threadId":"thread-2","includeTurns":false})
            )
        );
    }
}
