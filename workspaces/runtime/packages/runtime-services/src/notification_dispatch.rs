//! Transactional provider notifications with replay-safe receipts and checked stored state.

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize, Serializer, ser::SerializeMap};
use serde_json::{Value, json};
use studio_runtime_providers::Frame;
use studio_runtime_store::CachedSql;
use studio_runtime_store::{
    MonitorRecord, NotificationDispatchRepository, RuntimeStore, StoreError,
    SyncDocumentsRepository, Table, Transaction, sha256_hex,
};
use thiserror::Error;

#[path = "notification_dispatch/effects.rs"]
mod effects;
#[path = "notification_dispatch/items.rs"]
mod items;
#[path = "notification_dispatch/kinds.rs"]
mod kinds;
#[path = "notification_dispatch/notices.rs"]
pub(crate) mod notices;
#[path = "notification_dispatch/questions.rs"]
mod questions;
#[path = "notification_dispatch/snapshot.rs"]
mod snapshot;
pub use effects::{
    NotificationDependency, NotificationEffects, NotificationHourMetrics, NotificationObservation,
    NotificationPostCommit, NotificationPublicationBatch, NotificationStreamMetrics,
    RecoveredNotificationObservation, SharedJson, UnavailableNotificationEffects,
};

#[path = "notification_dispatch/stream.rs"]
mod stream;
#[path = "notification_dispatch/submission.rs"]
mod submission;
pub use stream::NotificationStreamBuffer;
use stream::{EnqueueResult, StreamKey};
use submission::{submission_record, validate_submission};

const ITEM_PREVIEW_CHARS: usize = 20_000;
const SUBMISSION_RECEIPT_PREFIX: &str = "provider-submission:";
const DELIVERY_RECEIPT_PREFIX: &str = "provider-notification:";
const SAVEPOINT: &str = "notification_dispatch_scope";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum DispatchOutcome {
    Written,
    RequiresService { service: NotificationDependency },
    Ignored { reason: String },
    Unhandled { kind: String, reason: String },
}

/// Immutable server identity, resolved before entering a writer transaction.
/// Native session identity supplied by the account worker, never reconstructed
/// from a journal generation. Replayed generations cannot acquire current controls.
#[derive(Clone)]
pub struct NativeProcessSource {
    pub account: String,
    pub connection: String,
    pub generation: Option<u64>,
    pub provider: String,
}

#[derive(Clone, Default)]
pub struct PublicationContext {
    server_id: Option<String>,
    native_source: Option<NativeProcessSource>,
}
impl PublicationContext {
    pub fn with_native_source(mut self, source: Option<NativeProcessSource>) -> Self {
        self.native_source = source;
        self
    }
    pub fn load(store: &RuntimeStore) -> Result<Self, NotificationDispatchError> {
        Ok(Self {
            server_id: store.local_server_id()?,
            native_source: None,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderDeliveryIdentity {
    pub provider_handle: String,
    pub generation: u64,
    pub cursor: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SubmittedInput {
    pub id: String,
    pub at: f64,
    pub kind: String,
    pub text: String,
    pub assets: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreparedSubmission {
    pub first_event_id: String,
    pub submitted_at: f64,
    pub created_at: f64,
    pub text: String,
    pub inputs: Vec<SubmittedInput>,
    pub assets: Value,
    pub radio_authorized: bool,
}

#[derive(Debug, Error)]
pub enum NotificationDispatchError {
    #[error("invalid provider notification or delivery identity")]
    InvalidNotification,
    #[error("notification output queue capacity reached")]
    StreamCapacity,
    #[error("invalid stored object {table}:{id}")]
    InvalidStoredObject { table: String, id: String },
    #[error("delivery identity was reused with different content")]
    ConflictingDelivery,
    #[error("submitted input identity was reused with different content")]
    ConflictingInput,
    #[error("submitted event is not authorized")]
    UnauthorizedSubmission,
    #[error("Not implemented: requires {0}")]
    Dependency(&'static str),
    #[error(transparent)]
    Store(StoreError),
    #[error(transparent)]
    Budget(#[from] crate::budget::BudgetError),
    #[error(transparent)]
    Move(#[from] crate::agent_move::MoveError),
    #[error("JSON serialization failed")]
    Json(#[from] serde_json::Error),
}

impl From<StoreError> for NotificationDispatchError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::InvalidStoredObject { table, id } => Self::InvalidStoredObject {
                table: table.into(),
                id,
            },
            StoreError::InvalidSyncObject { scope, id } => {
                Self::InvalidStoredObject { table: scope, id }
            }
            error => Self::Store(error),
        }
    }
}

/// Persist Python's submitted batch before the provider turn begins.
pub fn record_user_submission(
    transaction: &Transaction<'_>,
    agent_id: &str,
    submission: &PreparedSubmission,
) -> Result<DispatchOutcome, NotificationDispatchError> {
    in_savepoint(transaction, || {
        let id = format!("{agent_id}:{}", submission.first_event_id);
        let record = submission_record(&id, submission);
        let signature = sha256_json(&json!({
            "agent": agent_id,
            "firstEventId": submission.first_event_id,
            "submittedAt": submission.submitted_at,
            "createdAt": submission.created_at,
            "record": record,
            "text": submission.text,
            "inputTexts": submission.inputs.iter().map(|input| &input.text).collect::<Vec<_>>(),
        }))?;
        let receipt_id = format!(
            "{SUBMISSION_RECEIPT_PREFIX}{}",
            sha256_hex(format!("{agent_id}:{}", submission.first_event_id).as_bytes())
        );
        if let Some(receipt) =
            NotificationDispatchRepository::notification_receipt(transaction, &receipt_id)?
        {
            if receipt.signature != signature {
                return Err(NotificationDispatchError::ConflictingInput);
            }
            return decode_outcome(&receipt.result, &receipt_id);
        }
        validate_submission(transaction, agent_id, submission)?;
        if let Some((existing, created)) =
            NotificationDispatchRepository::transcript_item(transaction, agent_id, &id)?
        {
            let existing_text = saved_text(transaction, &id, &existing)?;
            if existing != record
                || existing_text != submission.text
                || created != submission.created_at
            {
                return Err(NotificationDispatchError::ConflictingInput);
            }
            return save_outcome(
                transaction,
                &receipt_id,
                &signature,
                DispatchOutcome::Written,
            );
        }

        NotificationDispatchRepository::put_transcript_item(
            transaction,
            &id,
            agent_id,
            &record,
            submission.created_at,
            &submission.text,
            true,
        )?;
        for input in &submission.inputs {
            NotificationDispatchRepository::link_transcript_item_to_event(
                transaction,
                agent_id,
                &input.id,
                &id,
            )?;
        }
        save_outcome(
            transaction,
            &receipt_id,
            &signature,
            DispatchOutcome::Written,
        )
    })
}

/// Transaction-aware read/respond service for model-authored complaints.
pub(crate) fn agent_complaint_action(
    tx: &Transaction<'_>,
    agent: &Value,
    args: &Value,
    request_id: &str,
    now: f64,
) -> Result<Value, NotificationDispatchError> {
    questions::agent_complaint_action(tx, agent, args, request_id, now)
}

/// A lead's message to the user (Python `codex_user_messages.send_to_user`).
pub(crate) fn submit_user_message(
    tx: &Transaction<'_>,
    lead: &Value,
    request_id: &str,
    text: &str,
    now: f64,
) -> Result<Value, NotificationDispatchError> {
    questions::submit_user_message(tx, lead, request_id, text, now)
}

/// Stored outcome and a staged connection-buffer replacement.
#[derive(Clone, Debug, PartialEq)]
pub struct NotificationDispatchResult {
    pub outcome: DispatchOutcome,
    pub stream_update: Option<NotificationStreamBuffer>,
    /// Execute only after the caller commits. Discard on rollback/commit failure.
    /// Replay returns an empty list, including after a lost acknowledgement.
    pub post_commit: NotificationPublicationBatch,
}

/// The caller owns the transaction and the connection's stream buffer. Commit
/// SQLite first, then apply `stream_update` (when present) to that buffer. On
/// rollback or commit failure discard the update. This call never mutates the
/// supplied buffer; a saved receipt returns the saved outcome and no update.
/// `observed_at` is never part of delivery identity.
pub fn dispatch_notification(
    transaction: &Transaction<'_>,
    agent_id: &str,
    publication: &PublicationContext,
    streams: &NotificationStreamBuffer,
    identity: &ProviderDeliveryIdentity,
    notification: &Frame,
    observed_at: f64,
) -> Result<NotificationDispatchResult, NotificationDispatchError> {
    dispatch_notification_with_effects(
        transaction,
        agent_id,
        publication,
        streams,
        identity,
        notification,
        observed_at,
        &UnavailableNotificationEffects,
    )
}

/// Dispatch with transactional budget and move-cache owners supplied by the worker.
// Preserve the existing dispatcher signature, adding only the injected effect owner.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_notification_with_effects(
    transaction: &Transaction<'_>,
    agent_id: &str,
    publication: &PublicationContext,
    streams: &NotificationStreamBuffer,
    identity: &ProviderDeliveryIdentity,
    notification: &Frame,
    observed_at: f64,
    effects: &dyn NotificationEffects,
) -> Result<NotificationDispatchResult, NotificationDispatchError> {
    let mut next_streams = streams.clone();
    let (kind, params) = notification_parts(notification)?;
    let mut before = None;
    // The stored agent after this dispatch when known without reading it back.
    let mut after_known = AfterAgent::Unknown;
    let mut fresh = false;
    let mut post_commit = NotificationPublicationBatch::default();
    let mut command_output_monitor_present = None;
    let result = in_savepoint(transaction, || {
        let outcome = (|| {
            if !observed_at.is_finite()
                || observed_at < 0.0
                || identity
                    .provider_handle
                    .strip_prefix("account:")
                    .is_none_or(str::is_empty)
                || identity.generation == 0
                || identity.cursor == 0
            {
                return Err(NotificationDispatchError::InvalidNotification);
            }
            let content_signature = notification_content_signature(agent_id, notification)?;
            let identity_signature = sha256_json(&json!({
                "providerHandle": identity.provider_handle,
                "generation": identity.generation,
                "cursor": identity.cursor,
            }))?;
            let receipt_id = format!("{DELIVERY_RECEIPT_PREFIX}{identity_signature}");
            if let Some(receipt) =
                NotificationDispatchRepository::notification_receipt(transaction, &receipt_id)?
            {
                if receipt.signature != content_signature {
                    return Err(NotificationDispatchError::ConflictingDelivery);
                }
                return decode_outcome(&receipt.result, &receipt_id);
            }

            fresh = true;
            if let Some(notice) = notices::NoticeKind::parse(kind)
                && notice.is_account_notice()
                && params["threadId"].as_str().is_none_or(str::is_empty)
            {
                effects.account_notice(transaction, kind, params, identity, observed_at)?;
                return save_outcome(
                    transaction,
                    &receipt_id,
                    &content_signature,
                    DispatchOutcome::Written,
                );
            }
            if kind == "provider/generationStarted" {
                let Some(agent) =
                    NotificationDispatchRepository::agent_record(transaction, agent_id)?
                else {
                    // Python applies generationStarted to the account/thread rate
                    // table and returns when no active rate entry exists. A
                    // notification can therefore be valid without an agent row,
                    // but the token-rate service is still required.
                    effects.generation_started()?;
                    return save_outcome(
                        transaction,
                        &receipt_id,
                        &content_signature,
                        DispatchOutcome::Written,
                    );
                };
                if agent.get("deletedAt").is_some_and(|value| !value.is_null()) {
                    return save_outcome(
                        transaction,
                        &receipt_id,
                        &content_signature,
                        DispatchOutcome::Ignored {
                            reason: "deleted-agent".to_owned(),
                        },
                    );
                }
                before = Some(agent);
                effects.generation_started()?;
                return save_outcome(
                    transaction,
                    &receipt_id,
                    &content_signature,
                    DispatchOutcome::Written,
                );
            }
            let mut agent = NotificationDispatchRepository::agent_record(transaction, agent_id)?
                .ok_or_else(|| stored("runtime_agents", agent_id))?;
            if agent.get("deletedAt").is_some_and(|value| !value.is_null()) {
                return save_outcome(
                    transaction,
                    &receipt_id,
                    &content_signature,
                    DispatchOutcome::Ignored {
                        reason: "deleted-agent".to_owned(),
                    },
                );
            }
            validate_agent(&agent, agent_id, &identity.provider_handle, kind, params)?;
            before = Some(agent.clone());
            // Whether a flush below changed the agent after `before` was taken.
            let mut agent_changed = false;
            if kind == "command/exec/outputDelta" {
                command_output_monitor_present = Some(
                    if let Some(process_id) = params
                        .get("processId")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                    {
                        Table::<MonitorRecord>::new(transaction)
                            .get(&process_id.to_owned())?
                            .is_some_and(|monitor| monitor.value()["status"] == "running")
                    } else {
                        false
                    },
                );
            }
            if matches!(
                kind,
                "item/agentMessage/delta" | "item/commandExecution/outputDelta"
            ) {
                let key = command_key(agent_id, identity, params)?;
                let mut key = key;
                key.method = kind.to_owned();
                for effect in effects.post_commit(
                    Some(&agent),
                    Some(&agent),
                    &notification_payload(notification),
                    identity,
                    observed_at,
                    &DispatchOutcome::Written,
                ) {
                    if let NotificationPostCommit::Analytics { observation, .. } = effect {
                        next_streams.stage_analytics(key.clone(), observation)?;
                    }
                }
            }
            let unload = kind == "thread/closed"
                || (kind == "thread/status/changed" && params["status"]["type"] == "notLoaded");
            if (unload || matches!(kind, "error" | "model/safetyBuffering/updated"))
                && let Some(thread) = params["threadId"].as_str().filter(|v| !v.is_empty())
            {
                let before = agent.clone();
                let turn = params["turnId"].as_str().filter(|v| !v.is_empty());
                let keys = next_streams
                    .selected(None)
                    .into_iter()
                    .filter(|key| {
                        key.thread == thread
                            && (unload || turn.is_none_or(|turn| key.turn.as_deref() == Some(turn)))
                    })
                    .collect();
                flush_keys(
                    transaction,
                    agent_id,
                    &mut agent,
                    &mut next_streams,
                    keys,
                    unload,
                    observed_at,
                )?;
                if agent != before {
                    persist_agent(transaction, agent_id, &agent, publication)?;
                    agent_changed = true;
                }
            }
            if let Some(key) = reasoning_stream_key(&agent, agent_id, identity, kind, params) {
                // Counted like a text delta: the agent is written at the next
                // flush, not once per delta. Its effects are the unbuffered ones.
                let outcome = match next_streams.enqueue_delta(key, "", params) {
                    EnqueueResult::Buffered => DispatchOutcome::Written,
                    EnqueueResult::Closed => DispatchOutcome::Ignored {
                        reason: "closed-stream".into(),
                    },
                    EnqueueResult::FlushRequired => {
                        unreachable!("reasoning deltas retain no text")
                    }
                };
                after_known = AfterAgent::Unchanged;
                return save_outcome(transaction, &receipt_id, &content_signature, outcome);
            }
            if kind == "item/commandExecution/outputDelta" {
                let key = command_key(agent_id, identity, params)?;
                let delta = params["delta"]
                    .as_str()
                    .ok_or(NotificationDispatchError::InvalidNotification)?;
                let outcome = match next_streams.enqueue_delta(key, delta, params) {
                    EnqueueResult::Buffered => DispatchOutcome::Written,
                    EnqueueResult::Closed => DispatchOutcome::Ignored {
                        reason: "closed-stream".into(),
                    },
                    EnqueueResult::FlushRequired => {
                        unreachable!("command output retains a bounded tail")
                    }
                };
                return save_outcome(transaction, &receipt_id, &content_signature, outcome);
            }
            if kind == "item/completed" && params["item"]["type"] != "agentMessage" {
                let mut scoped = params.to_owned();
                scoped["itemId"] = params["item"]["id"].clone();
                let key = command_key(agent_id, identity, &scoped)?;
                flush_selected(
                    transaction,
                    agent_id,
                    &mut agent,
                    &mut next_streams,
                    Some(&key),
                    observed_at,
                )?;
                agent_changed = true;
            }
            if kind == "item/started" && params["item"]["type"] == "commandExecution" {
                let mut scoped = params.to_owned();
                scoped["itemId"] = params["item"]["id"].clone();
                let key = command_key(agent_id, identity, &scoped)?;
                kinds::preserve_command_base(transaction, agent_id, &mut next_streams, &key)?;
            }
            if agent_changed {
                before = Some(agent.clone());
            }
            if let Some(outcome) = kinds::apply(
                transaction,
                &mut agent,
                kind,
                params,
                identity,
                publication,
                observed_at,
            )? {
                return save_outcome(transaction, &receipt_id, &content_signature, outcome);
            }
            let turn_id = params
                .get("turnId")
                .and_then(Value::as_str)
                .filter(|turn| !turn.is_empty());
            let stale_item = kind.starts_with("item/")
                && turn_id
                    .is_some_and(|turn| agent.get("turnId").and_then(Value::as_str) != Some(turn));

            // `kinds::apply` changes nothing for the kinds that reach here, so
            // `before` is still the stored agent.
            let outcome = match kind {
                "item/agentMessage/delta" => {
                    if stale_item {
                        DispatchOutcome::Ignored {
                            reason: "stale-turn-item".to_owned(),
                        }
                    } else {
                        match (
                            params.get("itemId").and_then(Value::as_str),
                            params.get("delta").and_then(Value::as_str),
                        ) {
                            (Some(item_id), Some(delta)) if !item_id.is_empty() => {
                                if write_text_notification(
                                    transaction,
                                    &mut agent,
                                    &mut next_streams,
                                    TextWriteInput {
                                        agent_id,
                                        account: identity
                                            .provider_handle
                                            .strip_prefix("account:")
                                            .unwrap_or_default(),
                                        generation: identity.generation,
                                        params,
                                        item_id,
                                        observed_at,
                                        update: TextUpdate::Delta(delta),
                                    },
                                )? {
                                    DispatchOutcome::Written
                                } else {
                                    DispatchOutcome::Ignored {
                                        reason: "closed-stream".to_owned(),
                                    }
                                }
                            }
                            _ => return Err(NotificationDispatchError::InvalidNotification),
                        }
                    }
                }
                "item/started" | "item/completed" => {
                    match params.get("item").and_then(Value::as_object) {
                        None => return Err(NotificationDispatchError::InvalidNotification),
                        Some(item)
                            if item.get("type").and_then(Value::as_str) != Some("agentMessage") =>
                        {
                            return Err(NotificationDispatchError::InvalidNotification);
                        }
                        Some(item)
                            if item
                                .get("id")
                                .and_then(Value::as_str)
                                .is_none_or(str::is_empty) =>
                        {
                            return Err(NotificationDispatchError::InvalidNotification);
                        }
                        Some(_) if stale_item => DispatchOutcome::Ignored {
                            reason: "stale-turn-item".to_owned(),
                        },
                        Some(item) if item.get("text").is_some_and(|text| !text.is_string()) => {
                            return Err(NotificationDispatchError::InvalidNotification);
                        }
                        Some(item) => {
                            let item_id =
                                item.get("id").and_then(Value::as_str).unwrap_or_default();
                            let snapshot = item.get("text").and_then(Value::as_str);
                            let phase = item.get("phase");
                            let update = if kind == "item/started" {
                                TextUpdate::Started(snapshot, phase)
                            } else {
                                TextUpdate::Completed(snapshot, phase)
                            };
                            if write_text_notification(
                                transaction,
                                &mut agent,
                                &mut next_streams,
                                TextWriteInput {
                                    agent_id,
                                    account: identity
                                        .provider_handle
                                        .strip_prefix("account:")
                                        .unwrap_or_default(),
                                    generation: identity.generation,
                                    params,
                                    item_id,
                                    observed_at,
                                    update,
                                },
                            )? {
                                if kind == "item/completed" && has_questions(item.get("questions"))
                                {
                                    questions::completed(transaction, &agent, params, observed_at)?;
                                }
                                DispatchOutcome::Written
                            } else {
                                DispatchOutcome::Ignored {
                                    reason: "closed-stream".to_owned(),
                                }
                            }
                        }
                    }
                }
                "thread/tokenUsage/updated" => {
                    effects.capture_usage(transaction, &mut agent, params, observed_at)?;
                    if crate::event_write::truthy(agent.get("movedFrom")) {
                        effects.capture_move_cache(transaction, &agent, params, observed_at)?;
                    }
                    apply_token_usage(&mut agent, params, observed_at)?
                }
                _ => return Err(NotificationDispatchError::InvalidNotification),
            };

            if kind == "item/agentMessage/delta"
                || matches!(outcome, DispatchOutcome::Ignored { .. })
            {
                after_known = if before.as_ref() == Some(&agent) {
                    AfterAgent::Unchanged
                } else {
                    AfterAgent::Stored(persist_agent(transaction, agent_id, &agent, publication)?)
                };
                return save_outcome(transaction, &receipt_id, &content_signature, outcome);
            }
            if !stale_item
                && !matches!(
                    outcome,
                    DispatchOutcome::Ignored { .. } | DispatchOutcome::Unhandled { .. }
                )
            {
                update_common_agent_fields(&mut agent, kind, params, observed_at)?;
            }
            if !matches!(outcome, DispatchOutcome::Ignored { .. }) {
                after_known =
                    AfterAgent::Stored(persist_agent(transaction, agent_id, &agent, publication)?);
            }
            save_outcome(transaction, &receipt_id, &content_signature, outcome)
        })()?;
        if fresh {
            let unchanged = matches!(after_known, AfterAgent::Unchanged);
            let stored = match after_known {
                AfterAgent::Stored(agent) => Some(agent),
                AfterAgent::Unknown if before.is_some() => {
                    NotificationDispatchRepository::agent_record(transaction, agent_id)?
                }
                AfterAgent::Unchanged | AfterAgent::Unknown => None,
            };
            let after = if unchanged {
                before.as_ref()
            } else {
                stored.as_ref()
            };
            // Every dispatch test witnesses that the tracked value is the row.
            #[cfg(test)]
            if before.is_some() {
                assert_eq!(
                    after,
                    NotificationDispatchRepository::agent_record(transaction, agent_id)?.as_ref(),
                    "tracked agent after {kind} differs from the stored row"
                );
            }
            if matches!(
                kind,
                "item/agentMessage/delta" | "item/commandExecution/outputDelta"
            ) && matches!(&outcome, DispatchOutcome::Ignored { reason } if reason == "stale-turn-item")
            {
                let mut key = command_key(agent_id, identity, params)?;
                key.method = kind.to_owned();
                if let Some(agent) = &before {
                    next_streams.flush_analytics(&key, agent);
                }
            }
            let mut current = effects.post_commit(
                before.as_ref(),
                after,
                &notification_payload(notification),
                identity,
                observed_at,
                &outcome,
            );
            if command_output_monitor_present == Some(false) {
                current
                    .retain(|effect| !matches!(effect, NotificationPostCommit::Analytics { .. }));
            }
            if matches!(
                kind,
                "item/agentMessage/delta" | "item/commandExecution/outputDelta"
            ) {
                current
                    .retain(|effect| !matches!(effect, NotificationPostCommit::Analytics { .. }));
            }
            for effect in &mut current {
                if let NotificationPostCommit::TokenRateAssociation(observation) = effect {
                    enrich_association(transaction, agent_id, observation)?;
                }
            }
            for effect in current {
                next_streams.queue_effect(effect);
            }
            post_commit = next_streams.take_post_commit()?;
        }
        Ok(outcome)
    });
    result.map(|outcome| NotificationDispatchResult {
        outcome,
        // The cloned buffer is a shallow transactional snapshot. Any mutated
        // entries have detached; a fresh receipt can safely return it without
        // comparing every retained stream tree to find changed entries.
        stream_update: (fresh && next_streams.changed_from(streams)).then_some(next_streams),
        post_commit,
    })
}

/// Store one task record through the canonical task writer, which also keeps
/// the task's sync window. Its caller owns the decision and the transaction.
pub(crate) fn put_task_record(
    tx: &Transaction<'_>,
    task: &Value,
) -> Result<(), NotificationDispatchError> {
    kinds::put_task(tx, task)
}

/// Shared completion formatter for recovery; its caller owns the recovery receipt.
/// No provider cursor is fabricated and no commit/publication happens here.
pub fn recover_task_in_transaction(
    tx: &Transaction<'_>,
    agent: &Value,
    previous: &Value,
    item: &Value,
    at: f64,
) -> Result<(), NotificationDispatchError> {
    in_savepoint(tx, || {
        if !at.is_finite()
            || at < 0.0
            || previous["agent"] != agent["id"]
            || previous["itemId"] != item["id"]
            || previous["type"] != item["type"]
        {
            return Err(NotificationDispatchError::InvalidNotification);
        }
        use studio_runtime_store::{Table, TaskId, TaskRecord};
        let task_id = previous["id"]
            .as_str()
            .ok_or(NotificationDispatchError::InvalidNotification)?;
        let current = Table::<TaskRecord>::new(tx).get(&TaskId::from(task_id))?;
        let stored_agent = NotificationDispatchRepository::agent_record(
            tx,
            agent["id"]
                .as_str()
                .ok_or(NotificationDispatchError::InvalidNotification)?,
        )?;
        if current.as_ref().map(|record| record.value()) != Some(previous)
            || stored_agent.as_ref() != Some(agent)
        {
            return Err(NotificationDispatchError::InvalidNotification);
        }
        let snapshot = snapshot::NotificationSnapshot::from_records(
            agent.clone(),
            Some(previous.clone()),
            None,
        )?;
        if previous["status"] != "running" {
            return Ok(());
        }
        let params = json!({"item":item,"turnId":previous["turnId"]});
        if let Some(task) = items::task_transition(
            snapshot.agent(),
            "item/completed",
            &params,
            snapshot.task(),
            true,
            at,
        )? {
            kinds::put_task(tx, &task)?;
            kinds::write_historical_item(
                tx,
                agent["id"]
                    .as_str()
                    .ok_or(NotificationDispatchError::InvalidNotification)?,
                &params,
                &task,
                at,
            )?;
        }
        Ok(())
    })
}

impl NotificationStreamBuffer {
    /// Canonical recovered completion without a fabricated delivery or receipt.
    /// The recovery closure owns this retained/staged account buffer. Persist the
    /// mutated agent and returned buffer with terminal settlement, then install
    /// the buffer and publish its ordered batch only after commit.
    #[allow(dead_code, clippy::too_many_arguments)] // wired by the recovery integrator
    pub(crate) fn recover_agent_message_in_transaction(
        &self,
        tx: &Transaction<'_>,
        agent: &mut Value,
        account: &str,
        generation: u64,
        turn: &Value,
        item: &Value,
        at: f64,
    ) -> Result<Option<NotificationStreamBuffer>, NotificationDispatchError> {
        let mut next = self.clone();
        let mut staged_agent = agent.clone();
        in_savepoint(tx, || {
            let agent_id = agent["id"]
                .as_str()
                .ok_or(NotificationDispatchError::InvalidNotification)?;
            let item_id = item["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or(NotificationDispatchError::InvalidNotification)?;
            if !at.is_finite()
                || at < 0.0
                || generation == 0
                || account.is_empty()
                || item["type"] != "agentMessage"
                || turn["id"].as_str().is_none_or(str::is_empty)
                || agent["turnId"] != turn["id"]
                || item.get("text").is_some_and(|text| !text.is_string())
            {
                return Err(NotificationDispatchError::InvalidNotification);
            }
            let params = json!({"threadId":agent["threadId"],"turnId":turn["id"],"item":item});
            validate_agent(
                agent,
                agent_id,
                &format!("account:{account}"),
                "item/completed",
                &params,
            )?;
            write_text_notification(
                tx,
                &mut staged_agent,
                &mut next,
                TextWriteInput {
                    agent_id,
                    account,
                    generation,
                    params: &params,
                    item_id,
                    observed_at: at,
                    update: TextUpdate::Completed(
                        item.get("text").and_then(Value::as_str),
                        item.get("phase"),
                    ),
                },
            )?;
            if has_questions(item.get("questions")) {
                questions::completed(tx, &staged_agent, &params, at)?;
            }
            update_common_agent_fields(&mut staged_agent, "item/completed", &params, at)?;
            Ok(())
        })?;
        *agent = staged_agent;
        Ok((next != *self).then_some(next))
    }
}

fn command_key(
    agent_id: &str,
    identity: &ProviderDeliveryIdentity,
    params: &Value,
) -> Result<StreamKey, NotificationDispatchError> {
    Ok(StreamKey {
        agent_id: agent_id.to_owned(),
        account: identity
            .provider_handle
            .strip_prefix("account:")
            .ok_or(NotificationDispatchError::InvalidNotification)?
            .into(),
        generation: identity.generation,
        thread: params["threadId"]
            .as_str()
            .ok_or(NotificationDispatchError::InvalidNotification)?
            .into(),
        turn: params["turnId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        item: params["itemId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or(NotificationDispatchError::InvalidNotification)?
            .into(),
        method: "item/commandExecution/outputDelta".into(),
    })
}

#[derive(Clone, Copy)]
enum TextUpdate<'a> {
    Started(Option<&'a str>, Option<&'a Value>),
    Delta(&'a str),
    Completed(Option<&'a str>, Option<&'a Value>),
}

struct TextWriteInput<'a> {
    agent_id: &'a str,
    account: &'a str,
    generation: u64,
    params: &'a Value,
    item_id: &'a str,
    observed_at: f64,
    update: TextUpdate<'a>,
}

fn saved_text(
    transaction: &Transaction<'_>,
    id: &str,
    record: &Value,
) -> Result<String, NotificationDispatchError> {
    if record.get("truncated").and_then(Value::as_bool) == Some(true) {
        return NotificationDispatchRepository::full_text(transaction, id, record)?
            .ok_or_else(|| stored("runtime_items", id));
    }
    record
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| stored("runtime_items", id))
}

fn notification_parts(notification: &Frame) -> Result<(&str, &Value), NotificationDispatchError> {
    static NULL: Value = Value::Null;
    let (kind, params) = match notification {
        Frame::Notification(frame) => (
            frame.method.as_str(),
            frame.params.as_ref().unwrap_or(&NULL),
        ),
        Frame::UnknownMethod(frame) => (
            frame.method.as_str(),
            frame.fields.get("params").unwrap_or(&NULL),
        ),
        Frame::Request(_) | Frame::Response { .. } => {
            return Err(NotificationDispatchError::InvalidNotification);
        }
    };
    Ok((kind, params))
}

fn notification_payload(notification: &Frame) -> Value {
    let (method, params, mut fields, include_params) = match notification {
        Frame::Notification(frame) => (
            frame.method.as_str(),
            frame.params.as_ref(),
            frame.fields.clone(),
            true,
        ),
        Frame::UnknownMethod(frame) => (
            frame.method.as_str(),
            frame.fields.get("params"),
            frame.fields.clone(),
            false,
        ),
        Frame::Request(_) | Frame::Response { .. } => return Value::Null,
    };
    fields.insert("method".to_owned(), Value::String(method.to_owned()));
    if include_params && let Some(params) = params.filter(|params| !params.is_null()) {
        fields.insert("params".to_owned(), params.clone());
    }
    Value::Object(fields)
}

#[derive(Clone, Copy)]
enum NotificationValue<'a> {
    Json(&'a Value),
    Text(&'a str),
}

impl Serialize for NotificationValue<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Json(value) => value.serialize(serializer),
            Self::Text(value) => serializer.serialize_str(value),
        }
    }
}

struct BorrowedNotification<'a> {
    entries: Vec<(&'a str, NotificationValue<'a>)>,
}

impl<'a> BorrowedNotification<'a> {
    fn new(notification: &'a Frame) -> Option<Self> {
        let (fields, method, params) = match notification {
            Frame::Notification(frame) => (
                &frame.fields,
                frame.method.as_str(),
                frame.params.as_ref().filter(|value| !value.is_null()),
            ),
            Frame::UnknownMethod(frame) => (
                &frame.fields,
                frame.method.as_str(),
                frame.fields.get("params").filter(|value| !value.is_null()),
            ),
            Frame::Request(_) | Frame::Response { .. } => return None,
        };
        let mut entries = fields
            .iter()
            .map(|(key, value)| (key.as_str(), NotificationValue::Json(value)))
            .collect::<Vec<_>>();
        Self::set(&mut entries, "method", NotificationValue::Text(method));
        if let Some(params) = params {
            Self::set(&mut entries, "params", NotificationValue::Json(params));
        }
        Some(Self { entries })
    }

    fn set(
        entries: &mut Vec<(&'a str, NotificationValue<'a>)>,
        key: &'a str,
        value: NotificationValue<'a>,
    ) {
        match entries.binary_search_by(|(existing, _)| existing.cmp(&key)) {
            Ok(index) => entries[index].1 = value,
            Err(index) => entries.insert(index, (key, value)),
        }
    }
}

impl Serialize for BorrowedNotification<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.entries.len()))?;
        for (key, value) in &self.entries {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

struct NotificationContentSignature<'a> {
    agent: &'a str,
    notification: BorrowedNotification<'a>,
}

impl Serialize for NotificationContentSignature<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("agent", self.agent)?;
        map.serialize_entry("notification", &self.notification)?;
        map.end()
    }
}

fn notification_content_signature(
    agent: &str,
    notification: &Frame,
) -> Result<String, NotificationDispatchError> {
    let payload = BorrowedNotification::new(notification)
        .ok_or(NotificationDispatchError::InvalidNotification)?;
    let bytes = serde_json::to_vec(&NotificationContentSignature {
        agent,
        notification: payload,
    })?;
    Ok(sha256_hex(&bytes))
}

fn validate_agent(
    agent: &Value,
    agent_id: &str,
    provider_handle: &str,
    kind: &str,
    params: &Value,
) -> Result<(), NotificationDispatchError> {
    validate_agent_write_fields(agent, agent_id)?;
    let thread = agent
        .get("threadId")
        .and_then(Value::as_str)
        .filter(|thread| !thread.is_empty())
        .ok_or_else(|| stored("runtime_agents", agent_id))?;
    let notification_thread = params.get("threadId").and_then(Value::as_str).or_else(|| {
        params
            .get("thread")
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str)
    });
    if matches!(
        kind,
        "item/agentMessage/delta" | "item/started" | "item/completed" | "thread/tokenUsage/updated"
    ) && notification_thread.is_none_or(str::is_empty)
    {
        return Err(NotificationDispatchError::InvalidNotification);
    }
    if notification_thread.is_some_and(|actual| actual != thread) {
        return Err(NotificationDispatchError::InvalidNotification);
    }
    let account = match agent.get("accountKey") {
        None => "default",
        Some(Value::String(account)) if !account.is_empty() => account,
        Some(_) => return Err(stored("runtime_agents", agent_id)),
    };
    if provider_handle != format!("account:{account}") {
        return Err(NotificationDispatchError::InvalidNotification);
    }
    Ok(())
}

fn validate_agent_write_fields(
    agent: &Value,
    agent_id: &str,
) -> Result<(), NotificationDispatchError> {
    if agent.get("id").and_then(Value::as_str) != Some(agent_id)
        || agent
            .get("epoch")
            .and_then(Value::as_i64)
            .is_none_or(|epoch| epoch < 0)
        || agent
            .get("rootId")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || agent.get("status").and_then(Value::as_str).is_none()
        || agent.get("events").and_then(Value::as_u64).is_none()
    {
        return Err(stored("runtime_agents", agent_id));
    }
    if agent
        .get("unhandledNotifications")
        .is_some_and(|counts| !counts.is_object())
    {
        return Err(stored("runtime_agents", agent_id));
    }
    if let Some(counts) = agent
        .get("unhandledNotifications")
        .and_then(Value::as_object)
    {
        for value in counts.values() {
            if value.get("count").and_then(Value::as_u64).is_none()
                || value
                    .get("reasons")
                    .is_some_and(|reasons| !reasons.is_object())
            {
                return Err(stored("runtime_agents", agent_id));
            }
            if let Some(reasons) = value.get("reasons").and_then(Value::as_object)
                && reasons.values().any(|count| count.as_u64().is_none())
            {
                return Err(stored("runtime_agents", agent_id));
            }
        }
    }
    Ok(())
}

fn write_text_notification(
    transaction: &Transaction<'_>,
    agent: &mut Value,
    streams: &mut NotificationStreamBuffer,
    input: TextWriteInput<'_>,
) -> Result<bool, NotificationDispatchError> {
    let TextWriteInput {
        agent_id,
        account,
        generation,
        params,
        item_id,
        observed_at,
        update,
    } = input;
    let id = format!("{agent_id}:{item_id}");
    let thread = params
        .get("threadId")
        .and_then(Value::as_str)
        .ok_or(NotificationDispatchError::InvalidNotification)?;
    let turn = params
        .get("turnId")
        .and_then(Value::as_str)
        .filter(|turn| !turn.is_empty())
        .map(str::to_owned);
    let key = StreamKey {
        agent_id: agent_id.to_owned(),
        account: account.to_owned(),
        generation,
        thread: thread.to_owned(),
        turn: turn.clone(),
        item: item_id.to_owned(),
        method: "item/agentMessage/delta".to_owned(),
    };
    if let TextUpdate::Delta(delta) = update {
        return match streams.enqueue_delta(key.clone(), delta, params) {
            EnqueueResult::Buffered => Ok(true),
            EnqueueResult::FlushRequired => {
                flush_text_stream(
                    transaction,
                    agent_id,
                    agent,
                    streams,
                    &key,
                    observed_at,
                    Some(delta),
                )?;
                Ok(true)
            }
            EnqueueResult::Closed => Ok(false),
        };
    }
    if matches!(update, TextUpdate::Started(..)) && streams.awaits_base(&key) {
        // The snapshot replaces the flushed text; later flushes continue the
        // flushed text, so keep it as the stream's durable base.
        let flushed = NotificationDispatchRepository::transcript_item(transaction, agent_id, &id)?
            .map(|(record, _)| saved_text(transaction, &id, &record))
            .transpose()?
            .unwrap_or_default();
        streams.preserve_base(&key, sha256_hex(flushed.as_bytes()));
        NotificationDispatchRepository::put_stream_base(
            transaction,
            &stream_base_id(&key),
            Some(&flushed),
        )?;
    }
    let (text, streaming, phase, index_search, completed) = match update {
        TextUpdate::Started(snapshot, phase) => (
            snapshot.unwrap_or_default().to_owned(),
            true,
            phase.cloned().unwrap_or(Value::Null),
            true,
            false,
        ),
        TextUpdate::Delta(_) => unreachable!("delta returned above"),
        TextUpdate::Completed(snapshot, phase) => {
            flush_selected(
                transaction,
                agent_id,
                agent,
                streams,
                Some(&key),
                observed_at,
            )?;
            // The flush above leaves the stored text current; read it only
            // when the provider sent no completed snapshot.
            let text = match snapshot.filter(|text| !text.is_empty()) {
                Some(text) => text.to_owned(),
                None => {
                    NotificationDispatchRepository::transcript_item(transaction, agent_id, &id)?
                        .map(|(record, _)| saved_text(transaction, &id, &record))
                        .transpose()?
                        .unwrap_or_default()
                }
            };
            (
                text,
                false,
                phase.cloned().unwrap_or(Value::Null),
                true,
                true,
            )
        }
    };
    let turn = turn.or_else(|| {
        agent
            .get("turnId")
            .and_then(Value::as_str)
            .filter(|turn| !turn.is_empty())
            .map(str::to_owned)
    });
    let record = assistant_item_record(&id, &text, observed_at, streaming, turn.as_deref(), phase);
    NotificationDispatchRepository::put_transcript_item(
        transaction,
        &id,
        agent_id,
        &record,
        observed_at,
        &text,
        index_search,
    )?;
    if completed {
        agent["activity"] = json!({"phase":"thinking", "at":observed_at});
        agent["lastAnswer"] = Value::String(suffix(&text, 16_000));
        agent["tail"] = Value::String(suffix(&text, 300));
    } else {
        agent["activity"] = json!({"phase":"writing", "at":observed_at});
    }
    Ok(true)
}

/// Flush this agent's streams from the shared connection buffer. Other agents'
/// entries remain untouched; callers flush each affected agent. Commit, then replace it with
/// the returned value; discard it on rollback. `None` means no state changed.
pub fn flush_notification_streams(
    transaction: &Transaction<'_>,
    agent_id: &str,
    publication: &PublicationContext,
    streams: &NotificationStreamBuffer,
    observed_at: f64,
) -> Result<Option<NotificationStreamBuffer>, NotificationDispatchError> {
    let mut next = streams.clone();
    in_savepoint(transaction, || {
        let mut agent = NotificationDispatchRepository::agent_record(transaction, agent_id)?
            .ok_or_else(|| stored("runtime_agents", agent_id))?;
        validate_agent_write_fields(&agent, agent_id)?;
        let before = agent.clone();
        flush_selected(
            transaction,
            agent_id,
            &mut agent,
            &mut next,
            None,
            observed_at,
        )?;
        if agent != before {
            persist_agent(transaction, agent_id, &agent, publication)?;
        }
        Ok(next.changed_from(streams).then_some(next))
    })
}

pub(crate) fn prune_inactive_streams(
    transaction: &Transaction<'_>,
    streams: &mut NotificationStreamBuffer,
    observed_at: f64,
    recovering: bool,
) -> Result<(), NotificationDispatchError> {
    for key in streams.selected(None) {
        let active: bool = transaction.query_row_cached(
            "SELECT EXISTS(SELECT 1 FROM runtime_agents WHERE account_key=?1 AND thread_id=?2 AND (?3 IS NULL OR turn_id=?3) AND deleted_at IS NULL AND (in_flight=1 OR status IN ('running','starting','approval')))",
            rusqlite::params![key.account,key.thread,key.turn], |row| row.get(0),
        ).map_err(StoreError::from)?;
        if active && recovering && !streams.has_pending(&key) && streams.has_base(&key) {
            let owner = stream_owner(transaction, &key)?;
            if let Some((id, raw)) = owner {
                let mut projection: Value =
                    serde_json::from_str(&raw).map_err(|_| stored("runtime_agents", &id))?;
                projection["turnId"] = json!(key.turn);
                projection["deletedAt"] = Value::Null;
                flush_one_stream(
                    transaction,
                    &id,
                    &mut projection,
                    streams,
                    &key,
                    observed_at,
                    false,
                )?;
            }
            continue;
        }
        if !active {
            if let Some((id, raw)) = stream_owner(transaction, &key)? {
                let mut projection: Value =
                    serde_json::from_str(&raw).map_err(|_| stored("runtime_agents", &id))?;
                projection["turnId"] = json!(key.turn);
                projection["deletedAt"] = Value::Null;
                flush_one_stream(
                    transaction,
                    &id,
                    &mut projection,
                    streams,
                    &key,
                    observed_at,
                    true,
                )?;
                continue;
            }
            NotificationDispatchRepository::put_stream_base(
                transaction,
                &stream_base_id(&key),
                None,
            )?;
            streams.close(&key);
        }
    }
    Ok(())
}

fn stream_owner(
    transaction: &Transaction<'_>,
    key: &StreamKey,
) -> Result<Option<(String, String)>, NotificationDispatchError> {
    let owner = if key.agent_id.is_empty() {
        transaction
            .query_row_cached(
                "SELECT id,record FROM runtime_agents WHERE account_key=?1 AND thread_id=?2",
                rusqlite::params![key.account, key.thread],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
    } else {
        transaction
            .query_row_cached(
                "SELECT id,record FROM runtime_agents WHERE id=?1 AND account_key=?2",
                rusqlite::params![key.agent_id, key.account],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
    };
    let owner = owner.map_err(StoreError::from)?;
    Ok(owner
        .map(|(id, raw)| {
            let raw = studio_runtime_store::agent_volatile::merged_text(transaction, &id, raw)?;
            Ok::<_, StoreError>((id, raw))
        })
        .transpose()?)
}

fn flush_one_stream(
    transaction: &Transaction<'_>,
    agent_id: &str,
    agent: &mut Value,
    streams: &mut NotificationStreamBuffer,
    key: &StreamKey,
    observed_at: f64,
    close: bool,
) -> Result<(), NotificationDispatchError> {
    if key.method == "item/commandExecution/outputDelta" {
        kinds::flush_command(
            transaction,
            agent_id,
            agent,
            streams,
            key,
            close,
            observed_at,
        )?;
    } else if REASONING_DELTA_METHODS.contains(&key.method.as_str()) {
        flush_reasoning_stream(agent, streams, key, observed_at)?;
    } else {
        flush_text_stream(
            transaction,
            agent_id,
            agent,
            streams,
            key,
            observed_at,
            None,
        )?;
    }
    if close {
        NotificationDispatchRepository::put_stream_base(transaction, &stream_base_id(key), None)?;
        streams.close(key);
    }
    Ok(())
}

/// Codex reasoning deltas carry no stored text; each one only counts as an
/// agent event. Like a Claude thinking delta (an agentMessage delta), a delta
/// of the current turn is buffered and counted at the next flush.
const REASONING_DELTA_METHODS: [&str; 2] = [
    "item/reasoning/textDelta",
    "item/reasoning/summaryTextDelta",
];

/// The stream of a reasoning delta that can be buffered: current turn, an
/// item id, and no retry status that the delta clears.
fn reasoning_stream_key(
    agent: &Value,
    agent_id: &str,
    identity: &ProviderDeliveryIdentity,
    kind: &str,
    params: &Value,
) -> Option<StreamKey> {
    #[derive(serde::Deserialize)]
    struct Delta<'a> {
        #[serde(rename = "turnId", borrow, default)]
        turn_id: Option<&'a str>,
        #[serde(rename = "itemId", borrow)]
        item_id: &'a str,
    }
    if !REASONING_DELTA_METHODS.contains(&kind) {
        return None;
    }
    let delta = Delta::deserialize(params).ok()?;
    let current_turn = agent.get("turnId").and_then(Value::as_str);
    if delta.item_id.is_empty()
        || delta
            .turn_id
            .is_some_and(|turn| !turn.is_empty() && current_turn != Some(turn))
        || agent["nativeStatus"]["phase"] == "retrying"
    {
        return None;
    }
    let mut key = command_key(agent_id, identity, params).ok()?;
    key.method = kind.to_owned();
    Some(key)
}

/// Count the buffered reasoning deltas of `key` as agent events.
fn flush_reasoning_stream(
    agent: &mut Value,
    streams: &mut NotificationStreamBuffer,
    key: &StreamKey,
    observed_at: f64,
) -> Result<(), NotificationDispatchError> {
    if let Some((_, count, params)) = streams.flush(key, "") {
        update_common_agent_fields(agent, &key.method, &params, observed_at)?;
        agent["events"] = Value::from(
            agent["events"]
                .as_u64()
                .unwrap_or_default()
                .saturating_add(count.saturating_sub(1)),
        );
    }
    Ok(())
}

fn stream_base_id(key: &StreamKey) -> String {
    format!(
        "account-worker:stream-base:{}",
        sha256_hex(
            serde_json::to_string(key)
                .expect("stream key serialization")
                .as_bytes()
        )
    )
}

fn durable_stream_base(
    transaction: &Transaction<'_>,
    streams: &NotificationStreamBuffer,
    key: &StreamKey,
) -> Result<Option<String>, NotificationDispatchError> {
    let Some(hash) = streams.base(key) else {
        return Ok(None);
    };
    let id = stream_base_id(key);
    let raw = NotificationDispatchRepository::stream_base(transaction, &id)?
        .ok_or_else(|| stored("runtime_operation_receipts", &id))?;
    let text: String =
        serde_json::from_str(&raw).map_err(|_| stored("runtime_operation_receipts", &id))?;
    if sha256_hex(text.as_bytes()) != hash {
        return Err(stored("runtime_operation_receipts", &id));
    }
    Ok(Some(text))
}

fn flush_selected(
    transaction: &Transaction<'_>,
    agent_id: &str,
    agent: &mut Value,
    streams: &mut NotificationStreamBuffer,
    terminal: Option<&StreamKey>,
    observed_at: f64,
) -> Result<(), NotificationDispatchError> {
    let keys = streams.selected(terminal);
    flush_keys(
        transaction,
        agent_id,
        agent,
        streams,
        keys,
        terminal.is_some(),
        observed_at,
    )
}

fn flush_keys(
    transaction: &Transaction<'_>,
    agent_id: &str,
    agent: &mut Value,
    streams: &mut NotificationStreamBuffer,
    keys: Vec<StreamKey>,
    close: bool,
    observed_at: f64,
) -> Result<(), NotificationDispatchError> {
    for key in keys {
        if agent.get("threadId").and_then(Value::as_str) != Some(key.thread.as_str())
            || agent
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
                != key.account
        {
            continue;
        }
        if agent.get("deletedAt").is_none_or(Value::is_null)
            && key.method != "item/commandExecution/outputDelta"
        {
            streams.flush_analytics(&key, agent);
        }
        if agent.get("deletedAt").is_some_and(|v| !v.is_null())
            || (key.method != "item/commandExecution/outputDelta"
                && key
                    .turn
                    .as_deref()
                    .is_some_and(|turn| agent.get("turnId").and_then(Value::as_str) != Some(turn)))
        {
            NotificationDispatchRepository::put_stream_base(
                transaction,
                &stream_base_id(&key),
                None,
            )?;
            streams.discard(&key);
            if close {
                streams.close(&key);
            }
            continue;
        }
        if key.method == "item/commandExecution/outputDelta" {
            kinds::flush_command(
                transaction,
                agent_id,
                agent,
                streams,
                &key,
                close,
                observed_at,
            )?;
            continue;
        }
        if REASONING_DELTA_METHODS.contains(&key.method.as_str()) {
            flush_reasoning_stream(agent, streams, &key, observed_at)?;
            if close {
                streams.close(&key);
            }
            continue;
        }
        flush_text_stream(
            transaction,
            agent_id,
            agent,
            streams,
            &key,
            observed_at,
            None,
        )?;
        if close {
            NotificationDispatchRepository::put_stream_base(
                transaction,
                &stream_base_id(&key),
                None,
            )?;
            streams.close(&key);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn flush_text_stream(
    transaction: &Transaction<'_>,
    agent_id: &str,
    agent: &mut Value,
    streams: &mut NotificationStreamBuffer,
    key: &StreamKey,
    observed_at: f64,
    delta: Option<&str>,
) -> Result<(), NotificationDispatchError> {
    streams.flush_analytics(key, agent);
    if delta.is_none() && !streams.has_pending(key) {
        return Ok(());
    }
    let id = format!("{agent_id}:{}", key.item);
    let old = NotificationDispatchRepository::transcript_item(transaction, agent_id, &id)?;
    // The stored item holds the flushed text unless item/started replaced it,
    // which preserved the flushed text as the durable base first.
    let base = durable_stream_base(transaction, streams, key)?;
    let previous = match base {
        Some(base) => {
            NotificationDispatchRepository::put_stream_base(
                transaction,
                &stream_base_id(key),
                None,
            )?;
            base
        }
        None => old
            .as_ref()
            .map(|(record, _)| saved_text(transaction, &id, record))
            .transpose()?
            .unwrap_or_default(),
    };
    if let Some((text, count, params)) = streams.flush_with_delta(key, &previous, delta) {
        let phase = old
            .as_ref()
            .and_then(|(record, _)| record.get("phase"))
            .cloned()
            .unwrap_or(Value::Null);
        let turn = key
            .turn
            .as_deref()
            .or_else(|| agent.get("turnId").and_then(Value::as_str));
        let record = assistant_item_record(&id, &text, observed_at, true, turn, phase);
        NotificationDispatchRepository::put_transcript_item(
            transaction,
            &id,
            agent_id,
            &record,
            observed_at,
            &text,
            // Search indexes the item at item/started and item/completed only.
            false,
        )?;
        update_common_agent_fields(agent, "item/agentMessage/delta", &params, observed_at)?;
        agent["events"] = Value::from(
            agent["events"]
                .as_u64()
                .unwrap_or_default()
                .saturating_add(count.saturating_sub(1)),
        );
        agent["activity"] = json!({"phase":"writing", "at":observed_at});
        agent["tail"] = Value::String(suffix(&text, 300));
    }
    Ok(())
}

fn enrich_association(
    tx: &Transaction<'_>,
    agent_id: &str,
    observation: &mut NotificationObservation,
) -> Result<(), NotificationDispatchError> {
    let p = &mut observation.params;
    let kind = p["item"]["type"].as_str().unwrap_or_default();
    if !matches!(
        observation.method.as_str(),
        "item/started" | "item/completed"
    ) || matches!(kind, "reasoning" | "userMessage" | "agentMessage")
        || (kind == "commandExecution"
            && observation.method == "item/completed"
            && p["item"]["aggregatedOutput"].is_null())
    {
        return Ok(());
    }
    let id = format!(
        "{agent_id}:{}",
        p["item"]["id"].as_str().unwrap_or_default()
    );
    // The existing item writer owns timing derivation. Reuse its stored output,
    // because Python mutates the item in the association arguments in place.
    if let Some((record, _)) = NotificationDispatchRepository::notification_item(tx, agent_id, &id)?
    {
        let text = saved_text(tx, &id, &record)?;
        if let Ok(displayed) = serde_json::from_str::<Value>(&text) {
            for field in ["startedAtMs", "completedAtMs", "durationMs"] {
                if let Some(value) = displayed.get(field) {
                    p["item"][field] = value.clone();
                }
            }
        }
    }
    Ok(())
}

fn assistant_item_record(
    id: &str,
    text: &str,
    at: f64,
    streaming: bool,
    turn_id: Option<&str>,
    phase: Value,
) -> Value {
    json!({
        "id": id,
        "role": "assistant",
        "title": "Assistant",
        "text": text.chars().take(ITEM_PREVIEW_CHARS).collect::<String>(),
        "truncated": text.chars().count() > ITEM_PREVIEW_CHARS,
        "at": at,
        "streaming": streaming,
        "turnId": turn_id,
        "phase": phase,
    })
}

fn suffix(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(limit)).collect()
}

fn apply_token_usage(
    agent: &mut Value,
    params: &Value,
    observed_at: f64,
) -> Result<DispatchOutcome, NotificationDispatchError> {
    let Some(usage) = params.get("tokenUsage").and_then(Value::as_object) else {
        return Err(NotificationDispatchError::InvalidNotification);
    };
    let tokens = usage.get("last").and_then(|last| last.get("totalTokens"));
    let window = usage.get("modelContextWindow");
    if tokens.is_some_and(|value| value.as_u64().is_none())
        || window.is_some_and(|value| !value.is_null() && value.as_u64().is_none())
    {
        return Err(NotificationDispatchError::InvalidNotification);
    }
    agent["contextUsage"] = json!({
        "tokens": tokens.cloned().unwrap_or(Value::Null),
        "window": window.cloned().unwrap_or(Value::Null),
        "at": observed_at,
    });
    Ok(DispatchOutcome::Written)
}

fn update_common_agent_fields(
    agent: &mut Value,
    method: &str,
    params: &Value,
    observed_at: f64,
) -> Result<(), NotificationDispatchError> {
    if !method.is_empty() {
        let events = agent
            .get("events")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        agent["events"] = Value::from(events.saturating_add(1));
        agent["lastEvent"] = Value::String(format_utc(observed_at)?);
    }
    if method == "item/agentMessage/delta"
        || (matches!(method, "item/started" | "item/completed")
            && !matches!(
                params["item"]["type"].as_str(),
                Some("reasoning" | "userMessage")
            ))
    {
        let turn = params
            .get("turnId")
            .and_then(Value::as_str)
            .filter(|turn| !turn.is_empty())
            .or_else(|| {
                agent
                    .get("turnId")
                    .and_then(Value::as_str)
                    .filter(|turn| !turn.is_empty())
            })
            .map(str::to_owned);
        if turn.as_deref() == agent.get("turnId").and_then(Value::as_str) {
            agent["nativeResponseTurn"] = turn
                .as_ref()
                .map_or(Value::Null, |turn| Value::String(turn.clone()));
            let mut clear_safety = false;
            if let Some(buffering) = agent
                .get_mut("nativeSafetyBuffering")
                .and_then(Value::as_object_mut)
                && buffering.get("turnId").and_then(Value::as_str) == turn.as_deref()
            {
                buffering.insert("responseStarted".to_owned(), Value::Bool(true));
                buffering.insert("showBufferingUi".to_owned(), Value::Bool(false));
                clear_safety = true;
            }
            if clear_safety
                && agent
                    .get("nativeStatus")
                    .and_then(|status| status.get("phase"))
                    .and_then(Value::as_str)
                    == Some("safety")
                && let Some(fields) = agent.as_object_mut()
            {
                let _removed = fields.remove("nativeStatus").is_some();
            }
        }
    }
    if matches!(
        method,
        "item/agentMessage/delta" | "item/started" | "item/completed"
    ) && agent.get("nativeStatus").is_some_and(|status| {
        status.get("phase").and_then(Value::as_str) == Some("retrying")
            && status.get("turnId").unwrap_or(&Value::Null)
                == params.get("turnId").unwrap_or(&Value::Null)
    }) && let Some(fields) = agent.as_object_mut()
    {
        fields.remove("nativeStatus");
        if params["item"]["type"] == "userMessage" {
            fields.insert(
                "activity".into(),
                json!({"phase":"thinking","at":observed_at}),
            );
        }
    }
    Ok(())
}

/// Store `agent` with its mode fields normalized and return the stored value.
fn persist_agent(
    transaction: &Transaction<'_>,
    agent_id: &str,
    agent: &Value,
    publication: &PublicationContext,
) -> Result<Value, NotificationDispatchError> {
    let mut normalized = agent.clone();
    crate::agents_create::mode_fields(&mut normalized);
    let agent = &normalized;
    NotificationDispatchRepository::put_agent_record(transaction, agent_id, agent)?;
    let context = crate::sync_entity_context::agent_view_context(
        transaction,
        agent,
        publication.server_id.as_deref(),
    )
    .map_err(|_| stored("runtime_agents", agent_id))?;
    let view = crate::sync_views::agent_entity_view(agent, &context)
        .ok_or_else(|| stored("runtime_agents", agent_id))?;
    SyncDocumentsRepository::put_entity(transaction, "agent", agent_id, view, false)?;
    Ok(normalized)
}

/// The stored agent after one dispatch, when the dispatch knows it.
enum AfterAgent {
    /// Read it back: a write path not tracked here may have stored it.
    Unknown,
    /// The dispatch stored nothing; the stored agent is `before`.
    Unchanged,
    /// The value this dispatch stored last.
    Stored(Value),
}

fn has_questions(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Array(values)) => !values.is_empty(),
        Some(Value::Object(values)) => !values.is_empty(),
        _ => false,
    }
}

fn decode_outcome(
    raw: &str,
    receipt_id: &str,
) -> Result<DispatchOutcome, NotificationDispatchError> {
    checked_outcome(
        serde_json::from_str(raw).map_err(|_| stored("runtime_operation_receipts", receipt_id))?,
    )
}

fn checked_outcome(outcome: DispatchOutcome) -> Result<DispatchOutcome, NotificationDispatchError> {
    match outcome {
        DispatchOutcome::RequiresService { service } => {
            Err(NotificationDispatchError::Dependency(service.name()))
        }
        outcome => Ok(outcome),
    }
}

fn save_outcome(
    transaction: &Transaction<'_>,
    receipt_id: &str,
    signature: &str,
    outcome: DispatchOutcome,
) -> Result<DispatchOutcome, NotificationDispatchError> {
    let outcome = checked_outcome(outcome)?;
    NotificationDispatchRepository::put_notification_receipt(
        transaction,
        receipt_id,
        signature,
        &serde_json::to_string(&outcome)?,
    )?;
    Ok(outcome)
}

fn sha256_json(value: &Value) -> Result<String, NotificationDispatchError> {
    let bytes = serde_json::to_vec(value)?;
    Ok(sha256_hex(&bytes))
}

fn stored(table: &'static str, id: &str) -> NotificationDispatchError {
    NotificationDispatchError::InvalidStoredObject {
        table: table.into(),
        id: id.to_owned(),
    }
}

fn in_savepoint<T>(
    transaction: &Transaction<'_>,
    operation: impl FnOnce() -> Result<T, NotificationDispatchError>,
) -> Result<T, NotificationDispatchError> {
    transaction
        .execute_batch(&format!("SAVEPOINT {SAVEPOINT}"))
        .map_err(StoreError::from)?;
    match operation() {
        Ok(value) => match transaction.execute_batch(&format!("RELEASE SAVEPOINT {SAVEPOINT}")) {
            Ok(()) => Ok(value),
            Err(error) => {
                drop(transaction.execute_batch(&format!(
                    "ROLLBACK TO SAVEPOINT {SAVEPOINT}; RELEASE SAVEPOINT {SAVEPOINT}"
                )));
                Err(StoreError::from(error).into())
            }
        },
        Err(error) => {
            if transaction
                .execute_batch(&format!(
                    "ROLLBACK TO SAVEPOINT {SAVEPOINT}; RELEASE SAVEPOINT {SAVEPOINT}"
                ))
                .is_err()
            {
                return Err(error);
            }
            Err(error)
        }
    }
}

fn format_utc(seconds: f64) -> Result<String, NotificationDispatchError> {
    let whole = seconds.floor();
    if whole < i64::MIN as f64 || whole > i64::MAX as f64 {
        return Err(NotificationDispatchError::InvalidNotification);
    }
    DateTime::<Utc>::from_timestamp(whole as i64, 0)
        .map(|time| time.to_rfc3339_opts(SecondsFormat::Secs, true))
        .ok_or(NotificationDispatchError::InvalidNotification)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use rusqlite::{Connection, params};
    use serde_json::{Value, json};

    use super::{
        DispatchOutcome, NotificationStreamBuffer, PreparedSubmission, ProviderDeliveryIdentity,
        PublicationContext, SubmittedInput, notification_content_signature, notification_payload,
        record_user_submission, sha256_json,
    };
    use studio_runtime_providers::{Frame, LineDecoder};
    use studio_runtime_store::{
        AgentRecord, AgentRepository, AgentStatus, ChatMessage, ChatMessageId, MessageRepository,
        NotificationDispatchRepository, QueueEvent, QueueEventId, QueueEventRepository,
        RuntimeStore, SchedulerRepository, TranscriptRepository,
    };

    #[test]
    fn borrowed_notification_signature_matches_owned_payload_with_unknown_fields() {
        let mut decoder = LineDecoder::new();
        let frames = decoder
            .push(
                b"{\"method\":\"future/notice\",\"params\":{\"nested\":[1,2]},\"futureField\":{\"keep\":true}}\n",
            )
            .expect("unknown notification decodes");
        decoder.finish().expect("fixture is complete");
        let frame = &frames[0];
        let payload = notification_payload(frame);
        assert_eq!(
            payload,
            json!({
                "method":"future/notice",
                "params":{"nested":[1,2]},
                "futureField":{"keep":true}
            })
        );
        assert_eq!(
            notification_content_signature("agent-1", frame).expect("borrowed signature"),
            sha256_json(&json!({"agent":"agent-1","notification":payload}))
                .expect("owned signature")
        );
    }

    // Legacy fixtures model a transaction-local buffer with an immediate timer.
    // The corpus exercises the caller's commit/apply and rollback/drop boundary.
    fn dispatch_notification(
        transaction: &rusqlite::Transaction<'_>,
        agent: &str,
        _state: &str,
        streams: &mut NotificationStreamBuffer,
        identity: &ProviderDeliveryIdentity,
        frame: &Frame,
        at: f64,
    ) -> Result<DispatchOutcome, super::NotificationDispatchError> {
        let publication = PublicationContext::default();
        let result = super::dispatch_notification(
            transaction,
            agent,
            &publication,
            streams,
            identity,
            frame,
            at,
        )?;
        if let Some(update) = result.stream_update {
            *streams = update;
        }
        if let Some(update) =
            super::flush_notification_streams(transaction, agent, &publication, streams, at)?
        {
            *streams = update;
        }
        Ok(result.outcome)
    }

    pub(super) fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch(
                &studio_runtime_store::agent_columns::fixture_sql("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT NOT NULL,kind TEXT NOT NULL,text TEXT NOT NULL,status TEXT NOT NULL,created REAL NOT NULL,epoch INTEGER NOT NULL,turn_id TEXT,error TEXT);
                 CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT NOT NULL,record TEXT NOT NULL,created REAL NOT NULL);
                 CREATE TABLE runtime_item_fulltext(id TEXT PRIMARY KEY,body TEXT NOT NULL);
                 CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL);
                 CREATE TABLE sync_entities(collection TEXT NOT NULL,id TEXT NOT NULL,seq INTEGER NOT NULL,hash TEXT NOT NULL,payload TEXT,deleted INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(collection,id));
                 CREATE TABLE sync_documents(seq INTEGER,scope TEXT,id TEXT,payload TEXT,deleted INTEGER);
                 CREATE TABLE sync_versions(seq INTEGER,scope TEXT,hash TEXT,deleted INTEGER,updated REAL);
                 CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE sync_entity_meta(key TEXT PRIMARY KEY,value TEXT);
                 CREATE VIRTUAL TABLE runtime_search USING fts5(id UNINDEXED,agent UNINDEXED,kind UNINDEXED,body,tokenize='unicode61');
                 CREATE TABLE runtime_search_rows(id TEXT PRIMARY KEY,search_rowid INTEGER NOT NULL UNIQUE);
                 CREATE TABLE runtime_search_indexed(id TEXT PRIMARY KEY);
                 INSERT INTO runtime_agents VALUES('agent','{\"id\":\"agent\",\"rootId\":\"agent\",\"epoch\":1,\"status\":\"running\",\"threadId\":\"thread\",\"accountKey\":\"default\",\"turnId\":\"turn\",\"inFlight\":true,\"events\":0,\"activeTools\":[],\"autoWake\":false}');
                 INSERT INTO sync_entities VALUES('agent','agent',1,'old','{\"collection\":\"agent\",\"id\":\"agent\",\"value\":{\"id\":\"agent\",\"status\":\"running\",\"threadId\":\"thread\",\"accountKey\":\"default\",\"turnId\":\"turn\",\"inFlight\":true,\"events\":0}}',0);"),
            )
            .expect("notification dispatch tables create");
        connection
            .execute_batch(studio_runtime_store::agent_volatile::agent_volatile_table_sql())
            .expect("volatile agent table creates");
        connection
    }

    pub(super) fn frame(value: &Value) -> Frame {
        let mut bytes = serde_json::to_vec(value).expect("notification serializes");
        bytes.push(b'\n');
        LineDecoder::new()
            .push(&bytes)
            .expect("notification decodes")
            .into_iter()
            .next()
            .expect("one notification frame")
    }

    fn identity(cursor: u64) -> ProviderDeliveryIdentity {
        ProviderDeliveryIdentity {
            provider_handle: "account:default".to_owned(),
            generation: 7,
            cursor,
        }
    }

    fn dispatch(
        transaction: &rusqlite::Transaction<'_>,
        streams: &mut NotificationStreamBuffer,
        cursor: u64,
        value: &Value,
        observed_at: f64,
    ) -> Result<DispatchOutcome, super::NotificationDispatchError> {
        dispatch_notification(
            transaction,
            "agent",
            ".",
            streams,
            &identity(cursor),
            &frame(value),
            observed_at,
        )
    }

    /// Hot notifications that change only volatile agent fields (storage
    /// format 2026-10-20) never write `runtime_agents`, for either provider.
    #[tokio::test]
    async fn hot_notifications_write_only_the_volatile_agent_row() {
        for provider in ["codex", "claude"] {
            let root = tempfile::tempdir().unwrap();
            let store = RuntimeStore::open(Some(root.path())).await.unwrap();
            store
                .transaction_with(move |tx| {
                    AgentRepository::put(
                        tx,
                        &AgentRecord::from_value(json!({
                            "id":"agent","rootId":"agent","epoch":1,"status":"running",
                            "threadId":"thread","accountKey":"default","provider":provider,
                            "turnId":"turn","inFlight":true,"autoWake":false,
                            "activeTools":[],"events":0
                        }))
                        .unwrap(),
                    )?;
                    tx.execute_batch(
                        "CREATE TEMP TABLE probe(n INTEGER); INSERT INTO probe VALUES(0);
                         CREATE TEMP TRIGGER probe_agents AFTER UPDATE ON main.runtime_agents
                         BEGIN UPDATE probe SET n=n+1; END;",
                    )?;
                    Ok::<_, studio_runtime_store::StoreError>(())
                })
                .await
                .unwrap();
            let notifications = [
                json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"answer","delta":"one "}}),
                json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"answer","delta":"two "}}),
                json!({"method":"thread/tokenUsage/updated","params":{"threadId":"thread","turnId":"turn","tokenUsage":{"total":{"totalTokens":9},"last":{"totalTokens":9},"modelContextWindow":1000}}}),
                json!({"method":"item/started","params":{"threadId":"thread","turnId":"turn","item":{"id":"cmd","type":"commandExecution","command":"ls","status":"inProgress"}}}),
                json!({"method":"item/completed","params":{"threadId":"thread","turnId":"turn","item":{"id":"cmd","type":"commandExecution","command":"ls","status":"completed","aggregatedOutput":"a","exitCode":0}}}),
            ];
            let mut streams = NotificationStreamBuffer::default();
            for (index, notification) in notifications.into_iter().enumerate() {
                let current = streams.clone();
                let (next, agent_updates) = store
                    .transaction_with(move |tx| {
                        let publication = PublicationContext::default();
                        let before: i64 = tx
                            .query_row("SELECT n FROM probe", [], |r| r.get(0))
                            .map_err(studio_runtime_store::StoreError::from)?;
                        let result = super::dispatch_notification_with_effects(
                            tx,
                            "agent",
                            &publication,
                            &current,
                            &identity(index as u64 + 1),
                            &frame(&notification),
                            100.0 + index as f64,
                            &crate::notification_runtime_effects::RuntimeNotificationEffects,
                        )?;
                        let mut next = result.stream_update.unwrap_or(current);
                        if let Some(update) = super::flush_notification_streams(
                            tx,
                            "agent",
                            &publication,
                            &next,
                            100.5 + index as f64,
                        )? {
                            next = update;
                        }
                        let after: i64 = tx
                            .query_row("SELECT n FROM probe", [], |r| r.get(0))
                            .map_err(studio_runtime_store::StoreError::from)?;
                        Ok::<_, super::NotificationDispatchError>((next, after - before))
                    })
                    .await
                    .unwrap();
                streams = next;
                // The first delta records the response turn in the record.
                if index > 0 {
                    assert_eq!(agent_updates, 0, "{provider} notification {index}");
                }
            }
            let connection = store.read().unwrap();
            let agent = AgentRepository::get(&connection, &"agent".into())
                .unwrap()
                .unwrap()
                .value();
            assert_eq!(agent["tail"], "one two ");
            assert_eq!(agent["contextUsage"]["tokens"], 9);
            assert_eq!(agent["activeTools"], json!([]));
            assert!(agent["events"].as_u64().unwrap() >= 4);
        }
    }

    #[tokio::test]
    async fn process_task_retains_the_actual_connection_only_for_its_generation() {
        let root = tempfile::tempdir().unwrap();
        let store = studio_runtime_store::RuntimeStore::open(Some(root.path()))
            .await
            .unwrap();
        store.transaction_with(|tx| {
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES('agent',?1)",[json!({
                "id":"agent","rootId":"agent","epoch":1,"status":"running","threadId":"thread",
                "accountKey":"default","provider":"codex","turnId":"turn","inFlight":true,"autoWake":true,
                "activeTools":[],"events":0
            }).to_string()]).map_err(studio_runtime_store::StoreError::from)?;
            let publication=PublicationContext::default().with_native_source(Some(super::NativeProcessSource { account:"default".into(), connection:"actual-connection-uuid".into(), generation:Some(7), provider:"codex".into() }));
            for (cursor,item,generation) in [(1,"command",7),(2,"old-command",6)] {
                let mut identity=identity(cursor);identity.generation=generation;
                super::dispatch_notification(tx,"agent",&publication,&NotificationStreamBuffer::default(),&identity,
                    &frame(&json!({"method":"item/started","params":{"threadId":"thread","turnId":"turn","item":{"id":item,"type":"commandExecution","command":"sleep 60","processId":"42"}}})),100.0)?;
            }
            Ok::<_,super::NotificationDispatchError>(())
        }).await.unwrap();
        let connection = store.read().unwrap();
        for (item, expected) in [
            ("command", json!("actual-connection-uuid")),
            ("old-command", Value::Null),
        ] {
            let raw: String = connection
                .query_row(
                    "SELECT record FROM runtime_tasks WHERE id=?",
                    [format!("agent:{item}")],
                    |r| r.get(0),
                )
                .unwrap();
            let record: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(record["sourceConnectionId"], expected);
            assert_eq!(record["ownerEpoch"], 1);
        }
    }

    #[tokio::test]
    async fn question_tool_item_is_stored_as_the_card_contract() {
        // The chat card parses this stored text; Python has the same witness.
        let contract: Value = serde_json::from_str(include_str!(
            "../../../../client/contracts/agent-question-item.json"
        ))
        .unwrap();
        let item = contract["item"].clone();
        let root = tempfile::tempdir().unwrap();
        let store = studio_runtime_store::RuntimeStore::open(Some(root.path()))
            .await
            .unwrap();
        let notification = json!({"method":"item/completed","params":{
            "threadId":"thread","turnId":"turn","item":item
        }});
        store
            .transaction_with(move |tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES('agent',?1)",
                    [json!({
                        "id":"agent","rootId":"agent","epoch":1,"status":"running","threadId":"thread",
                        "accountKey":"default","provider":"claude","turnId":"turn","inFlight":true,
                        "autoWake":true,"activeTools":[],"events":0,"isLead":true
                    })
                    .to_string()],
                )
                .map_err(studio_runtime_store::StoreError::from)?;
                super::dispatch_notification(
                    tx,
                    "agent",
                    &PublicationContext::default(),
                    &NotificationStreamBuffer::default(),
                    &identity(1),
                    &frame(&notification),
                    100.0,
                )?;
                Ok::<_, super::NotificationDispatchError>(())
            })
            .await
            .unwrap();
        let raw: String = store
            .read()
            .unwrap()
            .query_row(
                "SELECT record FROM runtime_items WHERE id='agent:toolu_contract'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let record: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(record["role"], "output");
        assert_eq!(record["title"], "mcpToolCall");
        assert_eq!(record["toolStatus"], "completed");
        let stored: Value = serde_json::from_str(record["text"].as_str().unwrap()).unwrap();
        // Every provider field survives; the dispatcher may add timing fields.
        for (key, value) in item.as_object().unwrap() {
            assert_eq!(&stored[key], value, "{key}");
        }
    }

    #[test]
    fn stale_safety_notice_keeps_current_turn_stream_pending() {
        let mut connection = database();
        let tx = connection.transaction().expect("transaction");
        let initial = super::dispatch_notification(&tx,"agent",&PublicationContext::default(),&NotificationStreamBuffer::default(),&identity(1),&frame(&json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"answer","delta":"pending"}})),1234.5).expect("buffer delta");
        let streams = initial.stream_update.expect("staged stream");
        let result = super::dispatch_notification(&tx,"agent",&PublicationContext::default(),&streams,&identity(2),&frame(&json!({"method":"model/safetyBuffering/updated","params":{"threadId":"thread","turnId":"old-turn","showBufferingUi":true}})),1234.5).expect("stale notice");
        assert!(
            result.stream_update.is_none(),
            "unrelated turn must not consume pending text"
        );
        let count: i64 = tx
            .query_row("SELECT count(*) FROM runtime_items", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, 0);
    }

    #[test]
    fn fresh_no_op_receipt_does_not_return_stream_update() {
        let mut connection = database();
        let tx = connection.transaction().expect("transaction");
        let result = super::dispatch_notification(
            &tx,
            "agent",
            &PublicationContext::default(),
            &NotificationStreamBuffer::default(),
            &identity(1),
            &frame(&json!({
                "method":"model/safetyBuffering/updated",
                "params":{"threadId":"thread","turnId":"old-turn","showBufferingUi":true}
            })),
            1234.5,
        )
        .expect("no-op safety notice records a receipt");
        assert!(
            result.stream_update.is_none(),
            "a fresh receipt without stream changes must not replace the buffer"
        );
    }

    async fn submitted_chat_runtime() -> (tempfile::TempDir, RuntimeStore) {
        let directory = tempfile::tempdir().expect("temporary runtime");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("runtime store");
        let event_id = "chat:message-1:agent";
        let message_text = "answer this prompt";
        let event_text = json!({
            "room":"room-1",
            "message_id":"message-1",
            "sender":"user",
            "sender_name":"User",
            "text":message_text
        })
        .to_string();
        store
            .transaction(move |tx| {
                let agent = AgentRecord::from_value(json!({
                    "id":"agent",
                    "rootId":"agent",
                    "epoch":1,
                    "status":AgentStatus::Starting.as_str(),
                    "threadId":"thread",
                    "accountKey":"default",
                    "turnEpoch":1,
                    "autoWake":true,
                    "inFlight":true,
                    "events":0,
                    "startAttempt":{
                        "id":"attempt-1",
                        "epoch":1,
                        "submitted":false,
                        "events":[event_id]
                    }
                }))?;
                AgentRepository::put(tx, &agent)?;
                QueueEventRepository::put(
                    tx,
                    &QueueEvent {
                        id: QueueEventId::from(event_id),
                        agent: "agent".into(),
                        kind: "agent_message".into(),
                        text: event_text.clone(),
                        status: "reserved".into(),
                        created: 1.0,
                        epoch: 1,
                        turn_id: None,
                        error: None,
                    },
                )?;
                tx.execute(
                    "INSERT INTO runtime_event_meta(id,record) VALUES(?1,?2)",
                    rusqlite::params![
                        event_id,
                        json!({"contextManifest":{"epoch":[null,0],"versions":{"worktreeReminder":"v1"},"sequence":1}}).to_string()
                    ],
                )?;
                MessageRepository::put(
                    tx,
                    &ChatMessage {
                        sequence: 1,
                        id: ChatMessageId::from("message-1"),
                        room: "room-1".into(),
                        sender: "user".into(),
                        text: message_text.into(),
                        created: 1.0,
                        deliveries: json!({"agent":"queued"}),
                    },
                )?;
                assert!(SchedulerRepository::mark_submitted_identity(
                    tx,
                    "agent",
                    1,
                    "attempt-1",
                    "ticket-1",
                    "default",
                    7,
                    "connection-1",
                    "thread",
                )?);
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .await
            .expect("submitted attempt is durable");
        (directory, store)
    }

    #[tokio::test]
    async fn user_message_receipt_binds_exact_submitted_turn_and_chat_delivery_once() {
        let (directory, store) = submitted_chat_runtime().await;
        let event_id = "chat:message-1:agent";
        let notification = frame(&json!({
            "method":"item/started",
            "params":{
                "threadId":"thread",
                "turnId":"turn-1",
                "item":{"id":"input-1","type":"userMessage","clientId":event_id}
            }
        }));
        let first_identity = identity(1);
        let first = store
            .transaction(move |tx| {
                super::dispatch_notification_with_effects(
                    tx,
                    "agent",
                    &PublicationContext::default(),
                    &NotificationStreamBuffer::default(),
                    &first_identity,
                    &notification,
                    2.0,
                    &crate::notification_runtime_effects::RuntimeNotificationEffects,
                )
                .map(|result| result.outcome)
                .map_err(|error| {
                    studio_runtime_store::StoreError::SchemaMismatch(error.to_string())
                })
            })
            .await
            .expect("positive native receipt commits");
        assert_eq!(first, DispatchOutcome::Written);
        drop(store);
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("runtime reopens");

        store
            .transaction(move |tx| {
                let event = QueueEventRepository::get(tx, &QueueEventId::from(event_id))?
                    .expect("submitted event remains");
                assert_eq!(event.status, "delivered");
                assert_eq!(event.turn_id.as_deref(), Some("turn-1"));
                let agent =
                    AgentRepository::get(tx, &"agent".into())?.expect("accepted agent remains");
                assert_eq!(agent.extra["turnId"], "turn-1");
                assert_eq!(agent.extra["events"], 1);
                assert!(agent.extra["lastEvent"].is_string());
                let message = MessageRepository::get(tx, &ChatMessageId::from("message-1"))?
                    .expect("chat message remains");
                assert_eq!(message.deliveries["agent"], "delivered");
                let (manifest_event, manifest): (String, String) = tx.query_row(
                    "SELECT event_id,record FROM runtime_context_manifests WHERE agent='agent' AND thread_id='' AND compactions=0",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                assert_eq!(manifest_event, event_id);
                assert_eq!(
                    serde_json::from_str::<Value>(&manifest)?["versions"]["worktreeReminder"],
                    "v1"
                );
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .await
            .expect("settled receipt state reads");

        let replay_identity = identity(1);
        let replay_notification = frame(&json!({
            "method":"item/started",
            "params":{
                "threadId":"thread",
                "turnId":"turn-1",
                "item":{"id":"input-1","type":"userMessage","clientId":event_id}
            }
        }));
        let replay = store
            .transaction(move |tx| {
                super::dispatch_notification_with_effects(
                    tx,
                    "agent",
                    &PublicationContext::default(),
                    &NotificationStreamBuffer::default(),
                    &replay_identity,
                    &replay_notification,
                    3.0,
                    &crate::notification_runtime_effects::RuntimeNotificationEffects,
                )
                .map(|result| result.outcome)
                .map_err(|error| {
                    studio_runtime_store::StoreError::SchemaMismatch(error.to_string())
                })
            })
            .await
            .expect("lost acknowledgement replays its saved receipt");
        assert_eq!(replay, DispatchOutcome::Written);
        store
            .transaction(move |tx| {
                let agent =
                    AgentRepository::get(tx, &"agent".into())?.expect("accepted agent remains");
                assert_eq!(agent.extra["events"], 1);
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .await
            .expect("receipt replay does not count the notification twice");
    }

    #[tokio::test]
    async fn turn_start_acceptance_projects_chat_delivery_in_its_transaction() {
        let (_directory, store) = submitted_chat_runtime().await;
        store
            .transaction(move |tx| {
                let accepted = SchedulerRepository::settle_started_provider(
                    tx,
                    "agent",
                    1,
                    "attempt-1",
                    "ticket-1",
                    "default",
                    7,
                    "thread",
                    "turn-1",
                )?;
                assert_eq!(accepted, Some(false));
                let event =
                    QueueEventRepository::get(tx, &QueueEventId::from("chat:message-1:agent"))?
                        .expect("submitted event remains");
                assert_eq!(event.status, "delivered");
                let message = MessageRepository::get(tx, &ChatMessageId::from("message-1"))?
                    .expect("chat message remains");
                assert_eq!(message.deliveries["agent"], "delivered");
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .await
            .expect("turn/start acceptance commits chat delivery");
    }

    #[test]
    fn injected_unload_refusal_keeps_pending_text_and_has_no_receipt() {
        let mut connection = database();
        let tx = connection.transaction().expect("transaction");
        let initial = super::dispatch_notification(&tx,"agent",&PublicationContext::default(),&NotificationStreamBuffer::default(),&identity(1),&frame(&json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"answer","delta":"pending"}})),1234.5).expect("buffer delta");
        let streams = initial.stream_update.expect("staged stream");
        let before = streams.clone();
        let result = super::dispatch_notification_with_effects(
            &tx,
            "agent",
            &PublicationContext::default(),
            &streams,
            &identity(2),
            &frame(&json!({"method":"thread/closed","params":{"threadId":"thread"}})),
            1234.5,
            &super::UnavailableNotificationEffects,
        );
        assert!(matches!(
            result,
            Err(super::NotificationDispatchError::Dependency(
                "turn-recovery/thread-unloaded"
            ))
        ));
        assert_eq!(streams, before);
        assert!(
            NotificationDispatchRepository::transcript_item(&tx, "agent", "agent:answer")
                .expect("read")
                .is_none()
        );
        assert_eq!(
            tx.query_row(
                "SELECT count(*) FROM runtime_operation_receipts",
                [],
                |row| row.get::<_, i64>(0)
            )
            .expect("receipt count"),
            1
        );
        tx.commit().expect("caller catches error and commits");
    }

    #[test]
    fn absent_service_rolls_back_all_notification_rows_and_receipt() {
        for method in [
            "thread/realtime/started",
            "account/updated",
            "thread/tokenUsage/updated",
        ] {
            let mut connection = database();
            let tx = connection.transaction().expect("transaction");
            let rows = |tx: &rusqlite::Transaction<'_>| {
                [
                    "runtime_agents",
                    "runtime_items",
                    "sync_entities",
                    "runtime_operation_receipts",
                ]
                .map(|table| {
                    let mut statement = tx
                        .prepare(&format!("SELECT * FROM {table} ORDER BY 1"))
                        .expect("rows");
                    let width = statement.column_count();
                    statement
                        .query_map([], |row| {
                            (0..width)
                                .map(|i| row.get::<_, rusqlite::types::Value>(i))
                                .collect::<Result<Vec<_>, _>>()
                        })
                        .expect("query")
                        .collect::<Result<Vec<_>, _>>()
                        .expect("records")
                })
            };
            let before = rows(&tx);
            let streams = NotificationStreamBuffer::default();
            let result = super::dispatch_notification(
                &tx,
                "agent",
                &PublicationContext::default(),
                &streams,
                &identity(1),
                &frame(
                    &json!({"method":method,"params":{"threadId":"thread","tokenUsage":{"total":{"totalTokens":5},"last":{"totalTokens":5},"modelContextWindow":1000}}}),
                ),
                1234.5,
            );
            assert!(
                matches!(result, Err(super::NotificationDispatchError::Dependency(_))),
                "{method}: {result:?}"
            );
            assert_eq!(
                before,
                rows(&tx),
                "{method} leaked effects or acknowledged the delivery"
            );
            tx.commit().expect("caller may commit after caught error");
        }
    }

    #[test]
    fn committed_delivery_redelivery_replays_result_and_changed_content_conflicts() {
        let mut connection = database();
        let mut streams = NotificationStreamBuffer::default();
        let delta = json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"answer","delta":"hello"}});
        let transaction = connection.transaction().expect("transaction begins");
        assert_eq!(
            dispatch(&transaction, &mut streams, 1, &delta, 10.0).expect("delta writes"),
            DispatchOutcome::Written
        );
        transaction
            .commit()
            .expect("first delivery commits before acknowledgement");

        let transaction = connection
            .transaction()
            .expect("redelivery transaction begins");
        assert_eq!(
            dispatch(&transaction, &mut streams, 1, &delta, 11.0)
                .expect("redelivery returns saved result"),
            DispatchOutcome::Written
        );
        let changed = json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"answer","delta":"changed"}});
        assert!(matches!(
            dispatch(&transaction, &mut streams, 1, &changed, 11.0),
            Err(super::NotificationDispatchError::ConflictingDelivery)
        ));
        transaction
            .commit()
            .expect("redelivery transaction remains committable");
        let item = TranscriptRepository::item(&connection, "agent", "agent:answer")
            .expect("transcript read succeeds")
            .expect("answer exists");
        assert_eq!(item.record["text"], "hello");
        let agent = NotificationDispatchRepository::agent_record(
            &connection
                .unchecked_transaction()
                .expect("read transaction begins"),
            "agent",
        )
        .expect("agent reads")
        .expect("agent exists");
        assert_eq!(agent["events"], 1);
    }

    #[test]
    fn snapshot_and_late_delta_order_matches_stream_buffer_completion_rules() {
        use studio_runtime_store::NotificationDispatchRepository;

        macro_rules! send {
            ($transaction:expr, $streams:expr, $cursor:expr, $value:expr, $at:expr) => {{
                let result = super::dispatch_notification(
                    $transaction,
                    "agent",
                    &PublicationContext::default(),
                    $streams,
                    &identity($cursor),
                    &frame($value),
                    $at,
                )
                .expect("notification dispatch succeeds");
                assert_eq!(result.outcome, DispatchOutcome::Written);
                if let Some(update) = result.stream_update {
                    *$streams = update;
                }
            }};
        }
        fn flush(
            transaction: &rusqlite::Transaction<'_>,
            streams: &mut NotificationStreamBuffer,
            at: f64,
        ) {
            if let Some(update) = super::flush_notification_streams(
                transaction,
                "agent",
                &PublicationContext::default(),
                streams,
                at,
            )
            .expect("stream flush succeeds")
            {
                *streams = update;
            }
        }

        // A recovered stream can carry a durable base plus queued deltas. A
        // started snapshot writes its transcript row and leaves that state intact.
        let mut connection = database();
        let transaction = connection.transaction().expect("transaction begins");
        let mut streams = NotificationStreamBuffer::default();
        send!(
            &transaction,
            &mut streams,
            1,
            &json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"answer","delta":"pending"}}),
            1.0
        );
        let key = super::stream::StreamKey {
            agent_id: "agent".to_owned(),
            account: "default".to_owned(),
            generation: 7,
            thread: "thread".to_owned(),
            turn: Some("turn".to_owned()),
            item: "answer".to_owned(),
            method: "item/agentMessage/delta".to_owned(),
        };
        let mut saved_streams = serde_json::to_value(&streams).expect("stream serializes");
        saved_streams["entries"][0]["base"] = Value::String(super::sha256_hex(b"base"));
        saved_streams["entries"][0]["flushed"] = Value::Bool(true);
        streams = serde_json::from_value(saved_streams).expect("persisted stream restores");
        NotificationDispatchRepository::put_stream_base(
            &transaction,
            &super::stream_base_id(&key),
            Some("base"),
        )
        .expect("durable stream base writes");
        send!(
            &transaction,
            &mut streams,
            2,
            &json!({"method":"item/started","params":{"threadId":"thread","turnId":"turn","item":{"id":"answer","type":"agentMessage","text":"snapshot"}}}),
            2.0
        );
        send!(
            &transaction,
            &mut streams,
            3,
            &json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"answer","delta":"late"}}),
            3.0
        );
        flush(&transaction, &mut streams, 4.0);
        let answer = TranscriptRepository::item(&transaction, "agent", "agent:answer")
            .expect("answer reads")
            .expect("answer exists");
        assert_eq!(answer.record["text"], "basependinglate");
        transaction.commit().expect("persisted-base case commits");

        // Without a persisted base, the flush uses the current snapshot row and
        // appends both deltas that were queued before and after item/started.
        let mut connection = database();
        let transaction = connection.transaction().expect("transaction begins");
        let mut streams = NotificationStreamBuffer::default();
        send!(
            &transaction,
            &mut streams,
            1,
            &json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"answer","delta":"pending"}}),
            1.0
        );
        send!(
            &transaction,
            &mut streams,
            2,
            &json!({"method":"item/started","params":{"threadId":"thread","turnId":"turn","item":{"id":"answer","type":"agentMessage","text":"snapshot"}}}),
            2.0
        );
        send!(
            &transaction,
            &mut streams,
            3,
            &json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"answer","delta":"late"}}),
            3.0
        );
        flush(&transaction, &mut streams, 4.0);
        let answer = TranscriptRepository::item(&transaction, "agent", "agent:answer")
            .expect("answer reads")
            .expect("answer exists");
        assert_eq!(answer.record["text"], "snapshotpendinglate");
        transaction.commit().expect("snapshot-base case commits");
    }

    #[test]
    fn route_refusal_is_clean_and_post_write_failure_rolls_back_savepoint() {
        let mut connection = database();
        let mut streams = NotificationStreamBuffer::default();
        let wrong_thread = json!({"method":"thread/tokenUsage/updated","params":{"threadId":"other","tokenUsage":{"last":{"totalTokens":4}}}});
        let transaction = connection.transaction().expect("transaction begins");
        assert!(matches!(
            dispatch(&transaction, &mut streams, 1, &wrong_thread, 1.0),
            Err(super::NotificationDispatchError::InvalidNotification)
        ));
        transaction
            .commit()
            .expect("refusal leaves caller transaction committable");

        connection.execute_batch("CREATE TRIGGER fail_sync_projection BEFORE UPDATE ON sync_entities BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").expect("failure trigger creates");
        let delta = json!({"method":"item/completed","params":{"threadId":"thread","turnId":"turn","item":{"id":"rollback","type":"agentMessage","text":"written then refused"}}});
        let transaction = connection.transaction().expect("transaction begins");
        assert!(dispatch(&transaction, &mut streams, 2, &delta, 2.0).is_err());
        transaction
            .commit()
            .expect("outer transaction commits after savepoint rollback");
        assert!(
            TranscriptRepository::item(&connection, "agent", "agent:rollback")
                .expect("item query succeeds")
                .is_none()
        );
        let agent = NotificationDispatchRepository::agent_record(
            &connection
                .unchecked_transaction()
                .expect("read transaction begins"),
            "agent",
        )
        .expect("agent reads")
        .expect("agent exists");
        assert_eq!(agent["events"], 0);
        let receipts: i64 = connection
            .query_row(
                "SELECT count(*) FROM runtime_operation_receipts",
                [],
                |row| row.get(0),
            )
            .expect("receipt count reads");
        assert_eq!(receipts, 0);
        connection
            .execute_batch("DROP TRIGGER fail_sync_projection")
            .expect("failure trigger drops");
        let transaction = connection.transaction().expect("retry transaction begins");
        assert_eq!(
            dispatch(&transaction, &mut streams, 3, &delta, 3.0).expect("retry writes"),
            DispatchOutcome::Written
        );
        transaction.commit().expect("retry commits");
        let item = TranscriptRepository::item(&connection, "agent", "agent:rollback")
            .expect("retried item reads")
            .expect("retried item exists");
        assert_eq!(item.record["text"], "written then refused");
    }

    #[test]
    fn prepared_submission_preserves_batch_text_metadata_assets_and_replay() {
        let mut connection = database();
        connection.execute_batch(
            "INSERT INTO runtime_events VALUES('e1','agent','user','hello','dispatching',1,1,'turn',NULL);
             INSERT INTO runtime_events VALUES('e2','agent','followup','please continue','dispatching',2,1,'turn',NULL);
             INSERT INTO runtime_event_meta VALUES('e1','{\"assets\":[\"asset-1\"]}');",
        ).expect("queued event batch creates");
        let submission = PreparedSubmission {
            first_event_id: "e1".to_owned(),
            submitted_at: 3.0,
            created_at: 3.1,
            text: "hello\n\n[Orchestration event: followup]\nplease continue".to_owned(),
            inputs: vec![
                SubmittedInput {
                    id: "e1".to_owned(),
                    at: 1.0,
                    kind: "user".to_owned(),
                    text: "hello".to_owned(),
                    assets: json!([{"id":"asset-1","name":"file.txt"}]),
                },
                SubmittedInput {
                    id: "e2".to_owned(),
                    at: 2.0,
                    kind: "followup".to_owned(),
                    text: "please continue".to_owned(),
                    assets: json!([]),
                },
            ],
            assets: json!([{"id":"asset-1","name":"file.txt"}]),
            radio_authorized: false,
        };
        let transaction = connection
            .transaction()
            .expect("submission transaction begins");
        assert_eq!(
            record_user_submission(&transaction, "agent", &submission)
                .expect("submitted batch writes"),
            DispatchOutcome::Written
        );
        transaction.commit().expect("submission commits");
        let item = TranscriptRepository::item(&connection, "agent", "agent:e1")
            .expect("user item reads")
            .expect("batch row exists");
        assert_eq!(item.record["text"], submission.text);
        assert_eq!(item.record["inputs"][1]["kind"], "followup");
        assert_eq!(item.record["inputs"][0]["assets"][0]["id"], "asset-1");
        for event_id in ["e1", "e2"] {
            let meta: String = connection
                .query_row(
                    "SELECT record FROM runtime_event_meta WHERE id=?1",
                    [event_id],
                    |row| row.get(0),
                )
                .expect("metadata reads");
            let meta: Value = serde_json::from_str(&meta).expect("metadata remains JSON");
            assert_eq!(meta["transcriptItemId"], "agent:e1");
            assert_eq!(meta["modelEventProjection"], 1);
        }
        let transaction = connection.transaction().expect("replay transaction begins");
        assert_eq!(
            record_user_submission(&transaction, "agent", &submission)
                .expect("submission replay returns saved outcome"),
            DispatchOutcome::Written
        );
        transaction.commit().expect("submission replay commits");
        connection
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.deletedAt','deleted') WHERE id='agent'",
                [],
            )
            .expect("agent deletion marker sets after submission");
        let transaction = connection
            .transaction()
            .expect("deleted-agent replay transaction begins");
        assert_eq!(
            record_user_submission(&transaction, "agent", &submission)
                .expect("saved submission replays before mutable-agent validation"),
            DispatchOutcome::Written
        );
        transaction.commit().expect("deleted-agent replay commits");
    }

    #[test]
    fn submission_refuses_a_foreign_followup_sender_before_any_write() {
        let mut connection = database();
        connection
            .execute(
                "INSERT INTO runtime_agents VALUES(?1,?2)",
                [
                    "foreign",
                    r#"{"id":"foreign","rootId":"other-team","epoch":1,"status":"running","threadId":"other","accountKey":"default","events":0}"#,
                ],
            )
            .expect("foreign agent creates");
        connection
            .execute_batch(
                "INSERT INTO runtime_events VALUES('foreign-followup','agent','followup','hello','dispatching',1,1,'turn',NULL);
                 INSERT INTO runtime_event_meta VALUES('foreign-followup','{\"senderId\":\"foreign\"}');",
            )
            .expect("foreign followup creates");
        let submission = PreparedSubmission {
            first_event_id: "foreign-followup".to_owned(),
            submitted_at: 1.0,
            created_at: 1.0,
            text: "hello".to_owned(),
            inputs: vec![SubmittedInput {
                id: "foreign-followup".to_owned(),
                at: 1.0,
                kind: "followup".to_owned(),
                text: "hello".to_owned(),
                assets: json!([]),
            }],
            assets: json!([]),
            radio_authorized: false,
        };
        let transaction = connection
            .transaction()
            .expect("foreign submission transaction begins");
        assert!(matches!(
            record_user_submission(&transaction, "agent", &submission),
            Err(super::NotificationDispatchError::UnauthorizedSubmission)
        ));
        transaction
            .commit()
            .expect("unauthorized submission leaves transaction committable");
        let writes: i64 = connection
            .query_row(
                "SELECT (SELECT count(*) FROM runtime_items)+(SELECT count(*) FROM runtime_operation_receipts)",
                [],
                |row| row.get(0),
            )
            .expect("submission write count reads");
        assert_eq!(writes, 0);
    }

    #[test]
    fn assistant_replacement_clears_old_metadata_and_preserves_start_tail() {
        let mut connection = database();
        connection
            .execute(
                "UPDATE runtime_agents SET record=?2 WHERE id='agent'",
                params![
                    "agent",
                    json!({
                        "id":"agent","rootId":"agent","epoch":1,"status":"running",
                        "threadId":"thread","accountKey":"default","turnId":"turn",
                        "inFlight":true,"events":0,"activeTools":[],"autoWake":false,
                        "tail":"previous tail","nativeStatus":{"phase":"retrying","turnId":"turn"}
                    })
                    .to_string(),
                ],
            )
            .expect("agent retry state sets");
        connection
            .execute(
                "INSERT INTO runtime_items VALUES('agent:answer','agent',?1,1.0)",
                [json!({
                    "id":"agent:answer","role":"assistant","title":"Assistant","text":"old",
                    "truncated":false,"at":1.0,"streaming":true,"turnId":"turn","phase":null,
                    "afterRestore":"checkpoint","inputs":[{"id":"old","at":1.0,"kind":"user",
                    "text":"old","truncated":false,"assets":[]}],"assets":["old"],"obsolete":"remove"
                })
                .to_string()],
            )
            .expect("old item with metadata inserts");
        let started = json!({"method":"item/started","params":{"threadId":"thread","turnId":"turn","item":{"id":"answer","type":"agentMessage","text":"replacement"}}});
        let mut streams = NotificationStreamBuffer::default();
        let transaction = connection.transaction().expect("start transaction begins");
        dispatch(&transaction, &mut streams, 1, &started, 2.0)
            .expect("standalone start writes a fresh transcript record");
        transaction.commit().expect("start commits");
        let item = TranscriptRepository::item(&connection, "agent", "agent:answer")
            .expect("replacement item reads")
            .expect("replacement item exists");
        assert_eq!(item.record["text"], "replacement");
        assert_eq!(item.record["turnId"], "turn");
        for field in ["afterRestore", "inputs", "assets", "obsolete"] {
            assert!(
                item.record.get(field).is_none(),
                "old field {field} survived"
            );
        }
        let agent = TranscriptRepository::agent(&connection, "agent")
            .expect("agent reads")
            .expect("agent exists");
        assert_eq!(agent["tail"], "previous tail");
        assert_eq!(agent["nativeResponseTurn"], "turn");
        assert!(agent.get("nativeStatus").is_none());

        let mut no_turn_agent = agent;
        no_turn_agent["turnId"] = Value::Null;
        no_turn_agent["nativeStatus"] = json!({"phase":"retrying","turnId":null});
        connection
            .execute(
                "UPDATE runtime_agents SET record=?1 WHERE id='agent'",
                [no_turn_agent.to_string()],
            )
            .expect("missing turn state stores");
        let completed = json!({"method":"item/completed","params":{"threadId":"thread","turnId":null,"item":{"id":"no-turn","type":"agentMessage","text":"answer"}}});
        let transaction = connection
            .transaction()
            .expect("null-turn transaction begins");
        dispatch(&transaction, &mut streams, 2, &completed, 3.0)
            .expect("null-turn completion writes");
        transaction.commit().expect("null-turn completion commits");
        let item = TranscriptRepository::item(&connection, "agent", "agent:no-turn")
            .expect("null-turn item reads")
            .expect("null-turn item exists");
        assert_eq!(item.record.get("turnId"), Some(&Value::Null));
        let agent = TranscriptRepository::agent(&connection, "agent")
            .expect("updated agent reads")
            .expect("updated agent exists");
        assert_eq!(agent.get("nativeResponseTurn"), Some(&Value::Null));
        assert!(agent.get("nativeStatus").is_none());
    }

    #[test]
    fn malformed_stored_agent_and_item_shapes_are_named_without_content_leaks() {
        let mut connection = database();
        connection
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.epoch',?1) WHERE id='agent'",
                ["SECRET_STORED_SENTINEL"],
            )
            .expect("malformed agent epoch sets");
        let mut streams = NotificationStreamBuffer::default();
        let transaction = connection
            .transaction()
            .expect("invalid agent transaction begins");
        let error = dispatch(
            &transaction,
            &mut streams,
            1,
            &json!({"method":"thread/tokenUsage/updated","params":{"threadId":"thread","tokenUsage":{"last":{"totalTokens":2}}}}),
            2.0,
        )
        .expect_err("malformed stored epoch is refused");
        assert!(error.to_string().contains("runtime_agents/agent"));
        assert!(error.to_string().contains("/epoch"));
        assert!(error.to_string().contains("value type string"));
        assert!(error.to_string().contains("length"));
        assert!(!error.to_string().contains("SECRET_STORED_SENTINEL"));
        transaction
            .commit()
            .expect("invalid agent refusal leaves transaction usable");

        connection
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.epoch',1) WHERE id='agent'",
                [],
            )
            .expect("agent epoch restored");
        connection
            .execute(
                "INSERT INTO runtime_items VALUES('agent:malformed','agent',?1,1.0)",
                [
                    json!({"id":"agent:malformed","role":"assistant","text":"x","inputs":7})
                        .to_string(),
                ],
            )
            .expect("malformed transcript input stores");
        let transaction = connection
            .transaction()
            .expect("invalid item transaction begins");
        let error = dispatch(
            &transaction,
            &mut streams,
            2,
            &json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"malformed","delta":"x"}}),
            3.0,
        )
        .expect_err("malformed stored input shape is refused");
        assert!(error.to_string().contains("runtime_items:agent:malformed"));
        transaction
            .commit()
            .expect("invalid item refusal leaves transaction usable");
    }

    /// Codex reasoning deltas are counted at the next flush instead of
    /// rewriting the agent per delta; the count matches one event per delta.
    #[test]
    fn reasoning_deltas_count_at_flush_without_agent_writes() {
        let mut connection = database();
        let transaction = connection.transaction().expect("transaction begins");
        let publication = PublicationContext::default();
        let stored = |tx: &rusqlite::Transaction<'_>| {
            TranscriptRepository::agent(tx, "agent")
                .expect("agent reads")
                .expect("agent exists")
        };
        let initial = stored(&transaction);
        let mut streams = NotificationStreamBuffer::default();
        for cursor in 1..=5 {
            let method = if cursor % 2 == 0 {
                "item/reasoning/summaryTextDelta"
            } else {
                "item/reasoning/textDelta"
            };
            let result = super::dispatch_notification(&transaction, "agent", &publication, &streams, &identity(cursor),
                &frame(&json!({"method":method,"params":{"threadId":"thread","turnId":"turn","itemId":"think","delta":"x"}})), 10.0)
                .expect("reasoning delta");
            assert_eq!(result.outcome, DispatchOutcome::Written);
            streams = result.stream_update.expect("buffered delta");
            assert_eq!(
                stored(&transaction),
                initial,
                "a buffered delta writes no agent"
            );
        }
        streams =
            super::flush_notification_streams(&transaction, "agent", &publication, &streams, 11.0)
                .expect("flush")
                .expect("flush changes the buffer");
        let agent = stored(&transaction);
        assert_eq!(agent["events"], 5);
        assert!(agent["lastEvent"].is_string());
        assert_eq!(streams.pending_bytes(), 0);
    }

    /// Stream flushes write the text once, to the item: no durable base copy
    /// and no search row until item/completed indexes the whole answer. A
    /// snapshot after a flush keeps the flushed text as the stream's base.
    #[test]
    fn stream_flushes_store_the_text_once_and_index_it_at_completion() {
        use rusqlite::OptionalExtension;
        let mut connection = database();
        let transaction = connection.transaction().expect("transaction begins");
        let mut streams = NotificationStreamBuffer::default();
        let delta = |item: &str, text: &str| json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":item,"delta":text}});
        let base_rows = |tx: &rusqlite::Transaction<'_>| -> i64 {
            tx.query_row(
                "SELECT count(*) FROM runtime_operation_receipts WHERE signature='stream-base'",
                [],
                |r| r.get(0),
            )
            .expect("base rows")
        };
        let search = |tx: &rusqlite::Transaction<'_>, id: &str| -> Option<String> {
            tx.query_row("SELECT body FROM runtime_search WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()
            .expect("search row")
        };
        let text = |tx: &rusqlite::Transaction<'_>, id: &str| -> String {
            let item = TranscriptRepository::item(tx, "agent", id)
                .expect("item reads")
                .expect("item exists");
            TranscriptRepository::full_text(tx, &item)
                .expect("full text")
                .unwrap_or_else(|| item.record["text"].as_str().expect("text").to_owned())
        };
        // A long answer, flushed after every delta.
        let mut expected = String::new();
        for part in 0..30_u64 {
            let chunk = char::from(b'a' + (part % 26) as u8)
                .to_string()
                .repeat(1_000);
            expected.push_str(&chunk);
            dispatch(
                &transaction,
                &mut streams,
                part + 1,
                &delta("long", &chunk),
                1.0,
            )
            .expect("delta");
            assert_eq!(text(&transaction, "agent:long"), expected);
            assert_eq!(base_rows(&transaction), 0);
            assert_eq!(search(&transaction, "agent:long"), None);
        }
        dispatch(&transaction, &mut streams, 31, &json!({"method":"item/completed","params":{"threadId":"thread","turnId":"turn","item":{"id":"long","type":"agentMessage","text":""}}}), 2.0).expect("completed");
        assert_eq!(text(&transaction, "agent:long"), expected);
        assert_eq!(
            search(&transaction, "agent:long").as_deref(),
            Some(expected.as_str())
        );
        // A snapshot after a flush: later deltas continue the flushed text.
        dispatch(&transaction, &mut streams, 32, &delta("short", "one"), 3.0).expect("delta");
        assert_eq!(base_rows(&transaction), 0);
        dispatch(&transaction, &mut streams, 33, &json!({"method":"item/started","params":{"threadId":"thread","turnId":"turn","item":{"id":"short","type":"agentMessage","text":"snap"}}}), 4.0).expect("started");
        assert_eq!(text(&transaction, "agent:short"), "snap");
        assert_eq!(base_rows(&transaction), 1);
        dispatch(&transaction, &mut streams, 34, &delta("short", "two"), 5.0).expect("delta");
        assert_eq!(text(&transaction, "agent:short"), "onetwo");
        assert_eq!(base_rows(&transaction), 0);
        dispatch(&transaction, &mut streams, 35, &json!({"method":"item/completed","params":{"threadId":"thread","turnId":"turn","item":{"id":"short","type":"agentMessage","text":""}}}), 6.0).expect("completed");
        assert_eq!(text(&transaction, "agent:short"), "onetwo");
        assert_eq!(
            search(&transaction, "agent:short").as_deref(),
            Some("onetwo")
        );
        assert_eq!(base_rows(&transaction), 0);
    }

    #[test]
    fn long_answers_use_canonical_bounded_agent_sync_projection() {
        let mut connection = database();
        let mut agent = TranscriptRepository::agent(&connection, "agent")
            .expect("agent reads")
            .expect("agent exists");
        agent["turnEpoch"] = json!(99);
        agent["nativeFailureHold"] = json!({"reason":"hidden"});
        agent["lastCompletedTurnError"] = json!({"message":"hidden"});
        agent["startOutcomeHold"] = json!({"attempt":"hidden"});
        agent["restartRecovery"] = json!({"stage":"held"});
        agent["startAttempt"] = json!({"id":"private-attempt","submitted":true});
        connection
            .execute(
                "UPDATE runtime_agents SET record=?1 WHERE id='agent'",
                [agent.to_string()],
            )
            .expect("runtime-only sync fields store");
        let long_answer = "x".repeat(5_001);
        let notification = json!({"method":"item/completed","params":{"threadId":"thread","turnId":"turn","item":{"id":"long","type":"agentMessage","text":long_answer}}});
        let mut streams = NotificationStreamBuffer::default();
        let transaction = connection
            .transaction()
            .expect("long answer transaction begins");
        dispatch(&transaction, &mut streams, 1, &notification, 2.0).expect("long answer writes");
        transaction.commit().expect("long answer commits");
        let raw_payload: String = connection
            .query_row(
                "SELECT payload FROM sync_entities WHERE collection='agent' AND id='agent'",
                [],
                |row| row.get(0),
            )
            .expect("agent sync payload reads");
        let payload: Value =
            serde_json::from_str(&raw_payload).expect("agent sync payload is JSON");
        assert_eq!(
            payload["value"]["lastAnswer"].as_str().map(str::len),
            Some(4_000)
        );
        for field in [
            "turnEpoch",
            "nativeFailureHold",
            "lastCompletedTurnError",
            "startOutcomeHold",
            "restartRecovery",
        ] {
            assert!(
                payload["value"].get(field).is_none(),
                "runtime field {field} leaked"
            );
        }
        assert!(payload["value"]["startAttempt"].get("id").is_none());
    }
}

#[cfg(test)]
#[path = "notification_dispatch/review_tests.rs"]
mod review_tests;
