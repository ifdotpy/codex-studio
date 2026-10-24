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
/// Operation-id prefix of a diagnostics probe: one unique, never retried, read-only
/// provider query whose result is not worth a durable record.
pub const DIAGNOSTICS_OPERATION_PREFIX: &str = "diagnostics:";

impl SessionRequest {
    /// Configuration reads have no side effects and can be repeated after a lost
    /// reply. Diagnostics probes are also read-only. Neither needs a durable
    /// receipt or a wait for an unrelated journal checkpoint.
    pub(super) fn is_volatile_read(&self) -> bool {
        self.method == Method::ConfigRead
            || (self.operation.starts_with(DIAGNOSTICS_OPERATION_PREFIX)
                && self.actor.is_none()
                && self.transfer.is_none()
                && matches!(
                    self.method,
                    Method::ClaudeDiagnostics | Method::ThreadLoadedList
                ))
    }
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
    /// True only when this receipt came from checkpoint-verified durable replay.
    pub replayed: bool,
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
impl RequestCommand {
    /// Answer a request the closing worker never sent.
    pub(super) fn not_sent(self) {
        let _ignored = self.reply.send(Err(PortError::NotSent));
    }
}
pub struct BarrierCommand {
    scope: ConnectionScope,
    deadline: Instant,
    reply: oneshot::Sender<Result<(), PortError>>,
}
pub struct VmNotificationCommand {
    pub(super) scope: ConnectionScope,
    pub(super) agent_id: String,
    pub(super) epoch: i64,
    pub(super) sequence: u64,
    pub(super) frame: studio_runtime_providers::Frame,
    pub(super) reply: oneshot::Sender<Result<(), PortError>>,
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
    pub async fn recover_connection(&self, agent_id: String) -> Result<Value, PortError> {
        if agent_id.is_empty() || !self.is_live() {
            return Err(PortError::Identity);
        }
        let (reply, receipt) = oneshot::channel();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
        tokio::time::timeout_at(
            deadline,
            self.commands
                .send(AccountCommand::ConnectionRecovery { agent_id, reply }),
        )
        .await
        .map_err(|_| PortError::NotSent)?
        .map_err(|_| PortError::NotSent)?;
        tokio::time::timeout_at(deadline, receipt)
            .await
            .map_err(|_| PortError::Unknown)?
            .map_err(|_| PortError::Unknown)?
    }
    pub async fn request(&self, request: SessionRequest) -> Result<SessionReceipt, PortError> {
        if request.scope.account != self.scope.account || !self.is_live() {
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

    /// Commit a guest provider notification through the actor's account worker
    /// before the guest journal cursor is acknowledged.
    pub async fn deliver_vm_notification(
        &self,
        account: &str,
        agent_id: &str,
        epoch: i64,
        sequence: u64,
        frame: studio_runtime_providers::Frame,
    ) -> Result<(), PortError> {
        let notification = match &frame {
            studio_runtime_providers::Frame::Notification(_) => true,
            studio_runtime_providers::Frame::UnknownMethod(frame) => {
                !frame.fields.contains_key("id")
            }
            _ => false,
        };
        if account != self.scope.account
            || agent_id.is_empty()
            || epoch < 0
            || sequence == 0
            || !notification
            || !self.is_live()
        {
            return Err(PortError::Identity);
        }
        let (reply, receipt) = oneshot::channel();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
        tokio::time::timeout_at(
            deadline,
            self.commands.send(AccountCommand::VmNotification(Box::new(
                VmNotificationCommand {
                    scope: self.scope.clone(),
                    agent_id: agent_id.to_owned(),
                    epoch,
                    sequence,
                    frame,
                    reply,
                },
            ))),
        )
        .await
        .map_err(|_| PortError::NotSent)?
        .map_err(|_| PortError::NotSent)?;
        tokio::time::timeout_at(deadline, receipt)
            .await
            .map_err(|_| PortError::Unknown)?
            .map_err(|_| PortError::Unknown)?
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
    let actorless_request = request.transfer.is_none()
        && request.actor.is_none()
        && valid_actorless_request_shape(request.method, &request.params, &request.operation);
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
    // These are the exact actor-bound calls issued by codex_native_voice.py:
    // _submit_start (lines 107-111), _stop_native (418), and speech (455).
    // Require the connection snapshot so a delayed voice operation cannot
    // cross a provider reconnect.
    let voice_request = request.transfer.is_none()
        && request.deleted_actor_session.is_none()
        && request.actor.is_some()
        && request.expected_scope.as_ref() == Some(&request.scope)
        && match request.method {
            Method::ThreadRealtimeStart => valid_voice_start(request),
            Method::ThreadRealtimeStop => valid_voice_stop(request, "voice:stop:"),
            Method::ThreadRealtimeAppendSpeech => valid_voice_speech(request),
            _ => false,
        };
    let actor_method = request.transfer.is_none()
        && request.actor.is_some()
        && matches!(
            request.method,
            Method::ThreadRead
                | Method::ThreadTurnsList
                | Method::ThreadItemsList
                | Method::ThreadBackgroundTerminalsList
                | Method::ThreadBackgroundTerminalsTerminate
                | Method::ThreadBackgroundTerminalsClean
                | Method::ThreadCommandExecutionBackground
                | Method::ThreadStart
                | Method::TurnStart
                | Method::ThreadFork
                | Method::ThreadCompactStart
                | Method::ReviewStart
                | Method::ThreadSettingsUpdate
                | Method::McpServerReconnect
                | Method::ClaudeState
                | Method::ClaudeRepairReport
                | Method::ClaudeCommands
                | Method::ClaudeFileBackups
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
        || voice_request
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
    if !(actorless_request || actor_method || transfer_method)
        || request.operation.is_empty()
        || request.operation.len() > 1024
        || request.params.to_string().len() > 2 * 1024 * 1024
        || !request.params.is_object()
        || (request.method == Method::McpServerReconnect
            && (request.expected_scope.as_ref() != Some(&request.scope)
                || !request.operation.starts_with("mcp-reconnect:")
                || request.params.as_object().is_none_or(|params| {
                    params.len() != 2
                        || !["threadId", "name"].iter().all(|key| {
                            params
                                .get(*key)
                                .and_then(Value::as_str)
                                .is_some_and(|v| !v.trim().is_empty())
                        })
                })))
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
        || (request.method == Method::ThreadBackgroundTerminalsList
            && !valid_background_terminals_list(&request.params))
        || (request.method == Method::ThreadBackgroundTerminalsTerminate
            && !valid_background_terminals_terminate(&request.params))
        || (request.method == Method::ThreadBackgroundTerminalsClean
            && !valid_background_terminals_clean(&request.params))
        || (request.method == Method::ThreadCommandExecutionBackground
            && !valid_command_background(&request.params))
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

fn valid_background_terminals_list(params: &Value) -> bool {
    let Some(params) = params.as_object() else {
        return false;
    };
    params
        .get("threadId")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
        && params.get("limit") == Some(&json!(100))
        && params
            .keys()
            .all(|key| matches!(key.as_str(), "threadId" | "limit" | "cursor"))
        && params.get("cursor").is_none_or(|cursor| {
            cursor.is_null() || cursor.as_str().is_some_and(|value| !value.is_empty())
        })
}

fn valid_background_terminals_terminate(params: &Value) -> bool {
    let Some(params) = params.as_object() else {
        return false;
    };
    params.len() == 2
        && params
            .get("threadId")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
        && params
            .get("processId")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
}

/// `thread/backgroundTerminals/clean` names exactly one thread.
fn valid_background_terminals_clean(params: &Value) -> bool {
    params.as_object().is_some_and(|params| {
        params.len() == 1
            && params
                .get("threadId")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty())
    })
}

/// `thread/commandExecution/background` names exactly one item of one turn.
fn valid_command_background(params: &Value) -> bool {
    let Some(params) = params.as_object() else {
        return false;
    };
    params.len() == 3
        && ["threadId", "turnId", "itemId"].iter().all(|key| {
            params
                .get(*key)
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty())
        })
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

pub(super) fn valid_actorless_request_shape(
    method: Method,
    params: &Value,
    operation: &str,
) -> bool {
    let Some(params) = params.as_object() else {
        return false;
    };
    let only = |required: &[&str], optional: &[&str]| {
        required.iter().all(|key| params.contains_key(*key))
            && params
                .keys()
                .all(|key| required.contains(&key.as_str()) || optional.contains(&key.as_str()))
    };
    match method {
        // Python codex_runtime.py:11661 always passes limit=50 and cursor (possibly null).
        Method::ThreadList => {
            only(&["limit", "cursor"], &[])
                && params.get("limit") == Some(&json!(50))
                && (params.get("cursor") == Some(&Value::Null)
                    || params
                        .get("cursor")
                        .and_then(Value::as_str)
                        .is_some_and(|cursor| !cursor.is_empty()))
        }
        // Python codex_native_sweep.py:_loaded uses limit=100 and optional cursor.
        Method::ThreadLoadedList => valid_page_read_shape(params, &["limit"], 100),
        // Python codex_catalog.py sends {limit,cursor?}; the history reader
        // codex_model_catalog_reader.py additionally sends includeHidden=true.
        Method::ModelList => {
            valid_page_read_shape(params, &["limit"], 100)
                || (valid_page_read_shape(params, &["limit", "includeHidden"], 100)
                    && params.get("includeHidden") == Some(&json!(true)))
        }
        // Python codex_diagnostics.py sends an empty object.
        Method::ClaudeDiagnostics => params.is_empty(),
        // Python codex_runtime.py:3904 requests only this stable monitor identity.
        Method::OperationStatus => {
            only(&["operationId"], &[])
                && params
                    .get("operationId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id.starts_with("monitor:") && id.len() > "monitor:".len())
        }
        // Python codex_runtime.py:11680-11684 imports only visible thread state.
        Method::ThreadRead => {
            only(&["threadId", "includeTurns"], &[])
                && params
                    .get("threadId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| !id.is_empty())
                && params.get("includeTurns") == Some(&json!(false))
        }
        Method::ThreadTurnsList => {
            only(&["threadId", "limit", "sortDirection", "itemsView"], &[])
                && params
                    .get("threadId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| !id.is_empty())
                && params.get("limit") == Some(&json!(20))
                && params.get("sortDirection") == Some(&json!("desc"))
                && params.get("itemsView") == Some(&json!("full"))
        }
        Method::AccountRead => {
            only(&["refreshToken"], &[]) && params.get("refreshToken") == Some(&json!(false))
        }
        Method::SkillsList => {
            only(&["cwds", "forceReload"], &[])
                && params
                    .get("cwds")
                    .and_then(Value::as_array)
                    .is_some_and(|cwds| {
                        cwds.len() == 1 && cwds[0].as_str().is_some_and(|cwd| !cwd.is_empty())
                    })
                && params.get("forceReload").is_some_and(Value::is_boolean)
        }
        Method::McpServerStatusList => {
            only(&["threadId", "limit"], &["cursor"])
                && params
                    .get("cursor")
                    .is_none_or(|v| v.as_str().is_some_and(|v| !v.is_empty()))
                && params.get("limit")
                    == Some(&json!(
                        studio_runtime_schema::shared_values::NATIVE_TERMINAL_ITEMS_PER_PAGE
                    ))
                && params.get("threadId").is_some_and(|thread_id| {
                    thread_id.is_null()
                        || thread_id
                            .as_str()
                            .is_some_and(|thread_id| !thread_id.is_empty())
                })
        }
        // The limits snapshot is a read issued by AccountSessionRuntime.
        Method::AccountRateLimitsRead => params.is_empty() && operation.starts_with("limits-read:"),
        // AccountSessionRuntime::login sends the device-code start once per
        // durable sign-in reservation; the operation is that request UUID.
        Method::AccountLoginStart => {
            only(&["type"], &[])
                && params.get("type") == Some(&json!("chatgptDeviceCode"))
                && uuid::Uuid::parse_str(operation).is_ok()
        }
        // AccountSessionRuntime::cancel_login cancels the provider login id
        // saved on that reservation; the operation is "<request UUID>:cancel".
        Method::AccountLoginCancel => {
            only(&["loginId"], &[])
                && params
                    .get("loginId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| !id.is_empty() && id.len() <= 8192)
                && operation
                    .strip_suffix(":cancel")
                    .is_some_and(|request| uuid::Uuid::parse_str(request).is_ok())
        }
        // Reset-credit consumption is the only actorless account write. Bind it
        // to the durable charge identity, distinct transport sequence, and exact
        // provider request shape.
        Method::AccountRateLimitResetCreditConsume => {
            let (idempotency_key, transport_attempt) = match operation.strip_prefix("limit-reset:")
            {
                Some(identity) => match identity.split_once(':') {
                    Some((key, attempt)) => (key, Some(attempt)),
                    None => (identity, None),
                },
                None => ("", None),
            };
            only(&["creditId", "idempotencyKey"], &[])
                && params
                    .get("creditId")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty() && value.len() <= 1024)
                && params
                    .get("idempotencyKey")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty() && value.len() <= 1024)
                && uuid::Uuid::parse_str(idempotency_key).is_ok()
                && transport_attempt
                    .is_none_or(|attempt| attempt.parse::<u64>().is_ok_and(|attempt| attempt > 0))
                && params.get("idempotencyKey").and_then(Value::as_str) == Some(idempotency_key)
        }
        _ => false,
    }
}

fn valid_page_read_shape(
    params: &serde_json::Map<String, Value>,
    required: &[&str],
    limit: u64,
) -> bool {
    required.iter().all(|key| params.contains_key(*key))
        && params
            .keys()
            .all(|key| required.contains(&key.as_str()) || key == "cursor")
        && params.get("limit") == Some(&json!(limit))
        && params
            .get("cursor")
            .is_none_or(|cursor| cursor.as_str().is_some_and(|value| !value.is_empty()))
}

fn valid_voice_start(request: &SessionRequest) -> bool {
    let Some(session_id) = request.operation.strip_prefix("voice:start:") else {
        return false;
    };
    if session_id.is_empty() || session_id.len() > 120 {
        return false;
    }
    let Some(params) = request.params.as_object() else {
        return false;
    };
    let Some(transport) = params.get("transport").and_then(Value::as_object) else {
        return false;
    };
    params.len() == 7
        && params
            .get("threadId")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
        && params.get("version") == Some(&json!("v3"))
        && params.get("outputModality") == Some(&json!("audio"))
        && params.get("realtimeSessionId").and_then(Value::as_str) == Some(session_id)
        && params.get("clientManagedHandoffs") == Some(&json!(false))
        && params.get("flushTranscriptTailOnSessionEnd") == Some(&json!(false))
        && transport.len() == 2
        && transport.get("type") == Some(&json!("webrtc"))
        && transport
            .get("sdp")
            .and_then(Value::as_str)
            .is_some_and(|sdp| sdp.starts_with("v=0") && sdp.len() <= 100_000)
}

fn valid_voice_stop(request: &SessionRequest, operation_prefix: &str) -> bool {
    let Some(session_id) = request.operation.strip_prefix(operation_prefix) else {
        return false;
    };
    !session_id.is_empty()
        && session_id.len() <= 120
        && request.params.as_object().is_some_and(|params| {
            params.len() == 1
                && params
                    .get("threadId")
                    .and_then(Value::as_str)
                    .is_some_and(|thread_id| !thread_id.is_empty())
        })
}

fn valid_voice_speech(request: &SessionRequest) -> bool {
    let Some(receipt_id) = request.operation.strip_prefix("voice:speech:") else {
        return false;
    };
    !receipt_id.is_empty()
        && receipt_id.len() <= 480
        && request.params.as_object().is_some_and(|params| {
            params.len() == 2
                && params
                    .get("threadId")
                    .and_then(Value::as_str)
                    .is_some_and(|thread_id| !thread_id.is_empty())
                && params.get("text").and_then(Value::as_str).is_some()
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
/// The retained `command/exec` call of monitor `id`, checked against its
/// operation identity. `None` when no request was submitted for it.
fn monitor_call(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    operation: &crate::monitor_policy::Operation,
) -> Result<Option<Call>, WorkerError> {
    #[derive(serde::Deserialize)]
    struct Submitted {
        call: Call,
    }
    let identity = format!("monitor:{id}");
    let key = Call::operation_key(&operation.account_key, &identity);
    let Some(value) = super::store::receipt(tx, &key, "submitted")? else {
        return Ok(None);
    };
    let submitted: Submitted = serde_json::from_value(value).map_err(|_| WorkerError::Capacity)?;
    let call = submitted.call;
    if call.operation != identity
        || call.method != Method::CommandExec.as_str()
        || call
            .actor
            .as_ref()
            .is_none_or(|actor| actor.agent != operation.agent || actor.epoch != operation.epoch)
    {
        return Err(WorkerError::PreparationRefused(
            "monitor retained receipt identity changed".into(),
        ));
    }
    Ok(Some(call))
}
/// Read a monitor's checkpoint-verified retained terminal receipt. This never
/// creates a provider request; recovered pending requests publish into this ledger.
pub async fn monitor_result(
    store: &RuntimeStore,
    id: &str,
    operation: &crate::monitor_policy::Operation,
) -> Result<Option<crate::account_session_runtime::ProviderResponse>, WorkerError> {
    let id = id.to_owned();
    let operation = operation.clone();
    store
        .read_async("runtime-services/retained_monitor_result", move |tx| {
            let Some(call) = monitor_call(tx, &id, &operation)? else {
                return Ok(None);
            };
            let receipt = match storage::replay_in_transaction(tx, &call)? {
                Ok(storage::Replay::Return(receipt)) => receipt,
                Ok(storage::Replay::Miss | storage::Replay::Retry(_)) => return Ok(None),
                Err(_) => {
                    return Err(WorkerError::PreparationRefused(
                        "monitor retained receipt authorization changed".into(),
                    ));
                }
            };
            Ok(match receipt.result {
                RequestResult::Reply(value) => Some(
                    crate::account_session_runtime::ProviderResponse::Reply(value),
                ),
                RequestResult::Rejected(value) => Some(
                    crate::account_session_runtime::ProviderResponse::Rejected(value),
                ),
                RequestResult::Unknown | RequestResult::NotSent => None,
            })
        })
        .await
}
/// The provider generation that received monitor `id`'s retained
/// `command/exec`. While that generation's provider process runs, its answer
/// reaches the retained ledger. `None` when no request was submitted for it,
/// or when the ledger proves that the request was not sent.
pub async fn monitor_request_generation(
    store: &RuntimeStore,
    id: &str,
    operation: &crate::monitor_policy::Operation,
) -> Result<Option<u64>, WorkerError> {
    let id = id.to_owned();
    let operation = operation.clone();
    store
        .read_async("runtime-services/retained_monitor_generation", move |tx| {
            let Some(call) = monitor_call(tx, &id, &operation)? else {
                return Ok(None);
            };
            if matches!(
                storage::replay_in_transaction(tx, &call)?,
                Ok(storage::Replay::Retry(_))
            ) {
                return Ok(None);
            }
            Ok(Some(call.scope.generation))
        })
        .await
}

impl Call {
    fn operation_key(account: &str, operation: &str) -> String {
        format!(
            "retained:{account}:{}",
            studio_runtime_store::sha256_hex(operation.as_bytes())
        )
    }
    fn key(&self) -> String {
        Self::operation_key(&self.scope.account, &self.operation)
    }
    fn receipt(&self, ticket: RequestTicket, result: RequestResult) -> SessionReceipt {
        SessionReceipt {
            scope: self.scope.clone(),
            operation: self.operation.clone(),
            ticket,
            result,
            replayed: false,
        }
    }
}
pub(super) struct PendingRequest {
    call: Call,
    ticket: RequestTicket,
    actor_id: Option<String>,
    reply: Option<Reply>,
    result: Option<RequestResult>,
    result_cursor: u64,
    /// No receipt exists (see `SessionRequest::is_volatile_read`): the reply is
    /// sent as soon as the result arrives and nothing is written.
    volatile: bool,
}
impl PendingRequest {
    /// The thread this request names, if any.
    pub(super) fn thread_id(&self) -> Option<&str> {
        self.call.params.get("threadId").and_then(Value::as_str)
    }
}
pub(super) struct PendingBarrier {
    cursor: u64,
    command: BarrierCommand,
}

mod runtime;
mod storage;
pub(super) use storage::recover;
pub use storage::vm_actor;
#[cfg(test)]
pub(super) async fn test_submitted(
    store: &RuntimeStore,
    call: &Call,
    request: &RecoveredRequest,
    ticket: Option<&RequestTicket>,
) -> Result<Result<(), PortError>, WorkerError> {
    storage::submitted(store, call, request, ticket).await
}

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
    fn mcp_reconnect_requires_actor_scope_and_exact_server_shape() {
        let mut request = turns_request("full", 1, true);
        request.method = Method::McpServerReconnect;
        request.operation = "mcp-reconnect:request".into();
        request.expected_scope = Some(request.scope.clone());
        request.params = json!({"threadId":"thread","name":"server"});
        assert!(validate(&request).is_ok());
        for params in [
            json!({"threadId":"thread","name":""}),
            json!({"threadId":"thread","name":"server","all":true}),
        ] {
            let mut invalid = request.clone();
            invalid.params = params;
            assert_eq!(validate(&invalid), Err(PortError::Invalid));
        }
        let mut invalid = request.clone();
        invalid.actor = None;
        assert_eq!(validate(&invalid), Err(PortError::Invalid));
        let mut invalid = request;
        invalid.expected_scope = None;
        assert_eq!(validate(&invalid), Err(PortError::Invalid));
        assert!(valid_actorless_request_shape(
            Method::McpServerStatusList,
            &json!({"threadId":"thread","limit":100,"cursor":"next"}),
            "catalog"
        ));
    }

    #[test]
    fn command_background_names_exactly_one_item_of_one_turn() {
        let request = |params: Value, actor: bool| SessionRequest {
            method: Method::ThreadCommandExecutionBackground,
            params,
            ..turns_request("full", 1, actor)
        };
        let exact = json!({"threadId":"thread","turnId":"turn","itemId":"item"});
        assert!(validate(&request(exact.clone(), true)).is_ok());
        assert_eq!(validate(&request(exact, false)), Err(PortError::Invalid));
        for params in [
            json!({"threadId":"thread","turnId":"turn"}),
            json!({"threadId":"thread","turnId":"turn","itemId":""}),
            json!({"threadId":"thread","turnId":"turn","itemId":"item","extra":1}),
        ] {
            assert_eq!(validate(&request(params, true)), Err(PortError::Invalid));
        }
    }

    #[test]
    fn background_terminal_clean_names_exactly_one_thread() {
        let request = |params: Value, actor: bool| SessionRequest {
            method: Method::ThreadBackgroundTerminalsClean,
            params,
            ..turns_request("full", 1, actor)
        };
        assert!(validate(&request(json!({"threadId":"thread"}), true)).is_ok());
        assert_eq!(
            validate(&request(json!({"threadId":"thread"}), false)),
            Err(PortError::Invalid)
        );
        for params in [
            json!({}),
            json!({"threadId":""}),
            json!({"threadId":"thread","processId":"process"}),
        ] {
            assert_eq!(validate(&request(params, true)), Err(PortError::Invalid));
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
    fn actorless_reads_accept_only_documented_exact_parameter_shapes() {
        let cases = [
            (Method::ThreadList, json!({"limit":50,"cursor":null})),
            (Method::ThreadLoadedList, json!({"limit":100})),
            (
                Method::ThreadLoadedList,
                json!({"limit":100,"cursor":"next"}),
            ),
            (Method::ModelList, json!({"limit":100})),
            (Method::ModelList, json!({"limit":100,"cursor":"next"})),
            (Method::ModelList, json!({"limit":100,"includeHidden":true})),
            (
                Method::SkillsList,
                json!({"cwds":["/workspace"],"forceReload":false}),
            ),
            (
                Method::SkillsList,
                json!({"cwds":["/workspace"],"forceReload":true}),
            ),
            (
                Method::McpServerStatusList,
                json!({"threadId":null,"limit":100}),
            ),
            (
                Method::McpServerStatusList,
                json!({"threadId":"thread-1","limit":100}),
            ),
            (Method::ClaudeDiagnostics, json!({})),
            (Method::AccountRateLimitsRead, json!({})),
            (
                Method::AccountLoginStart,
                json!({"type":"chatgptDeviceCode"}),
            ),
            (
                Method::AccountLoginCancel,
                json!({"loginId":"fixture-native-login"}),
            ),
            (
                Method::OperationStatus,
                json!({"operationId":"monitor:fixture"}),
            ),
            (Method::AccountRead, json!({"refreshToken":false})),
            (
                Method::ThreadRead,
                json!({"threadId":"fixture-thread","includeTurns":false}),
            ),
            (
                Method::ThreadTurnsList,
                json!({"threadId":"fixture-thread","limit":20,"sortDirection":"desc","itemsView":"full"}),
            ),
        ];
        for (method, params) in cases {
            let operation = match method {
                Method::AccountRateLimitsRead => "limits-read:fixture",
                Method::AccountLoginStart => "6f1d2a3b-4c5d-4e6f-8a9b-0c1d2e3f4a5b",
                Method::AccountLoginCancel => "6f1d2a3b-4c5d-4e6f-8a9b-0c1d2e3f4a5b:cancel",
                _ => "fixture-operation",
            };
            assert!(
                valid_actorless_request_shape(method, &params, operation),
                "{method:?}: {params}"
            );
            let mut extended = params;
            extended["unapproved"] = json!(true);
            assert!(
                !valid_actorless_request_shape(method, &extended, operation),
                "{method:?} accepted extra params: {extended}"
            );
        }
        // A sign-in request is bound to its durable reservation identity.
        assert!(!valid_actorless_request_shape(
            Method::AccountLoginStart,
            &json!({"type":"chatgptDeviceCode"}),
            "fixture-operation"
        ));
        assert!(!valid_actorless_request_shape(
            Method::AccountLoginStart,
            &json!({"type":"apiKey"}),
            "6f1d2a3b-4c5d-4e6f-8a9b-0c1d2e3f4a5b"
        ));
        assert!(!valid_actorless_request_shape(
            Method::AccountLoginCancel,
            &json!({"loginId":"fixture-native-login"}),
            "6f1d2a3b-4c5d-4e6f-8a9b-0c1d2e3f4a5b"
        ));
        assert!(!valid_actorless_request_shape(
            Method::AccountLoginCancel,
            &json!({"loginId":""}),
            "6f1d2a3b-4c5d-4e6f-8a9b-0c1d2e3f4a5b:cancel"
        ));
        assert!(!valid_actorless_request_shape(
            Method::ThreadRead,
            &json!({"threadId":"fixture-thread"}),
            "fixture-operation"
        ));
        assert!(!valid_actorless_request_shape(
            Method::ThreadTurnsList,
            &json!({"threadId":"fixture-thread","limit":10,"sortDirection":"desc","itemsView":"full"}),
            "fixture-operation"
        ));
        assert!(!valid_actorless_request_shape(
            Method::SkillsList,
            &json!({"cwds":["/workspace"],"forceReload":false,"extra":true}),
            "fixture-operation"
        ));
        assert!(!valid_actorless_request_shape(
            Method::McpServerStatusList,
            &json!({"threadId":"thread-1","limit":50}),
            "fixture-operation"
        ));
    }

    #[test]
    fn native_command_terminal_requests_are_actor_bound_and_exact() {
        let mut request = turns_request("full", 1, true);
        request.method = Method::ThreadBackgroundTerminalsList;
        request.params = json!({"threadId":"fixture-thread","limit":100});
        assert!(validate(&request).is_ok());
        request.params["unexpected"] = json!(true);
        assert_eq!(validate(&request), Err(PortError::Invalid));

        request.method = Method::ThreadBackgroundTerminalsTerminate;
        request.params = json!({"threadId":"fixture-thread","processId":"44"});
        assert!(validate(&request).is_ok());
        request.actor = None;
        assert_eq!(validate(&request), Err(PortError::Invalid));
        request.actor = Some(RequestActor {
            agent: "fixture-agent".into(),
            epoch: 1,
        });
        request.params["unexpected"] = json!(true);
        assert_eq!(validate(&request), Err(PortError::Invalid));
    }

    #[test]
    fn retained_worker_accepts_only_bound_actorless_reset_credit_shapes() {
        let request = |method, operation: &str, params| SessionRequest {
            scope: ConnectionScope {
                account: "default".into(),
                connection: "fixture-connection".into(),
                generation: 1,
            },
            expected_scope: None,
            operation: operation.into(),
            actor: None,
            transfer: None,
            deleted_actor_session: None,
            method,
            params,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(1),
        };
        assert!(
            validate(&request(
                Method::AccountRateLimitsRead,
                "limits-read:account:1",
                json!({})
            ))
            .is_ok()
        );
        let key = "11000000-0000-4000-8000-000000000001";
        let params = json!({"creditId":"credit-A","idempotencyKey":key});
        assert!(
            validate(&request(
                Method::AccountRateLimitResetCreditConsume,
                &format!("limit-reset:{key}:1"),
                params.clone(),
            ))
            .is_ok()
        );
        assert!(
            validate(&request(
                Method::AccountRateLimitResetCreditConsume,
                &format!("limit-reset:{key}"),
                params.clone(),
            ))
            .is_ok(),
            "persisted legacy receipts remain valid"
        );
        for (operation, params) in [
            (format!("limit-reset:{key}:0"), params.clone()),
            (format!("limit-reset:{key}:first"), params.clone()),
            ("limit-reset:other:2".into(), params.clone()),
            (
                format!("limit-reset:{key}:2"),
                json!({"creditId":"credit-A","idempotencyKey":"11000000-0000-4000-8000-000000000002"}),
            ),
        ] {
            assert_eq!(
                validate(&request(
                    Method::AccountRateLimitResetCreditConsume,
                    &operation,
                    params,
                )),
                Err(PortError::Invalid)
            );
        }
        assert_eq!(
            validate(&request(
                Method::AccountRateLimitResetCreditConsume,
                "limit-reset:other:1",
                params,
            )),
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
        request.params = json!({
            "threadId":"fixture-thread",
            "model":"fixture-model",
            "serviceTier":"default"
        });
        request.operation = "native-action".into();
        assert_eq!(validate(&request), Err(PortError::Invalid));
    }

    #[test]
    fn voice_calls_require_exact_actor_scope_and_provider_shapes() {
        let mut request = turns_request("full", 1, true);
        request.expected_scope = Some(request.scope.clone());
        request.operation = "voice:start:session-1".into();
        request.method = Method::ThreadRealtimeStart;
        request.params = json!({
            "threadId":"fixture-thread",
            "version":"v3",
            "outputModality":"audio",
            "realtimeSessionId":"session-1",
            "clientManagedHandoffs":false,
            "flushTranscriptTailOnSessionEnd":false,
            "transport":{"type":"webrtc","sdp":"v=0 fixture-offer"}
        });
        assert!(validate(&request).is_ok());

        request.params["unexpected"] = json!(true);
        assert_eq!(validate(&request), Err(PortError::Invalid));
        request.params = json!({
            "threadId":"fixture-thread",
            "version":"v3",
            "outputModality":"audio",
            "realtimeSessionId":"other-session",
            "clientManagedHandoffs":false,
            "flushTranscriptTailOnSessionEnd":false,
            "transport":{"type":"webrtc","sdp":"v=0 fixture-offer"}
        });
        request.params["realtimeSessionId"] = json!("other-session");
        assert_eq!(validate(&request), Err(PortError::Invalid));
        request.params["realtimeSessionId"] = json!("session-1");
        request.expected_scope = None;
        assert_eq!(validate(&request), Err(PortError::Invalid));

        request.expected_scope = Some(request.scope.clone());
        request.method = Method::ThreadRealtimeStop;
        request.operation = "voice:stop:session-1".into();
        request.params = json!({"threadId":"fixture-thread"});
        assert!(validate(&request).is_ok());
        request.operation = "voice:other:session-1".into();
        assert_eq!(validate(&request), Err(PortError::Invalid));

        request.method = Method::ThreadRealtimeAppendSpeech;
        request.operation = "voice:speech:receipt-1".into();
        request.params = json!({"threadId":"fixture-thread","text":"hello"});
        assert!(validate(&request).is_ok());
        request.params["extra"] = json!(1);
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
