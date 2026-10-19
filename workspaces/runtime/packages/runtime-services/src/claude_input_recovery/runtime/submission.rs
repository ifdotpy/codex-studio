//! The worker Submitted transaction: freeze, commit and bind in one pass.
use super::{SubmittedBinding, bind_submitted, freeze_loaded, invalid, reserved};
use crate::{
    account_worker::{WorkerError, commit_prepared_turn},
    notification_dispatch::PreparedSubmission,
    scheduler::TurnIntent,
};
use serde_json::Value;
use studio_runtime_store::Transaction;

/// Worker Submitted transaction: call instead of freeze/commit/bind separately.
/// `freeze_start` checks the prepared params against the reserved attempt.
/// Propagate every error to roll back the caller's entire transaction. Native I/O
/// is permitted only after it commits. A failed ownership check is an error, so
/// the freeze can never be accidentally committed with an Ok(false) result.
pub fn commit_start(
    tx: &Transaction<'_>,
    intent: &TurnIntent,
    request_id: &str,
    binding: SubmittedBinding,
    params: &Value,
    submission: &PreparedSubmission,
) -> Result<(), WorkerError> {
    let rows = reserved(tx, &intent.agent_id, &intent.attempt_id, &binding.transport)?;
    if submission.first_event_id != params["clientUserMessageId"]
        || submission
            .inputs
            .iter()
            .map(|i| Some(i.id.as_str()))
            .ne(rows
                .events
                .iter()
                .map(|e| e.get("id").and_then(Value::as_str)))
    {
        return Err(invalid(&intent.agent_id).into());
    }
    freeze_loaded(tx, &intent.agent_id, rows, params, &submission.text)?;
    if !commit_prepared_turn(
        tx,
        intent,
        request_id,
        binding.generation,
        &binding.transport.connection_id,
        submission,
    )? {
        return Err(invalid(&intent.agent_id).into());
    }
    bind_submitted(tx, &intent.agent_id, &intent.attempt_id, binding)?;
    Ok(())
}
