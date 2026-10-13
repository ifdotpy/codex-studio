mod pipeline;
mod receipts;
mod snapshot;
use super::*;
use crate::notification_dispatch::{PreparedSubmission, SubmittedInput};
#[cfg(test)]
pub(super) use pipeline::prepare;
pub(super) use pipeline::{
    CommitGuard, admit, commit, persist_compilation, prepare_ready, workspace,
};
pub use receipts::remember_delivered_manifests;
pub(super) use receipts::{accept_thread, accept_turn};
pub use snapshot::PreparationResources;
use studio_runtime_store::AgentRecord;

fn refusal(reason: String) -> WorkerError {
    WorkerError::PreparationRefused(reason)
}

/// Compile a submission after the caller has established thread and workspace readiness.
/// The worker owns admission and calls the same compiler through its guarded pipeline.
pub struct PreparationInput<'a> {
    pub intent: &'a TurnIntent,
    pub agent: &'a AgentRecord,
    pub transaction: &'a studio_runtime_store::Transaction<'a>,
    pub resources: &'a PreparationResources,
    pub state_root: &'a std::path::Path,
    pub submitted_at: f64,
}
pub struct PreparedTurn {
    pub params: Value,
    pub submission: PreparedSubmission,
    pub compiled: crate::turn_prepare::PreparedSubmission,
}
pub fn prepare_turn(input: PreparationInput<'_>) -> Result<PreparedTurn, String> {
    prepare_compiled(input).map_err(|e| e.to_string())
}
fn prepare_compiled(input: PreparationInput<'_>) -> Result<PreparedTurn, WorkerError> {
    let record = input.agent.value();
    let thread = snapshot::thread_snapshot(
        input.transaction,
        input.resources,
        &record,
        "corpus",
        input.state_root,
    )?;
    let (mut snapshot, events) = snapshot::submission_snapshot(
        input.transaction,
        input.resources,
        &record,
        input.intent,
        input.state_root,
    )?;
    snapshot.prepared_at = input.submitted_at;
    snapshot.operation_id = format!(
        "account:{}:{}:turn/start",
        input.intent.account_key, input.intent.attempt_id
    );
    let compiled =
        crate::turn_prepare::compile(&snapshot, &events).map_err(|e| refusal(e.to_string()))?;
    let mut params = crate::thread_prepare::turn_params(
        &thread,
        &crate::thread_prepare::TurnInput {
            events: events
                .iter()
                .map(|e| crate::thread_prepare::TurnEvent {
                    kind: e.kind.clone(),
                    text: e.text.clone(),
                })
                .collect(),
            has_assets: events.iter().any(|e| !e.metadata.assets.is_empty()),
        },
    )
    .map_err(|e| refusal(format!("thread-preparation/{e:?}")))?;
    params.insert("input".into(), json!(compiled.provider_inputs));
    params.insert(
        "clientUserMessageId".into(),
        json!(compiled.event_ids.first()),
    );
    let submission = PreparedSubmission {
        first_event_id: compiled.event_ids[0].clone(),
        submitted_at: input.submitted_at,
        created_at: input.intent.events[0].created,
        text: compiled
            .fulltext
            .clone()
            .unwrap_or_else(|| compiled.transcript.text.clone()),
        inputs: input
            .intent
            .events
            .iter()
            .zip(&compiled.transcript.inputs)
            .map(|(e, t)| SubmittedInput {
                id: e.id.clone(),
                at: e.created,
                kind: e.kind.clone(),
                text: e.text.clone(),
                assets: json!(t.assets),
            })
            .collect(),
        assets: json!(compiled.transcript.assets),
    };
    Ok(PreparedTurn {
        params: json!(params),
        submission,
        compiled,
    })
}
/// Persist compiler-owned metadata after the ordinary submission identity check.
/// Kept public for lifecycle boundary replay; production uses the guarded worker.
pub fn persist_prepared_compilation(
    tx: &studio_runtime_store::Transaction<'_>,
    compiled: &crate::turn_prepare::PreparedSubmission,
) -> Result<(), WorkerError> {
    pipeline::persist_compilation(tx, compiled)
}
