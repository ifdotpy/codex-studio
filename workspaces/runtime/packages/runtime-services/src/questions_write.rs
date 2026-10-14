//! Atomic writes for durable questions and user complaints.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, QuestionsWriteRepository, Transaction,
};
use thiserror::Error;
use uuid::Uuid;

use crate::event_write::{self, EventWriteError};

const USER: &str = "user";
const PENDING: &str = "pending";
const IN_PROGRESS: &str = "in_progress";
const RESOLVED: &str = "resolved";
const DECLINED: &str = "declined";

#[derive(Debug, Error)]
pub enum QuestionsWriteError {
    #[error("{0}")]
    InvalidRequest(&'static str),
    #[error("{0}")]
    Conflict(&'static str),
    #[error("invalid stored object {object}/{id}")]
    InvalidStoredObject { object: &'static str, id: String },
    #[error("Not implemented: requires provider-session/question-answer")]
    QuestionAnswerUnavailable,
    #[error(transparent)]
    Event(#[from] EventWriteError),
    #[error(transparent)]
    Store(#[from] studio_runtime_store::StoreError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("system clock is before the Unix epoch")]
    Clock,
}

pub struct ComplaintResponseWrite<'a> {
    pub complaint_id: &'a str,
    pub request_id: &'a str,
    pub text: &'a str,
    pub status: &'a str,
    pub version: i64,
    pub signature: &'a str,
}

pub fn defer_question(
    transaction: &Transaction<'_>,
    id: &str,
    deferred: bool,
    validate: impl Fn(&Value) -> bool,
) -> Result<Value, QuestionsWriteError> {
    let mut record = request(transaction, id, &validate)?;
    if !is_question(&record) {
        return Err(QuestionsWriteError::InvalidRequest(
            "Only a question can be deferred",
        ));
    }
    if record.get("status").and_then(Value::as_str) != Some(PENDING) {
        return Err(QuestionsWriteError::InvalidRequest(
            "This question is no longer pending",
        ));
    }
    let agent_id = record
        .get("agent")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_stored("runtime_requests", id))?;
    let agent = stored_agent(transaction, agent_id)?
        .ok_or(QuestionsWriteError::InvalidRequest("Unknown agent"))?;
    if event_write::truthy(agent.extra.get("deletedAt")) {
        return Err(QuestionsWriteError::InvalidRequest(
            "This conversation was deleted",
        ));
    }
    let previous_deferred = record
        .get("deferred")
        .cloned()
        .unwrap_or(Value::Bool(false));
    if previous_deferred != Value::Bool(deferred) {
        record["deferred"] = json!(deferred);
        record["deferredAt"] = json!(unix_time()?);
        record["deferredBy"] = json!(USER);
        put_request(transaction, id, &record, &validate)?;
    }
    Ok(json!({"id":id,"status":PENDING,"deferred":deferred}))
}

pub fn delete_question(
    transaction: &Transaction<'_>,
    id: &str,
    validate: impl Fn(&Value) -> bool,
) -> Result<Value, QuestionsWriteError> {
    let mut record = request(transaction, id, &validate)?;
    let agent_id = record
        .get("agent")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_stored("runtime_requests", id))?;
    let agent = stored_agent(transaction, agent_id)?
        .ok_or(QuestionsWriteError::InvalidRequest("Unknown agent"))?;
    if !state_only_delete_allowed(
        &record,
        event_write::truthy(agent.extra.get("deletedAt")),
        true,
    )? {
        return Ok(json!({"id":id,"status":"deleted"}));
    }
    mark_state_only_deleted(&mut record, unix_time()?);
    put_request(transaction, id, &record, &validate)?;
    Ok(json!({"id":id,"status":"deleted"}))
}

pub(crate) fn state_only_delete_allowed(
    record: &Value,
    agent_deleted: bool,
    reject_pending_native: bool,
) -> Result<bool, QuestionsWriteError> {
    if !is_question(record) {
        return Err(QuestionsWriteError::InvalidRequest(
            "Only a question can be deleted",
        ));
    }
    if agent_deleted {
        return Err(QuestionsWriteError::InvalidRequest(
            "This agent was deleted",
        ));
    }
    if event_write::truthy(record.get("deletedAt")) {
        return Ok(false);
    }
    if reject_pending_native
        && record.get("status").and_then(Value::as_str) == Some(PENDING)
        && record.get("method").and_then(Value::as_str) != Some("agent/asyncQuestion")
    {
        return Err(QuestionsWriteError::QuestionAnswerUnavailable);
    }
    if matches!(
        record.get("status").and_then(Value::as_str),
        Some("answering" | "uncertain")
    ) {
        return Err(QuestionsWriteError::InvalidRequest(
            "Question delivery is uncertain; wait for its receipt",
        ));
    }
    Ok(true)
}

pub(crate) fn mark_state_only_deleted(record: &mut Value, deleted_at: f64) {
    record["status"] = json!("deleted");
    record["deletedAt"] = json!(deleted_at);
    record["deletedBy"] = json!(USER);
    record["deferred"] = json!(false);
}

pub fn submit_complaint(
    transaction: &Transaction<'_>,
    lead_id: &str,
    request_id: &str,
    text: &str,
    validate: impl Fn(&Value) -> bool,
) -> Result<Value, QuestionsWriteError> {
    let actor = stored_agent(transaction, lead_id)?
        .ok_or(QuestionsWriteError::InvalidRequest("Invalid request"))?;
    if event_write::truthy(actor.extra.get("deletedAt")) {
        return Err(QuestionsWriteError::InvalidRequest(
            "This agent was deleted",
        ));
    }
    let lead = stored_agent(transaction, &actor.root_id.0)?.ok_or(
        QuestionsWriteError::InvalidRequest("A complaint needs an existing lead"),
    )?;
    if lead.id != lead.root_id || !event_write::truthy(lead.extra.get("isLead")) {
        return Err(QuestionsWriteError::InvalidRequest(
            "A complaint needs an existing lead",
        ));
    }
    if event_write::truthy(lead.extra.get("deletedAt")) {
        return Err(QuestionsWriteError::InvalidRequest(
            "A complaint needs an existing lead",
        ));
    }
    let text = event_write::python_strip(text);
    if text.is_empty() || text.chars().count() > 12_000 {
        return Err(QuestionsWriteError::InvalidRequest(
            "Describe the complaint in 1 to 12000 characters",
        ));
    }
    let id = Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("complaint:user:{request_id}").as_bytes(),
    )
    .to_string();
    let now = unix_time()?;
    if let Some(raw) = QuestionsWriteRepository::complaint(transaction, &id)? {
        let previous = parse_complaint(&raw, &id, &validate)?;
        if previous.get("text").and_then(Value::as_str) != Some(text)
            || previous.get("author").and_then(Value::as_str) != Some(USER)
            || previous.get("leadId").and_then(Value::as_str) != Some(lead.id.0.as_str())
        {
            return Err(QuestionsWriteError::InvalidRequest(
                "This complaint id has different content",
            ));
        }
        return Ok(previous);
    }
    let record = json!({
        "id": id,
        "leadId": lead.id.0,
        "author": USER,
        "text": text,
        "recipient": "lead",
        "version": 1,
        "status": "open",
        "created": now,
        "updated": now,
        "readAt": null,
        "responses": []
    });
    if !validate(&record) {
        return Err(invalid_stored(
            "runtime_complaints",
            record["id"].as_str().unwrap_or_default(),
        ));
    }
    let id = record["id"]
        .as_str()
        .ok_or_else(|| QuestionsWriteError::InvalidRequest("Invalid complaint"))?;
    QuestionsWriteRepository::put_complaint(transaction, id, &event_write::python_json(&record))?;
    let message = complaint_message(&record);
    event_write::enqueue_event(
        transaction,
        &lead,
        format!("complaint:{id}"),
        "complaint",
        message,
        now,
    )?;
    Ok(record)
}

pub fn respond_to_complaint(
    transaction: &Transaction<'_>,
    input: ComplaintResponseWrite<'_>,
    validate: impl Fn(&Value) -> bool,
) -> Result<Value, QuestionsWriteError> {
    let ComplaintResponseWrite {
        complaint_id,
        request_id,
        text,
        status,
        version,
        signature,
    } = input;
    let receipt_id = format!("user:{request_id}");
    if let Some(saved) = event_write::read_receipt(
        transaction,
        Some(&receipt_id),
        signature,
        "runtime_complaints",
        complaint_id,
        &validate,
    )? {
        return Ok(saved);
    }
    let raw = QuestionsWriteRepository::complaint(transaction, complaint_id)?
        .ok_or(QuestionsWriteError::InvalidRequest("Unknown complaint"))?;
    let mut complaint = parse_complaint(&raw, complaint_id, &validate)?;
    let recipient = complaint
        .get("recipient")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_stored("runtime_complaints", complaint_id))?
        .to_owned();
    let author = complaint
        .get("author")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_stored("runtime_complaints", complaint_id))?
        .to_owned();
    let lead_id = complaint
        .get("leadId")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_stored("runtime_complaints", complaint_id))?;
    if recipient != USER {
        let lead = stored_agent(transaction, lead_id)?
            .ok_or(QuestionsWriteError::InvalidRequest("Unknown managed agent"))?;
        let answered = complaint
            .get("responses")
            .and_then(Value::as_array)
            .is_some_and(|responses| {
                responses
                    .iter()
                    .any(|response| response.get("author").and_then(Value::as_str) == Some(lead_id))
            });
        let stopped = event_write::truthy(lead.extra.get("deletedAt"))
            || !event_write::truthy(lead.extra.get("autoWake"));
        if !matches!(status, RESOLVED | DECLINED) || !(answered || stopped) {
            return Err(QuestionsWriteError::InvalidRequest(
                "This complaint requires a response from its orchestrator",
            ));
        }
    }
    if complaint.get("version").and_then(Value::as_i64) != Some(version) {
        return Err(QuestionsWriteError::Conflict(
            "This complaint changed. Review the latest response before replying",
        ));
    }
    let text = event_write::python_strip(text);
    if text.is_empty()
        || text.chars().count() > 12_000
        || !matches!(status, IN_PROGRESS | RESOLVED | DECLINED)
    {
        return Err(QuestionsWriteError::InvalidRequest(
            "Record an action or reason and select in_progress, resolved, or declined",
        ));
    }
    let now = unix_time()?;
    let response_id = format!("user:{request_id}");
    let response = json!({
        "id": response_id,
        "author": USER,
        "text": text,
        "status": status,
        "at": now
    });
    complaint["responses"]
        .as_array_mut()
        .ok_or_else(|| invalid_stored("runtime_complaints", complaint_id))?
        .push(response.clone());
    complaint["status"] = json!(status);
    complaint["updated"] = json!(now);
    if complaint.get("readAt").is_none_or(Value::is_null) {
        complaint["readAt"] = json!(now);
    }
    complaint["version"] = json!(version + 1);
    if !validate(&complaint) {
        return Err(invalid_stored("runtime_complaints", complaint_id));
    }
    let reporter = stored_agent(transaction, &author)?
        .ok_or(QuestionsWriteError::InvalidRequest("Unknown managed agent"))?;
    QuestionsWriteRepository::put_complaint(
        transaction,
        complaint_id,
        &event_write::python_json(&complaint),
    )?;
    if !event_write::truthy(reporter.extra.get("deletedAt")) {
        let event = json!({
            "complaint_id": complaint_id,
            "responder": USER,
            "response": response
        });
        event_write::enqueue_event(
            transaction,
            &reporter,
            format!("complaint-response:{response_id}"),
            "complaint_response",
            event_write::python_json(&event),
            now,
        )?;
    }
    event_write::save_receipt(transaction, Some(&receipt_id), signature, &complaint)?;
    Ok(complaint)
}

fn request(
    transaction: &Transaction<'_>,
    id: &str,
    validate: &impl Fn(&Value) -> bool,
) -> Result<Value, QuestionsWriteError> {
    let raw = QuestionsWriteRepository::request(transaction, id)?
        .ok_or(QuestionsWriteError::InvalidRequest("Unknown question"))?;
    let record: Value =
        serde_json::from_str(&raw).map_err(|_| invalid_stored("runtime_requests", id))?;
    if record.get("id").and_then(Value::as_str) != Some(id) || !validate(&record) {
        return Err(invalid_stored("runtime_requests", id));
    }
    Ok(record)
}

fn put_request(
    transaction: &Transaction<'_>,
    id: &str,
    record: &Value,
    validate: &impl Fn(&Value) -> bool,
) -> Result<(), QuestionsWriteError> {
    if !validate(record) {
        return Err(invalid_stored("runtime_requests", id));
    }
    QuestionsWriteRepository::put_request(transaction, id, &event_write::python_json(record))?;
    Ok(())
}

fn is_question(record: &Value) -> bool {
    match record.get("method").and_then(Value::as_str) {
        Some("agent/asyncQuestion" | "item/tool/requestUserInput") => true,
        Some("mcpServer/elicitation/request") => {
            record
                .get("params")
                .and_then(|params| params.get("mode"))
                .and_then(Value::as_str)
                == Some("form")
        }
        _ => false,
    }
}

fn complaint_message(record: &Value) -> String {
    let complaint = json!({
        "complaint_id": record["id"],
        "author": USER,
        "author_name": USER,
        "text": record["text"],
        "status": record["status"],
        "created": record["created"]
    });
    event_write::python_json(&json!({
        "complaints": [complaint],
        "required": "Respond to each complaint with orchestration_complaint action=respond before finishing. The full complaint is in this message. No separate read call is required."
    }))
}

fn stored_agent(
    transaction: &Transaction<'_>,
    id: &str,
) -> Result<Option<AgentRecord>, QuestionsWriteError> {
    let key = AgentId::from(id);
    let Some(raw) = AgentRepository::get_raw(transaction, &key)? else {
        return Ok(None);
    };
    let agent: AgentRecord =
        serde_json::from_str(&raw).map_err(|_| invalid_stored("runtime_agents", id))?;
    if agent.id.0 != id {
        return Err(invalid_stored("runtime_agents", id));
    }
    Ok(Some(agent))
}

fn parse_complaint(
    raw: &str,
    id: &str,
    validate: &impl Fn(&Value) -> bool,
) -> Result<Value, QuestionsWriteError> {
    let complaint: Value =
        serde_json::from_str(raw).map_err(|_| invalid_stored("runtime_complaints", id))?;
    if complaint.get("id").and_then(Value::as_str) != Some(id) || !validate(&complaint) {
        return Err(invalid_stored("runtime_complaints", id));
    }
    Ok(complaint)
}

fn invalid_stored(object: &'static str, id: &str) -> QuestionsWriteError {
    QuestionsWriteError::InvalidStoredObject {
        object,
        id: id.to_owned(),
    }
}

fn unix_time() -> Result<f64, QuestionsWriteError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .map_err(|_| QuestionsWriteError::Clock)
}
