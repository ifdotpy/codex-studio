//! Durable control caller; a submitted request without a provider receipt is held.
use super::*;
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
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
    fn call(&self, method: &str, params: Value) -> ControlFuture<'_, Value>;
    fn call_scoped(
        &self,
        _scope: ControlScope,
        method: &str,
        params: Value,
    ) -> ControlFuture<'_, Value> {
        self.call(method, params)
    }
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
        format!("{:p}:{}", self, request.id)
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
        native: Value,
        queue: Value,
    ) -> ControlFuture<'_, ControlSnapshot>;
    fn provider(
        &self,
        expected: ControlSnapshot,
        result: Value,
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
            } else if matches!(&result, Err(ControlError::RewindNotApplied(_))) {
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
            request.request_id.clone()
        } else {
            format!(
                "claude-retry:{}",
                request_signature(&json!([request.request_id, attempt]))
            )
        };
        let scope = ControlScope {
            agent: snapshot.agent.id.clone(),
            epoch: snapshot.epoch(),
            request_id: Some(transport_request),
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
                let native = self
                    .bound_call(
                        binding,
                        &scope,
                        "claude/state",
                        json!({"threadId":thread}),
                        20,
                    )
                    .await?;
                let queue = if request.action == Action::Rollback {
                    self.bound_call(
                        binding,
                        &scope,
                        "thread/queue/list",
                        json!({"threadId":thread}),
                        20,
                    )
                    .await?
                } else {
                    json!({})
                };
                *uncertain = true;
                *snapshot = match self
                    .store
                    .submitted(snapshot.clone(), request.clone(), native, queue)
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
                let (method, params) = if request.action == Action::Settings {
                    (
                        "claude/settings",
                        json!({"threadId":thread,"settings":request.settings}),
                    )
                } else {
                    (
                        "thread/rollback",
                        json!({"threadId":thread,"turnId":request.turn,"requestId":request.request_id,"restoreFiles":request.restore_files}),
                    )
                };
                match self
                    .bound_call(
                        binding,
                        &scope,
                        method,
                        params,
                        if request.restore_files {
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
                        if !request.restore_files || matches!(error, ControlError::NotSent) {
                            *uncertain = false;
                        } else {
                            return Err(ControlError::Unknown);
                        }
                        return Err(error);
                    }
                    result => result?,
                }
            } else {
                json!({"settings":request.settings})
            };
            *uncertain = true;
            *snapshot = self.store.provider(snapshot.clone(), result).await?;
        }
        self.store.complete(snapshot.clone(), request.clone()).await
    }
    async fn bound_call(
        &self,
        binding: &ControlBinding,
        scope: &ControlScope,
        method: &str,
        params: Value,
        seconds: u64,
    ) -> Result<Value> {
        if &self.session.binding() != binding {
            return Err(ControlError::Changed);
        }
        let mut scope = scope.clone();
        // Idle reads must be fresh and must not consume the mutation's receipt identity.
        if matches!(method, "claude/state" | "thread/queue/list") {
            scope.request_id = None;
        }
        let result = self.call(scope, method, params, seconds).await?;
        if &self.session.binding() != binding {
            return Err(ControlError::Unknown);
        }
        Ok(result)
    }
    async fn call(
        &self,
        scope: ControlScope,
        method: &str,
        params: Value,
        seconds: u64,
    ) -> Result<Value> {
        tokio::time::timeout(
            Duration::from_secs(seconds),
            self.session.call_scoped(scope, method, params),
        )
        .await
        .map_err(|_| ControlError::Unknown)?
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
                    self.bound_call(
                        &binding,
                        &scope,
                        "claude/state",
                        json!({"threadId":thread}),
                        20,
                    )
                    .await?
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
                self.bound_call(
                    &binding,
                    &scope,
                    "claude/commands",
                    json!({"cwd":snapshot.agent.cwd}),
                    25,
                )
                .await
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
                self.bound_call(
                    &binding,
                    &scope,
                    "claude/stopTask",
                    json!({"threadId":thread,"taskId":task}),
                    20,
                )
                .await
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
