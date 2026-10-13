//! Keep the original transcript receipt identity on a proven frozen retry.
use super::{SubmittedBinding, bind_submitted, freeze_start, invalid, reserved};
use crate::{
    account_worker::{WorkerError, commit_prepared_turn},
    notification_dispatch::{PreparedSubmission, SubmittedInput},
    scheduler::TurnIntent,
};
use serde_json::Value;
use studio_runtime_store::{NotificationDispatchRepository, StoreResult, Transaction};

/// Resolve the complete original submission, including clocks and asset views.
/// Current events supply full input text (the transcript previews may truncate).
/// `record_user_submission` still verifies the original receipt signature; this
/// helper cannot bless a changed submission or reuse a different input's receipt.
fn original_submission(
    tx: &Transaction<'_>,
    agent: &str,
    submission: &PreparedSubmission,
) -> StoreResult<PreparedSubmission> {
    let item = format!("{agent}:{}", submission.first_event_id);
    let (record, created_at) = NotificationDispatchRepository::transcript_item(tx, agent, &item)?
        .ok_or_else(|| invalid(agent))?;
    let inputs = record
        .get("inputs")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid(agent))?
        .iter()
        .map(|input| {
            let id = input
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid(agent))?;
            let (kind, text, at) = NotificationDispatchRepository::user_event(tx, agent, id)?
                .ok_or_else(|| invalid(agent))?;
            if input.get("kind").and_then(Value::as_str) != Some(&kind)
                || input.get("at").and_then(Value::as_f64) != Some(at)
            {
                return Err(invalid(agent));
            }
            Ok(SubmittedInput {
                id: id.into(),
                kind,
                text,
                at,
                assets: input.get("assets").cloned().ok_or_else(|| invalid(agent))?,
            })
        })
        .collect::<StoreResult<Vec<_>>>()?;
    if inputs
        .iter()
        .map(|i| &i.id)
        .ne(submission.inputs.iter().map(|i| &i.id))
    {
        return Err(invalid(agent));
    }
    Ok(PreparedSubmission {
        first_event_id: submission.first_event_id.clone(),
        submitted_at: record
            .get("at")
            .and_then(Value::as_f64)
            .ok_or_else(|| invalid(agent))?,
        created_at,
        text: submission.text.clone(),
        inputs,
        assets: record
            .get("assets")
            .cloned()
            .ok_or_else(|| invalid(agent))?,
    })
}

/// Worker Submitted transaction: call instead of freeze/commit/bind separately.
/// Preparation has already resolved the frozen params and text with start_input.
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
    let original;
    let submission = if rows
        .agent
        .pointer("/startAttempt/claudeRetryOf")
        .and_then(Value::as_str)
        .is_some()
    {
        // Freeze below proves that the text/configuration/events still match the
        // positively rejected attempt before any durable receipt is replayed.
        original = original_submission(tx, &intent.agent_id, submission)?;
        &original
    } else {
        submission
    };
    freeze_start(
        tx,
        &intent.agent_id,
        &intent.attempt_id,
        binding.transport.clone(),
        params,
        &submission.text,
    )?;
    if !commit_prepared_turn(tx, intent, request_id, binding.generation, submission)? {
        return Err(invalid(&intent.agent_id).into());
    }
    bind_submitted(tx, &intent.agent_id, &intent.attempt_id, binding)?;
    Ok(())
}
