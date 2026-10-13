//! Transactional provider notifications with replay-safe receipts and checked stored state.

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_providers::Frame;
use studio_runtime_store::{
    NotificationDispatchRepository, StoreError, SyncDocumentsRepository, Transaction, sha256_hex,
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
pub use effects::{NotificationDependency, NotificationEffects, UnavailableNotificationEffects};

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

#[derive(Clone, Debug, Eq, PartialEq)]
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
}

#[derive(Debug, Error)]
pub enum NotificationDispatchError {
    #[error("invalid provider notification or delivery identity")]
    InvalidNotification,
    #[error("open notification stream capacity reached")]
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

/// Stored outcome and a staged connection-buffer replacement.
#[derive(Clone, Debug, PartialEq)]
pub struct NotificationDispatchResult {
    pub outcome: DispatchOutcome,
    pub stream_update: Option<NotificationStreamBuffer>,
}

/// The caller owns the transaction and the connection's stream buffer. Commit
/// SQLite first, then apply `stream_update` (when present) to that buffer. On
/// rollback or commit failure discard the update. This call never mutates the
/// supplied buffer; a saved receipt returns the saved outcome and no update.
/// `observed_at` is never part of delivery identity.
pub fn dispatch_notification(
    transaction: &Transaction<'_>,
    agent_id: &str,
    state_dir: &str,
    streams: &NotificationStreamBuffer,
    identity: &ProviderDeliveryIdentity,
    notification: &Frame,
    observed_at: f64,
) -> Result<NotificationDispatchResult, NotificationDispatchError> {
    dispatch_notification_with_effects(
        transaction,
        agent_id,
        state_dir,
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
    state_dir: &str,
    streams: &NotificationStreamBuffer,
    identity: &ProviderDeliveryIdentity,
    notification: &Frame,
    observed_at: f64,
    effects: &dyn NotificationEffects,
) -> Result<NotificationDispatchResult, NotificationDispatchError> {
    let mut next_streams = streams.clone();
    let result = in_savepoint(transaction, || {
        if !observed_at.is_finite()
            || observed_at < 0.0
            || identity.provider_handle.is_empty()
            || identity.generation == 0
            || identity.cursor == 0
        {
            return Err(NotificationDispatchError::InvalidNotification);
        }
        let (kind, params, payload) = notification_parts(notification)?;
        let content_signature = sha256_json(&json!({"agent": agent_id, "notification": payload}))?;
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
        validate_agent(&agent, agent_id, &identity.provider_handle, &kind, &params)?;
        let unload = kind == "thread/closed"
            || (kind == "thread/status/changed" && params["status"]["type"] == "notLoaded");
        if (unload || matches!(kind.as_str(), "error" | "model/safetyBuffering/updated"))
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
                StreamFlushOptions {
                    allow_reassigned_owner: false,
                    close_stream: unload,
                },
                observed_at,
            )?;
            if agent != before {
                persist_agent(transaction, agent_id, &agent, state_dir)?;
            }
        }
        if kind == "item/commandExecution/outputDelta" {
            let key = command_key(agent_id, identity, &params)?;
            let delta = params["delta"]
                .as_str()
                .ok_or(NotificationDispatchError::InvalidNotification)?;
            let outcome = match next_streams.enqueue_delta(key, delta, &params) {
                EnqueueResult::Buffered => DispatchOutcome::Written,
                EnqueueResult::Closed => DispatchOutcome::Ignored {
                    reason: "closed-stream".into(),
                },
                EnqueueResult::Full => return Err(NotificationDispatchError::StreamCapacity),
                EnqueueResult::FlushRequired => {
                    unreachable!("command output retains a bounded tail")
                }
            };
            return save_outcome(transaction, &receipt_id, &content_signature, outcome);
        }
        if kind == "item/completed" && params["item"]["type"] != "agentMessage" {
            let mut scoped = params.clone();
            scoped["itemId"] = params["item"]["id"].clone();
            let key = command_key(agent_id, identity, &scoped)?;
            flush_selected(
                transaction,
                agent_id,
                &mut agent,
                &mut next_streams,
                Some(&key),
                observed_at,
                StreamFlushOptions {
                    allow_reassigned_owner: false,
                    close_stream: true,
                },
            )?;
        }
        if kind == "item/started" && params["item"]["type"] == "commandExecution" {
            let mut scoped = params.clone();
            scoped["itemId"] = params["item"]["id"].clone();
            let key = command_key(agent_id, identity, &scoped)?;
            kinds::preserve_command_base(transaction, agent_id, &mut next_streams, &key)?;
        }
        if let Some(outcome) = kinds::apply(
            transaction,
            &mut agent,
            &kind,
            &params,
            identity,
            state_dir,
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

        let before = agent.clone();
        let outcome = match kind.as_str() {
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
                                    identity,
                                    params: &params,
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
                        let item_id = item.get("id").and_then(Value::as_str).unwrap_or_default();
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
                                identity,
                                params: &params,
                                item_id,
                                observed_at,
                                update,
                            },
                        )? {
                            if kind == "item/completed" && has_questions(item.get("questions")) {
                                questions::completed(transaction, &agent, &params, observed_at)?;
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
                effects.capture_usage(transaction, &mut agent, &params, observed_at)?;
                if crate::event_write::truthy(agent.get("movedFrom")) {
                    effects.capture_move_cache(transaction, &agent, &params, observed_at)?;
                }
                apply_token_usage(&mut agent, &params, observed_at)?
            }
            _ => return Err(NotificationDispatchError::InvalidNotification),
        };

        if kind == "item/agentMessage/delta" || matches!(outcome, DispatchOutcome::Ignored { .. }) {
            if agent != before {
                persist_agent(transaction, agent_id, &agent, state_dir)?;
            }
            return save_outcome(transaction, &receipt_id, &content_signature, outcome);
        }
        if !stale_item
            && !matches!(
                outcome,
                DispatchOutcome::Ignored { .. } | DispatchOutcome::Unhandled { .. }
            )
        {
            update_common_agent_fields(&mut agent, &kind, &params, observed_at)?;
        }
        if !matches!(outcome, DispatchOutcome::Ignored { .. }) {
            persist_agent(transaction, agent_id, &agent, state_dir)?;
        }
        save_outcome(transaction, &receipt_id, &content_signature, outcome)
    });
    result.map(|outcome| NotificationDispatchResult {
        outcome,
        stream_update: (next_streams != *streams).then_some(next_streams),
    })
}

fn command_key(
    agent_id: &str,
    identity: &ProviderDeliveryIdentity,
    params: &Value,
) -> Result<StreamKey, NotificationDispatchError> {
    Ok(StreamKey {
        agent_id: agent_id.into(),
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
    identity: &'a ProviderDeliveryIdentity,
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

fn notification_parts(
    notification: &Frame,
) -> Result<(String, Value, Value), NotificationDispatchError> {
    let (kind, params, fields) = match notification {
        Frame::Notification(frame) => (
            frame.method.as_str().to_owned(),
            frame.params.clone().unwrap_or(Value::Null),
            frame.fields.clone(),
        ),
        Frame::UnknownMethod(frame) => (
            frame.method.clone(),
            frame.fields.get("params").cloned().unwrap_or(Value::Null),
            frame.fields.clone(),
        ),
        Frame::Request(_) | Frame::Response { .. } => {
            return Err(NotificationDispatchError::InvalidNotification);
        }
    };
    let mut payload = fields;
    payload.insert("method".to_owned(), Value::String(kind.clone()));
    if !params.is_null() {
        payload.insert("params".to_owned(), params.clone());
    }
    Ok((kind, params, Value::Object(payload)))
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
        identity,
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
        account: identity
            .provider_handle
            .strip_prefix("account:")
            .unwrap_or_default()
            .to_owned(),
        generation: identity.generation,
        thread: thread.to_owned(),
        turn: turn.clone(),
        item: item_id.to_owned(),
        method: "item/agentMessage/delta".to_owned(),
    };
    let old = NotificationDispatchRepository::transcript_item(transaction, agent_id, &id)?;
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
                    true,
                )?;
                Ok(true)
            }
            EnqueueResult::Closed => Ok(false),
            EnqueueResult::Full => Err(NotificationDispatchError::StreamCapacity),
        };
    }
    let previous = old
        .as_ref()
        .map(|(record, _)| saved_text(transaction, &id, record))
        .transpose()?
        .unwrap_or_default();
    if matches!(update, TextUpdate::Started(..)) {
        for existing in streams.selected(Some(&key)) {
            if streams.preserve_base(&existing, sha256_hex(previous.as_bytes())) {
                NotificationDispatchRepository::put_stream_base(
                    transaction,
                    &stream_base_id(&existing),
                    Some(&previous),
                )?;
            }
        }
    }
    let (text, streaming, phase, index_search, completed) = match update {
        TextUpdate::Started(snapshot, phase) => (
            snapshot.unwrap_or_default().to_owned(),
            true,
            phase.cloned().unwrap_or(Value::Null),
            true,
            false,
        ),
        TextUpdate::Delta(_) => unreachable!("delta handled before transcript read"),
        TextUpdate::Completed(snapshot, phase) => {
            flush_selected(
                transaction,
                agent_id,
                agent,
                streams,
                Some(&key),
                observed_at,
                StreamFlushOptions {
                    allow_reassigned_owner: false,
                    close_stream: true,
                },
            )?;
            let latest =
                NotificationDispatchRepository::transcript_item(transaction, agent_id, &id)?;
            let latest_text = latest
                .as_ref()
                .map(|(record, _)| saved_text(transaction, &id, record))
                .transpose()?
                .unwrap_or(previous);
            (
                snapshot
                    .filter(|text| !text.is_empty())
                    .unwrap_or(&latest_text)
                    .to_owned(),
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
    state_dir: &str,
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
            StreamFlushOptions {
                allow_reassigned_owner: false,
                close_stream: false,
            },
        )?;
        if agent != before {
            persist_agent(transaction, agent_id, &agent, state_dir)?;
        }
        Ok((next != *streams).then_some(next))
    })
}

pub(crate) fn prune_inactive_streams(
    transaction: &Transaction<'_>,
    streams: &mut NotificationStreamBuffer,
    observed_at: f64,
    recovering: bool,
) -> Result<(), NotificationDispatchError> {
    for key in streams.selected(None) {
        let active: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_agents WHERE COALESCE(json_extract(record,'$.accountKey'),'default')=?1 AND json_extract(record,'$.threadId')=?2 AND (?3 IS NULL OR json_extract(record,'$.turnId')=?3) AND json_extract(record,'$.deletedAt') IS NULL AND (json_extract(record,'$.inFlight')=1 OR json_extract(record,'$.status') IN ('running','starting','approval')))",
            rusqlite::params![key.account,key.thread,key.turn], |row| row.get(0),
        ).map_err(StoreError::from)?;
        if recovering && active && !streams.has_pending(&key) && streams.has_base(&key) {
            let owner = if key.agent_id.is_empty() {
                transaction
                    .query_row(
                        "SELECT id,record FROM runtime_agents WHERE COALESCE(json_extract(record,'$.accountKey'),'default')=?1 AND json_extract(record,'$.threadId')=?2",
                        rusqlite::params![key.account, key.thread],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()
            } else {
                transaction
                    .query_row(
                        "SELECT id,record FROM runtime_agents WHERE id=?1",
                        [&key.agent_id],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()
            }
            .map_err(StoreError::from)?;
            if let Some((id, raw)) = owner {
                let mut projection: Value =
                    serde_json::from_str(&raw).map_err(|_| stored("runtime_agents", &id))?;
                projection["turnId"] = json!(key.turn);
                projection["deletedAt"] = Value::Null;
                flush_selected(
                    transaction,
                    &id,
                    &mut projection,
                    streams,
                    Some(&key),
                    observed_at,
                    StreamFlushOptions {
                        allow_reassigned_owner: true,
                        close_stream: false,
                    },
                )?;
            }
            continue;
        }
        if !active {
            // Cursor acknowledgement already made this text durable. Retiring a
            // stream must materialize it even after its turn/agent becomes inactive.
            let owner = if key.agent_id.is_empty() {
                transaction
                    .query_row(
                        "SELECT id,record FROM runtime_agents WHERE COALESCE(json_extract(record,'$.accountKey'),'default')=?1 AND json_extract(record,'$.threadId')=?2",
                        rusqlite::params![key.account, key.thread],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()
            } else {
                transaction
                    .query_row(
                        "SELECT id,record FROM runtime_agents WHERE id=?1",
                        [&key.agent_id],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()
            }
            .map_err(StoreError::from)?;
            let Some((id, raw)) = owner else {
                // The owner may have been deleted or have legacy identity that
                // cannot be recovered. Keep the durable stream, but do not make
                // account startup fail permanently.
                continue;
            };
            let mut projection: Value =
                serde_json::from_str(&raw).map_err(|_| stored("runtime_agents", &id))?;
            // Only the transcript is written. Never restore an obsolete lifecycle.
            projection["turnId"] = json!(key.turn);
            projection["deletedAt"] = Value::Null;
            flush_selected(
                transaction,
                &id,
                &mut projection,
                streams,
                Some(&key),
                observed_at,
                StreamFlushOptions {
                    allow_reassigned_owner: true,
                    close_stream: true,
                },
            )?;
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

fn stream_base_id(key: &StreamKey) -> String {
    #[derive(Serialize)]
    struct StreamIdentity<'a> {
        account: &'a str,
        generation: u64,
        thread: &'a str,
        turn: &'a Option<String>,
        item: &'a str,
        method: &'a str,
    }
    let durable_identity = StreamIdentity {
        account: &key.account,
        generation: key.generation,
        thread: &key.thread,
        turn: &key.turn,
        item: &key.item,
        method: &key.method,
    };
    format!(
        "account-worker:stream-base:{}",
        sha256_hex(
            serde_json::to_string(&durable_identity)
                .expect("stream identity serialization")
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

#[derive(Clone, Copy)]
struct StreamFlushOptions {
    allow_reassigned_owner: bool,
    close_stream: bool,
}

fn flush_selected(
    transaction: &Transaction<'_>,
    agent_id: &str,
    agent: &mut Value,
    streams: &mut NotificationStreamBuffer,
    terminal: Option<&StreamKey>,
    observed_at: f64,
    options: StreamFlushOptions,
) -> Result<(), NotificationDispatchError> {
    let keys = streams.selected(terminal);
    flush_keys(
        transaction,
        agent_id,
        agent,
        streams,
        keys,
        options,
        observed_at,
    )
}

fn flush_keys(
    transaction: &Transaction<'_>,
    agent_id: &str,
    agent: &mut Value,
    streams: &mut NotificationStreamBuffer,
    keys: Vec<StreamKey>,
    options: StreamFlushOptions,
    observed_at: f64,
) -> Result<(), NotificationDispatchError> {
    for key in keys {
        let reassigned = agent.get("threadId").and_then(Value::as_str) != Some(key.thread.as_str())
            || agent
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
                != key.account;
        if reassigned && !(options.allow_reassigned_owner && key.agent_id == agent_id) {
            continue;
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
            if options.close_stream {
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
                options.close_stream,
                observed_at,
            )?;
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
            !reassigned && !options.allow_reassigned_owner,
        )?;
        if options.close_stream {
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
    update_agent: bool,
) -> Result<(), NotificationDispatchError> {
    let id = format!("{agent_id}:{}", key.item);
    let old = NotificationDispatchRepository::transcript_item(transaction, agent_id, &id)?;
    let previous = old
        .as_ref()
        .map(|(record, _)| saved_text(transaction, &id, record))
        .transpose()?
        .unwrap_or_default();
    let previous = durable_stream_base(transaction, streams, key)?.unwrap_or(previous);
    if let Some((text, count, params)) = streams.flush_with_delta(key, &previous, delta) {
        NotificationDispatchRepository::put_stream_base(transaction, &stream_base_id(key), None)?;
        let phase = old
            .as_ref()
            .and_then(|(record, _)| record.get("phase"))
            .cloned()
            .unwrap_or(Value::Null);
        let turn = key
            .turn
            .as_deref()
            .or_else(|| agent.get("turnId").and_then(Value::as_str));
        let mut record = assistant_item_record(&id, &text, observed_at, true, turn, phase);
        if let Some(status) = old
            .as_ref()
            .and_then(|(record, _)| record.get("turnStatus"))
        {
            record["turnStatus"] = status.clone();
        }
        NotificationDispatchRepository::put_transcript_item(
            transaction,
            &id,
            agent_id,
            &record,
            observed_at,
            &text,
            text.chars().count() > ITEM_PREVIEW_CHARS,
        )?;
        if update_agent && count > 0 {
            update_common_agent_fields(agent, "item/agentMessage/delta", &params, observed_at)?;
            agent["events"] = Value::from(
                agent["events"]
                    .as_u64()
                    .unwrap_or_default()
                    .saturating_add(count - 1),
            );
            agent["activity"] = json!({"phase":"writing", "at":observed_at});
            agent["tail"] = Value::String(suffix(&text, 300));
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

fn persist_agent(
    transaction: &Transaction<'_>,
    agent_id: &str,
    agent: &Value,
    state_dir: &str,
) -> Result<(), NotificationDispatchError> {
    let mut normalized = agent.clone();
    crate::agents_create::mode_fields(&mut normalized);
    let agent = &normalized;
    NotificationDispatchRepository::put_agent_record(transaction, agent_id, agent)?;
    let project_server_id = crate::sync_entity_context::local_server_id(state_dir)?;
    let context = crate::sync_entity_context::agent_view_context(
        transaction,
        agent,
        project_server_id.as_deref(),
    )
    .map_err(|_| stored("runtime_agents", agent_id))?;
    let view = crate::sync_views::agent_entity_view(agent, &context)
        .ok_or_else(|| stored("runtime_agents", agent_id))?;
    SyncDocumentsRepository::put_entity(transaction, "agent", agent_id, view, false)?;
    Ok(())
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
mod tests {
    use rusqlite::{Connection, params};
    use serde_json::{Value, json};

    use super::{
        DispatchOutcome, NotificationStreamBuffer, PreparedSubmission, ProviderDeliveryIdentity,
        SubmittedInput, record_user_submission,
    };
    use studio_runtime_providers::{Frame, LineDecoder};
    use studio_runtime_store::{NotificationDispatchRepository, TranscriptRepository};

    // Legacy fixtures model a transaction-local buffer with an immediate timer.
    // The corpus exercises the caller's commit/apply and rollback/drop boundary.
    fn dispatch_notification(
        transaction: &rusqlite::Transaction<'_>,
        agent: &str,
        state: &str,
        streams: &mut NotificationStreamBuffer,
        identity: &ProviderDeliveryIdentity,
        frame: &Frame,
        at: f64,
    ) -> Result<DispatchOutcome, super::NotificationDispatchError> {
        let result =
            super::dispatch_notification(transaction, agent, state, streams, identity, frame, at)?;
        if let Some(update) = result.stream_update {
            *streams = update;
        }
        if let Some(update) =
            super::flush_notification_streams(transaction, agent, state, streams, at)?
        {
            *streams = update;
        }
        Ok(result.outcome)
    }

    fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
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
                 INSERT INTO sync_entities VALUES('agent','agent',1,'old','{\"collection\":\"agent\",\"id\":\"agent\",\"value\":{\"id\":\"agent\",\"status\":\"running\",\"threadId\":\"thread\",\"accountKey\":\"default\",\"turnId\":\"turn\",\"inFlight\":true,\"events\":0}}',0);",
            )
            .expect("notification dispatch tables create");
        connection
    }

    fn frame(value: &Value) -> Frame {
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

    #[test]
    fn stale_safety_notice_keeps_current_turn_stream_pending() {
        let mut connection = database();
        let tx = connection.transaction().expect("transaction");
        let initial = super::dispatch_notification(&tx,"agent",".",&NotificationStreamBuffer::default(),&identity(1),&frame(&json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"answer","delta":"pending"}})),1234.5).expect("buffer delta");
        let streams = initial.stream_update.expect("staged stream");
        let result = super::dispatch_notification(&tx,"agent",".",&streams,&identity(2),&frame(&json!({"method":"model/safetyBuffering/updated","params":{"threadId":"thread","turnId":"old-turn","showBufferingUi":true}})),1234.5).expect("stale notice");
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
    fn injected_unload_refusal_keeps_pending_text_and_has_no_receipt() {
        let mut connection = database();
        let tx = connection.transaction().expect("transaction");
        let initial = super::dispatch_notification(&tx,"agent",".",&NotificationStreamBuffer::default(),&identity(1),&frame(&json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"answer","delta":"pending"}})),1234.5).expect("buffer delta");
        let streams = initial.stream_update.expect("staged stream");
        let before = streams.clone();
        let result = super::dispatch_notification_with_effects(
            &tx,
            "agent",
            ".",
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
                ".",
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
    fn bounded_open_streams_refuse_without_writing() {
        let cases: Vec<Value> = serde_json::from_str(include_str!(
            "../tests/turn_lifecycle/notification_cases.json"
        ))
        .expect("cases");
        let case = cases
            .iter()
            .find(|c| c["id"] == "bounded-open-streams")
            .expect("boundary case");
        let mut streams = NotificationStreamBuffer::default();
        for index in 0..case["bound"].as_u64().expect("bound") {
            assert!(matches!(
                streams.enqueue_delta(
                    super::stream::StreamKey {
                        agent_id: "agent".into(),
                        account: "default".into(),
                        generation: 7,
                        thread: "thread".into(),
                        turn: Some("turn".into()),
                        item: index.to_string(),
                        method: "item/agentMessage/delta".into(),
                    },
                    "pending",
                    &json!({})
                ),
                super::stream::EnqueueResult::Buffered
            ));
        }
        let original = streams.clone();
        let mut connection = database();
        let tx = connection.transaction().expect("transaction");
        let before = tx.total_changes();
        let result = super::dispatch_notification(
            &tx,
            "agent",
            ".",
            &streams,
            &identity(1),
            &frame(
                &json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"overflow","delta":"retained"}}),
            ),
            1.0,
        );
        assert!(matches!(
            result,
            Err(super::NotificationDispatchError::StreamCapacity)
        ));
        assert_eq!(tx.total_changes(), before);
        assert_eq!(streams, original);
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
        let mut connection = database();
        let mut streams = NotificationStreamBuffer::default();
        let transaction = connection.transaction().expect("transaction begins");
        dispatch(&transaction, &mut streams, 1, &json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"answer","delta":"before"}}), 1.0).expect("early delta writes");
        dispatch(&transaction, &mut streams, 2, &json!({"method":"item/started","params":{"threadId":"thread","turnId":"turn","item":{"id":"answer","type":"agentMessage","text":"snapshot"}}}), 2.0).expect("snapshot replaces flushed text");
        dispatch(&transaction, &mut streams, 3, &json!({"method":"item/completed","params":{"threadId":"thread","turnId":"turn","item":{"id":"answer","type":"agentMessage","text":""}}}), 3.0).expect("empty completion flushes text");
        dispatch(&transaction, &mut streams, 4, &json!({"method":"item/completed","params":{"threadId":"thread","turnId":"turn","item":{"id":"late","type":"agentMessage","text":"final"}}}), 4.0).expect("completion without a pending stream writes");
        dispatch(&transaction, &mut streams, 5, &json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"late","delta":"late"}}), 5.0).expect("first late delta starts an open stream");
        let new_generation = ProviderDeliveryIdentity {
            provider_handle: "account:default".to_owned(),
            generation: 8,
            cursor: 6,
        };
        assert_eq!(
            dispatch_notification(
                &transaction,
                "agent",
                ".",
                &mut streams,
                &new_generation,
                &frame(&json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"answer","delta":"late"}})),
                6.0,
            )
            .expect("new generation is not blocked by the old closed stream"),
            DispatchOutcome::Written
        );
        transaction.commit().expect("notification sequence commits");

        let answer = TranscriptRepository::item(&connection, "agent", "agent:answer")
            .expect("answer reads")
            .expect("answer exists");
        assert_eq!(answer.record["text"], "snapshotlate");
        assert_eq!(answer.record["streaming"], true);
        let late = TranscriptRepository::item(&connection, "agent", "agent:late")
            .expect("late row reads")
            .expect("late row exists");
        assert_eq!(late.record["text"], "finallate");
        assert_eq!(late.record["streaming"], true);
    }

    #[test]
    fn reassigned_stream_owner_materializes_and_missing_owner_does_not_block_recovery() {
        let mut connection = database();
        let transaction = connection.transaction().expect("transaction begins");
        let key = super::stream::StreamKey {
            agent_id: "agent".into(),
            account: "default".into(),
            generation: 7,
            thread: "thread".into(),
            turn: Some("turn".into()),
            item: "answer".into(),
            method: "item/agentMessage/delta".into(),
        };
        let mut streams = NotificationStreamBuffer::default();
        assert!(matches!(
            streams.enqueue_delta(key, "retained text", &json!({})),
            super::stream::EnqueueResult::Buffered
        ));
        transaction
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.accountKey','new-account','$.threadId','new-thread') WHERE id='agent'",
                [],
            )
            .expect("transfer stream owner");
        super::prune_inactive_streams(&transaction, &mut streams, 10.0, true)
            .expect("reassigned stream materializes");
        transaction.commit().expect("owner transcript commits");
        let item = TranscriptRepository::item(&connection, "agent", "agent:answer")
            .expect("transcript reads")
            .expect("reassigned stream is retained");
        assert_eq!(item.record["text"], "retained text");
        assert_eq!(streams.pending_bytes(), 0);

        let mut connection = database();
        let transaction = connection.transaction().expect("orphan transaction begins");
        transaction
            .execute("DELETE FROM runtime_agents WHERE id='agent'", [])
            .expect("remove stream owner");
        let mut streams = NotificationStreamBuffer::default();
        streams.enqueue_delta(
            super::stream::StreamKey {
                agent_id: "agent".into(),
                account: "default".into(),
                generation: 7,
                thread: "thread".into(),
                turn: Some("turn".into()),
                item: "orphan".into(),
                method: "item/agentMessage/delta".into(),
            },
            "still durable",
            &json!({}),
        );
        super::prune_inactive_streams(&transaction, &mut streams, 11.0, true)
            .expect("ownerless stream does not block account recovery");
        assert_eq!(streams.pending_bytes(), "still durable".len());
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
        assert!(error.to_string().contains("runtime_agents:agent"));
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
