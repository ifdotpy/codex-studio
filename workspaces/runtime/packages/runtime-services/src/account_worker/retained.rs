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
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TransferAuthorization {
    pub transfer_id: String,
    pub member: String,
    pub source_account: String,
    pub thread_id: String,
}
#[derive(Clone)]
pub struct SessionRequest {
    pub scope: ConnectionScope,
    pub expected_scope: Option<ConnectionScope>,
    pub operation: String,
    pub actor: Option<RequestActor>,
    pub transfer: Option<TransferAuthorization>,
    pub deleted_actor_session: Option<String>,
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
    #[error("retained-session/expected-scope-mismatch")]
    ScopeMismatch,
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
    let turn_view = request.params.get("itemsView").and_then(Value::as_str);
    let turn_limit = request.params.get("limit").and_then(Value::as_u64);
    let valid_turn_view = turn_view == Some("full")
        || (turn_view == Some("notLoaded")
            && request.actor.is_some()
            && matches!(turn_limit, Some(1 | 10)));
    if request
        .expected_scope
        .as_ref()
        .is_some_and(|expected| expected != &request.scope)
    {
        return Err(PortError::ScopeMismatch);
    }
    let actorless_read = request.transfer.is_none()
        && request.actor.is_none()
        && matches!(
            request.method,
            Method::ThreadList
                | Method::ThreadLoadedList
                | Method::ModelList
                | Method::ClaudeDiagnostics
                | Method::OperationStatus
                | Method::AccountRead
                | Method::ThreadRead
                | Method::ThreadTurnsList
        )
        && (request.method != Method::ThreadList || request.params.get("threadId").is_none())
        && (request.method != Method::AccountRead
            || request.params == json!({"refreshToken": false}));
    let deleted_voice_stop = request
        .deleted_actor_session
        .as_ref()
        .is_some_and(|session_id| {
            !session_id.is_empty()
                && request.transfer.is_none()
                && request.actor.is_some()
                && request.expected_scope.as_ref() == Some(&request.scope)
                && request.method == Method::ThreadRealtimeStop
                && request.operation == format!("voice:delete-stop:{session_id}")
                && request.params.as_object().is_some_and(|params| {
                    params.len() == 1
                        && request
                            .params
                            .get("threadId")
                            .and_then(Value::as_str)
                            .is_some_and(|id| !id.is_empty())
                })
        });
    let actor_method = request.transfer.is_none()
        && request.actor.is_some()
        && matches!(
            request.method,
            Method::ThreadRead
                | Method::ThreadTurnsList
                | Method::ThreadItemsList
                | Method::ThreadBackgroundTerminalsList
                | Method::ThreadStart
                | Method::TurnStart
                | Method::ThreadFork
                | Method::ThreadCompactStart
                | Method::ReviewStart
                | Method::ThreadSettingsUpdate
                | Method::ClaudeState
                | Method::ClaudeCommands
                | Method::ClaudeSettings
                | Method::ClaudeStopTask
                | Method::ThreadRollback
                | Method::ThreadQueueList
                | Method::TurnInterrupt
                | Method::ConfigRead
                | Method::CommandExec
                | Method::CommandExecWrite
                | Method::CommandExecResize
                | Method::CommandExecTerminate
        )
        || deleted_voice_stop;
    let transfer_method = request.transfer.as_ref().is_some_and(|transfer| {
        matches!(
            request.method,
            Method::ThreadQueueList
                | Method::ThreadBackgroundTerminalsList
                | Method::ThreadUnsubscribe
        ) && !transfer.transfer_id.is_empty()
            && !transfer.member.is_empty()
            && !transfer.source_account.is_empty()
            && !transfer.thread_id.is_empty()
            && request.scope.account == transfer.source_account
            && request
                .params
                .as_object()
                .is_some_and(|params| params.len() == 1 && params.contains_key("threadId"))
            && request.params.get("threadId").and_then(Value::as_str)
                == Some(transfer.thread_id.as_str())
            && request.operation.starts_with(&format!(
                "transfer:{}:{}:",
                transfer.transfer_id, transfer.member
            ))
    });
    if !(actorless_read || actor_method || transfer_method)
        || request.operation.is_empty()
        || request.operation.len() > 1024
        || request.params.to_string().len() > 2 * 1024 * 1024
        || !request.params.is_object()
        || (request.method == Method::ThreadRead
            && request
                .params
                .get("includeTurns")
                .is_some_and(|value| value != &json!(false)))
        || (request.method == Method::ThreadTurnsList
            && (request
                .params
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 20
                || request.params.get("sortDirection").and_then(Value::as_str) != Some("desc")
                // full is used by codex_runtime.py import_thread;
                // notLoaded limits 1 and 10 are used by codex_safety_buffering.py,
                // codex_context_repair.py, codex_connection_recovery.py, and
                // codex_turn_recovery.py respectively.
                || !valid_turn_view))
        || (request.method == Method::ThreadSettingsUpdate && !valid_settings_update(request))
        || (request.method == Method::OperationStatus
            && !request
                .params
                .get("operationId")
                .and_then(Value::as_str)
                .is_some_and(|id| id.starts_with("monitor:") && id.len() > "monitor:".len()))
        || (request.method == Method::ConfigRead
            && (request.params.get("includeLayers") != Some(&json!(false))
                || request.params.as_object().is_none_or(|params| {
                    params
                        .keys()
                        .any(|key| !matches!(key.as_str(), "cwd" | "includeLayers"))
                })))
        || (matches!(
            request.method,
            Method::CommandExec
                | Method::CommandExecWrite
                | Method::CommandExecResize
                | Method::CommandExecTerminate
        ) && request
            .params
            .get("processId")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty))
    {
        return Err(PortError::Invalid);
    }
    Ok(())
}

fn valid_settings_update(request: &SessionRequest) -> bool {
    let Some(params) = request.params.as_object() else {
        return false;
    };
    if !request.operation.ends_with(":settings")
        || request.operation.len() <= ":settings".len()
        || params.keys().any(|key| {
            !matches!(
                key.as_str(),
                "threadId"
                    | "model"
                    | "serviceTier"
                    | "approvalPolicy"
                    | "sandboxPolicy"
                    | "effort"
            )
        })
        || !["threadId", "model"].into_iter().all(|key| {
            params
                .get(key)
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
        })
        || !matches!(
            params.get("serviceTier").and_then(Value::as_str),
            Some("default" | "priority")
        )
        || params
            .get("approvalPolicy")
            .is_some_and(|value| !matches!(value.as_str(), Some("never" | "on-request")))
        || params
            .get("effort")
            .is_some_and(|value| !value.as_str().is_some_and(|effort| !effort.is_empty()))
    {
        return false;
    }
    params.get("sandboxPolicy").is_none_or(|sandbox| {
        let Some(policy) = sandbox.as_object() else {
            return false;
        };
        match policy.get("type").and_then(Value::as_str) {
            Some("readOnly" | "dangerFullAccess") => policy.len() == 1,
            Some("workspaceWrite") => {
                policy.len() == 3
                    && policy
                        .get("writableRoots")
                        .and_then(Value::as_array)
                        .is_some_and(|roots| {
                            roots
                                .iter()
                                .all(|root| root.as_str().is_some_and(|path| !path.is_empty()))
                        })
                    && policy.get("networkAccess").is_some_and(Value::is_boolean)
            }
            _ => false,
        }
    })
}
#[derive(Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub(super) struct Call {
    scope: ConnectionScope,
    operation: String,
    actor: Option<RequestActor>,
    transfer: Option<TransferAuthorization>,
    deleted_actor_session: Option<String>,
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

pub mod consumers;

#[cfg(test)]
mod validation_tests {
    use super::*;
    use serde_json::json;

    fn turns_request(items_view: &str, limit: u64, actor: bool) -> SessionRequest {
        SessionRequest {
            scope: ConnectionScope {
                account: "default".into(),
                connection: "fixture-connection".into(),
                generation: 1,
            },
            expected_scope: None,
            operation: "fixture-operation".into(),
            actor: actor.then(|| RequestActor {
                agent: "fixture-agent".into(),
                epoch: 1,
            }),
            transfer: None,
            deleted_actor_session: None,
            method: Method::ThreadTurnsList,
            params: json!({
                "threadId":"fixture-thread",
                "limit":limit,
                "sortDirection":"desc",
                "itemsView":items_view
            }),
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(1),
        }
    }

    #[test]
    fn only_documented_actor_reads_get_bounded_not_loaded_turns() {
        assert!(validate(&turns_request("notLoaded", 1, true)).is_ok());
        assert!(validate(&turns_request("notLoaded", 10, true)).is_ok());
        assert_eq!(
            validate(&turns_request("notLoaded", 2, true)),
            Err(PortError::Invalid)
        );
        assert_eq!(
            validate(&turns_request("notLoaded", 11, true)),
            Err(PortError::Invalid)
        );
        assert_eq!(
            validate(&turns_request("notLoaded", 1, false)),
            Err(PortError::Invalid)
        );
        assert!(validate(&turns_request("full", 20, false)).is_ok());
        assert_eq!(
            validate(&turns_request("full", 21, false)),
            Err(PortError::Invalid)
        );
    }

    #[test]
    fn thread_settings_update_requires_exact_native_action_shape() {
        let mut request = turns_request("full", 1, true);
        request.method = Method::ThreadSettingsUpdate;
        request.operation = "native-action:settings".into();
        request.params = json!({
            "threadId":"fixture-thread",
            "model":"fixture-model",
            "serviceTier":"default",
            "approvalPolicy":"never",
            "sandboxPolicy":{"type":"workspaceWrite","writableRoots":["/fixture"],"networkAccess":false},
            "effort":"medium"
        });
        assert!(validate(&request).is_ok());

        request.params["unexpected"] = json!(true);
        assert_eq!(validate(&request), Err(PortError::Invalid));
        request.params.as_object_mut().unwrap().remove("unexpected");
        request.operation = "native-action".into();
        assert_eq!(validate(&request), Err(PortError::Invalid));
    }

    #[test]
    fn monitor_requests_require_actor_and_narrow_config_shape() {
        let mut request = turns_request("full", 1, true);
        request.method = Method::ConfigRead;
        request.params = json!({"cwd":"/fixture/workspace", "includeLayers":false});
        assert!(validate(&request).is_ok());
        request.params["includeLayers"] = json!(true);
        assert_eq!(validate(&request), Err(PortError::Invalid));
        request.params = json!({"cwd":"/fixture/workspace", "includeLayers":false, "other":true});
        assert_eq!(validate(&request), Err(PortError::Invalid));

        request.method = Method::CommandExec;
        request.params = json!({"processId":"monitor-fixture"});
        request.actor = None;
        assert_eq!(validate(&request), Err(PortError::Invalid));
        request.actor = Some(RequestActor {
            agent: "fixture-agent".into(),
            epoch: 1,
        });
        assert!(validate(&request).is_ok());
        request.params = json!({});
        assert_eq!(validate(&request), Err(PortError::Invalid));
    }

    #[test]
    fn transfer_source_requests_are_thread_and_method_scoped() {
        let mut request = turns_request("full", 1, false);
        request.scope.account = "source-account".into();
        request.expected_scope = Some(request.scope.clone());
        request.operation = "transfer:transfer-a:agent-a:queue-check".into();
        request.method = Method::ThreadQueueList;
        request.params = json!({"threadId":"source-thread"});
        request.transfer = Some(TransferAuthorization {
            transfer_id: "transfer-a".into(),
            member: "agent-a".into(),
            source_account: "source-account".into(),
            thread_id: "source-thread".into(),
        });
        assert!(validate(&request).is_ok());

        request.params["threadId"] = json!("another-thread");
        assert_eq!(validate(&request), Err(PortError::Invalid));
        request.params = json!({"threadId":"source-thread", "cursor":"page"});
        assert_eq!(validate(&request), Err(PortError::Invalid));
        request.params = json!({"threadId":"source-thread"});
        request.operation = "provider:agent-a:queue-check".into();
        assert_eq!(validate(&request), Err(PortError::Invalid));

        request.operation = "transfer:transfer-a:agent-a:unsubscribe".into();
        request.method = Method::ThreadUnsubscribe;
        assert!(validate(&request).is_ok());
        request.method = Method::ThreadFork;
        assert_eq!(validate(&request), Err(PortError::Invalid));
    }
}
