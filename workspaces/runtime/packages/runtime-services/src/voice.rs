//! Persisted voice records, session projections and transcript delivery.

use std::collections::BTreeSet;

use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRepository, Connection, QuestionsReadsRepository, RuntimeStore, StoreError,
    VoiceRecord, VoiceRepository,
};
use thiserror::Error;

use crate::event_write::{self, EventWriteError};

#[path = "voice/policy.rs"]
pub mod policy;

#[path = "voice/audio.rs"]
mod audio;

pub use audio::{VoiceAudioError, save_audio};

#[path = "voice/provider.rs"]
pub mod provider;

const MAX_RECORD_TEXT: usize = 100_000;
const MAX_IDENTITY: usize = 240;
const MAX_PAYLOAD: usize = 100_000;
const MAX_DELIVERY_RECORDS: usize = 2_000;
const MAX_DELIVERY_TEXT: usize = 32_000;
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
const RECORD_KINDS: &[&str] = &[
    "user",
    "courier",
    "assistant",
    "event",
    "playback",
    "orchestrator",
];
const PLAYBACK_STATES: &[&str] = &["queued", "playing", "played", "interrupted", "unknown"];
const SESSION_STATES: &[&str] = &[
    "connecting",
    "ready",
    "stopping",
    "ended",
    "failed",
    "lost",
    "unknown",
];
const APPROVAL_METHODS: &[&str] = &[
    "monitor/approve",
    "item/commandExecution/requestApproval",
    "item/fileChange/requestApproval",
];

#[derive(Debug, Error)]
pub enum VoiceError {
    #[error("{0}")]
    InvalidRequest(&'static str),
    #[error("{0}")]
    Conflict(&'static str),
    #[error("invalid stored object {object}/{id}")]
    InvalidStoredObject { object: &'static str, id: String },
    #[error(transparent)]
    Coordination(#[from] crate::coordination::CoordinationError),
    #[error("Not implemented: requires {0}")]
    RequiresDependency(&'static str),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    EventWrite(#[from] EventWriteError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub struct VoiceRecordInput {
    pub agent_id: String,
    pub session_id: String,
    pub event_id: String,
    pub kind: String,
    pub text: String,
    pub item_id: String,
    pub previous_item_id: String,
    pub payload: Option<Value>,
    pub internal: bool,
}

pub fn status(store: &RuntimeStore, agent_id: &str) -> Result<Value, VoiceError> {
    let connection = store.read()?;
    checked_agent(&connection, agent_id)?;
    Ok(json!({"configured": true, "transport": "native", "auth": "chatgpt"}))
}

pub fn record(store: &RuntimeStore, input: VoiceRecordInput) -> Result<Value, VoiceError> {
    let VoiceRecordInput {
        agent_id,
        session_id,
        event_id,
        kind,
        text,
        item_id,
        previous_item_id,
        payload,
        internal,
    } = input;
    let payload = payload.unwrap_or_else(|| json!({}));
    let encoded = serde_json::to_string(&payload)?;
    let input = VoiceRecord {
        sequence: 0,
        id: event_id,
        agent: agent_id.clone(),
        session: session_id,
        kind: kind.clone(),
        text,
        item_id,
        previous_item_id,
        payload: encoded,
        created: 0.0,
    };
    validate_record_input(&input, &payload)?;
    if !internal && (kind == "assistant" || kind == "orchestrator") {
        return Err(VoiceError::InvalidRequest(
            "Only the orchestrator can publish speech",
        ));
    }
    if payload.get("utterance_order").is_some_and(|order| {
        order
            .as_i64()
            .is_none_or(|order| !(0..=1_000_000_000).contains(&order))
    }) {
        return Err(VoiceError::InvalidRequest("Invalid utterance order"));
    }
    if kind == "playback"
        && (!PLAYBACK_STATES.contains(&input.text.as_str())
            || !payload.get("record_id").is_some_and(Value::is_string))
    {
        return Err(VoiceError::InvalidRequest("Invalid playback status"));
    }

    store.transaction_with_error(|transaction| {
        checked_agent(transaction, &agent_id)?;
        if !input.session.is_empty()
            && !VoiceRepository::session_exists(transaction, &agent_id, &input.session)?
        {
            return Err(VoiceError::InvalidRequest("Unknown voice session"));
        }
        if let Some(previous) = VoiceRepository::record(transaction, &input.id)? {
            validate_record(&previous)?;
            let old_payload: Value = serde_json::from_str(&previous.payload)
                .map_err(|_| invalid("voice_records", &previous.id))?;
            if previous.agent != input.agent
                || previous.session != input.session
                || previous.kind != input.kind
                || previous.text != input.text
                || previous.item_id != input.item_id
                || previous.previous_item_id != input.previous_item_id
                || old_payload != payload
            {
                return Err(VoiceError::Conflict(
                    "Voice event identity conflicts with its original content",
                ));
            }
            return previous.value().map_err(VoiceError::from);
        }
        let mut saved = input;
        saved.created = event_write::unix_time()?;
        VoiceRepository::insert_record(transaction, &saved)?;
        VoiceRepository::record(transaction, &saved.id)?
            .ok_or_else(|| invalid("voice_records", &saved.id))?
            .value()
            .map_err(VoiceError::from)
    })
}

pub fn records(store: &RuntimeStore, agent_id: &str, after: i64) -> Result<Value, VoiceError> {
    let (agent, rows, delivered_batches, session) = store.read_transaction(|connection| {
        Ok((
            AgentRepository::get(connection, &AgentId::from(agent_id))?,
            VoiceRepository::records(connection, agent_id, after)?,
            VoiceRepository::delivered(connection, agent_id)?,
            VoiceRepository::latest_session(connection, agent_id)?,
        ))
    })?;
    validate_agent(agent, agent_id)?;
    let mut records = Vec::with_capacity(rows.len());
    for row in &rows {
        validate_record(row)?;
        records.push(row.value()?);
    }
    let mut delivered = Vec::new();
    for (delivery_id, batch) in delivered_batches {
        let values: Vec<Value> =
            serde_json::from_str(&batch).map_err(|_| invalid("voice_deliveries", &delivery_id))?;
        if values.iter().any(|value| !value.is_string()) {
            return Err(invalid("voice_deliveries", &delivery_id));
        }
        delivered.extend(values);
    }
    if let Some(session) = session.as_ref() {
        validate_session(session)?;
    }
    let cursor = rows.last().map_or(after, |row| row.sequence);
    Ok(json!({"records": records, "cursor": cursor, "delivered": delivered, "session": session}))
}

pub fn approvals(store: &RuntimeStore, agent_id: &str) -> Result<Value, VoiceError> {
    let (agent, rows) = store.read_transaction(|connection| {
        Ok((
            AgentRepository::get(connection, &AgentId::from(agent_id))?,
            QuestionsReadsRepository::questions(connection)?,
        ))
    })?;
    validate_agent(agent, agent_id)?;
    let mut requests = Vec::new();
    for (row, raw) in rows {
        let row_id = row.id().to_string();
        let value: Value =
            serde_json::from_str(&raw).map_err(|_| invalid("runtime_requests", &row_id))?;
        if !value.is_object() || value.get("id").and_then(Value::as_str).is_none() {
            return Err(invalid("runtime_requests", &row_id));
        }
        if value.get("agent").and_then(Value::as_str) == Some(agent_id)
            && value.get("status").and_then(Value::as_str) == Some("pending")
            && value
                .get("method")
                .and_then(Value::as_str)
                .is_some_and(|method| APPROVAL_METHODS.contains(&method))
        {
            requests.push(value);
        }
    }
    Ok(json!({"requests": requests}))
}

pub fn submit(
    store: &RuntimeStore,
    agent_id: &str,
    message_id: &str,
    record_ids: &[String],
    edited_text: Option<&str>,
) -> Result<Value, VoiceError> {
    if message_id.is_empty()
        || record_ids.is_empty()
        || record_ids.len() > MAX_DELIVERY_RECORDS
        || record_ids.iter().collect::<BTreeSet<_>>().len() != record_ids.len()
    {
        return Err(VoiceError::InvalidRequest(
            "Select new transcript records to send",
        ));
    }
    if edited_text.is_some_and(|text| {
        event_write::python_strip(text).is_empty() || text.chars().count() > MAX_DELIVERY_TEXT
    }) {
        return Err(VoiceError::InvalidRequest(
            "Edited transcript must contain 1 to 32000 characters",
        ));
    }

    let admission = crate::message_admission::PreparedAdmission::load(&store.read()?, agent_id)?;
    #[cfg(test)]
    ingress_tests::support::after_preparation(agent_id);
    store.transaction_with_error(|transaction| {
        checked_agent(transaction, agent_id)?;
        let input_records = serde_json::to_string(record_ids)?;
        let text = match VoiceRepository::delivery(transaction, message_id)? {
            Some(old) => {
                let old_records: Vec<String> = serde_json::from_str(&old.records)
                    .map_err(|_| invalid("voice_deliveries", &old.id))?;
                if old.agent != agent_id
                    || old_records != record_ids
                    || old.edited_text.as_deref() != edited_text
                {
                    return Err(VoiceError::Conflict(
                        "Voice delivery identity conflicts with its original content",
                    ));
                }
                old.text
            }
            None => {
                let mut reserved = BTreeSet::new();
                for (delivery_id, batch) in VoiceRepository::deliveries(transaction, agent_id)? {
                    let identities: Vec<String> = serde_json::from_str(&batch)
                        .map_err(|_| invalid("voice_deliveries", &delivery_id))?;
                    reserved.extend(identities);
                }
                if record_ids.iter().any(|id| reserved.contains(id)) {
                    return Err(VoiceError::Conflict(
                        "Some transcript records already belong to another delivery. Retry the original delivery",
                    ));
                }
                let mut selected = Vec::with_capacity(record_ids.len());
                for id in record_ids {
                    let Some(row) = VoiceRepository::record(transaction, id)? else {
                        return Err(VoiceError::InvalidRequest("Unknown transcript record"));
                    };
                    validate_record(&row)?;
                    let payload: Value = serde_json::from_str(&row.payload)
                        .map_err(|_| invalid("voice_records", id))?;
                    if row.agent != agent_id || !matches!(row.kind.as_str(), "user" | "courier") {
                        return Err(VoiceError::InvalidRequest("Unknown transcript record"));
                    }
                    if event_write::truthy(payload.get("native")) {
                        return Err(VoiceError::InvalidRequest(
                            "Native voice already handles delegation. Do not send its transcript again",
                        ));
                    }
                    selected.push((row, payload));
                }
                let mut session_order = std::collections::BTreeMap::<String, i64>::new();
                for (row, _) in &selected {
                    session_order
                        .entry(row.session.clone())
                        .and_modify(|sequence| *sequence = (*sequence).min(row.sequence))
                        .or_insert(row.sequence);
                }
                selected.sort_by_key(|(row, payload)| {
                    (
                        session_order[&row.session],
                        payload
                            .get("utterance_order")
                            .and_then(Value::as_i64)
                            .unwrap_or(row.sequence),
                        row.sequence,
                    )
                });
                let generated = format!(
                    "Voice conversation, full transcript (speech recognition can contain errors):\n\n{}",
                    selected
                        .iter()
                        .map(|(row, _)| format!(
                            "{}: {}",
                            if row.kind == "user" { "User" } else { "Voice courier" },
                            row.text
                        ))
                        .collect::<Vec<_>>()
                        .join("\n\n")
                );
                let text = edited_text.unwrap_or(&generated).to_owned();
                if text.chars().count() > MAX_DELIVERY_TEXT {
                    return Err(VoiceError::InvalidRequest(
                        "The transcript exceeds 32000 characters. Send a smaller selection; no text was truncated",
                    ));
                }
                VoiceRepository::insert_delivery(
                    transaction,
                    message_id,
                    agent_id,
                    &input_records,
                    &text,
                    edited_text,
                )?;
                text
            }
        };
        let text = event_write::python_strip(&text).to_owned();
        if text.is_empty() || text.chars().count() > MAX_DELIVERY_TEXT {
            return Err(VoiceError::InvalidRequest("Invalid transcript delivery"));
        }
        let result = crate::coordination::post_message_with_admission(
            transaction,
            crate::coordination::PostManagedMessage {
                room: agent_id,
                id: message_id,
                text: &text,
                delivery: "queue",
                assets: &[],
            },
            &admission,
        )?;
        Ok(result.receipt)
    })
}

pub fn end(store: &RuntimeStore, agent_id: &str, session_id: &str) -> Result<Value, VoiceError> {
    let (agent, session) = store.read_transaction(|connection| {
        Ok((
            AgentRepository::get(connection, &AgentId::from(agent_id))?,
            VoiceRepository::session(connection, session_id, agent_id)?,
        ))
    })?;
    validate_agent(agent, agent_id)?;
    let Some(session) = session else {
        return Err(VoiceError::InvalidRequest("Unknown voice session"));
    };
    validate_session(&session)?;
    if session.get("ended").is_some_and(|ended| !ended.is_null())
        || session.get("state").and_then(Value::as_str) == Some("ended")
    {
        return Ok(session);
    }
    Err(VoiceError::RequiresDependency(
        "account-worker/voice-session",
    ))
}

pub fn speech_receipt(
    store: &RuntimeStore,
    agent_id: &str,
    session_id: &str,
    record_id: &str,
    request_id: Option<&str>,
) -> Result<Value, VoiceError> {
    let id = request_id
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{session_id}:{record_id}"));
    if id.chars().count() > 480 {
        return Err(VoiceError::InvalidRequest("Invalid voice speech identity"));
    }
    let (agent, session, receipt) = store.read_transaction(|connection| {
        Ok((
            AgentRepository::get(connection, &AgentId::from(agent_id))?,
            VoiceRepository::session(connection, session_id, agent_id)?,
            VoiceRepository::speech_receipt(connection, &id)?,
        ))
    })?;
    validate_agent(agent, agent_id)?;
    let Some(session) = session else {
        return Err(VoiceError::InvalidRequest("Unknown voice session"));
    };
    validate_session(&session)?;
    if let Some(receipt) = receipt {
        if receipt.get("session").and_then(Value::as_str) != Some(session_id)
            || receipt.get("record_id").and_then(Value::as_str) != Some(record_id)
        {
            return Err(VoiceError::Conflict(
                "Voice speech identity conflicts with its original content",
            ));
        }
        validate_speech_receipt(&receipt)?;
        return Ok(receipt);
    }
    if session.get("ended").is_some_and(|ended| !ended.is_null())
        || session.get("state").and_then(Value::as_str) != Some("ready")
    {
        return Err(VoiceError::InvalidRequest(
            "Start voice before audio playback",
        ));
    }
    Err(VoiceError::RequiresDependency(
        "account-worker/voice-session",
    ))
}

pub fn approval_speech(
    store: &RuntimeStore,
    agent_id: &str,
    request_id: &str,
) -> Result<Value, VoiceError> {
    let approvals = approvals(store, agent_id)?;
    let request = approvals
        .get("requests")
        .and_then(Value::as_array)
        .and_then(|requests| {
            requests
                .iter()
                .find(|request| request.get("id").and_then(Value::as_str) == Some(request_id))
        })
        .ok_or(VoiceError::InvalidRequest(
            "The approval request is no longer pending",
        ))?;
    let fingerprint =
        studio_runtime_store::sha256_hex(event_write::python_json(request).as_bytes());
    let params = serde_json::to_string(request.get("params").unwrap_or(&Value::Null))?;
    let text = format!("Запрос разрешения. {params}. Для подтверждения скажите «разрешаю».");
    if text.chars().count() > 4096 {
        return Err(VoiceError::InvalidRequest(
            "This permission is too long for voice. Use its approval button",
        ));
    }
    record(
        store,
        VoiceRecordInput {
            agent_id: agent_id.to_owned(),
            session_id: String::new(),
            event_id: format!("approval-speech:{request_id}:{fingerprint}"),
            kind: "orchestrator".to_owned(),
            text,
            item_id: String::new(),
            previous_item_id: String::new(),
            payload: Some(json!({"request_id": request_id, "fingerprint": fingerprint})),
            internal: true,
        },
    )
}

fn checked_agent(
    connection: &Connection,
    id: &str,
) -> Result<studio_runtime_store::AgentRecord, VoiceError> {
    let agent = AgentRepository::get(connection, &AgentId::from(id))?
        .ok_or(VoiceError::InvalidRequest("Invalid request"))?;
    validate_agent(Some(agent), id)
}

fn validate_agent(
    agent: Option<studio_runtime_store::AgentRecord>,
    id: &str,
) -> Result<studio_runtime_store::AgentRecord, VoiceError> {
    let agent = agent.ok_or(VoiceError::InvalidRequest("Invalid request"))?;
    if agent.id.0 != id {
        return Err(invalid("runtime_agents", id));
    }
    if event_write::truthy(agent.extra.get("deletedAt")) {
        return Err(VoiceError::InvalidRequest("This agent was deleted"));
    }
    if agent.extra.get("isLead").and_then(Value::as_bool) != Some(true) {
        return Err(VoiceError::InvalidRequest(
            "Voice is available in an orchestrator chat only",
        ));
    }
    Ok(agent)
}

fn validate_record(record: &VoiceRecord) -> Result<(), VoiceError> {
    validate_record_content(record)?;
    if record.sequence <= 0 || record.sequence > MAX_SAFE_INTEGER {
        return Err(invalid("voice_records", &record.id));
    }
    Ok(())
}

fn validate_record_input(record: &VoiceRecord, payload: &Value) -> Result<(), VoiceError> {
    if record.id.is_empty()
        || record.id.chars().count() > MAX_IDENTITY
        || record.agent.is_empty()
        || record.session.chars().count() > MAX_IDENTITY
        || record.item_id.chars().count() > MAX_IDENTITY
        || record.previous_item_id.chars().count() > MAX_IDENTITY
        || !RECORD_KINDS.contains(&record.kind.as_str())
        || record.text.chars().count() > MAX_RECORD_TEXT
        || !payload.is_object()
        || payload.to_string().chars().count() > MAX_PAYLOAD
    {
        return Err(VoiceError::InvalidRequest("Invalid voice record"));
    }
    Ok(())
}

fn validate_record_content(record: &VoiceRecord) -> Result<(), VoiceError> {
    let payload: Value =
        serde_json::from_str(&record.payload).map_err(|_| invalid("voice_records", &record.id))?;
    if record.id.is_empty()
        || record.id.chars().count() > MAX_IDENTITY
        || record.agent.is_empty()
        || record.session.chars().count() > MAX_IDENTITY
        || record.item_id.chars().count() > MAX_IDENTITY
        || record.previous_item_id.chars().count() > MAX_IDENTITY
        || !RECORD_KINDS.contains(&record.kind.as_str())
        || record.text.chars().count() > MAX_RECORD_TEXT
        || !payload.is_object()
        || record.payload.chars().count() > MAX_PAYLOAD
        || !record.created.is_finite()
    {
        return Err(invalid("voice_records", &record.id));
    }
    Ok(())
}

fn validate_session(session: &Value) -> Result<(), VoiceError> {
    let id = session
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if id.is_empty()
        || !session
            .get("state")
            .and_then(Value::as_str)
            .is_some_and(|state| SESSION_STATES.contains(&state))
        || !is_nullable_string(session.get("sdp"))
        || !is_nullable_string(session.get("error"))
        || !is_nullable_number(session.get("ended"))
    {
        return Err(invalid("voice_sessions", id));
    }
    Ok(())
}

fn validate_speech_receipt(receipt: &Value) -> Result<(), VoiceError> {
    let id = receipt
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if id.is_empty()
        || !receipt
            .get("session")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
        || !receipt
            .get("record_id")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
        || !receipt
            .get("state")
            .and_then(Value::as_str)
            .is_some_and(|state| matches!(state, "pending" | "submitted" | "failed" | "unknown"))
        || !is_nullable_string(receipt.get("error"))
    {
        return Err(invalid("voice_speech_requests", id));
    }
    Ok(())
}

fn is_nullable_string(value: Option<&Value>) -> bool {
    value.is_some_and(|value| value.is_null() || value.is_string())
}

fn is_nullable_number(value: Option<&Value>) -> bool {
    value.is_some_and(|value| value.is_null() || value.as_f64().is_some_and(f64::is_finite))
}

fn invalid(object: &'static str, id: &str) -> VoiceError {
    VoiceError::InvalidStoredObject {
        object,
        id: id.to_owned(),
    }
}

#[cfg(test)]
#[path = "../tests/wave7_p03_gate_tests/voice.rs"]
mod ingress_tests;
