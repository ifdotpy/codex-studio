//! Persist the notification kinds handled by this Rust slice.
//!
//! | Provider kind | Partial reason when not handled |
//! |---|---|
//! | `turn/started` | `turn-lifecycle-not-ported` |
//! | `turn/completed` | `turn-lifecycle-not-ported` |
//! | `turn/plan/updated` | `turn-lifecycle-not-ported` |
//! | `turn/diff/updated` | `turn-lifecycle-not-ported` |
//! | `item/started` / `item/completed` with `userMessage` | `non-text-provider-item` |
//! | `item/started` / `item/completed` with `reasoning` | `non-text-provider-item` |
//! | `item/started` / `item/completed` with `commandExecution` | `non-text-provider-item` |
//! | `item/started` / `item/completed` with `dynamicToolCall` | `non-text-provider-item` |
//! | `item/started` / `item/completed` with `mcpToolCall` | `non-text-provider-item` |
//! | `item/started` / `item/completed` with `webSearch` | `non-text-provider-item` |
//! | `item/started` / `item/completed` with `fileChange` | `non-text-provider-item` |
//! | `item/started` / `item/completed` with `contextCompaction` | `non-text-provider-item` |
//! | `item/started` / `item/completed` with `exitedReviewMode` | `non-text-provider-item` |
//! | `item/started` / `item/completed` with any other non-`agentMessage` type | `non-text-provider-item` |
//! | `item/agentMessage/delta` | handled as text |
//! | `item/started` / `item/completed` with `agentMessage` | handled as text; questions add `questions-service/async-question` |
//! | `thread/tokenUsage/updated` | partial reasons: `usage-policy/budget-capture`, `agent-move/cache-capture-not-ported`, `analytics/token-rate-observation-not-ported` |
//! | `serverRequest/resolved` | `approval-question-or-non-text-item-not-ported` |
//! | `item/commandExecution/outputDelta` | `approval-question-or-non-text-item-not-ported` |
//! | `item/reasoning/textDelta` | `approval-question-or-non-text-item-not-ported` |
//! | `item/reasoning/summaryTextDelta` | `approval-question-or-non-text-item-not-ported` |
//! | `item/autoApprovalReview/started` / `item/autoApprovalReview/completed` | `approval-question-or-non-text-item-not-ported` |
//! | `error` | `provider-error-state-not-ported` |
//! | `model/safetyBuffering/updated` | `provider-notice-not-ported` |
//! | `model/verification` | `provider-notice-not-ported` |
//! | `modelProvider/authRecoveryStarted` / `modelProvider/authRecoveryCompleted` | `provider-notice-not-ported` |
//! | `warning` | `notification-kind-not-ported` |
//! | `guardianWarning` | `notification-kind-not-ported` |
//! | `configWarning` | `notification-kind-not-ported` |
//! | `deprecationNotice` | `notification-kind-not-ported` |
//! | `mcpServer/startupStatus/updated` | `notification-kind-not-ported` |
//! | `mcpServer/oauthLogin/completed` | `notification-kind-not-ported` |
//! | `autoApprovalReview/strictReviewRequired` | `notification-kind-not-ported` |
//! | `hook/started` / `hook/completed` | `notification-kind-not-ported` |
//! | `account/login/completed` | `notification-kind-not-ported` |
//! | `account/updated` | `notification-kind-not-ported` |
//! | `account/rateLimits/updated` | `notification-kind-not-ported` |
//! | `command/exec/outputDelta` | `notification-kind-not-ported` |
//! | `provider/generationStarted` | `notification-kind-not-ported` |
//! | `thread/closed` | `notification-kind-not-ported` |
//! | `thread/status/changed` | `notification-kind-not-ported` |
//! | `thread/realtime/item/started` | `notification-kind-not-ported` |
//! | `thread/realtime/item/completed` | `notification-kind-not-ported` |
//! | `thread/realtime/transcript/delta` | `notification-kind-not-ported` |
//! | `thread/realtime/transcript/done` | `notification-kind-not-ported` |
//! | `thread/realtime/sdp` | `notification-kind-not-ported` |
//! | `thread/realtime/error` | `notification-kind-not-ported` |
//! | `thread/realtime/closed` | `notification-kind-not-ported` |
//! | Any other provider kind | `notification-kind-not-ported` |
//!
//! Unhandled kinds only increment the named counters. Their Python side effects
//! remain owned by the follow-up slices.

use serde_json::{Map, Value, json};
use studio_runtime_providers::Frame;
use studio_runtime_store::{
    EventWriteRepository, NotificationDispatchRepository, StoreError, Transaction, sha256_hex,
};
use thiserror::Error;

const ITEM_PREVIEW_CHARS: usize = 20_000;
const SUBMISSION_RECEIPT_PREFIX: &str = "provider-submission:";
const DELIVERY_RECEIPT_PREFIX: &str = "provider-notification:";
const SAVEPOINT: &str = "notification_dispatch_scope";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DispatchOutcome {
    Written,
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
    #[error("invalid stored object {table}:{id}")]
    InvalidStoredObject { table: &'static str, id: String },
    #[error("delivery identity was reused with different content")]
    ConflictingDelivery,
    #[error("submitted input identity was reused with different content")]
    ConflictingInput,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("JSON serialization failed")]
    Json(#[from] serde_json::Error),
}

/// Persist Python's submitted batch before the provider turn begins.
pub fn record_user_submission(
    transaction: &Transaction<'_>,
    agent_id: &str,
    submission: &PreparedSubmission,
) -> Result<DispatchOutcome, NotificationDispatchError> {
    in_savepoint(transaction, || {
        validate_submission(transaction, agent_id, submission)?;
        let id = format!("{agent_id}:{}", submission.first_event_id);
        let record = submission_record(&id, submission);
        let signature = sha256_json(&json!({
            "agent": agent_id,
            "firstEventId": submission.first_event_id,
            "submittedAt": submission.submitted_at,
            "createdAt": submission.created_at,
            "record": record,
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
            let Some((existing, created)) =
                NotificationDispatchRepository::transcript_item(transaction, agent_id, &id)?
            else {
                return Err(stored("runtime_items", &id));
            };
            if existing != record
                || saved_text(transaction, &id, &existing)? != submission.text
                || created != submission.created_at
            {
                return Err(stored("runtime_items", &id));
            }
            return decode_outcome(&receipt.result, &receipt_id);
        }
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
            let outcome = DispatchOutcome::Written;
            let result = outcome_json(&outcome).to_string();
            NotificationDispatchRepository::put_notification_receipt(
                transaction,
                &receipt_id,
                &signature,
                &result,
            )?;
            return Ok(outcome);
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
        let outcome = DispatchOutcome::Written;
        let result = outcome_json(&outcome).to_string();
        NotificationDispatchRepository::put_notification_receipt(
            transaction,
            &receipt_id,
            &signature,
            &result,
        )?;
        Ok(outcome)
    })
}

/// Apply one decoded provider frame using its durable supervisor delivery ID.
/// `observed_at` is used for first-write timestamps and is never part of identity.
pub fn dispatch_notification(
    transaction: &Transaction<'_>,
    agent_id: &str,
    identity: &ProviderDeliveryIdentity,
    notification: &Frame,
    observed_at: f64,
) -> Result<DispatchOutcome, NotificationDispatchError> {
    in_savepoint(transaction, || {
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
        validate_agent(&agent, agent_id, &identity.provider_handle, &kind, &params)?;
        validate_agent_write_fields(&agent, agent_id)?;
        let turn_id = params.get("turnId").and_then(Value::as_str);
        let stale_item = kind.starts_with("item/")
            && turn_id
                .is_some_and(|turn| agent.get("turnId").and_then(Value::as_str) != Some(turn));

        let mut extra_reasons = Vec::new();
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
                                agent_id,
                                &mut agent,
                                item_id,
                                observed_at,
                                TextUpdate::Delta(delta),
                            )? {
                                DispatchOutcome::Written
                            } else {
                                DispatchOutcome::Ignored {
                                    reason: "closed-stream".to_owned(),
                                }
                            }
                        }
                        (None | Some(""), _) => unhandled(kind.clone(), "missing-item-identity"),
                        _ => unhandled(kind.clone(), "invalid-text-delta"),
                    }
                }
            }
            "item/started" | "item/completed" => {
                match params.get("item").and_then(Value::as_object) {
                    None => unhandled(kind.clone(), "missing-provider-item"),
                    Some(item)
                        if item.get("type").and_then(Value::as_str) != Some("agentMessage") =>
                    {
                        unhandled(kind.clone(), "non-text-provider-item")
                    }
                    Some(item)
                        if item
                            .get("id")
                            .and_then(Value::as_str)
                            .is_none_or(str::is_empty) =>
                    {
                        unhandled(kind.clone(), "missing-item-identity")
                    }
                    Some(_) if stale_item => DispatchOutcome::Ignored {
                        reason: "stale-turn-item".to_owned(),
                    },
                    Some(item) if item.get("text").is_some_and(|text| !text.is_string()) => {
                        unhandled(kind.clone(), "invalid-item-text")
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
                            agent_id,
                            &mut agent,
                            item_id,
                            observed_at,
                            update,
                        )? {
                            if kind == "item/completed" && has_questions(item.get("questions")) {
                                extra_reasons.push("questions-service/async-question");
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
                extra_reasons.extend([
                    "usage-policy/budget-capture",
                    "agent-move/cache-capture-not-ported",
                    "analytics/token-rate-observation-not-ported",
                ]);
                apply_token_usage(&mut agent, &params, observed_at)?
            }
            _ => {
                let reason = unhandled_reason(&kind);
                unhandled(kind.clone(), reason)
            }
        };

        if matches!(outcome, DispatchOutcome::Ignored { .. }) {
            let result = outcome_json(&outcome).to_string();
            NotificationDispatchRepository::put_notification_receipt(
                transaction,
                &receipt_id,
                &content_signature,
                &result,
            )?;
            return Ok(outcome);
        }
        if let DispatchOutcome::Unhandled {
            kind: unhandled_kind,
            reason,
        } = &outcome
        {
            record_notification(&mut agent, unhandled_kind, reason)?;
        }
        if stale_item {
            for reason in extra_reasons {
                record_notification_reason(&mut agent, &kind, reason)?;
            }
            persist_agent(transaction, agent_id, &agent)?;
            let result = outcome_json(&outcome).to_string();
            NotificationDispatchRepository::put_notification_receipt(
                transaction,
                &receipt_id,
                &content_signature,
                &result,
            )?;
            return Ok(outcome);
        }
        if !extra_reasons.is_empty() && !matches!(outcome, DispatchOutcome::Unhandled { .. }) {
            record_notification_count(&mut agent, &kind)?;
        }
        for reason in extra_reasons {
            record_notification_reason(&mut agent, &kind, reason)?;
        }
        if !matches!(
            outcome,
            DispatchOutcome::Ignored { .. } | DispatchOutcome::Unhandled { .. }
        ) {
            update_common_agent_fields(&mut agent, &kind, &params, observed_at);
        }
        if !matches!(outcome, DispatchOutcome::Ignored { .. }) {
            persist_agent(transaction, agent_id, &agent)?;
        }
        let result = outcome_json(&outcome).to_string();
        NotificationDispatchRepository::put_notification_receipt(
            transaction,
            &receipt_id,
            &content_signature,
            &result,
        )?;
        Ok(outcome)
    })
}

#[derive(Clone, Copy)]
enum TextUpdate<'a> {
    Started(Option<&'a str>, Option<&'a Value>),
    Delta(&'a str),
    Completed(Option<&'a str>, Option<&'a Value>),
}

fn validate_submission(
    transaction: &Transaction<'_>,
    agent_id: &str,
    submission: &PreparedSubmission,
) -> Result<(), NotificationDispatchError> {
    if submission.first_event_id.is_empty()
        || submission.inputs.is_empty()
        || submission.inputs[0].id != submission.first_event_id
        || !submission.submitted_at.is_finite()
        || submission.submitted_at < 0.0
        || !submission.created_at.is_finite()
        || submission.created_at < 0.0
        || !submission.assets.is_array()
    {
        return Err(NotificationDispatchError::InvalidNotification);
    }
    let agent = NotificationDispatchRepository::agent_record(transaction, agent_id)?
        .ok_or_else(|| stored("runtime_agents", agent_id))?;
    if agent.get("deletedAt").is_some_and(|value| !value.is_null()) {
        return Err(stored("runtime_agents", agent_id));
    }
    let mut unique = std::collections::BTreeSet::new();
    for input in &submission.inputs {
        if input.id.is_empty()
            || !unique.insert(input.id.as_str())
            || !matches!(input.kind.as_str(), "user" | "followup")
            || !input.at.is_finite()
            || input.at < 0.0
            || !input.assets.is_array()
        {
            return Err(stored("runtime_events", &input.id));
        }
        let Some((kind, text, created)) =
            NotificationDispatchRepository::user_event(transaction, agent_id, &input.id)?
        else {
            return Err(stored("runtime_events", &input.id));
        };
        if kind != input.kind || text != input.text || created != input.at {
            return Err(stored("runtime_events", &input.id));
        }
        let meta = NotificationDispatchRepository::event_metadata(transaction, &input.id)?;
        let meta_assets = meta.get("assets").cloned().unwrap_or_else(|| json!([]));
        let Some(meta_ids) = asset_ids(&meta_assets) else {
            return Err(stored("runtime_event_meta", &input.id));
        };
        let Some(input_ids) = asset_ids(&input.assets) else {
            return Err(NotificationDispatchError::InvalidNotification);
        };
        if meta_ids != input_ids {
            return Err(stored("runtime_event_meta", &input.id));
        }
        if meta
            .get("transcriptItemId")
            .and_then(Value::as_str)
            .is_some_and(|value| value != format!("{agent_id}:{}", submission.first_event_id))
        {
            return Err(stored("runtime_event_meta", &input.id));
        }
    }
    let mut expected_asset_ids = Vec::new();
    for input in &submission.inputs {
        let Some(ids) = asset_ids(&input.assets) else {
            return Err(NotificationDispatchError::InvalidNotification);
        };
        expected_asset_ids.extend(ids);
    }
    if asset_ids(&submission.assets).as_deref() != Some(expected_asset_ids.as_slice()) {
        return Err(NotificationDispatchError::InvalidNotification);
    }
    Ok(())
}

fn asset_ids(assets: &Value) -> Option<Vec<&str>> {
    assets
        .as_array()?
        .iter()
        .map(|asset| {
            asset
                .as_str()
                .or_else(|| asset.get("id").and_then(Value::as_str))
                .filter(|id| !id.is_empty())
        })
        .collect()
}

fn submission_record(id: &str, submission: &PreparedSubmission) -> Value {
    let mut remaining = ITEM_PREVIEW_CHARS;
    let inputs = submission
        .inputs
        .iter()
        .map(|input| {
            let excerpt = input.text.chars().take(remaining).collect::<String>();
            remaining = remaining.saturating_sub(excerpt.chars().count());
            json!({
                "id": input.id,
                "at": input.at,
                "kind": input.kind,
                "text": excerpt,
                "truncated": excerpt.chars().count() < input.text.chars().count(),
                "assets": input.assets,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "id": id,
        "role": "user",
        "title": "User",
        "text": submission.text.chars().take(ITEM_PREVIEW_CHARS).collect::<String>(),
        "truncated": submission.text.chars().count() > ITEM_PREVIEW_CHARS,
        "at": submission.submitted_at,
        "assets": submission.assets,
        "inputs": inputs,
    })
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
    if agent.get("id").and_then(Value::as_str) != Some(agent_id)
        || agent.get("deletedAt").is_some_and(|value| !value.is_null())
    {
        return Err(stored("runtime_agents", agent_id));
    }
    let thread = agent
        .get("threadId")
        .and_then(Value::as_str)
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
    let account = agent
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    if provider_handle != format!("account:{account}") {
        return Err(NotificationDispatchError::InvalidNotification);
    }
    Ok(())
}

fn validate_agent_write_fields(
    agent: &Value,
    agent_id: &str,
) -> Result<(), NotificationDispatchError> {
    if agent.get("events").and_then(Value::as_u64).is_none() {
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
    agent_id: &str,
    agent: &mut Value,
    item_id: &str,
    observed_at: f64,
    update: TextUpdate<'_>,
) -> Result<bool, NotificationDispatchError> {
    let id = format!("{agent_id}:{item_id}");
    let old = NotificationDispatchRepository::transcript_item(transaction, agent_id, &id)?;
    let previous_text = match old.as_ref() {
        Some((record, _)) => saved_text(transaction, &id, record)?,
        None => String::new(),
    };
    let is_closed = agent
        .get("_notificationDispatchClosed")
        .and_then(Value::as_object)
        .is_some_and(|closed| closed.get(item_id).and_then(Value::as_bool) == Some(true));
    let mut stream_map = agent
        .get("_notificationDispatchStreams")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let stream_map = stream_map
        .as_object_mut()
        .ok_or_else(|| stored("runtime_agents", agent_id))?;
    let mut state = stream_map
        .get(item_id)
        .cloned()
        .unwrap_or_else(|| json!({"base": previous_text, "delta": "", "pending": false}));
    let (text, streaming, index_search, close_stream) = match update {
        TextUpdate::Started(snapshot, _) => {
            let base = snapshot.unwrap_or("");
            state["base"] = Value::String(base.to_owned());
            let delta = state.get("delta").and_then(Value::as_str).unwrap_or("");
            (format!("{base}{delta}"), true, true, false)
        }
        TextUpdate::Delta(_) if is_closed => return Ok(false),
        TextUpdate::Delta(delta) => {
            let prior = state.get("delta").and_then(Value::as_str).unwrap_or("");
            let combined = format!("{prior}{delta}");
            state["delta"] = Value::String(combined.clone());
            state["pending"] = Value::Bool(true);
            let base = state.get("base").and_then(Value::as_str).unwrap_or("");
            (format!("{base}{combined}"), true, false, false)
        }
        TextUpdate::Completed(snapshot, _) => {
            let text = snapshot
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    let base = state
                        .get("base")
                        .and_then(Value::as_str)
                        .unwrap_or(&previous_text);
                    let delta = state.get("delta").and_then(Value::as_str).unwrap_or("");
                    format!("{base}{delta}")
                });
            (
                text,
                false,
                true,
                state.get("pending").and_then(Value::as_bool) == Some(true),
            )
        }
    };
    let created = old.as_ref().map_or(observed_at, |(_, created)| *created);
    let mut record = old.map_or_else(
        || json!({"id": id, "role": "assistant", "title": "Assistant"}),
        |(record, _)| record,
    );
    record["id"] = Value::String(id.clone());
    record["role"] = Value::String("assistant".to_owned());
    record["title"] = Value::String("Assistant".to_owned());
    record["text"] = Value::String(text.chars().take(ITEM_PREVIEW_CHARS).collect());
    record["truncated"] = Value::Bool(text.chars().count() > ITEM_PREVIEW_CHARS);
    record["streaming"] = Value::Bool(streaming);
    record["at"] = json!(observed_at);
    if let Some(turn) = agent.get("turnId").and_then(Value::as_str) {
        record["turnId"] = Value::String(turn.to_owned());
    }
    match update {
        TextUpdate::Started(_, phase) | TextUpdate::Completed(_, phase) => {
            record["phase"] = phase.cloned().unwrap_or(Value::Null);
        }
        TextUpdate::Delta(_) => {
            record
                .as_object_mut()
                .ok_or_else(|| stored("runtime_items", &id))?
                .entry("phase")
                .or_insert(Value::Null);
        }
    }
    NotificationDispatchRepository::put_transcript_item(
        transaction,
        &id,
        agent_id,
        &record,
        created,
        &text,
        index_search || text.chars().count() > ITEM_PREVIEW_CHARS,
    )?;
    match update {
        TextUpdate::Started(_, _) | TextUpdate::Delta(_) => {
            stream_map.insert(item_id.to_owned(), state);
        }
        TextUpdate::Completed(_, _) => {
            stream_map.remove(item_id);
        }
    }
    agent["_notificationDispatchStreams"] = Value::Object(stream_map.clone());
    if close_stream {
        let closed = agent
            .as_object_mut()
            .ok_or_else(|| stored("runtime_agents", agent_id))?
            .entry("_notificationDispatchClosed")
            .or_insert_with(|| Value::Object(Map::new()));
        closed
            .as_object_mut()
            .ok_or_else(|| stored("runtime_agents", agent_id))?
            .insert(item_id.to_owned(), Value::Bool(true));
    }
    let answer = text
        .chars()
        .rev()
        .take(16_000)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    let tail = text
        .chars()
        .rev()
        .take(300)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    if matches!(update, TextUpdate::Completed(_, _)) {
        agent["activity"] = json!({"phase":"thinking", "at":observed_at});
        agent["lastAnswer"] = Value::String(answer);
    } else {
        agent["activity"] = json!({"phase":"writing", "at":observed_at});
    }
    agent["tail"] = Value::String(tail);
    Ok(true)
}

fn apply_token_usage(
    agent: &mut Value,
    params: &Value,
    observed_at: f64,
) -> Result<DispatchOutcome, NotificationDispatchError> {
    let Some(usage) = params.get("tokenUsage").and_then(Value::as_object) else {
        return Ok(unhandled(
            "thread/tokenUsage/updated",
            "invalid-token-usage-object",
        ));
    };
    let tokens = usage.get("last").and_then(|last| last.get("totalTokens"));
    let window = usage.get("modelContextWindow");
    if tokens.is_some_and(|value| value.as_u64().is_none())
        || window.is_some_and(|value| !value.is_null() && value.as_u64().is_none())
    {
        return Ok(unhandled(
            "thread/tokenUsage/updated",
            "invalid-token-usage-values",
        ));
    }
    agent["contextUsage"] = json!({
        "tokens": tokens.cloned().unwrap_or(Value::Null),
        "window": window.cloned().unwrap_or(Value::Null),
        "at": observed_at,
    });
    Ok(DispatchOutcome::Written)
}

fn update_common_agent_fields(agent: &mut Value, method: &str, params: &Value, observed_at: f64) {
    if !method.is_empty() {
        let events = agent
            .get("events")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        agent["events"] = Value::from(events.saturating_add(1));
        agent["lastEvent"] = Value::String(format_utc(observed_at));
    }
    if method == "item/agentMessage/delta" || method == "item/started" || method == "item/completed"
    {
        let turn = params
            .get("turnId")
            .and_then(Value::as_str)
            .or_else(|| agent.get("turnId").and_then(Value::as_str))
            .map(str::to_owned);
        if turn
            .as_deref()
            .is_some_and(|turn| agent.get("turnId").and_then(Value::as_str) == Some(turn))
        {
            let turn = turn.unwrap_or_default();
            agent["nativeResponseTurn"] = Value::String(turn.clone());
            let mut clear_safety = false;
            if let Some(buffering) = agent
                .get_mut("nativeSafetyBuffering")
                .and_then(Value::as_object_mut)
                && buffering.get("turnId").and_then(Value::as_str) == Some(turn.as_str())
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
}

fn persist_agent(
    transaction: &Transaction<'_>,
    agent_id: &str,
    agent: &Value,
) -> Result<(), NotificationDispatchError> {
    NotificationDispatchRepository::put_agent_record(transaction, agent_id, agent)?;
    let status = agent
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| stored("runtime_agents", agent_id))?;
    let in_flight = agent
        .get("inFlight")
        .and_then(Value::as_bool)
        .ok_or_else(|| stored("runtime_agents", agent_id))?;
    let turn = agent.get("turnId").and_then(Value::as_str);
    EventWriteRepository::set_agent_sync_lifecycle(transaction, agent_id, status, in_flight, turn)?;
    Ok(())
}

fn record_notification(
    agent: &mut Value,
    kind: &str,
    reason: &str,
) -> Result<(), NotificationDispatchError> {
    record_notification_count(agent, kind)?;
    record_notification_reason(agent, kind, reason)
}

fn record_notification_count(
    agent: &mut Value,
    kind: &str,
) -> Result<(), NotificationDispatchError> {
    let id = agent
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let counts = agent
        .as_object_mut()
        .ok_or_else(|| stored("runtime_agents", &id))?
        .entry("unhandledNotifications")
        .or_insert_with(|| Value::Object(Map::new()));
    let counts = counts
        .as_object_mut()
        .ok_or_else(|| stored("runtime_agents", &id))?;
    let entry = counts
        .entry(kind.to_owned())
        .or_insert_with(|| json!({"count":0,"reasons":{}}));
    let entry = entry
        .as_object_mut()
        .ok_or_else(|| stored("runtime_agents", &id))?;
    let count = entry
        .get("count")
        .and_then(Value::as_u64)
        .ok_or_else(|| stored("runtime_agents", &id))?;
    entry.insert("count".to_owned(), Value::from(count.saturating_add(1)));
    entry
        .entry("reasons")
        .or_insert_with(|| Value::Object(Map::new()));
    Ok(())
}

fn record_notification_reason(
    agent: &mut Value,
    kind: &str,
    reason: &str,
) -> Result<(), NotificationDispatchError> {
    let id = agent
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let entry = agent
        .as_object_mut()
        .and_then(|fields| fields.get_mut("unhandledNotifications"))
        .and_then(Value::as_object_mut)
        .and_then(|fields| fields.get_mut(kind))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| stored("runtime_agents", &id))?;
    let reasons = entry
        .entry("reasons")
        .or_insert_with(|| Value::Object(Map::new()));
    let reasons = reasons
        .as_object_mut()
        .ok_or_else(|| stored("runtime_agents", &id))?;
    let count = reasons.get(reason).map_or(Ok(0), |value| {
        value.as_u64().ok_or_else(|| stored("runtime_agents", &id))
    })?;
    reasons.insert(reason.to_owned(), Value::from(count.saturating_add(1)));
    Ok(())
}

fn unhandled(kind: impl Into<String>, reason: &'static str) -> DispatchOutcome {
    DispatchOutcome::Unhandled {
        kind: kind.into(),
        reason: reason.to_owned(),
    }
}

fn unhandled_reason(kind: &str) -> &'static str {
    match kind {
        "turn/started" | "turn/completed" | "turn/plan/updated" | "turn/diff/updated" => {
            "turn-lifecycle-not-ported"
        }
        "serverRequest/resolved"
        | "item/commandExecution/outputDelta"
        | "item/reasoning/textDelta"
        | "item/reasoning/summaryTextDelta"
        | "item/autoApprovalReview/started"
        | "item/autoApprovalReview/completed" => "approval-question-or-non-text-item-not-ported",
        "error" => "provider-error-state-not-ported",
        "model/safetyBuffering/updated"
        | "model/verification"
        | "modelProvider/authRecoveryStarted"
        | "modelProvider/authRecoveryCompleted" => "provider-notice-not-ported",
        _ if kind.starts_with("item/") => "approval-question-or-non-text-item-not-ported",
        _ => "notification-kind-not-ported",
    }
}

fn has_questions(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Array(values)) => !values.is_empty(),
        Some(Value::Object(values)) => !values.is_empty(),
        _ => false,
    }
}

fn outcome_json(outcome: &DispatchOutcome) -> Value {
    match outcome {
        DispatchOutcome::Written => json!({"status":"written"}),
        DispatchOutcome::Ignored { reason } => json!({"status":"ignored","reason":reason}),
        DispatchOutcome::Unhandled { kind, reason } => {
            json!({"status":"unhandled","kind":kind,"reason":reason})
        }
    }
}

fn decode_outcome(
    raw: &str,
    receipt_id: &str,
) -> Result<DispatchOutcome, NotificationDispatchError> {
    let value: Value =
        serde_json::from_str(raw).map_err(|_| stored("runtime_operation_receipts", receipt_id))?;
    match value.get("status").and_then(Value::as_str) {
        Some("written") => Ok(DispatchOutcome::Written),
        Some("ignored") => value
            .get("reason")
            .and_then(Value::as_str)
            .map(|reason| DispatchOutcome::Ignored {
                reason: reason.to_owned(),
            })
            .ok_or_else(|| stored("runtime_operation_receipts", receipt_id)),
        Some("unhandled") => Ok(DispatchOutcome::Unhandled {
            kind: value
                .get("kind")
                .and_then(Value::as_str)
                .ok_or_else(|| stored("runtime_operation_receipts", receipt_id))?
                .to_owned(),
            reason: value
                .get("reason")
                .and_then(Value::as_str)
                .ok_or_else(|| stored("runtime_operation_receipts", receipt_id))?
                .to_owned(),
        }),
        _ => Err(stored("runtime_operation_receipts", receipt_id)),
    }
}

fn sha256_json(value: &Value) -> Result<String, NotificationDispatchError> {
    let bytes = serde_json::to_vec(value)?;
    Ok(sha256_hex(&bytes))
}

fn stored(table: &'static str, id: &str) -> NotificationDispatchError {
    NotificationDispatchError::InvalidStoredObject {
        table,
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

fn format_utc(seconds: f64) -> String {
    let whole = seconds.floor() as i64;
    let days = whole.div_euclid(86_400);
    let day_seconds = whole.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        day_seconds / 3_600,
        day_seconds % 3_600 / 60,
        day_seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use rusqlite::Connection;
    use serde_json::{Value, json};

    use super::{
        DispatchOutcome, PreparedSubmission, ProviderDeliveryIdentity, SubmittedInput,
        dispatch_notification, record_user_submission,
    };
    use studio_runtime_providers::{Frame, LineDecoder};
    use studio_runtime_store::{NotificationDispatchRepository, TranscriptRepository};

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
        cursor: u64,
        value: &Value,
        observed_at: f64,
    ) -> Result<DispatchOutcome, super::NotificationDispatchError> {
        dispatch_notification(
            transaction,
            "agent",
            &identity(cursor),
            &frame(value),
            observed_at,
        )
    }

    #[test]
    fn committed_delivery_redelivery_replays_result_and_changed_content_conflicts() {
        let mut connection = database();
        let delta = json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"answer","delta":"hello"}});
        let transaction = connection.transaction().expect("transaction begins");
        assert_eq!(
            dispatch(&transaction, 1, &delta, 10.0).expect("delta writes"),
            DispatchOutcome::Written
        );
        transaction
            .commit()
            .expect("first delivery commits before acknowledgement");

        let transaction = connection
            .transaction()
            .expect("redelivery transaction begins");
        assert_eq!(
            dispatch(&transaction, 1, &delta, 11.0).expect("redelivery returns saved result"),
            DispatchOutcome::Written
        );
        let changed = json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"answer","delta":"changed"}});
        assert!(matches!(
            dispatch(&transaction, 1, &changed, 11.0),
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
        let transaction = connection.transaction().expect("transaction begins");
        dispatch(&transaction, 1, &json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"answer","delta":"before"}}), 1.0).expect("early delta writes");
        dispatch(&transaction, 2, &json!({"method":"item/started","params":{"threadId":"thread","turnId":"turn","item":{"id":"answer","type":"agentMessage","text":"snapshot"}}}), 2.0).expect("snapshot writes");
        dispatch(&transaction, 3, &json!({"method":"item/completed","params":{"threadId":"thread","turnId":"turn","item":{"id":"answer","type":"agentMessage","text":""}}}), 3.0).expect("empty completion flushes text");
        dispatch(&transaction, 4, &json!({"method":"item/completed","params":{"threadId":"thread","turnId":"turn","item":{"id":"late","type":"agentMessage","text":"final"}}}), 4.0).expect("completion without a pending stream writes");
        dispatch(&transaction, 5, &json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"late","delta":"late"}}), 5.0).expect("first late delta starts an open stream");
        transaction.commit().expect("notification sequence commits");

        let answer = TranscriptRepository::item(&connection, "agent", "agent:answer")
            .expect("answer reads")
            .expect("answer exists");
        assert_eq!(answer.record["text"], "snapshotbefore");
        assert_eq!(answer.record["streaming"], false);
        let late = TranscriptRepository::item(&connection, "agent", "agent:late")
            .expect("late row reads")
            .expect("late row exists");
        assert_eq!(late.record["text"], "finallate");
        assert_eq!(late.record["streaming"], true);
    }

    #[test]
    fn real_python_submission_and_notification_path_matches_rust_projections() {
        let server =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../apps/server/tests/fixtures/provider-replay/restart-mid-turn-supervisor-replay.json");
        let script = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/notification_dispatch_python.py");
        let database_path = std::env::temp_dir().join(format!(
            "notification-dispatch-parity-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after unix epoch")
                .as_nanos()
        ));
        let python = std::env::var_os("CODEX_AGENTS_PYTHON")
            .or_else(|| std::env::var_os("PYTHON"))
            .unwrap_or_else(|| "python3".into());
        let output = Command::new(python)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .arg(script)
            .arg(server)
            .arg(&fixture_path)
            .arg(&database_path)
            .output()
            .expect("Python parity oracle starts");
        assert!(
            output.status.success(),
            "Python parity oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected: Value = serde_json::from_slice(&output.stdout)
            .expect("Python parity oracle emits one JSON object");
        let seed_path = std::path::PathBuf::from(format!("{}.seed", database_path.display()));
        let mut connection = Connection::open(&seed_path).expect("Python seed database opens");

        let prepared = &expected["prepared"];
        let inputs = prepared["inputs"]
            .as_array()
            .expect("prepared inputs are an array")
            .iter()
            .map(|input| SubmittedInput {
                id: input["id"].as_str().expect("input id is text").to_owned(),
                at: input["at"].as_f64().expect("input time is numeric"),
                kind: input["kind"]
                    .as_str()
                    .expect("input kind is text")
                    .to_owned(),
                text: input["text"]
                    .as_str()
                    .expect("input text is text")
                    .to_owned(),
                assets: input["assets"].clone(),
            })
            .collect();
        let submission = PreparedSubmission {
            first_event_id: prepared["firstEventId"]
                .as_str()
                .expect("first event id is text")
                .to_owned(),
            submitted_at: prepared["submittedAt"]
                .as_f64()
                .expect("submission time is numeric"),
            created_at: prepared["createdAt"]
                .as_f64()
                .expect("creation time is numeric"),
            text: prepared["text"]
                .as_str()
                .expect("submission text is text")
                .to_owned(),
            inputs,
            assets: prepared["assets"].clone(),
        };
        let transaction = connection
            .transaction()
            .expect("Rust parity transaction begins");
        assert_eq!(
            record_user_submission(&transaction, "agent", &submission)
                .expect("Rust writes prepared submission"),
            DispatchOutcome::Written
        );
        let fixture: Value =
            serde_json::from_slice(&std::fs::read(&fixture_path).expect("recorded fixture reads"))
                .expect("recorded fixture is JSON");
        for (cursor, event) in fixture["events"]
            .as_array()
            .expect("fixture events are an array")
            .iter()
            .filter(|event| {
                matches!(
                    event["method"].as_str(),
                    Some("item/agentMessage/delta" | "item/completed")
                )
            })
            .enumerate()
        {
            let mut event = event.clone();
            replace_fixture_ids(&mut event);
            assert_eq!(
                dispatch_notification(
                    &transaction,
                    "agent",
                    &identity(cursor as u64 + 1),
                    &frame(&event),
                    1234.5,
                )
                .expect("Rust applies recorded notification"),
                DispatchOutcome::Written
            );
        }
        for (index, event) in expected["orderingEvents"]
            .as_array()
            .expect("Python ordering events are an array")
            .iter()
            .enumerate()
        {
            assert_eq!(
                dispatch_notification(
                    &transaction,
                    "agent",
                    &identity(index as u64 + 3),
                    &frame(event),
                    1234.5,
                )
                .expect("Rust applies reordered notification"),
                DispatchOutcome::Written
            );
        }
        transaction
            .commit()
            .expect("Rust parity transaction commits");

        let rust_items = TranscriptRepository::items_for_agent(&connection, "agent")
            .expect("Rust transcript reads")
            .into_iter()
            .map(|item| (item.id, item.record))
            .collect::<serde_json::Map<_, _>>();
        assert_eq!(Value::Object(rust_items), expected["items"]);

        let mut rust_agent = TranscriptRepository::agent(&connection, "agent")
            .expect("Rust agent reads")
            .expect("Rust agent exists");
        let private_fields = rust_agent.as_object_mut().expect("Rust agent is an object");
        private_fields.remove("_notificationDispatchStreams");
        private_fields.remove("_notificationDispatchClosed");
        assert_eq!(rust_agent, expected["agent"]);

        let (seq, hash, payload): (i64, String, String) = connection
            .query_row(
                "SELECT seq,hash,payload FROM sync_entities WHERE collection='agent' AND id='agent'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("Rust sync projection reads");
        let payload: Value = serde_json::from_str(&payload).expect("Rust sync payload is JSON");
        assert_eq!(
            seq,
            expected["sync"]["seq"]
                .as_i64()
                .expect("Python seq is integer")
        );
        assert_eq!(
            hash,
            expected["sync"]["hash"]
                .as_str()
                .expect("Python hash is text")
        );
        assert_eq!(payload, expected["sync"]["payload"]);

        let mut event_meta = serde_json::Map::new();
        let mut statement = connection
            .prepare("SELECT id,record FROM runtime_event_meta ORDER BY id")
            .expect("event metadata query prepares");
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("event metadata query runs");
        for row in rows {
            let (id, record) = row.expect("event metadata row reads");
            event_meta.insert(
                id,
                serde_json::from_str(&record).expect("event metadata is JSON"),
            );
        }
        drop(statement);
        assert_eq!(Value::Object(event_meta), expected["eventMeta"]);

        let notification_receipts = {
            let mut statement = connection
                .prepare("SELECT result FROM runtime_operation_receipts WHERE id LIKE 'provider-notification:%' ORDER BY id")
                .expect("notification receipt query prepares");
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .expect("notification receipt query runs");
            rows.map(|row| {
                serde_json::from_str::<Value>(&row.expect("receipt reads"))
                    .expect("receipt result is JSON")
            })
            .collect::<Vec<_>>()
        };
        let expected_receipts = expected["notificationResults"]
            .as_array()
            .expect("Python notification results are an array")
            .iter()
            .map(|result| json!({"status": result["status"]}))
            .collect::<Vec<_>>();
        assert_eq!(notification_receipts, expected_receipts);
        let submission_receipt: String = connection
            .query_row(
                "SELECT result FROM runtime_operation_receipts WHERE id LIKE 'provider-submission:%'",
                [],
                |row| row.get(0),
            )
            .expect("submission receipt exists");
        assert_eq!(
            serde_json::from_str::<Value>(&submission_receipt).expect("submission receipt is JSON"),
            json!({"status":"written"})
        );

        let legacy_search = {
            let mut statement = connection
                .prepare("SELECT s.id,s.agent,s.kind,s.body FROM runtime_search_rows r JOIN runtime_search s ON s.rowid=r.search_rowid ORDER BY s.id")
                .expect("legacy search query prepares");
            let rows = statement
                .query_map([], |row| {
                    Ok(json!({"id":row.get::<_,String>(0)?,"agent":row.get::<_,String>(1)?,"kind":row.get::<_,String>(2)?,"body":row.get::<_,String>(3)?}))
                })
                .expect("legacy search query runs");
            rows.map(|row| row.expect("legacy search row reads"))
                .collect::<Vec<_>>()
        };
        let next_search = {
            let mut statement = connection
                .prepare(
                    "SELECT id,agent,kind,search_rowid FROM runtime_search_next_meta ORDER BY id",
                )
                .expect("next search query prepares");
            let rows = statement
                .query_map([], |row| {
                    Ok(json!({"id":row.get::<_,String>(0)?,"agent":row.get::<_,String>(1)?,"kind":row.get::<_,String>(2)?,"searchRowId":row.get::<_,i64>(3)?}))
                })
                .expect("next search query runs");
            rows.map(|row| row.expect("next search row reads"))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            json!({"legacy":legacy_search,"next":next_search}),
            expected["search"]
        );
        drop(connection);
        drop(std::fs::remove_file(database_path));
        drop(std::fs::remove_file(seed_path));
    }

    fn replace_fixture_ids(value: &mut Value) {
        match value {
            Value::Object(fields) => {
                for value in fields.values_mut() {
                    replace_fixture_ids(value);
                }
            }
            Value::Array(values) => {
                for value in values {
                    replace_fixture_ids(value);
                }
            }
            Value::String(value) if value == "$threadId" => *value = "thread".to_owned(),
            Value::String(value) if value == "$turnId" => {
                *value = "replay-turn-1".to_owned();
            }
            _ => {}
        }
    }

    #[test]
    fn route_refusal_is_clean_and_post_write_failure_rolls_back_savepoint() {
        let mut connection = database();
        let wrong_thread = json!({"method":"thread/tokenUsage/updated","params":{"threadId":"other","tokenUsage":{"last":{"totalTokens":4}}}});
        let transaction = connection.transaction().expect("transaction begins");
        assert!(matches!(
            dispatch(&transaction, 1, &wrong_thread, 1.0),
            Err(super::NotificationDispatchError::InvalidNotification)
        ));
        transaction
            .commit()
            .expect("refusal leaves caller transaction committable");

        connection.execute_batch("CREATE TRIGGER fail_sync_projection BEFORE UPDATE ON sync_entities BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").expect("failure trigger creates");
        let delta = json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"rollback","delta":"written then refused"}});
        let transaction = connection.transaction().expect("transaction begins");
        assert!(dispatch(&transaction, 2, &delta, 2.0).is_err());
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
    }

    #[test]
    fn prepared_submission_preserves_batch_text_metadata_assets_and_replay() {
        let mut connection = database();
        connection.execute_batch(
            "INSERT INTO runtime_events VALUES('e1','agent','user','hello','dispatching',1,1,'turn',NULL);
             INSERT INTO runtime_events VALUES('e2','agent','followup','please continue','dispatching',2,1,'turn',NULL);
             INSERT INTO runtime_event_meta VALUES('e1','{\"assets\":[\"asset-1\"]}');
             INSERT INTO runtime_event_meta VALUES('e2','{\"assets\":[]}');",
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
    }
}
