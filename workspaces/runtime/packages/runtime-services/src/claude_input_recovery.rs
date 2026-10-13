//! Frozen Claude input can be retried only after an exact, positive rejection.
//! This module has no I/O. Its caller loads and applies records in one transaction.
pub mod store;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use studio_runtime_store::{AgentStatus, sha256_hex};

const MAX_BATCH: usize = 32;
const RECEIPT_FIELDS: &[&str] = &[
    "id",
    "epoch",
    "accountKey",
    "threadId",
    "connectionId",
    "nativeOperationId",
    "events",
    "submitted",
    "supervisorIdentity",
    "claudeInputRequest",
];
const IDENTITY_FIELDS: &[&str] = &[
    "id",
    "epoch",
    "accountKey",
    "threadId",
    "connectionId",
    "nativeOperationId",
    "events",
];
const SOURCE_FIELDS: &[&str] = &[
    "cwd",
    "rootId",
    "parentId",
    "role",
    "isLead",
    "model",
    "effort",
    "nativeEffort",
    "fastMode",
    "yoloMode",
    "imageWorkspace",
    "imageWorkspaceReady",
    "compactions",
];

#[derive(Debug, thiserror::Error)]
#[error("Invalid Claude retry record at {location}")]
pub struct InvalidRecord {
    pub location: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Attempt {
    id: String,
    epoch: i64,
    account_key: String,
    thread_id: Option<String>,
    connection_id: Option<String>,
    native_operation_id: Option<String>,
    events: Vec<String>,
    submitted: Option<bool>,
    #[serde(default)]
    active_at_reservation: Option<bool>,
    created: Option<f64>,
    claude_input_request: Option<FrozenInput>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FrozenInput {
    input: Vec<Value>,
    source: Map<String, Value>,
    configuration: Map<String, Value>,
    events: Vec<Value>,
    transcript_item_id: String,
    client_user_message_id: String,
    text_hash: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Agent {
    id: String,
    epoch: i64,
    status: AgentStatus,
    provider: String,
    #[serde(default = "default_account")]
    account_key: String,
    thread_id: Option<String>,
    turn_id: Option<String>,
    turn_epoch: Option<i64>,
    #[serde(default)]
    auto_wake: bool,
    #[serde(default)]
    native_failure_hold: bool,
}
fn default_account() -> String {
    "default".into()
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum EventStatus {
    Pending,
    Reserved,
    Dispatching,
    Uncertain,
    Delivered,
    Cancelled,
    Failed,
}

/// SQL columns and the separate metadata row; no provider-derived booleans here.
#[derive(Clone, Deserialize, Serialize)]
pub struct EventRecord {
    id: String,
    agent: String,
    epoch: i64,
    kind: String,
    text: String,
    created: f64,
    status: EventStatus,
    turn_id: Option<String>,
    error: Option<Value>,
    metadata: Map<String, Value>,
}
impl EventRecord {
    fn snapshot(&self) -> Value {
        json!({"id":self.id,"kind":self.kind,"created":self.created,
               "assets":self.metadata.get("assets").cloned().unwrap_or(json!([])),
               "textHash":sha256_hex(self.text.as_bytes())})
    }
}

/// Captured by the account owner, never inferred from a provider response.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportSnapshot {
    account_key: String,
    connection_id: String,
    supervisor_identity: Option<Value>,
    closed: bool,
}

/// Positive native evidence. A timeout or an unknown write is deliberately absent.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Rejection {
    Rpc { code: i64, data: Map<String, Value> },
    Turn { turn: Value },
}

pub struct RecoverySnapshot {
    agent: Agent,
    agent_record: Map<String, Value>,
    attempt: Attempt,
    attempt_record: Map<String, Value>,
    saved: Map<String, Value>,
    events: Vec<EventRecord>,
    transport: TransportSnapshot,
    runtime_closed: bool,
    turn_has_work: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryOutcome {
    Retry,
    Held,
}

#[derive(Serialize)]
pub struct RecoveryDecision {
    pub outcome: RecoveryOutcome,
    pub agent: Value,
    pub attempt: Value,
    pub events: Vec<EventRecord>,
}

fn invalid(location: &'static str) -> InvalidRecord {
    InvalidRecord {
        location: location.into(),
    }
}
fn decode<T: serde::de::DeserializeOwned>(
    value: &Value,
    location: &'static str,
) -> Result<T, InvalidRecord> {
    serde_json::from_value(value.clone()).map_err(|_| invalid(location))
}
use crate::event_write::truthy as present;
fn field<'a>(record: &'a Map<String, Value>, key: &str) -> &'a Value {
    record.get(key).unwrap_or(&Value::Null)
}
fn fields_equal(left: &Map<String, Value>, right: &Map<String, Value>, fields: &[&str]) -> bool {
    fields
        .iter()
        .all(|key| field(left, key) == field(right, key))
}
fn source(agent: &Map<String, Value>) -> Map<String, Value> {
    SOURCE_FIELDS
        .iter()
        .map(|key| ((*key).into(), field(agent, key).clone()))
        .collect()
}
fn selected(record: &Map<String, Value>, fields: &[&str]) -> Map<String, Value> {
    fields
        .iter()
        .map(|key| ((*key).into(), field(record, key).clone()))
        .collect()
}

impl RecoverySnapshot {
    /// One checked entry from Python-written records. The repository must select
    /// the saved attempt by a run joined to this agent, inside its transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn from_records(
        agent: Value,
        attempt: Value,
        saved: Value,
        events: Vec<Value>,
        transport: TransportSnapshot,
        runtime_closed: bool,
        turn_has_work: bool,
    ) -> Result<Self, InvalidRecord> {
        for (name, value, location) in [
            ("PersistedAgentRecord", &agent, "agent"),
            ("PersistedClaudeAttemptRecord", &attempt, "attempt"),
            ("PersistedClaudeAttemptRecord", &saved, "savedAttempt"),
        ] {
            if let Some((pointer, _)) =
                studio_runtime_schema::first_error(name, value).map_err(|_| invalid(location))?
            {
                return Err(InvalidRecord {
                    location: format!("{location}{pointer}"),
                });
            }
        }
        let agent_typed: Agent = decode(&agent, "agent")?;
        let attempt_typed: Attempt = decode(&attempt, "attempt")?;
        let _: Attempt = decode(&saved, "savedAttempt")?;
        if events.len() > MAX_BATCH || attempt_typed.events.len() > MAX_BATCH {
            return Err(invalid("events"));
        }
        for event in &events {
            if let Some((pointer, _)) = studio_runtime_schema::first_error(
                "RuntimeEventMetadataRecord",
                event.get("metadata").unwrap_or(&Value::Null),
            )
            .map_err(|_| invalid("events/metadata"))?
            {
                return Err(InvalidRecord {
                    location: format!("events/metadata{pointer}"),
                });
            }
        }
        let events = events
            .iter()
            .map(|v| decode(v, "events"))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            agent: agent_typed,
            agent_record: decode(&agent, "agent")?,
            attempt: attempt_typed,
            attempt_record: decode(&attempt, "attempt")?,
            saved: decode(&saved, "savedAttempt")?,
            events,
            transport,
            runtime_closed,
            turn_has_work,
        })
    }

    fn eligible(&self, allow_held: bool) -> bool {
        !self.runtime_closed
            && self.agent.provider == "claude"
            && self.agent.auto_wake
            && self.agent.status != AgentStatus::Paused
            && (allow_held || !self.agent.native_failure_hold)
            && ![
                "deletedAt",
                "agentArchive",
                "accountTransferId",
                "workspaceOperation",
            ]
            .iter()
            .any(|key| present(self.agent_record.get(*key)))
            && crate::sync_views::native_thread_block(&Value::Object(self.agent_record.clone()))
                .is_none()
    }
    fn same_transport(&self) -> bool {
        !self.transport.closed
            && self.transport.account_key == self.agent.account_key
            && self.attempt.connection_id.as_deref() == Some(self.transport.connection_id.as_str())
            && field(&self.attempt_record, "supervisorIdentity")
                == self
                    .transport
                    .supervisor_identity
                    .as_ref()
                    .unwrap_or(&Value::Null)
    }

    pub fn recover(&self, rejection: &Rejection) -> Option<RecoveryDecision> {
        let a = &self.attempt;
        let first = a.events.first()?;
        if !self.eligible(false)
            || a.submitted != Some(true)
            || self.agent_record.get("startAttempt")
                != Some(&Value::Object(self.attempt_record.clone()))
            || present(self.attempt_record.get("action"))
            || a.id.is_empty()
            || a.epoch != self.agent.epoch
            || self.agent.turn_epoch.unwrap_or(self.agent.epoch) != self.agent.epoch
            || a.account_key != self.agent.account_key
            || a.thread_id != self.agent.thread_id
            || a.thread_id.as_deref().is_none_or(str::is_empty)
            || !self.same_transport()
            || a.events.iter().any(String::is_empty)
            || a.events
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != a.events.len()
            || a.native_operation_id.as_deref()
                != Some(format!("turn:{}:{}:attempt:{}", self.agent.id, first, a.id).as_str())
            || !fields_equal(&self.attempt_record, &self.saved, RECEIPT_FIELDS)
            || field(&self.saved, "activeAtReservation")
                != field(&self.attempt_record, "activeAtReservation")
            || present(self.saved.get("claudeInputRejection"))
        {
            return None;
        }
        let (turn_id, data) = match rejection {
            Rejection::Rpc { code, data } => {
                if *code != -32000
                    || a.active_at_reservation == Some(true)
                    || self.agent.turn_id.is_some()
                    || present(self.attempt_record.get("turnId"))
                    || present(self.attempt_record.get("observedTurnId"))
                {
                    return None;
                }
                (None, data)
            }
            Rejection::Turn { turn } => {
                let id = turn.get("id")?.as_str()?;
                let data = turn.get("error")?.get("data")?.as_object()?;
                if id.is_empty()
                    || turn.get("status")? != "failed"
                    || turn.get("startOutcome")? != "not_applied"
                    || turn.get("inputSubmitted").is_some_and(|v| v != false)
                    || (a.active_at_reservation == Some(true)
                        && turn.get("clientUserMessageId") != Some(&json!(first)))
                    || turn.get("clientUserMessageId").is_some_and(|v| v != first)
                    || self.turn_has_work
                    || a.created.is_none()
                {
                    return None;
                }
                let observed = self
                    .attempt_record
                    .get("turnId")
                    .filter(|v| present(Some(v)))
                    .or_else(|| self.attempt_record.get("observedTurnId"));
                if observed != Some(&json!(id))
                    || ["turnId", "observedTurnId"].iter().any(|key| {
                        self.attempt_record
                            .get(*key)
                            .is_some_and(|v| !v.is_null() && v != id)
                    })
                    || self.agent.turn_id.as_deref().is_some_and(|v| v != id)
                    || (self.agent.turn_id.is_none()
                        && self.agent_record.get("lastCompletedTurn") != Some(&json!(id)))
                {
                    return None;
                }
                (Some(id), data)
            }
        };
        if data.get("turnStartOutcome") != Some(&json!("not_applied"))
            || data.get("inputSubmitted").is_some_and(|v| v != false)
        {
            return None;
        }
        let input = a.claude_input_request.as_ref()?;
        if input.source != source(&self.agent_record)
            || input.client_user_message_id != *first
            || self.events.len() != a.events.len()
            || self
                .events
                .iter()
                .map(EventRecord::snapshot)
                .collect::<Vec<_>>()
                != input.events
        {
            return None;
        }
        for (event, id) in self.events.iter().zip(&a.events) {
            if event.id != *id
                || event.agent != self.agent.id
                || event.epoch != self.agent.epoch
                || !matches!(
                    event.status,
                    EventStatus::Reserved
                        | EventStatus::Dispatching
                        | EventStatus::Uncertain
                        | EventStatus::Delivered
                )
                || (event.status == EventStatus::Delivered
                    && (turn_id.is_none() || event.turn_id.as_deref() != turn_id))
                || event
                    .turn_id
                    .as_deref()
                    .is_some_and(|id| Some(id) != turn_id)
            {
                return None;
            }
            if let Some(native) = event.metadata.get("native").filter(|v| present(Some(v))) {
                let native = native.as_object()?;
                if native.get("agent") != Some(&json!(self.agent.id))
                    || native.get("epoch") != Some(&json!(self.agent.epoch))
                    || native.get("threadId") != Some(&json!(self.agent.thread_id))
                    || native.get("accountKey") != Some(&json!(self.agent.account_key))
                {
                    return None;
                }
            }
        }
        let retry = data.get("claudePreparationFailure") != Some(&json!("account_validation"))
            && !self
                .events
                .iter()
                .any(|e| present(e.metadata.get("claudePreInputRetryUsed")));
        let mut receipt = selected(&self.attempt_record, IDENTITY_FIELDS);
        receipt.extend([
            ("agent".into(), json!(self.agent.id)),
            ("turnId".into(), json!(turn_id)),
            ("outcome".into(), json!("not_applied")),
            ("retry".into(), json!(retry)),
        ]);
        let mut saved = self.saved.clone();
        saved.insert("claudeInputRejection".into(), Value::Object(receipt));
        saved.insert("executionOutcome".into(), json!("not_applied"));
        let mut events = self.events.clone();
        for event in &mut events {
            if retry {
                event
                    .metadata
                    .insert("claudePreInputRetryUsed".into(), json!(true));
            }
            event.status = EventStatus::Pending;
            event.turn_id = None;
            event.error = None;
        }
        let mut agent = self.agent_record.clone();
        let mut marker = selected(&self.attempt_record, IDENTITY_FIELDS);
        for key in ["submitted", "supervisorIdentity"] {
            marker.insert(key.into(), field(&self.attempt_record, key).clone());
        }
        agent.insert("claudePreInputRetry".into(), Value::Object(marker));
        agent.remove("startAttempt");
        agent.remove("startOutcomeHold");
        agent.insert("inFlight".into(), json!(false));
        agent.insert("turnId".into(), Value::Null);
        if retry {
            agent.remove("nativeFailureHold");
            agent.insert("status".into(), json!("queued"));
            agent.insert("error".into(), Value::Null);
        } else {
            agent.insert("status".into(), json!("failed"));
            agent.insert("nativeFailureHold".into(), json!(true));
        }
        Some(RecoveryDecision {
            outcome: if retry {
                RecoveryOutcome::Retry
            } else {
                RecoveryOutcome::Held
            },
            agent: Value::Object(agent),
            attempt: Value::Object(saved),
            events,
        })
    }
}

/// Capture before Submitted is committed. Configuration and input are retained
/// exactly; dynamic tools are refreshed independently at the next admission.
pub fn capture_input(
    agent: &Value,
    events: &[Value],
    params: &Value,
    text: &str,
) -> Result<Value, InvalidRecord> {
    let actor: Agent = decode(agent, "agent")?;
    let record: Map<String, Value> = decode(agent, "agent")?;
    let events: Vec<EventRecord> = events
        .iter()
        .map(|event| decode(event, "events"))
        .collect::<Result<_, _>>()?;
    let first = events.first().ok_or_else(|| invalid("events"))?;
    if events.len() > MAX_BATCH {
        return Err(invalid("events"));
    }
    let mut configuration: Map<String, Value> = decode(params, "params")?;
    let input: Vec<Value> = decode(
        &configuration
            .remove("input")
            .ok_or_else(|| invalid("params/input"))?,
        "params/input",
    )?;
    let client = configuration
        .remove("clientUserMessageId")
        .ok_or_else(|| invalid("params/clientUserMessageId"))?;
    configuration.remove("dynamicTools");
    Ok(
        json!({"input":input,"source":source(&record),"clientUserMessageId":client,
        "configuration":configuration,"events":events.iter().map(EventRecord::snapshot).collect::<Vec<_>>(),
        "transcriptItemId":format!("{}:{}",actor.id,first.id),"textHash":sha256_hex(text.as_bytes())}),
    )
}

impl RecoverySnapshot {
    pub fn rejected_start_saved(&self) -> bool {
        let Some(receipt) = self
            .saved
            .get("claudeInputRejection")
            .and_then(Value::as_object)
        else {
            return false;
        };
        fields_equal(&self.attempt_record, &self.saved, RECEIPT_FIELDS)
            && receipt.get("outcome") == Some(&json!("not_applied"))
            && receipt.get("agent") == Some(&json!(self.agent.id))
            && fields_equal(receipt, &self.attempt_record, IDENTITY_FIELDS)
    }

    fn saved_retry(&self, allow_held: bool, later_user: bool) -> Option<&FrozenInput> {
        let marker = self.agent_record.get("claudePreInputRetry")?.as_object()?;
        if !self.eligible(allow_held)
            || marker.get("epoch") != Some(&json!(self.agent.epoch))
            || marker.get("accountKey") != Some(&json!(self.agent.account_key))
            || marker.get("threadId") != Some(&json!(self.agent.thread_id))
            || !marker.iter().all(|(k, v)| field(&self.saved, k) == v)
            || !self.rejected_start_saved()
        {
            return None;
        }
        let request = self.attempt.claude_input_request.as_ref()?;
        let first = self.attempt.events.first()?;
        if request.transcript_item_id != format!("{}:{}", self.agent.id, first)
            || request.client_user_message_id != *first
        {
            return None;
        }
        let mut captured = request.source.clone();
        let mut current = source(&self.agent_record);
        if captured != current {
            let old = captured.remove("compactions")?.as_i64()?;
            let new = current.remove("compactions")?.as_i64()?;
            if !later_user || new <= old || captured != current {
                return None;
            }
        }
        (self.same_transport() || later_user).then_some(request)
    }

    fn unchanged_events(&self, request: &FrozenInput) -> bool {
        self.events.len() == self.attempt.events.len()
            && self.events.iter().zip(&self.attempt.events).all(|(e, id)| {
                e.id == *id && e.agent == self.agent.id && e.epoch == self.agent.epoch
            })
            && self
                .events
                .iter()
                .map(EventRecord::snapshot)
                .collect::<Vec<_>>()
                == request.events
    }

    /// Dispatch must reserve this entire ordered batch, never absorb later input.
    pub fn retry_batch(&self, later_user: bool) -> Option<Vec<String>> {
        let request = self.saved_retry(false, later_user)?;
        (self.unchanged_events(request)
            && self
                .events
                .iter()
                .all(|e| e.status == EventStatus::Pending && e.turn_id.is_none()))
        .then(|| self.attempt.events.clone())
    }

    /// Revalidate the reserved request and its full transcript immediately before I/O.
    pub fn retry_input(
        &self,
        later_user: bool,
        transcript: &Value,
        fulltext: Option<&str>,
    ) -> Result<Option<Value>, InvalidRecord> {
        let reservation = self
            .agent_record
            .get("startAttempt")
            .and_then(Value::as_object);
        let retry_of = reservation
            .and_then(|a| a.get("claudeRetryOf"))
            .filter(|v| present(Some(v)));
        if retry_of.is_none() {
            return if present(self.agent_record.get("claudePreInputRetry")) {
                Err(invalid("startAttempt/claudeRetryOf"))
            } else {
                Ok(None)
            };
        }
        let request = self
            .saved_retry(false, later_user)
            .ok_or_else(|| invalid("claudePreInputRetry"))?;
        if retry_of != Some(&json!(self.attempt.id))
            || reservation.and_then(|a| a.get("events")) != Some(&json!(self.attempt.events))
            || !self.unchanged_events(request)
        {
            return Err(invalid("events"));
        }
        let text = self
            .input_text(request, transcript, fulltext)
            .ok_or_else(|| invalid("transcript"))?;
        Ok(Some(
            json!({"input":request.input,"source":request.source,"configuration":request.configuration,
            "events":request.events,"transcriptItemId":request.transcript_item_id,
            "clientUserMessageId":request.client_user_message_id,"textHash":request.text_hash,"text":text}),
        ))
    }

    fn input_text<'a>(
        &self,
        request: &FrozenInput,
        transcript: &'a Value,
        fulltext: Option<&'a str>,
    ) -> Option<&'a str> {
        let text = fulltext.or_else(|| transcript.get("text").and_then(Value::as_str))?;
        (transcript.get("id") == Some(&json!(request.transcript_item_id))
            && transcript.get("role") == Some(&json!("user"))
            && sha256_hex(text.as_bytes()) == request.text_hash)
            .then_some(text)
    }

    pub fn resume_held(
        &self,
        later_user: bool,
        transcript: &Value,
        fulltext: Option<&str>,
    ) -> Option<Value> {
        if !self.agent.native_failure_hold
            || !later_user
            || present(self.agent_record.get("inFlight"))
            || self.agent.turn_id.is_some()
            || present(self.agent_record.get("startAttempt"))
        {
            return None;
        }
        let request = self.saved_retry(true, later_user)?;
        if !self.unchanged_events(request)
            || self
                .events
                .iter()
                .any(|e| e.status != EventStatus::Pending || e.turn_id.is_some())
            || self.input_text(request, transcript, fulltext).is_none()
        {
            return None;
        }
        let mut agent = self.agent_record.clone();
        agent.remove("nativeFailureHold");
        agent.insert("status".into(), json!("queued"));
        agent.insert("error".into(), Value::Null);
        Some(Value::Object(agent))
    }
}

pub struct RetiredRetry {
    pub agent: Value,
    pub reservation: Option<Value>,
    pub events: Vec<EventRecord>,
}
impl RecoverySnapshot {
    /// Stop advances the epoch. Only the exact old, proven-unsent retry can retire.
    pub fn retire_stopped(&self, reservation: Option<&Value>) -> Option<RetiredRetry> {
        let marker = self.agent_record.get("claudePreInputRetry")?.as_object()?;
        let epoch = marker.get("epoch")?.as_i64()?;
        if self.runtime_closed
            || self.agent.provider != "claude"
            || epoch >= self.agent.epoch
            || !marker.iter().all(|(k, v)| field(&self.saved, k) == v)
            || !self.rejected_start_saved()
            || self.events.len() != self.attempt.events.len()
        {
            return None;
        }
        if !self
            .events
            .iter()
            .zip(&self.attempt.events)
            .all(|(event, id)| {
                event.id == *id
                    && event.agent == self.agent.id
                    && event.epoch == epoch
                    && event.turn_id.is_none()
                    && matches!(
                        event.status,
                        EventStatus::Cancelled | EventStatus::Reserved | EventStatus::Dispatching
                    )
            })
        {
            return None;
        }
        let mut agent = self.agent_record.clone();
        let mut events = self.events.clone();
        let mut updated = None;
        if events
            .iter()
            .any(|event| event.status != EventStatus::Cancelled)
        {
            if [
                "inFlight",
                "turnId",
                "deletedAt",
                "agentArchive",
                "accountTransferId",
                "workspaceOperation",
            ]
            .iter()
            .any(|key| present(agent.get(*key)))
                || crate::sync_views::native_thread_block(&Value::Object(agent.clone())).is_some()
                || marker.get("accountKey") != Some(&json!(self.agent.account_key))
                || marker.get("threadId") != Some(&json!(self.agent.thread_id))
                || !self.same_transport()
                || self.attempt.claude_input_request.as_ref()?.source != source(&agent)
            {
                return None;
            }
            let attempt = agent.get("startAttempt")?.as_object()?;
            if attempt.get("claudeRetryOf") != marker.get("id")
                || attempt.get("epoch") != Some(&json!(epoch))
                || attempt.get("events") != Some(&json!(self.attempt.events))
                || attempt.get("accountKey") != marker.get("accountKey")
                || attempt.get("submitted") != Some(&json!(false))
                || [
                    "action",
                    "activeAtReservation",
                    "nativeOperationId",
                    "turnId",
                    "observedTurnId",
                ]
                .iter()
                .any(|key| present(attempt.get(*key)))
                || ["threadId", "connectionId"].iter().any(|key| {
                    !field(attempt, key).is_null() && field(attempt, key) != field(marker, key)
                })
            {
                return None;
            }
            let mut saved = reservation?.clone();
            if saved.get("id") != attempt.get("id")
                || saved.get("submission") != Some(&json!("unsent"))
                || saved.get("claudeRetryOf") != marker.get("id")
                || !RECEIPT_FIELDS
                    .iter()
                    .all(|key| saved.get(*key).unwrap_or(&Value::Null) == field(attempt, key))
            {
                return None;
            }
            if events.iter().map(EventRecord::snapshot).collect::<Vec<_>>()
                != self.attempt.claude_input_request.as_ref()?.events
            {
                return None;
            }
            let reason = "Cancelled by Stop before Claude retry input submission";
            saved["executionOutcome"] = json!("unsent");
            saved["notSubmittedReason"] = json!(reason);
            updated = Some(saved);
            for event in &mut events {
                if event.status != EventStatus::Cancelled {
                    event.status = EventStatus::Cancelled;
                    event.error = Some(reason.into());
                }
            }
            agent.remove("startAttempt");
        }
        agent.remove("claudePreInputRetry");
        Some(RetiredRetry {
            agent: Value::Object(agent),
            reservation: updated,
            events,
        })
    }
}
