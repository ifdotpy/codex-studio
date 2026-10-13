//! Scoped requests on the account's existing session. Intent and results share
//! the account receipt repository; callers never own or reopen a provider.
use super::*;
use crate::recovery_history::ConnectionScope;
use std::time::Instant;
use studio_runtime_providers::RequestTicket;
use tokio::sync::oneshot;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RequestActor {
    pub agent: String,
    pub epoch: i64,
}
#[derive(Clone)]
pub struct SessionRequest {
    pub scope: ConnectionScope,
    pub operation: String,
    pub actor: RequestActor,
    pub method: Method,
    pub params: Value,
    pub deadline: Instant,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum RequestResult {
    Reply(Value),
    Rejected(Value),
    NotSent,
    Unknown,
}
#[derive(Clone, Debug)]
pub struct SessionReceipt {
    pub scope: ConnectionScope,
    pub operation: String,
    pub ticket: RequestTicket,
    pub result: RequestResult,
}
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum PortError {
    #[error("retained-session/identity-changed")]
    Identity,
    #[error("retained-session/invalid-request")]
    Invalid,
    #[error("retained-session/not-sent")]
    NotSent,
    #[error("retained-session/unknown")]
    Unknown,
    #[error("retained-session/deadline")]
    Deadline,
    #[error("retained-session/capacity")]
    Capacity,
}
type Reply = oneshot::Sender<Result<SessionReceipt, PortError>>;
pub struct RequestCommand {
    request: SessionRequest,
    reply: Reply,
}
pub struct BarrierCommand {
    scope: ConnectionScope,
    deadline: Instant,
    reply: oneshot::Sender<Result<(), PortError>>,
}
#[derive(Clone)]
pub struct RetainedSessionPort {
    pub(super) scope: ConnectionScope,
    pub(super) commands: mpsc::Sender<AccountCommand>,
    pub(super) stop: Shutdown,
}
impl RetainedSessionPort {
    pub fn scope(&self) -> &ConnectionScope {
        &self.scope
    }
    pub fn is_live(&self) -> bool {
        !*self.stop.subscribe().borrow() && !self.commands.is_closed()
    }
    pub async fn request(&self, request: SessionRequest) -> Result<SessionReceipt, PortError> {
        if request.scope != self.scope || !self.is_live() {
            return Err(PortError::Identity);
        }
        validate(&request)?;
        let deadline = tokio::time::Instant::from_std(request.deadline);
        if deadline <= tokio::time::Instant::now() {
            return Err(PortError::NotSent);
        }
        let (reply, receipt) = oneshot::channel();
        tokio::time::timeout_at(
            deadline,
            self.commands
                .send(AccountCommand::RetainedRequest(Box::new(RequestCommand {
                    request,
                    reply,
                }))),
        )
        .await
        .map_err(|_| PortError::NotSent)?
        .map_err(|_| PortError::NotSent)?;
        tokio::time::timeout_at(deadline, receipt)
            .await
            .map_err(|_| PortError::Unknown)?
            .map_err(|_| PortError::Unknown)?
    }
    pub async fn barrier(
        &self,
        scope: &ConnectionScope,
        deadline: Instant,
    ) -> Result<(), PortError> {
        if scope != &self.scope || !self.is_live() {
            return Err(PortError::Identity);
        }
        let deadline_tokio = tokio::time::Instant::from_std(deadline);
        let (reply, receipt) = oneshot::channel();
        tokio::time::timeout_at(
            deadline_tokio,
            self.commands
                .send(AccountCommand::JournalBarrier(BarrierCommand {
                    scope: scope.clone(),
                    deadline,
                    reply,
                })),
        )
        .await
        .map_err(|_| PortError::Deadline)?
        .map_err(|_| PortError::Identity)?;
        tokio::time::timeout_at(deadline_tokio, receipt)
            .await
            .map_err(|_| PortError::Deadline)?
            .map_err(|_| PortError::Identity)?
    }
}
fn validate(request: &SessionRequest) -> Result<(), PortError> {
    if !matches!(
        request.method,
        Method::ThreadRead
            | Method::ThreadTurnsList
            | Method::ThreadItemsList
            | Method::ThreadBackgroundTerminalsList
            | Method::ClaudeState
            | Method::ClaudeCommands
            | Method::ClaudeSettings
            | Method::ClaudeStopTask
            | Method::ThreadRollback
            | Method::ThreadQueueList
    ) || request.operation.is_empty()
        || request.operation.len() > 1024
        || request.params.to_string().len() > 2 * 1024 * 1024
        || !request.params.is_object()
        || (request.method == Method::ThreadRead
            && request.params.get("includeTurns") != Some(&json!(false)))
    {
        return Err(PortError::Invalid);
    }
    Ok(())
}
#[derive(Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub(super) struct Call {
    scope: ConnectionScope,
    operation: String,
    actor: RequestActor,
    method: String,
    params: Value,
}
impl Call {
    fn key(&self) -> String {
        format!(
            "retained:{}:{}",
            self.scope.account,
            studio_runtime_store::sha256_hex(self.operation.as_bytes())
        )
    }
    fn receipt(&self, ticket: RequestTicket, result: RequestResult) -> SessionReceipt {
        SessionReceipt {
            scope: self.scope.clone(),
            operation: self.operation.clone(),
            ticket,
            result,
        }
    }
}
pub(super) struct PendingRequest {
    call: Call,
    ticket: RequestTicket,
    reply: Option<Reply>,
    result: Option<RequestResult>,
    result_cursor: u64,
}
pub(super) struct PendingBarrier {
    cursor: u64,
    command: BarrierCommand,
}

mod runtime;
mod storage;
pub(super) use storage::recover;
