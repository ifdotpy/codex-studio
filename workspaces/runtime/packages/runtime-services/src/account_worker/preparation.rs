mod pipeline;
mod receipts;
mod snapshot;
use super::*;
#[cfg(test)]
pub(super) use pipeline::prepare;
pub(super) use pipeline::{
    CommitGuard, admit, commit, persist_compilation, prepare_ready, workspace,
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
    /// Own the same submission transaction as account_worker::store::submitted.
    pub fn commit(
        &self,
        store: &RuntimeStore,
        request_id: &str,
        generation: u64,
    ) -> Result<bool, WorkerError> {
        scheduler::lifecycle_transaction_with(store, |tx| {
            let prepared = &self.0;
            if !SchedulerRepository::reconcile_obsolete_before_submit(
                tx,
                &prepared.intent.agent_id,
                prepared.intent.epoch,
                &prepared.intent.attempt_id,
            )? {
                return Ok(false);
            }
            let pending = Pending {
                intent: prepared.intent.clone(),
                method: "turn/start".into(),
                operation: format!(
                    "account:{}:{}:turn/start",
                    prepared.intent.account_key, prepared.intent.attempt_id
                ),
                thread: prepared.thread.clone(),
                compiled: prepared.compiled.clone(),
            };
            pipeline::commit(tx, &pending, &prepared.guard)?;
            let submission = prepared
                .submission
                .as_ref()
                .ok_or_else(|| refusal("preparation/turn-required".into()))?;
            let committed =
                commit_prepared_turn(tx, &prepared.intent, request_id, generation, submission)?;
            if committed && let Some(compiled) = &prepared.compiled {
                pipeline::persist_compilation(tx, compiled)?;
            }
            Ok(committed)
        })
    }
}
