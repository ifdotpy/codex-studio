//! Durable control caller; a submitted request without a provider receipt is held.
use super::*;
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use studio_runtime_providers::Method;
use studio_runtime_schema::{
    claude_controls::{ClaudeSettings, ControlId},
    queue_command::Field,
};
pub type ControlFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
/// Implementations bind calls to one retained account/connection/generation.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ControlBinding {
    account: String,
    connection: String,
    generation: u64,
}
impl ControlBinding {
    pub fn new(account: String, connection: String, generation: u64) -> Result<Self> {
        if account.is_empty()
            || connection.is_empty()
            || account.len() > 1024
            || connection.len() > 1024
            || generation == 0
        {
            return Err(ControlError::Request);
        }
        Ok(Self {
            account,
            connection,
            generation,
        })
    }
    pub fn account(&self) -> &str {
        &self.account
    }
}
/// Provider methods and their parameters stay associated until the retained wire boundary.
/// Mutations carry their durable transport request identity; rollback also carries
/// the original request and turn identities used by the bridge's proof.
///
/// ```compile_fail
/// use studio_runtime_services::claude_controls::service::ControlInvocation;
/// let call = ControlInvocation::Rollback { thread_id: "thread".into(), restore_files: true };
/// ```
/// ```compile_fail
/// use studio_runtime_services::claude_controls::service::ControlInvocation;
/// let turn_id = serde_json::from_str("\"turn\"").unwrap();
/// let call = ControlInvocation::Rollback { thread_id: "thread".into(), turn_id, restore_files: true, transport_request_id: "transport".into() };
/// ```
/// ```compile_fail
/// use studio_runtime_services::claude_controls::service::ControlInvocation;
/// let request_id = serde_json::from_str("\"request\"").unwrap();
/// let call = ControlInvocation::Rollback { thread_id: "thread".into(), request_id, restore_files: true, transport_request_id: "transport".into() };
/// ```
/// ```compile_fail
/// use studio_runtime_services::claude_controls::service::ControlInvocation;
/// let request_id = serde_json::from_str("\"request\"").unwrap();
/// let turn_id = serde_json::from_str("\"turn\"").unwrap();
/// let call = ControlInvocation::Rollback { thread_id: "thread".into(), request_id, turn_id, restore_files: true };
/// ```
/// ```compile_fail
/// use studio_runtime_services::claude_controls::service::ControlInvocation;
/// let call = ControlInvocation::Settings { thread_id: "thread".into(), settings: Default::default() };
/// ```
#[derive(Clone, Debug)]
pub enum ControlInvocation {
    State {
        thread_id: String,
    },
    Queue {
        thread_id: String,
    },
    RepairReport {
        thread_id: String,
    },
    Commands {
        cwd: String,
    },
    Settings {
        thread_id: String,
        settings: ClaudeSettings,
        request_id: String,
    },
    Rollback {
        thread_id: String,
        request_id: ControlId,
        turn_id: ControlId,
        restore_files: bool,
        transport_request_id: String,
    },
    StopTask {
        thread_id: String,
        task_id: String,
        request_id: String,
    },
    FileBackups {
        thread_id: String,
        settled_request_ids: Option<Vec<String>>,
        request_id: String,
    },
}
impl ControlInvocation {
    pub fn method(&self) -> Method {
        match self {
            Self::State { .. } => Method::ClaudeState,
            Self::Queue { .. } => Method::ThreadQueueList,
            Self::RepairReport { .. } => Method::ClaudeRepairReport,
            Self::Commands { .. } => Method::ClaudeCommands,
            Self::Settings { .. } => Method::ClaudeSettings,
            Self::Rollback { .. } => Method::ThreadRollback,
            Self::StopTask { .. } => Method::ClaudeStopTask,
            Self::FileBackups { .. } => Method::ClaudeFileBackups,
        }
    }
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Self::Settings { request_id, .. }
            | Self::StopTask { request_id, .. }
            | Self::FileBackups { request_id, .. } => Some(request_id),
            Self::Rollback {
                transport_request_id,
                ..
            } => Some(transport_request_id),
            Self::State { .. }
            | Self::Queue { .. }
            | Self::RepairReport { .. }
            | Self::Commands { .. } => None,
        }
    }
    /// Serialize exactly the established provider body for signing and transport.
    pub fn params(&self) -> Value {
        match self {
            Self::State { thread_id }
            | Self::Queue { thread_id }
            | Self::RepairReport { thread_id } => json!({"threadId":thread_id}),
            Self::Commands { cwd } => json!({"cwd":cwd}),
            Self::Settings {
                thread_id,
                settings,
                ..
            } => json!({"threadId":thread_id,"settings":settings}),
            Self::Rollback {
                thread_id,
                request_id,
                turn_id,
                restore_files,
                ..
            } => {
                json!({"threadId":thread_id,"requestId":request_id,"turnId":turn_id,"restoreFiles":restore_files})
            }
            Self::StopTask {
                thread_id, task_id, ..
            } => json!({"threadId":thread_id,"taskId":task_id}),
            Self::FileBackups {
                thread_id,
                settled_request_ids,
                ..
            } => json!({"threadId":thread_id,"settledRequestIds":settled_request_ids}),
        }
    }
}

/// Known fields used to prove idleness; SDK task details and extensions are opaque.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeState {
    pub turns: Vec<NativeTurn>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub tasks: Field<Vec<Value>>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeTurn {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub status: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub can_restore_files: Field<bool>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}
#[derive(Clone, Debug, serde::Deserialize)]
pub struct NativeQueue {
    #[serde(default)]
    pub data: Field<Vec<Value>>,
    #[serde(default)]
    pub items: Field<Vec<Value>>,
}
impl NativeQueue {
    pub fn busy(&self) -> bool {
        let selected = if self.data.is_missing() {
            &self.items
        } else {
            &self.data
        };
        selected.value().is_some_and(|items| !items.is_empty())
    }
}

/// A settings mutation and a rollback require different idle proofs.
#[derive(Clone, Debug)]
pub enum NativeProof {
    Settings(NativeState),
    Rollback {
        state: NativeState,
        queue: NativeQueue,
    },
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct SettingsReply {
    pub settings: serde_json::Map<String, Value>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackReply {
    pub thread_id: String,
    pub native_id: String,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub files_restored: Field<bool>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// The only provider replies that can settle a control mutation.
#[derive(Debug)]
pub enum MutationReply {
    Settings(SettingsReply),
    Rollback(RollbackReply),
}
impl MutationReply {
    pub fn into_value(self) -> Result<Value> {
        match self {
            Self::Settings(reply) => serde_json::to_value(reply),
            Self::Rollback(reply) => serde_json::to_value(reply),
        }
        .map_err(|_| ControlError::Unknown)
    }
}

/// The retained wire accepts arbitrary provider JSON. Once received, the method
/// selects the corresponding reply; only SDK-specific and forwarded payloads stay opaque.
#[derive(Debug)]
pub enum ControlReply {
    State(NativeState),
    Queue(NativeQueue),
    RepairReport(Value),
    Commands(Value),
    Settings(SettingsReply),
    Rollback(RollbackReply),
    StopTask(Value),
    FileBackups(Value),
}
impl ControlReply {
    pub fn from_wire(call: &ControlInvocation, value: Value) -> Result<Self> {
        Ok(match call {
            ControlInvocation::State { .. } => {
                Self::State(serde_json::from_value(value).map_err(|_| ControlError::Unknown)?)
            }
            ControlInvocation::Queue { .. } => {
                Self::Queue(serde_json::from_value(value).map_err(|_| ControlError::Unknown)?)
            }
            ControlInvocation::RepairReport { .. } => Self::RepairReport(value),
            ControlInvocation::Commands { .. } => Self::Commands(value),
            ControlInvocation::Settings { .. } => {
                Self::Settings(serde_json::from_value(value).map_err(|_| ControlError::Unknown)?)
            }
            ControlInvocation::Rollback { .. } => {
                Self::Rollback(serde_json::from_value(value).map_err(|_| ControlError::Unknown)?)
            }
            ControlInvocation::StopTask { .. } => Self::StopTask(value),
            ControlInvocation::FileBackups { .. } => Self::FileBackups(value),
        })
    }
}

/// Agent/epoch come from the checked stored snapshot. A mutation's request ID
/// is the durable control identity; read-only calls need no replay identity.
#[derive(Clone, PartialEq, Eq)]
pub struct ControlScope {
    pub agent: String,
    pub epoch: u64,
    pub request_id: Option<String>,
}
pub trait ControlSession: Send + Sync {
    fn binding(&self) -> ControlBinding;
    fn call_scoped(
        &self,
        scope: ControlScope,
        call: ControlInvocation,
    ) -> ControlFuture<'_, ControlReply>;
}
/// Every method compares source, epoch and reservation inside the same transaction.
/// `reserve` resolves shared workspace idle evidence before its comparison transaction.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FailureEvidence {
    Uncertain,
    BeforeSubmission,
    // The retained transport proves that this exact native request was not sent.
    NotSent,
    // The bridge proves this exact preparation failed before any native effects.
    NotApplied,
}
pub trait ControlStorage: Send + Sync {
    fn execution_key(&self, request: &ControlRequest) -> String {
        format!("{:p}:{}", self, request.id())
    }
    fn reserve(
        &self,
        request: ControlRequest,
        binding: ControlBinding,
    ) -> ControlFuture<'_, ControlSnapshot>;
    fn submitted(
        &self,
        expected: ControlSnapshot,
        request: ControlRequest,
        proof: NativeProof,
    ) -> ControlFuture<'_, ControlSnapshot>;
    fn provider(
        &self,
        expected: ControlSnapshot,
        request: ControlRequest,
        result: MutationReply,
    ) -> ControlFuture<'_, ControlSnapshot>;
    fn complete(
        &self,
        expected: ControlSnapshot,
        request: ControlRequest,
    ) -> ControlFuture<'_, Value>;
    fn failed(&self, expected: ControlSnapshot, evidence: FailureEvidence)
    -> ControlFuture<'_, ()>;
}
pub struct Controls {
    store: Arc<dyn ControlStorage>,
    session: Arc<dyn ControlSession>,
}
impl Controls {
    pub fn new(store: Arc<dyn ControlStorage>, session: Arc<dyn ControlSession>) -> Self {
        Self { store, session }
    }
    pub async fn execute(&self, request: ControlRequest) -> Result<Value> {
        let _running =
            crate::checkpoints::RestoreExecution::acquire(self.store.execution_key(&request))
                .map_err(|_| ControlError::Busy)?;
        let binding = self.session.binding();
        let mut snapshot = self.store.reserve(request.clone(), binding.clone()).await?;
        if snapshot.agent.account_key != binding.account {
            return Err(ControlError::Changed);
        }
        if let Some(op) = snapshot.saved.as_ref()
            && op["phase"] == "completed"
        {
            return Ok(op.get("result").cloned().unwrap_or(Value::Null));
        }
        let mut uncertain = snapshot
            .saved
            .as_ref()
            .is_some_and(|op| truthy(op.get("submitted")));
        let result = self
            .perform(&request, &binding, &mut snapshot, &mut uncertain)
            .await;
        if result.is_err() {
            let evidence = if matches!(&result, Err(ControlError::NotSent)) {
                FailureEvidence::NotSent
            } else if matches!(
                &result,
                Err(ControlError::RewindNotApplied(_) | ControlError::SettingsNotApplied(_))
            ) {
                FailureEvidence::NotApplied
            } else if uncertain {
                FailureEvidence::Uncertain
            } else {
                FailureEvidence::BeforeSubmission
            };
            self.store.failed(snapshot, evidence).await?;
        }
        result
    }
    async fn perform(
        &self,
        request: &ControlRequest,
        binding: &ControlBinding,
        snapshot: &mut ControlSnapshot,
        uncertain: &mut bool,
    ) -> Result<Value> {
        let attempt = snapshot
            .saved
            .as_ref()
            .and_then(|op| op.get("controlAttempt"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let transport_request = if attempt == 0 {
            request.request_id().to_owned()
        } else {
            format!(
                "claude-retry:{}",
                request_signature(&json!([request.request_id(), attempt]))
            )
        };
        let scope = ControlScope {
            agent: snapshot.agent.id.clone(),
            epoch: snapshot.epoch(),
            request_id: Some(transport_request.clone()),
        };
        if snapshot
            .saved
            .as_ref()
            .is_none_or(|op| op.get("provider").is_none_or(Value::is_null))
        {
            let result = if let Some(thread) = snapshot.agent.thread_id.clone() {
                if *uncertain {
                    return Err(ControlError::Unknown);
                }
                let native = match self
                    .bound_call(
                        binding,
                        &scope,
                        ControlInvocation::State {
                            thread_id: thread.clone(),
                        },
                        20,
                    )
                    .await?
                {
                    ControlReply::State(state) => state,
                    ControlReply::Queue(_)
                    | ControlReply::RepairReport(_)
                    | ControlReply::Commands(_)
                    | ControlReply::Settings(_)
                    | ControlReply::Rollback(_)
                    | ControlReply::StopTask(_)
                    | ControlReply::FileBackups(_) => return Err(ControlError::Unknown),
                };
                let queue = if matches!(request, ControlRequest::Rollback { .. }) {
                    Some(
                        match self
                            .bound_call(
                                binding,
                                &scope,
                                ControlInvocation::Queue {
                                    thread_id: thread.clone(),
                                },
                                20,
                            )
                            .await?
                        {
                            ControlReply::Queue(queue) => queue,
                            ControlReply::State(_)
                            | ControlReply::RepairReport(_)
                            | ControlReply::Commands(_)
                            | ControlReply::Settings(_)
                            | ControlReply::Rollback(_)
                            | ControlReply::StopTask(_)
                            | ControlReply::FileBackups(_) => return Err(ControlError::Unknown),
                        },
                    )
                } else {
                    None
                };
                *uncertain = true;
                *snapshot = match self
                    .store
                    .submitted(
                        snapshot.clone(),
                        request.clone(),
                        match queue {
                            Some(queue) => NativeProof::Rollback {
                                state: native,
                                queue,
                            },
                            None => NativeProof::Settings(native),
                        },
                    )
                    .await
                {
                    Ok(next) => next,
                    Err(error) => {
                        if matches!(
                            error,
                            ControlError::Busy | ControlError::Changed | ControlError::Request
                        ) {
                            *uncertain = false;
                        }
                        return Err(error);
                    }
                };
                let call = match request {
                    ControlRequest::Settings { settings, .. } => ControlInvocation::Settings {
                        thread_id: thread,
                        settings: settings.clone(),
                        request_id: transport_request.clone(),
                    },
                    ControlRequest::Rollback {
                        request_id,
                        turn_id,
                        restore_files,
                        ..
                    } => ControlInvocation::Rollback {
                        thread_id: thread,
                        request_id: request_id.clone(),
                        turn_id: turn_id.clone(),
                        restore_files: *restore_files,
                        transport_request_id: transport_request.clone(),
                    },
                };
                match self
                    .bound_call(
                        binding,
                        &scope,
                        call,
                        if request.restore_files() {
                            FILE_RESTORE_TIMEOUT_SECONDS
                        } else {
                            25
                        },
                    )
                    .await
                {
                    Err(error @ (ControlError::NotSent | ControlError::Request)) => {
                        // A provider rejection can follow a partial file restore.
                        // Only an explicit not-sent receipt proves no file effect.
                        if !request.restore_files() || matches!(error, ControlError::NotSent) {
                            *uncertain = false;
                        } else {
                            return Err(ControlError::Unknown);
                        }
                        return Err(error);
                    }
                    Ok(ControlReply::Settings(result))
                        if matches!(request, ControlRequest::Settings { .. }) =>
                    {
                        MutationReply::Settings(result)
                    }
                    Ok(ControlReply::Rollback(result))
                        if matches!(request, ControlRequest::Rollback { .. }) =>
                    {
                        MutationReply::Rollback(result)
                    }
                    Ok(_) => return Err(ControlError::Unknown),
                    Err(error) => return Err(error),
                }
            } else {
                match request {
                    ControlRequest::Settings { settings, .. } => {
                        MutationReply::Settings(SettingsReply {
                            settings: serde_json::to_value(settings)
                                .map_err(|_| ControlError::Unknown)?
                                .as_object()
                                .cloned()
                                .ok_or(ControlError::Unknown)?,
                            extra: Default::default(),
                        })
                    }
                    ControlRequest::Rollback { .. } => return Err(ControlError::Busy),
                }
            };
            *uncertain = true;
            *snapshot = self
                .store
                .provider(snapshot.clone(), request.clone(), result)
                .await?;
        }
        self.store.complete(snapshot.clone(), request.clone()).await
    }
    async fn bound_call(
        &self,
        binding: &ControlBinding,
        scope: &ControlScope,
        call: ControlInvocation,
        seconds: u64,
    ) -> Result<ControlReply> {
        if &self.session.binding() != binding {
            return Err(ControlError::Changed);
        }
        let mut scope = scope.clone();
        // Idle reads must be fresh and must not consume the mutation's receipt identity.
        if matches!(
            call,
            ControlInvocation::State { .. } | ControlInvocation::Queue { .. }
        ) {
            scope.request_id = None;
        }
        let result = tokio::time::timeout(
            Duration::from_secs(seconds),
            self.session.call_scoped(scope, call),
        )
        .await
        .map_err(|_| ControlError::Unknown)??;
        if &self.session.binding() != binding {
            return Err(ControlError::Unknown);
        }
        Ok(result)
    }
    /// The caller supplies the latest durable operation for state projection.
    pub async fn read_or_stop(
        &self,
        agent: &Value,
        body: &Value,
        operation: Option<Value>,
    ) -> Result<Value> {
        let snapshot = ControlSnapshot::from_records(agent.clone(), operation)?;
        let binding = self.session.binding();
        let scope = ControlScope {
            agent: snapshot.agent.id.clone(),
            epoch: snapshot.epoch(),
            request_id: body
                .get("request_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        };
        let action = body.get("action").and_then(Value::as_str);
        let lazy = agent.get("lazyAccountTransfer").filter(|v| truthy(Some(v)));
        let account = if action == Some("state") {
            lazy.and_then(|v| v.get("sourceAccountKey"))
                .and_then(Value::as_str)
                .unwrap_or(&snapshot.agent.account_key)
        } else {
            &snapshot.agent.account_key
        };
        if binding.account() != account {
            return Err(ControlError::Changed);
        }
        let thread = if action == Some("state") && lazy.is_some() {
            lazy.and_then(|v| v.get("sourceThreadId"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        } else {
            snapshot.agent.thread_id.clone()
        };
        match action {
            Some("state") => {
                let mut result = if let Some(thread) = thread {
                    match self
                        .bound_call(
                            &binding,
                            &scope,
                            ControlInvocation::State {
                                thread_id: thread.clone(),
                            },
                            20,
                        )
                        .await?
                    {
                        ControlReply::State(state) => {
                            serde_json::to_value(state).map_err(|_| ControlError::Unknown)?
                        }
                        ControlReply::Queue(_)
                        | ControlReply::RepairReport(_)
                        | ControlReply::Commands(_)
                        | ControlReply::Settings(_)
                        | ControlReply::Rollback(_)
                        | ControlReply::StopTask(_)
                        | ControlReply::FileBackups(_) => return Err(ControlError::Unknown),
                    }
                } else {
                    json!({"settings":agent.get("claudeOptions").cloned().unwrap_or(json!({})),"turns":[],"tasks":[]})
                };
                if !result.is_object() {
                    return Err(ControlError::Unknown);
                }
                if let Some(op) = snapshot.saved {
                    result["controlOperation"] = json!({"requestId":op.get("requestId"),"turnId":op.get("turnId"),"restoreFiles":op.get("restoreFiles").and_then(Value::as_bool).unwrap_or(false),"phase":op.get("phase"),"error":op.get("error")});
                }
                Ok(result)
            }
            Some("commands") => {
                match self
                    .bound_call(
                        &binding,
                        &scope,
                        ControlInvocation::Commands {
                            cwd: snapshot.agent.cwd,
                        },
                        25,
                    )
                    .await?
                {
                    ControlReply::Commands(result) => Ok(result),
                    ControlReply::State(_)
                    | ControlReply::Queue(_)
                    | ControlReply::RepairReport(_)
                    | ControlReply::Settings(_)
                    | ControlReply::Rollback(_)
                    | ControlReply::StopTask(_)
                    | ControlReply::FileBackups(_) => Err(ControlError::Unknown),
                }
            }
            Some("stop_task") => {
                if truthy(agent.get("accountTransferId")) {
                    return Err(ControlError::Changed);
                }
                let task = body
                    .get("task_id")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty() && t.len() <= 1024)
                    .ok_or(ControlError::Request)?;
                let mut scope = scope;
                let thread = thread.ok_or(ControlError::Request)?;
                // The task identity is stable even when a client omits request_id.
                scope.request_id.get_or_insert_with(|| {
                    format!("stop-task:{}", request_signature(&json!([thread, task])))
                });
                let request_id = scope.request_id.clone().ok_or(ControlError::Request)?;
                match self
                    .bound_call(
                        &binding,
                        &scope,
                        ControlInvocation::StopTask {
                            thread_id: thread,
                            task_id: task.to_owned(),
                            request_id,
                        },
                        20,
                    )
                    .await?
                {
                    ControlReply::StopTask(result) => Ok(result),
                    ControlReply::State(_)
                    | ControlReply::Queue(_)
                    | ControlReply::RepairReport(_)
                    | ControlReply::Commands(_)
                    | ControlReply::Settings(_)
                    | ControlReply::Rollback(_)
                    | ControlReply::FileBackups(_) => Err(ControlError::Unknown),
                }
            }
            _ => Err(ControlError::Request),
        }
    }
}
pub fn native_command(value: &str) -> Result<&str> {
    static COMMAND: std::sync::OnceLock<std::result::Result<regex::Regex, regex::Error>> =
        std::sync::OnceLock::new();
    let text = value.trim();
    if text.len() > 256 * 1024
        || !COMMAND
            .get_or_init(|| regex::Regex::new(r"^[/$][\w:.-]+(?:[ \t]+[^\r\n\x00]*)?$"))
            .as_ref()
            .is_ok_and(|r| r.is_match(text))
    {
        return Err(ControlError::Request);
    }
    Ok(text)
}
