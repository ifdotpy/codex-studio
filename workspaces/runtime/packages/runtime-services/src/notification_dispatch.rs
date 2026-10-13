//! Persist the text and usage portion of provider notification streams.

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use studio_runtime_providers::Frame;
use studio_runtime_store::{NotificationDispatchRepository, StoreError, Transaction};
use thiserror::Error;

const ITEM_PREVIEW_CHARS: usize = 20_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DispatchOutcome {
    Written,
    Duplicate,
    Ignored { reason: &'static str },
    Unhandled { kind: String, reason: &'static str },
}

#[derive(Debug, Error)]
pub enum NotificationDispatchError {
    #[error("invalid provider notification")]
    InvalidNotification,
    #[error("invalid stored object {0}")]
    InvalidStoredObject(&'static str),
    #[error("conflicting user input identity")]
    ConflictingInput,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("JSON serialization failed")]
    Json(#[from] serde_json::Error),
}

/// Materialize a submitted user event before its provider turn starts.
///
/// The event identity is the transcript identity, so retrying the submission
/// cannot create a second visible item.
pub fn record_user_submission(
    transaction: &Transaction<'_>,
    agent_id: &str,
    event_id: &str,
) -> Result<DispatchOutcome, NotificationDispatchError> {
    let Some((kind, text, created)) =
        NotificationDispatchRepository::user_event(transaction, agent_id, event_id)?
    else {
        return Err(NotificationDispatchError::InvalidStoredObject(
            "runtime_events",
        ));
    };
    if !matches!(kind.as_str(), "user" | "followup") {
        return Err(NotificationDispatchError::InvalidNotification);
    }
    let id = format!("{agent_id}:{event_id}");
    if let Some((existing, _)) =
        NotificationDispatchRepository::transcript_item(transaction, agent_id, &id)?
    {
        let existing_text = NotificationDispatchRepository::full_text(transaction, &id, &existing)?
            .ok_or(NotificationDispatchError::InvalidStoredObject(
                "runtime_items",
            ))?;
        if existing_text == text {
            return Ok(DispatchOutcome::Duplicate);
        }
        return Err(NotificationDispatchError::ConflictingInput);
    }

    let preview = text.chars().take(ITEM_PREVIEW_CHARS).collect::<String>();
    let truncated = text.chars().count() > ITEM_PREVIEW_CHARS;
    let record = json!({
        "id": id,
        "role": "user",
        "title": "User",
        "text": preview,
        "truncated": truncated,
        "at": created,
        "assets": [],
        "inputs": [{
            "id": event_id,
            "at": created,
            "kind": kind,
            "text": preview,
            "truncated": truncated,
            "assets": []
        }]
    });
    NotificationDispatchRepository::put_transcript_item(
        transaction,
        &id,
        agent_id,
        &record,
        created,
        &text,
    )?;
    NotificationDispatchRepository::link_transcript_item_to_event(
        transaction,
        agent_id,
        event_id,
        &id,
    )?;
    Ok(DispatchOutcome::Written)
}

/// Apply one decoded provider frame inside the turn actor's transaction.
///
/// The actor supplies a stable observation time; all other decisions depend
/// only on the stored records, turn identity, and decoded frame.
pub fn dispatch_notification(
    transaction: &Transaction<'_>,
    agent_id: &str,
    turn_id: Option<&str>,
    notification: &Frame,
    observed_at: f64,
) -> Result<DispatchOutcome, NotificationDispatchError> {
    if !observed_at.is_finite() || observed_at < 0.0 {
        return Err(NotificationDispatchError::InvalidNotification);
    }
    let (kind, params, payload) = notification_parts(notification)?;
    let signature = notification_signature(agent_id, turn_id, observed_at, &payload)?;
    let receipt_id = format!("provider-notification:{signature}");
    if let Some((stored_signature, _)) =
        NotificationDispatchRepository::notification_receipt(transaction, &receipt_id)?
    {
        if stored_signature != signature {
            return Err(NotificationDispatchError::InvalidStoredObject(
                "runtime_operation_receipts",
            ));
        }
        return Ok(DispatchOutcome::Duplicate);
    }

    let outcome = match kind.as_str() {
        "item/agentMessage/delta" => {
            dispatch_delta(transaction, agent_id, turn_id, &params, observed_at)?
        }
        "item/started" | "item/completed" => {
            dispatch_item(transaction, agent_id, turn_id, &kind, &params, observed_at)?
        }
        "thread/tokenUsage/updated" => {
            dispatch_token_usage(transaction, agent_id, &params, observed_at)?
        }
        _ => DispatchOutcome::Unhandled {
            kind: kind.clone(),
            reason: unhandled_reason(&kind),
        },
    };

    let budget_capture_unhandled = kind == "thread/tokenUsage/updated";
    match (&outcome, budget_capture_unhandled) {
        (DispatchOutcome::Unhandled { kind, reason }, true) => {
            record_unhandled_reasons(
                transaction,
                agent_id,
                kind,
                &[reason, "usage-policy/budget-capture"],
            )?;
        }
        (DispatchOutcome::Unhandled { kind, reason }, false) => {
            record_unhandled(transaction, agent_id, kind, reason)?;
        }
        (_, true) => {
            record_unhandled(transaction, agent_id, &kind, "usage-policy/budget-capture")?;
        }
        (_, false) => {}
    }
    let result = serde_json::to_string(&outcome_json(&outcome))?;
    NotificationDispatchRepository::put_notification_receipt(
        transaction,
        &receipt_id,
        &signature,
        &result,
    )?;
    Ok(outcome)
}

fn notification_parts(
    notification: &Frame,
) -> Result<(String, Value, Value), NotificationDispatchError> {
    let (kind, params, mut fields) = match notification {
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
    fields.insert("method".to_owned(), Value::String(kind.clone()));
    if !params.is_null() {
        fields.insert("params".to_owned(), params.clone());
    }
    Ok((kind, params, Value::Object(fields)))
}

fn notification_signature(
    agent_id: &str,
    turn_id: Option<&str>,
    observed_at: f64,
    payload: &Value,
) -> Result<String, NotificationDispatchError> {
    let material = json!({
        "agent": agent_id,
        "turn": turn_id,
        "observedAt": observed_at,
        "notification": payload,
    });
    let bytes = serde_json::to_vec(&material)?;
    let digest = Sha256::digest(bytes);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn dispatch_delta(
    transaction: &Transaction<'_>,
    agent_id: &str,
    turn_id: Option<&str>,
    params: &Value,
    observed_at: f64,
) -> Result<DispatchOutcome, NotificationDispatchError> {
    if !turn_matches(turn_id, params) {
        return Ok(DispatchOutcome::Ignored {
            reason: "notification-belongs-to-another-turn",
        });
    }
    let Some(item_id) = params.get("itemId").and_then(Value::as_str) else {
        return Ok(unhandled(
            "item/agentMessage/delta",
            "missing-item-identity",
        ));
    };
    let Some(delta) = params.get("delta").and_then(Value::as_str) else {
        return Ok(unhandled("item/agentMessage/delta", "invalid-text-delta"));
    };
    let full_id = format!("{agent_id}:{item_id}");
    write_assistant_item(
        transaction,
        agent_id,
        &full_id,
        turn_id,
        params.get("turnId").and_then(Value::as_str),
        observed_at,
        AssistantUpdate::Delta(delta),
    )
}

fn dispatch_item(
    transaction: &Transaction<'_>,
    agent_id: &str,
    turn_id: Option<&str>,
    method: &str,
    params: &Value,
    observed_at: f64,
) -> Result<DispatchOutcome, NotificationDispatchError> {
    if !turn_matches(turn_id, params) {
        return Ok(DispatchOutcome::Ignored {
            reason: "notification-belongs-to-another-turn",
        });
    }
    let Some(item) = params.get("item").and_then(Value::as_object) else {
        return Ok(unhandled(method, "missing-provider-item"));
    };
    if item.get("type").and_then(Value::as_str) != Some("agentMessage") {
        return Ok(unhandled(method, "non-text-provider-item"));
    }
    let Some(item_id) = item.get("id").and_then(Value::as_str) else {
        return Ok(unhandled(method, "missing-item-identity"));
    };
    if item.get("text").is_some_and(|text| !text.is_string()) {
        return Ok(unhandled(method, "invalid-item-text"));
    }
    let update = if method == "item/started" {
        AssistantUpdate::Started(item.get("text").and_then(Value::as_str), item.get("phase"))
    } else {
        AssistantUpdate::Completed(item.get("text").and_then(Value::as_str), item.get("phase"))
    };
    let full_id = format!("{agent_id}:{item_id}");
    write_assistant_item(
        transaction,
        agent_id,
        &full_id,
        turn_id,
        params.get("turnId").and_then(Value::as_str),
        observed_at,
        update,
    )
}

#[derive(Clone, Copy)]
enum AssistantUpdate<'a> {
    Started(Option<&'a str>, Option<&'a Value>),
    Delta(&'a str),
    Completed(Option<&'a str>, Option<&'a Value>),
}

fn write_assistant_item(
    transaction: &Transaction<'_>,
    agent_id: &str,
    full_id: &str,
    context_turn: Option<&str>,
    notification_turn: Option<&str>,
    observed_at: f64,
    update: AssistantUpdate<'_>,
) -> Result<DispatchOutcome, NotificationDispatchError> {
    let old = NotificationDispatchRepository::transcript_item(transaction, agent_id, full_id)?;
    if old
        .as_ref()
        .is_some_and(|(record, _)| record.get("streaming").and_then(Value::as_bool) == Some(false))
    {
        return Ok(DispatchOutcome::Ignored {
            reason: "item-already-completed",
        });
    }
    let (mut record, created) = match old {
        Some((record, created)) => (record, created),
        None => (
            json!({
                "id": full_id,
                "role": "assistant",
                "title": "Assistant",
                "text": "",
                "at": observed_at,
            }),
            observed_at,
        ),
    };
    let previous_text = NotificationDispatchRepository::full_text(transaction, full_id, &record)?
        .or_else(|| {
            (record.get("truncated").and_then(Value::as_bool) != Some(true)).then(String::new)
        })
        .ok_or(NotificationDispatchError::InvalidStoredObject(
            "runtime_items",
        ))?;
    let (text, streaming) = match update {
        AssistantUpdate::Started(snapshot, _) => (
            if previous_text.is_empty() {
                snapshot.unwrap_or("").to_owned()
            } else {
                previous_text
            },
            true,
        ),
        AssistantUpdate::Delta(delta) => (format!("{previous_text}{delta}"), true),
        AssistantUpdate::Completed(snapshot, _) => (
            snapshot
                .filter(|text| !text.is_empty())
                .unwrap_or(&previous_text)
                .to_owned(),
            false,
        ),
    };
    let preview = text.chars().take(ITEM_PREVIEW_CHARS).collect::<String>();
    record["text"] = Value::String(preview);
    record["truncated"] = Value::Bool(text.chars().count() > ITEM_PREVIEW_CHARS);
    record["streaming"] = Value::Bool(streaming);
    record["at"] = json!(observed_at);
    let phase = match update {
        AssistantUpdate::Started(_, phase) | AssistantUpdate::Completed(_, phase) => phase.cloned(),
        AssistantUpdate::Delta(_) => record.get("phase").cloned(),
    };
    record["phase"] = phase.unwrap_or(Value::Null);
    if let Some(turn) = notification_turn.or(context_turn) {
        record["turnId"] = Value::String(turn.to_owned());
    }
    NotificationDispatchRepository::put_transcript_item(
        transaction,
        full_id,
        agent_id,
        &record,
        created,
        &text,
    )?;
    Ok(DispatchOutcome::Written)
}

fn dispatch_token_usage(
    transaction: &Transaction<'_>,
    agent_id: &str,
    params: &Value,
    observed_at: f64,
) -> Result<DispatchOutcome, NotificationDispatchError> {
    let Some(usage) = params.get("tokenUsage").and_then(Value::as_object) else {
        return Ok(unhandled(
            "thread/tokenUsage/updated",
            "invalid-token-usage-object",
        ));
    };
    let last = usage.get("last").and_then(Value::as_object);
    let tokens = last.and_then(|last| last.get("totalTokens"));
    let window = usage.get("modelContextWindow");
    if tokens.is_some_and(|value| !is_nonnegative_integer(value))
        || window.is_some_and(|value| !value.is_null() && !value.is_number())
    {
        return Ok(unhandled(
            "thread/tokenUsage/updated",
            "invalid-token-usage-values",
        ));
    }
    let mut agent = NotificationDispatchRepository::agent_record(transaction, agent_id)?.ok_or(
        NotificationDispatchError::InvalidStoredObject("runtime_agents"),
    )?;
    agent["contextUsage"] = json!({
        "tokens": tokens.cloned().unwrap_or(Value::Null),
        "window": window.cloned().unwrap_or(Value::Null),
        "at": observed_at,
    });
    NotificationDispatchRepository::put_agent_record(transaction, agent_id, &agent)?;
    Ok(DispatchOutcome::Written)
}

fn is_nonnegative_integer(value: &Value) -> bool {
    value.as_u64().is_some()
}

fn turn_matches(expected: Option<&str>, params: &Value) -> bool {
    match (expected, params.get("turnId").and_then(Value::as_str)) {
        (Some(expected), Some(actual)) => expected == actual,
        _ => true,
    }
}

fn unhandled(kind: &str, reason: &'static str) -> DispatchOutcome {
    DispatchOutcome::Unhandled {
        kind: kind.to_owned(),
        reason,
    }
}

fn unhandled_reason(kind: &str) -> &'static str {
    if kind.starts_with("turn/") {
        "turn-lifecycle-not-ported"
    } else if kind == "serverRequest/resolved" || kind.starts_with("item/") {
        "approval-question-or-non-text-item-not-ported"
    } else if kind == "error" {
        "provider-error-state-not-ported"
    } else if kind.starts_with("model/") || kind.starts_with("modelProvider/") {
        "provider-notice-not-ported"
    } else {
        "notification-kind-not-ported"
    }
}

fn record_unhandled(
    transaction: &Transaction<'_>,
    agent_id: &str,
    kind: &str,
    reason: &str,
) -> Result<(), NotificationDispatchError> {
    record_unhandled_reasons(transaction, agent_id, kind, &[reason])
}

fn record_unhandled_reasons(
    transaction: &Transaction<'_>,
    agent_id: &str,
    kind: &str,
    reasons_to_add: &[&str],
) -> Result<(), NotificationDispatchError> {
    let mut agent = NotificationDispatchRepository::agent_record(transaction, agent_id)?.ok_or(
        NotificationDispatchError::InvalidStoredObject("runtime_agents"),
    )?;
    let counts = agent
        .as_object_mut()
        .ok_or(NotificationDispatchError::InvalidStoredObject(
            "runtime_agents",
        ))?
        .entry("unhandledNotifications")
        .or_insert_with(|| Value::Object(Map::new()));
    let counts = counts
        .as_object_mut()
        .ok_or(NotificationDispatchError::InvalidStoredObject(
            "runtime_agents",
        ))?;
    let entry = counts
        .entry(kind.to_owned())
        .or_insert_with(|| json!({"count": 0, "reasons": {}}));
    let entry = entry
        .as_object_mut()
        .ok_or(NotificationDispatchError::InvalidStoredObject(
            "runtime_agents",
        ))?;
    let count = entry.get("count").and_then(Value::as_u64).ok_or(
        NotificationDispatchError::InvalidStoredObject("runtime_agents"),
    )?;
    entry.insert("count".to_owned(), Value::from(count.saturating_add(1)));
    let reasons = entry
        .entry("reasons".to_owned())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or(NotificationDispatchError::InvalidStoredObject(
            "runtime_agents",
        ))?;
    for reason in reasons_to_add {
        let reason_count = match reasons.get(*reason) {
            Some(count) => count
                .as_u64()
                .ok_or(NotificationDispatchError::InvalidStoredObject(
                    "runtime_agents",
                ))?,
            None => 0,
        };
        reasons.insert(
            (*reason).to_owned(),
            Value::from(reason_count.saturating_add(1)),
        );
    }
    NotificationDispatchRepository::put_agent_record(transaction, agent_id, &agent)?;
    Ok(())
}

fn outcome_json(outcome: &DispatchOutcome) -> Value {
    match outcome {
        DispatchOutcome::Written => json!({"status": "written"}),
        DispatchOutcome::Duplicate => json!({"status": "duplicate"}),
        DispatchOutcome::Ignored { reason } => json!({"status": "ignored", "reason": reason}),
        DispatchOutcome::Unhandled { kind, reason } => {
            json!({"status": "unhandled", "kind": kind, "reason": reason})
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{DispatchOutcome, dispatch_notification, record_user_submission};
    use rusqlite::Connection;
    use serde_json::{Value, json};
    use studio_runtime_providers::{Frame, LineDecoder};
    use studio_runtime_store::{NotificationDispatchRepository, TranscriptRepository};

    const REPLAY: &str = include_str!(
        "../../../apps/server/tests/fixtures/provider-replay/restart-mid-turn-supervisor-replay.json"
    );
    static DATABASE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
                 INSERT INTO runtime_agents VALUES('agent', '{\"id\":\"agent\",\"rootId\":\"agent\",\"epoch\":1,\"status\":\"running\",\"threadId\":\"thread\",\"turnId\":\"replay-turn-1\",\"inFlight\":true,\"events\":0}');
                 INSERT INTO runtime_events VALUES('user-event','agent','user','Reply with the fixture result.','dispatching',10,1,'replay-turn-1',NULL);
                 INSERT INTO runtime_event_meta VALUES('user-event','{}');",
            )
            .expect("fixture tables create");
        connection
    }

    fn decode(value: &Value) -> Frame {
        let mut bytes = serde_json::to_vec(value).expect("notification serializes");
        bytes.push(b'\n');
        LineDecoder::new()
            .push(&bytes)
            .expect("provider frame decodes")
            .into_iter()
            .next()
            .expect("one provider frame")
    }

    fn substitute(value: &mut Value) {
        match value {
            Value::String(value) if value == "$threadId" => *value = "thread".to_owned(),
            Value::String(value) if value == "$turnId" => *value = "replay-turn-1".to_owned(),
            Value::Array(values) => values.iter_mut().for_each(substitute),
            Value::Object(values) => values.values_mut().for_each(substitute),
            _ => {}
        }
    }

    #[test]
    fn recorded_replay_writes_input_and_answer_and_transcript_reader_returns_them() {
        let connection = database();
        let replay: Value = serde_json::from_str(REPLAY).expect("recorded replay parses");
        let events = replay["events"].as_array().expect("recorded events");
        let transaction = connection
            .unchecked_transaction()
            .expect("transaction starts");
        assert_eq!(
            record_user_submission(&transaction, "agent", "user-event")
                .expect("submitted input is written"),
            DispatchOutcome::Written
        );
        assert_eq!(
            transaction
                .query_row(
                    "SELECT json_extract(record,'$.transcriptItemId') FROM runtime_event_meta WHERE id='user-event'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("event transcript projection reads"),
            "agent:user-event"
        );
        for (index, event) in events.iter().enumerate() {
            let mut event = event.clone();
            substitute(&mut event);
            let outcome = dispatch_notification(
                &transaction,
                "agent",
                Some("replay-turn-1"),
                &decode(&event),
                20.0 + index as f64,
            )
            .expect("recorded notification dispatches");
            if event["method"] == "turn/completed" {
                assert!(matches!(outcome, DispatchOutcome::Unhandled { .. }));
            } else {
                assert_eq!(outcome, DispatchOutcome::Written);
            }
        }
        transaction.commit().expect("transaction commits");

        let rows = TranscriptRepository::items_for_agent(&connection, "agent")
            .expect("Rust transcript reader lists items");
        let parsed = rows
            .into_iter()
            .map(|item| (item.id, item.record))
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut actual = serde_json::Map::new();
        for (id, mut record) in parsed {
            record
                .as_object_mut()
                .expect("transcript record is an object")
                .remove("at");
            actual.insert(id, record);
        }
        assert_eq!(Value::Object(actual), python_replay_rows());
    }

    #[test]
    fn redelivery_and_reordered_item_events_do_not_duplicate_or_reopen_completed_text() {
        let connection = database();
        let completed = json!({"method":"item/completed","params":{"turnId":"replay-turn-1","item":{"id":"answer","type":"agentMessage","text":"final"}}});
        let started = json!({"method":"item/started","params":{"turnId":"replay-turn-1","item":{"id":"answer","type":"agentMessage","text":"initial"}}});
        let delta = json!({"method":"item/agentMessage/delta","params":{"turnId":"replay-turn-1","itemId":"answer","delta":"late"}});
        let transaction = connection
            .unchecked_transaction()
            .expect("transaction starts");
        assert_eq!(
            dispatch_notification(
                &transaction,
                "agent",
                Some("replay-turn-1"),
                &decode(&completed),
                10.0
            )
            .expect("completion writes"),
            DispatchOutcome::Written
        );
        assert_eq!(
            dispatch_notification(
                &transaction,
                "agent",
                Some("replay-turn-1"),
                &decode(&completed),
                10.0
            )
            .expect("redelivery is safe"),
            DispatchOutcome::Duplicate
        );
        assert!(matches!(
            dispatch_notification(
                &transaction,
                "agent",
                Some("replay-turn-1"),
                &decode(&started),
                12.0
            )
            .expect("late start is ignored"),
            DispatchOutcome::Ignored { .. }
        ));
        assert!(matches!(
            dispatch_notification(
                &transaction,
                "agent",
                Some("replay-turn-1"),
                &decode(&delta),
                13.0
            )
            .expect("late delta is ignored"),
            DispatchOutcome::Ignored { .. }
        ));
        transaction.commit().expect("transaction commits");
        let row = NotificationDispatchRepository::transcript_item(
            &connection
                .unchecked_transaction()
                .expect("read transaction"),
            "agent",
            "agent:answer",
        )
        .expect("item reads")
        .expect("answer item exists");
        assert_eq!(row.0["text"], "final");
        assert_eq!(row.0["streaming"], false);
    }

    #[test]
    fn refusal_does_not_write_a_transcript_item_or_event_projection() {
        let connection = database();
        connection
            .execute(
                "UPDATE runtime_events SET kind='assistant' WHERE id='user-event'",
                [],
            )
            .expect("event kind changes");
        let transaction = connection
            .unchecked_transaction()
            .expect("transaction starts");
        assert!(matches!(
            record_user_submission(&transaction, "agent", "user-event"),
            Err(super::NotificationDispatchError::InvalidNotification)
        ));
        let item_count: i64 = transaction
            .query_row("SELECT count(*) FROM runtime_items", [], |row| row.get(0))
            .expect("transcript row count reads");
        let transcript_item: Option<String> = transaction
            .query_row(
                "SELECT json_extract(record,'$.transcriptItemId') FROM runtime_event_meta WHERE id='user-event'",
                [],
                |row| row.get(0),
            )
            .expect("event projection reads");
        assert_eq!(item_count, 0);
        assert_eq!(transcript_item, None);
        transaction.commit().expect("transaction commits");
    }

    #[test]
    fn identical_distinct_fragments_append_while_stable_redelivery_is_deduplicated() {
        let connection = database();
        let transaction = connection
            .unchecked_transaction()
            .expect("transaction starts");
        let repeated_delta = json!({"method":"item/agentMessage/delta","params":{"turnId":"replay-turn-1","itemId":"repeated","delta":"same"}});
        assert_eq!(
            dispatch_notification(
                &transaction,
                "agent",
                Some("replay-turn-1"),
                &decode(&repeated_delta),
                1.0,
            )
            .expect("first fragment writes"),
            DispatchOutcome::Written
        );
        assert_eq!(
            dispatch_notification(
                &transaction,
                "agent",
                Some("replay-turn-1"),
                &decode(&repeated_delta),
                2.0,
            )
            .expect("second identical fragment writes"),
            DispatchOutcome::Written
        );
        assert_eq!(
            dispatch_notification(
                &transaction,
                "agent",
                Some("replay-turn-1"),
                &decode(&repeated_delta),
                2.0,
            )
            .expect("stable redelivery deduplicates"),
            DispatchOutcome::Duplicate
        );
        transaction.commit().expect("transaction commits");
        let row = TranscriptRepository::item(&connection, "agent", "agent:repeated")
            .expect("transcript item reads")
            .expect("item exists");
        assert_eq!(row.record["text"], "samesame");
    }

    #[test]
    fn empty_completion_uses_buffered_text_and_long_text_round_trips() {
        let connection = database();
        let transaction = connection
            .unchecked_transaction()
            .expect("transaction starts");
        let delta = json!({"method":"item/agentMessage/delta","params":{"turnId":"replay-turn-1","itemId":"buffered","delta":"kept answer"}});
        let empty_completion = json!({"method":"item/completed","params":{"turnId":"replay-turn-1","item":{"id":"buffered","type":"agentMessage","text":""}}});
        dispatch_notification(
            &transaction,
            "agent",
            Some("replay-turn-1"),
            &decode(&delta),
            40.0,
        )
        .expect("delta writes");
        dispatch_notification(
            &transaction,
            "agent",
            Some("replay-turn-1"),
            &decode(&empty_completion),
            41.0,
        )
        .expect("empty completion preserves buffered text");

        let long_text = "x".repeat(20_001);
        let long_completion = json!({"method":"item/completed","params":{"turnId":"replay-turn-1","item":{"id":"long","type":"agentMessage","text":long_text}}});
        dispatch_notification(
            &transaction,
            "agent",
            Some("replay-turn-1"),
            &decode(&long_completion),
            42.0,
        )
        .expect("long completion writes");
        transaction.commit().expect("transaction commits");

        let buffered = TranscriptRepository::item(&connection, "agent", "agent:buffered")
            .expect("buffered transcript item reads")
            .expect("buffered item exists");
        assert_eq!(buffered.record["text"], "kept answer");
        assert_eq!(buffered.record["streaming"], false);
        let long = TranscriptRepository::item(&connection, "agent", "agent:long")
            .expect("long transcript item reads")
            .expect("long item exists");
        assert_eq!(
            long.record["text"]
                .as_str()
                .expect("transcript preview is text")
                .chars()
                .count(),
            20_000
        );
        assert_eq!(long.record["truncated"], true);
        assert_eq!(
            TranscriptRepository::full_text(&connection, &long)
                .expect("full text reads")
                .expect("long payload exists"),
            "x".repeat(20_001)
        );
    }

    #[test]
    fn transcript_reader_reads_python_written_state() {
        let path = temporary_database_path();
        if path.exists() {
            fs::remove_file(&path).expect("stale Python oracle database is removed");
        }
        let output = python_oracle(Some(&path));
        assert!(
            output.status.success(),
            "Python notification oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let connection = Connection::open(&path).expect("Python state database opens");
        let items = TranscriptRepository::items_for_agent(&connection, "agent")
            .expect("Rust transcript reader accepts Python state");
        let ids = items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert!(ids.contains("agent:user-event"));
        assert!(ids.contains("agent:resume-answer"));
        let answer = items
            .iter()
            .find(|item| item.id == "agent:resume-answer")
            .expect("Python answer exists");
        assert_eq!(
            TranscriptRepository::full_text(&connection, answer)
                .expect("Python answer full text reads")
                .as_deref(),
            Some("Recovered output.")
        );
        drop(connection);
        fs::remove_file(path).expect("temporary Python database is removed");
    }

    #[test]
    fn token_usage_is_persisted_and_unknown_kinds_count_once() {
        let connection = database();
        let usage = json!({"method":"thread/tokenUsage/updated","params":{"tokenUsage":{"last":{"totalTokens":17},"modelContextWindow":120000}}});
        let incomplete_usage = json!({"method":"thread/tokenUsage/updated","params":{}});
        let unknown = json!({"method":"future/notice","params":{"state":"ready"}});
        let transaction = connection
            .unchecked_transaction()
            .expect("transaction starts");
        assert_eq!(
            dispatch_notification(
                &transaction,
                "agent",
                Some("replay-turn-1"),
                &decode(&usage),
                30.0
            )
            .expect("usage applies"),
            DispatchOutcome::Written
        );
        assert_eq!(
            dispatch_notification(
                &transaction,
                "agent",
                Some("replay-turn-1"),
                &decode(&usage),
                30.0
            )
            .expect("usage redelivery is safe"),
            DispatchOutcome::Duplicate
        );
        assert!(matches!(
            dispatch_notification(
                &transaction,
                "agent",
                Some("replay-turn-1"),
                &decode(&incomplete_usage),
                31.0
            )
            .expect("incomplete usage is accounted for"),
            DispatchOutcome::Unhandled { .. }
        ));
        assert!(matches!(
            dispatch_notification(
                &transaction,
                "agent",
                Some("replay-turn-1"),
                &decode(&unknown),
                32.0
            )
            .expect("unknown kind is recorded"),
            DispatchOutcome::Unhandled { .. }
        ));
        assert_eq!(
            dispatch_notification(
                &transaction,
                "agent",
                Some("replay-turn-1"),
                &decode(&unknown),
                32.0
            )
            .expect("unknown redelivery is safe"),
            DispatchOutcome::Duplicate
        );
        transaction.commit().expect("transaction commits");
        let agent = NotificationDispatchRepository::agent_record(
            &connection
                .unchecked_transaction()
                .expect("read transaction"),
            "agent",
        )
        .expect("agent reads")
        .expect("agent exists");
        assert_eq!(agent["contextUsage"]["tokens"], 17);
        assert_eq!(agent["contextUsage"]["window"], 120000);
        assert_eq!(
            agent["unhandledNotifications"]["thread/tokenUsage/updated"]["count"],
            2
        );
        assert_eq!(
            agent["unhandledNotifications"]["thread/tokenUsage/updated"]["reasons"]["usage-policy/budget-capture"],
            2
        );
        assert_eq!(
            agent["unhandledNotifications"]["thread/tokenUsage/updated"]["reasons"]["invalid-token-usage-object"],
            1
        );
        assert_eq!(agent["unhandledNotifications"]["future/notice"]["count"], 1);
        assert_eq!(
            agent["unhandledNotifications"]["future/notice"]["reasons"]["notification-kind-not-ported"],
            1
        );
    }

    fn python_replay_rows() -> Value {
        let output = python_oracle(None);
        assert!(
            output.status.success(),
            "Python notification oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("Python oracle rows are JSON")
    }

    fn python_oracle(database: Option<&std::path::Path>) -> std::process::Output {
        let mut command = Command::new(python_executable());
        command
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/notification_dispatch_python.py"))
            .arg(server_source())
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
                "../../apps/server/tests/fixtures/provider-replay/restart-mid-turn-supervisor-replay.json",
            ))
            .env("PYTHONDONTWRITEBYTECODE", "1");
        if let Some(database) = database {
            command.arg(database);
        }
        command
            .output()
            .expect("run Python notification oracle with isolated interpreter")
    }

    fn temporary_database_path() -> PathBuf {
        let sequence = DATABASE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "studio-notification-dispatch-{}-{sequence}.sqlite",
            std::process::id()
        ))
    }

    fn server_source() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src")
    }

    fn python_executable() -> PathBuf {
        if let Some(path) = std::env::var_os("CODEX_AGENTS_PYTHON") {
            return PathBuf::from(path);
        }
        if let Some(path) = std::env::var_os("PYTHON") {
            return PathBuf::from(path);
        }
        let output = Command::new("python3")
            .arg(server_source().join("codex_python.py"))
            .output()
            .expect("resolve isolated Python through codex_python.py");
        assert!(
            output.status.success(),
            "isolated Python missing; run install-cli.py"
        );
        PathBuf::from(
            String::from_utf8(output.stdout)
                .expect("interpreter path is UTF-8")
                .trim(),
        )
    }
}
