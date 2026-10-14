mod pipeline;
mod receipts;
mod snapshot;
use super::*;
#[cfg(test)]
pub(super) use pipeline::prepare;
pub(super) use pipeline::{
    CommitGuard, admit, commit, commit_claude, persist_compilation, prepare_ready, workspace,
};
pub use receipts::remember_delivered_manifests;
pub(super) use receipts::{accept_thread, accept_turn};
pub use snapshot::PreparationResources;

fn refusal(reason: String) -> WorkerError {
    WorkerError::PreparationRefused(reason)
}

/// Synchronous preparation entry for callers already on a blocking executor.
/// Runs the same admission, workspace, compiler and guarded commit as the worker.
/// Provider transport and receipt reduction remain owned by the account worker.
pub struct PreparedTurn(pipeline::RequestPreparation);
pub fn prepare_turn(
    store: &RuntimeStore,
    resources: &PreparationResources,
    intent: &TurnIntent,
    generation: u64,
) -> Result<PreparedTurn, WorkerError> {
    pipeline::prepare(store, resources, intent, Method::TurnStart, generation).map(PreparedTurn)
}
impl PreparedTurn {
    /// Exact native method and parameters handed to SessionControl::prepare.
    pub fn native_request(&self) -> Value {
        json!({"method":self.0.method.as_str(),"params":self.0.params})
    }
    /// Enter the production submission owner, including durable transport receipt
    /// persistence and refusal settlement. No provider write occurs here.
    pub fn commit(
        &self,
        store: &RuntimeStore,
        request_id: &str,
        generation: u64,
    ) -> Result<bool, WorkerError> {
        let prepared = &self.0;
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
            &pending,
            &prepared.intent.account_key,
            &request,
            None,
            prepared.submission.as_ref(),
            Some(&prepared.guard),
        )
    }
}
