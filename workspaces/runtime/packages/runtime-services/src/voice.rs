//! Persisted voice records, session projections and transcript delivery.

use std::collections::BTreeSet;

use serde_json::{Value, json};
use studio_runtime_schema::shared_values::MAX_SAFE_INTEGER;
use studio_runtime_store::{
    AgentId, AgentRepository, Connection, QuestionsReadsRepository, RuntimeStore, StoreError,
    VoiceRecord, VoiceRepository,
};
use thiserror::Error;

use crate::event_write::{self, EventWriteError};

#[path = "voice/account_session.rs"]
pub mod account_session;

#[path = "voice/audio.rs"]
mod audio;

pub use audio::{VoiceAudioError, save_audio};

#[path = "voice/policy.rs"]
pub mod policy;

#[path = "voice/provider.rs"]
pub mod provider;

pub use crate::voice_runtime as runtime;

const MAX_RECORD_TEXT: usize = 100_000;
const MAX_IDENTITY: usize = 240;
const MAX_PAYLOAD: usize = 100_000;
const MAX_DELIVERY_RECORDS: usize = 2_000;
const MAX_DELIVERY_TEXT: usize = 32_000;
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

pub async fn status(store: &RuntimeStore, agent_id: &str) -> Result<Value, VoiceError> {
    let agent_id = agent_id.to_owned();
    store
        .read_async("runtime-services/voice", move |connection| {
            checked_agent(connection, &agent_id)
        })
        .await?;
    Ok(json!({"configured": true, "transport": "native", "auth": "chatgpt"}))
}

pub async fn record(store: &RuntimeStore, input: VoiceRecordInput) -> Result<Value, VoiceError> {
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

    store
        .transaction_with_error(move |transaction| {
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
        .await
}

/// Characters that one `orchestration_speak` text may hold (Python `VoiceStore.speak`).
const MAX_SPEECH_TEXT: usize = 4096;

pub struct SpeakRequest {
    pub store: std::sync::Arc<RuntimeStore>,
    pub provider: Option<std::sync::Arc<dyn crate::voice_runtime::VoiceRequestPort>>,
    pub agent_id: String,
    pub text: String,
    /// The tool request identity; the record identity is `speak:<request_id>`.
    pub request_id: String,
    /// The caller's turn epoch, when the caller is a model turn.
    pub epoch: Option<i64>,
}

/// Python `VoiceStore.speak`: save the orchestrator's speech once, then submit
/// it to the ready voice session of that chat. Without a ready session the
/// record stays saved and playback is not confirmed.
pub async fn speak(request: SpeakRequest) -> Result<Value, provider::VoiceProviderError> {
    let SpeakRequest {
        store,
        provider,
        agent_id,
        text,
        request_id,
        epoch,
    } = request;
    if text.chars().count() > MAX_SPEECH_TEXT || event_write::python_strip(&text).is_empty() {
        return Err(
            VoiceError::InvalidRequest("Speech text must contain 1 to 4096 characters").into(),
        );
    }
    let agent_for_check = agent_id.clone();
    let active = store
        .read_async(
            "runtime-services/voice",
            move |connection| -> Result<Option<String>, VoiceError> {
                let agent =
                    AgentRepository::get(connection, &AgentId::from(agent_for_check.as_str()))?;
                let Some(agent) = agent.filter(|agent| {
                    agent.extra.get("isLead").and_then(Value::as_bool) == Some(true)
                        && !event_write::truthy(agent.extra.get("deletedAt"))
                }) else {
                    return Err(VoiceError::InvalidRequest(
                        "Voice is available in an orchestrator chat only",
                    ));
                };
                if epoch.is_some_and(|epoch| {
                    agent.epoch != epoch || agent.extra.get("autoWake") != Some(&Value::Bool(true))
                }) {
                    return Err(VoiceError::InvalidRequest(
                        "The orchestrator turn is stopped or replaced",
                    ));
                }
                ready_session(connection, &agent_for_check)
            },
        )
        .await?;
    // A ready session needs the provider adapter. Refuse before the record
    // exists rather than report a saved speech that could not play.
    if active.is_some() && provider.is_none() {
        return Err(provider::VoiceProviderError::RequiresDependency(
            "voice/provider-session",
        ));
    }
    let row = record(
        &store,
        VoiceRecordInput {
            agent_id: agent_id.clone(),
            session_id: String::new(),
            event_id: format!("speak:{request_id}"),
            kind: "orchestrator".to_owned(),
            text,
            item_id: String::new(),
            previous_item_id: String::new(),
            payload: None,
            internal: true,
        },
    )
    .await?;
    let agent_for_session = agent_id.clone();
    let active = store
        .read_async("runtime-services/voice", move |connection| {
            ready_session(connection, &agent_for_session)
        })
        .await?;
    let Some(session) = active else {
        return Ok(json!({"record": row, "status": "saved", "playback": "not confirmed"}));
    };
    let provider = provider.ok_or(provider::VoiceProviderError::RequiresDependency(
        "voice/provider-session",
    ))?;
    let record_id = row["id"]
        .as_str()
        .ok_or_else(|| invalid("voice_records", &agent_id))?
        .to_owned();
    let receipt = provider::speech(store, provider, agent_id, session, record_id, None).await?;
    let mut value = json!({"record": row});
    if let Some(fields) = receipt.as_object() {
        for (key, field) in fields {
            value[key] = field.clone();
        }
    }
    Ok(value)
}

/// Python `speak`: the agent's ready, unended voice session.
fn ready_session(connection: &Connection, agent_id: &str) -> Result<Option<String>, VoiceError> {
    use rusqlite::OptionalExtension;
    Ok(connection
        .query_row(
            "SELECT id FROM voice_sessions WHERE agent=?1 AND state='ready' AND ended IS NULL",
            [agent_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(StoreError::from)?)
}

pub async fn records(
    store: &RuntimeStore,
    agent_id: &str,
    after: i64,
) -> Result<Value, VoiceError> {
    let agent_for_read = agent_id.to_owned();
    let (agent, rows, delivered_batches, session) = store
        .read_async("runtime-services/voice", move |connection| {
            let agent_id = agent_for_read.as_str();
            Ok::<_, StoreError>((
                AgentRepository::get(connection, &AgentId::from(agent_id))?,
                VoiceRepository::records(connection, agent_id, after)?,
                VoiceRepository::delivered(connection, agent_id)?,
                VoiceRepository::latest_session(connection, agent_id)?,
            ))
        })
        .await?;
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

/// A bounded transient interval. Large histories retain the HTTP recovery path.
/// The returned cursor advances even when the interval cannot be inlined.
pub async fn stream_records(
    store: &RuntimeStore,
    agent_id: &str,
    after: i64,
) -> Result<(i64, Option<Value>), VoiceError> {
    use studio_runtime_schema::shared_values::SYNC_VOICE_STREAM_MAX_RECORDS;
    let id = agent_id.to_owned();
    let (agent, rows, session, cursor) = store
        .read_async("runtime-services/voice-stream", move |connection| {
            Ok::<_, StoreError>((
                AgentRepository::get(connection, &AgentId::from(id.as_str()))?,
                VoiceRepository::records_page(
                    connection,
                    &id,
                    after,
                    SYNC_VOICE_STREAM_MAX_RECORDS + 1,
                )?,
                VoiceRepository::latest_session(connection, &id)?,
                VoiceRepository::record_sequence(connection, &id)?,
            ))
        })
        .await?;
    validate_agent(agent, agent_id)?;
    if rows.len() > SYNC_VOICE_STREAM_MAX_RECORDS as usize {
        return Ok((cursor, None));
    }
    let cursor = rows.last().map_or(after, |row| row.sequence);
    let mut records = Vec::with_capacity(rows.len());
    for row in rows {
        validate_record(&row)?;
        records.push(row.value()?);
    }
    if let Some(session) = &session {
        validate_session(session)?;
    }
    Ok((
        cursor,
        Some(json!({"after":after,"cursor":cursor,"records":records,"session":session})),
    ))
}

pub async fn approvals(store: &RuntimeStore, agent_id: &str) -> Result<Value, VoiceError> {
    let agent_for_read = AgentId::from(agent_id);
    let (agent, rows) = store
        .read_async("runtime-services/voice", move |connection| {
            Ok::<_, StoreError>((
                AgentRepository::get(connection, &agent_for_read)?,
                QuestionsReadsRepository::questions(connection)?,
            ))
        })
        .await?;
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

pub async fn submit(
    store: std::sync::Arc<RuntimeStore>,
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

    let agent_for_preparation = agent_id.to_owned();
    let admission = store
        .read_async("runtime-services/voice", move |connection| {
            crate::message_admission::PreparedAdmission::read(connection, &agent_for_preparation)
        })
        .await?;
    let admission = admission.prepare().await?;
    #[cfg(test)]
    ingress_tests::support::after_preparation(agent_id);
    let agent_id = agent_id.to_owned();
    let message_id = message_id.to_owned();
    let record_ids = record_ids.to_vec();
    let edited_text = edited_text.map(str::to_owned);
    store.transaction_with_error(move |transaction| {
        checked_agent(transaction, &agent_id)?;
        let input_records = serde_json::to_string(&record_ids)?;
        let text = match VoiceRepository::delivery(transaction, &message_id)? {
            Some(old) => {
                let old_records: Vec<String> = serde_json::from_str(&old.records)
                    .map_err(|_| invalid("voice_deliveries", &old.id))?;
                if old.agent != agent_id
                    || old_records != record_ids
                    || old.edited_text.as_deref() != edited_text.as_deref()
                {
                    return Err(VoiceError::Conflict(
                        "Voice delivery identity conflicts with its original content",
                    ));
                }
                old.text
            }
            None => {
                let mut reserved = BTreeSet::new();
                for (delivery_id, batch) in VoiceRepository::deliveries(transaction, &agent_id)? {
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
                    let Some(row) = VoiceRepository::record(transaction, &id)? else {
                        return Err(VoiceError::InvalidRequest("Unknown transcript record"));
                    };
                    validate_record(&row)?;
                    let payload: Value = serde_json::from_str(&row.payload)
                        .map_err(|_| invalid("voice_records", &id))?;
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
                let text = edited_text.as_deref().unwrap_or(&generated).to_owned();
                if text.chars().count() > MAX_DELIVERY_TEXT {
                    return Err(VoiceError::InvalidRequest(
                        "The transcript exceeds 32000 characters. Send a smaller selection; no text was truncated",
                    ));
                }
                VoiceRepository::insert_delivery(
                    transaction,
                    &message_id,
                    &agent_id,
                    &input_records,
                    &text,
                    edited_text.as_deref(),
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
                room: &agent_id,
                id: &message_id,
                text: &text,
                delivery: studio_runtime_schema::delivery::DeliveryMoment::CurrentTurn,
                assets: &[],
            },
            &admission,
        )?;
        Ok(result.receipt)
    })
    .await
}

pub async fn approval_speech(
    store: std::sync::Arc<RuntimeStore>,
    agent_id: &str,
    request_id: &str,
) -> Result<Value, VoiceError> {
    let approvals = approvals(&store, agent_id).await?;
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
    let fingerprint = approval_fingerprint(request);
    let params = studio_runtime_store::python_json(request.get("params").unwrap_or(&Value::Null));
    let text = format!("Запрос разрешения. {params}. Для подтверждения скажите «разрешаю».");
    if text.chars().count() > 4096 {
        return Err(VoiceError::InvalidRequest(
            "This permission is too long for voice. Use its approval button",
        ));
    }
    record(
        &store,
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
    .await
}

pub(super) fn approval_fingerprint(request: &Value) -> String {
    studio_runtime_store::sha256_hex(event_write::python_json(request).as_bytes())
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
