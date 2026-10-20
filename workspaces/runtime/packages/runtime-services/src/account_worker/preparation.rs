mod pipeline;
mod receipts;
mod snapshot;
use super::*;
#[cfg(test)]
pub(super) use pipeline::prepare;
pub(super) use pipeline::{
    Admission, CommitGuard, RequestPreparation, admit, commit, commit_claude, persist_compilation,
    prepare_ready, workspace,
};
pub(crate) use pipeline::{CATALOG_PENDING_REASON, WORKSPACE_PENDING_REASON};
pub use receipts::remember_delivered_manifests;
pub(super) use receipts::{accept_thread, accept_turn};
pub use snapshot::{
    PreparationResources, VmProviderAuthFuture, VmProviderLaunch, VmProviderLaunchFuture,
    VmProviderLaunchRequest, VmProviderLaunchResolver, VmProviderRefreshFuture,
};
pub(crate) use snapshot::{actor_tool_definitions, progress_context};

fn refusal(reason: String) -> WorkerError {
    WorkerError::PreparationRefused(reason)
}

/// Synchronous preparation entry for callers already on a blocking executor.
/// Runs the same admission, workspace, compiler and guarded commit as the worker.
/// Provider transport and receipt reduction remain owned by the account worker.
pub struct PreparedTurn(pipeline::RequestPreparation);

/// Readiness and provider settings produced by the shared turn preparation
/// pipeline for a compact/review action. Settings are submitted separately by
/// the native-action receipt owner.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedNativeAction {
    pub thread_id: String,
    pub settings: serde_json::Value,
}

/// Complete native `thread/start` parameters from the shared preparation
/// pipeline, including developer context, tool definitions, and permissions.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedNativeThread {
    pub params: serde_json::Value,
}

/// The planned fresh thread of a native review child, kept until its reply is
/// reduced by the ordinary thread receipt reducer.
pub(crate) struct PreparedReviewThread {
    attempt: Box<crate::thread_prepare::PreparedAttempt>,
}
impl PreparedReviewThread {
    pub(crate) fn params(&self) -> serde_json::Value {
        serde_json::Value::Object(self.attempt.params.clone())
    }
    pub(crate) fn request_digest(&self) -> &str {
        &self.attempt.request_digest
    }
    pub(crate) fn attempt_id(&self) -> &str {
        &self.attempt.attempt_id
    }
}

/// Filesystem observations of one review thread receipt reduction.
pub(crate) struct ReviewThreadFilesystem(snapshot::FilesystemSnapshot);
impl ReviewThreadFilesystem {
    pub(crate) fn capture(store: &RuntimeStore, agent_id: &str) -> Result<Self, WorkerError> {
        snapshot::FilesystemSnapshot::capture(store, agent_id).map(Self)
    }
}

pub(crate) use pipeline::{accept_review_thread, prepare_review_thread, review_thread_submitted};

/// Monitor thread identity and exact permission params prepared from the current
/// actor record. Unlike turn preparation this is valid during an active turn.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedMonitorCommand {
    pub thread_id: String,
    pub settings: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SafetyRetryPreparation {
    pub agent_id: String,
    pub attempt_id: String,
    pub generation: u64,
    pub source_thread_id: String,
    pub turn_id: String,
    pub model: String,
    pub input: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreparedSafetyRetry {
    pub attempt_id: String,
    pub thread_id: String,
    pub fork_params: serde_json::Value,
    pub turn_params: serde_json::Value,
}
impl PreparedSafetyRetry {
    /// Bind the provider's acknowledged fork identity into the complete turn map.
    pub fn with_thread_id(&mut self, thread_id: &str) -> Result<(), WorkerError> {
        if thread_id.is_empty() || thread_id.len() > 200 {
            return Err(refusal("safety-retry/thread-id-invalid".into()));
        }
        let params = self
            .turn_params
            .as_object_mut()
            .ok_or_else(|| refusal("safety-retry/turn-params-invalid".into()))?;
        params.insert("threadId".into(), serde_json::json!(thread_id));
        self.thread_id = thread_id.into();
        Ok(())
    }
}

pub async fn prepare_native_action(
    store: Arc<RuntimeStore>,
    resources: &PreparationResources,
    agent_id: &str,
    attempt_id: &str,
    generation: u64,
) -> Result<PreparedNativeAction, WorkerError> {
    pipeline::prepare_native_action(store, resources, agent_id, attempt_id, generation).await
}

pub async fn prepare_native_thread(
    store: Arc<RuntimeStore>,
    resources: &PreparationResources,
    agent_id: &str,
    attempt_id: &str,
    generation: u64,
) -> Result<PreparedNativeThread, WorkerError> {
    pipeline::prepare_native_thread(store, resources, agent_id, attempt_id, generation).await
}

/// Prepare a destination thread for the exact active account transfer owner.
pub async fn prepare_account_transfer_native_thread(
    store: Arc<RuntimeStore>,
    resources: &PreparationResources,
    agent_id: &str,
    transfer_id: &str,
    attempt_id: &str,
    generation: u64,
) -> Result<PreparedNativeThread, WorkerError> {
    pipeline::prepare_account_transfer_native_thread(
        store,
        resources,
        agent_id,
        transfer_id,
        attempt_id,
        generation,
    )
    .await
}

/// Prepare permission settings for an already-admitted monitor command. This
/// deliberately skips native-action startAttempt admission; monitor owners
/// authorize the running monitor and pass its observed agent epoch.
pub async fn prepare_monitor_command(
    store: Arc<RuntimeStore>,
    resources: &PreparationResources,
    agent_id: &str,
    epoch: u64,
) -> Result<PreparedMonitorCommand, WorkerError> {
    pipeline::prepare_monitor_command(store, resources, agent_id, epoch).await
}

pub async fn prepare_safety_retry(
    store: Arc<RuntimeStore>,
    resources: &PreparationResources,
    request: &SafetyRetryPreparation,
) -> Result<PreparedSafetyRetry, WorkerError> {
    pipeline::prepare_safety_retry(store, resources, request).await
}

pub async fn prepare_turn(
    store: Arc<RuntimeStore>,
    resources: &PreparationResources,
    intent: &TurnIntent,
    generation: u64,
) -> Result<PreparedTurn, WorkerError> {
    pipeline::prepare(store, resources, intent, Method::TurnStart, generation)
        .await
        .map(PreparedTurn)
}
/// Prepare with an identity supplied by the account-session owner. This keeps
/// transport receipts tied to the same connection the caller will submit on.
pub async fn prepare_turn_with_connection_id(
    store: Arc<RuntimeStore>,
    resources: &PreparationResources,
    intent: &TurnIntent,
    generation: u64,
    connection_id: &str,
) -> Result<PreparedTurn, WorkerError> {
    pipeline::prepare_with_connection_id(
        store,
        resources,
        intent,
        Method::TurnStart,
        generation,
        connection_id,
    )
    .await
    .map(PreparedTurn)
}
impl PreparedTurn {
    /// Exact native method and parameters handed to SessionControl::prepare.
    pub fn native_request(&self) -> Value {
        json!({"method":self.0.method.as_str(),"params":self.0.params})
    }
    /// Enter the production submission owner, including durable transport receipt
    /// persistence and refusal settlement. No provider write occurs here.
    pub async fn commit(
        self,
        store: &RuntimeStore,
        request_id: &str,
        generation: u64,
    ) -> Result<bool, WorkerError> {
        let prepared = self.0;
        let operation = format!(
            "account:{}:{}:{}",
            prepared.intent.account_key,
            prepared.intent.attempt_id,
            prepared.method.as_str()
        );
        let request = RecoveredRequest {
            ticket: studio_runtime_providers::RequestTicket {
                id: RequestId::String(request_id.into()),
                generation,
                method: prepared.method,
            },
            operation_id: OperationId(operation.clone()),
            thread_id: prepared.intent.thread_id.clone(),
            lightweight: false,
            deadline_ms: 60_000,
            unknown: false,
            remote_id: None,
        };
        let pending = Pending {
            intent: prepared.intent.clone(),
            method: prepared.method.as_str().into(),
            operation,
            thread: prepared.thread.clone(),
            compiled: prepared.compiled.clone(),
        };
        store::submitted(
            store,
            pending,
            &prepared.intent.account_key,
            request,
            store::SubmissionEvidence {
                turn: None,
                submission: prepared.submission,
                guard: Some(prepared.guard),
                connection_id: prepared.connection_id,
                effect: None,
            },
        )
        .await
    }
}

#[cfg(test)]
mod safety_retry_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn safety_retry_binds_fork_receipt_to_complete_turn_params() {
        let mut prepared = PreparedSafetyRetry {
            attempt_id: "attempt-1".into(),
            thread_id: "attempt-1".into(),
            fork_params: json!({"threadId":"source"}),
            turn_params: json!({"threadId":"attempt-1","input":[]}),
        };

        assert!(prepared.with_thread_id("forked-thread").is_ok());

        assert_eq!(prepared.thread_id, "forked-thread");
        assert_eq!(prepared.turn_params["threadId"], "forked-thread");
        assert_eq!(prepared.turn_params["input"], json!([]));
        assert!(prepared.with_thread_id("").is_err());
    }
}
