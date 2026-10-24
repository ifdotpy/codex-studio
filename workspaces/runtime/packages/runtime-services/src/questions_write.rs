//! Atomic writes for durable questions and user complaints.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, QuestionsWriteRepository, RuntimeStore,
    SyncDocumentsRepository, Transaction,
};
use thiserror::Error;
use uuid::Uuid;

use crate::event_write::{self, EventWriteError};
use crate::{
    coordination::{self, CoordinationError, SystemFollowup},
    host_project_runtime::PreparedHost,
    message_admission::MessageAdmission,
    projects::ProjectsError,
};

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
    #[error(transparent)]
    Coordination(#[from] CoordinationError),
    #[error(transparent)]
    Projects(#[from] ProjectsError),
}

pub struct ComplaintResponseWrite<'a> {
    pub complaint_id: &'a str,
    pub request_id: &'a str,
    pub text: &'a str,
    pub status: &'a str,
    pub version: i64,
    pub signature: &'a str,
}

pub struct ComplaintWriteResult {
    pub record: Value,
    pub event_inserted: bool,
    pub wake_event_worker: bool,
}

pub struct AsyncQuestionAnswerResult {
    pub response: Value,
    pub event_inserted: bool,
}

pub struct AsyncQuestionAnswerInput<'a, V> {
    pub id: &'a str,
    pub data: &'a Value,
    pub text: &'a str,
    pub admission: &'a dyn MessageAdmission,
    pub prepared: &'a PreparedHost,
    pub now: f64,
    pub validate: V,
}

pub async fn answer_async_question_in_store(
    store: std::sync::Arc<RuntimeStore>,
    id: &str,
    data: &Value,
    now: f64,
    validate: impl Fn(&Value) -> bool + Send + Sync + 'static,
) -> Result<Option<AsyncQuestionAnswerResult>, QuestionsWriteError> {
    let id = id.to_owned();
    let data = data.clone();
    let validate: std::sync::Arc<dyn Fn(&Value) -> bool + Send + Sync> =
        std::sync::Arc::new(validate);
    let validate_for_preparation = std::sync::Arc::clone(&validate);
    let id_for_preparation = id.clone();
    let data_for_preparation = data.clone();
    let read = store
        .read_async("runtime-services/questions_write", move |transaction| {
            let raw = QuestionsWriteRepository::request(transaction, &id_for_preparation)?
                .ok_or(QuestionsWriteError::InvalidRequest("Unknown question"))?;
            let record: Value = serde_json::from_str(&raw)
                .map_err(|_| invalid_stored("runtime_requests", &id_for_preparation))?;
            if record.get("id").and_then(Value::as_str) != Some(id_for_preparation.as_str())
                || !validate_for_preparation(&record)
            {
                return Err(invalid_stored("runtime_requests", &id_for_preparation));
            }
            if record.get("method").and_then(Value::as_str) != Some("agent/asyncQuestion") {
                return Ok(None);
            }
            let agent_id = record
                .get("agent")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_stored("runtime_requests", &id_for_preparation))?;
            let text = async_answer_text(&record, &data_for_preparation)?;
            let admission =
                crate::message_admission::MessageIngressAdmission::read(transaction, agent_id)?;
            Ok(Some((text, admission)))
        })
        .await?;
    let Some((text, admission)) = read else {
        return Ok(None);
    };
    let admission = admission.prepare().await?;
    let prepared = PreparedHost::load(&store).await?;
    let validate_for_transaction = std::sync::Arc::clone(&validate);
    let result = store
        .transaction_with_error(move |transaction| {
            answer_async_question(
                transaction,
                AsyncQuestionAnswerInput {
                    id: &id,
                    data: &data,
                    text: &text,
                    admission: &admission,
                    prepared: &prepared,
                    now,
                    validate: move |record: &Value| validate_for_transaction(record),
                },
            )
        })
        .await?;
    Ok(Some(result))
}

fn async_answer_text(record: &Value, data: &Value) -> Result<String, QuestionsWriteError> {
    let questions = record
        .pointer("/params/questions")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            invalid_stored(
                "runtime_requests",
                record["id"].as_str().unwrap_or_default(),
            )
        })?;
    let answers = data.get("answers").and_then(Value::as_object).ok_or(
        QuestionsWriteError::InvalidRequest("Supply question answers"),
    )?;
    let mut sections = Vec::with_capacity(questions.len());
    for question in questions {
        let question_id = question.get("id").and_then(Value::as_str).ok_or_else(|| {
            invalid_stored(
                "runtime_requests",
                record["id"].as_str().unwrap_or_default(),
            )
        })?;
        let prompt = question
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or(question_id);
        let choices = answers
            .get(question_id)
            .and_then(|answer| answer.get("answers"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if choices.iter().any(|choice| !choice.is_string()) {
            return Err(QuestionsWriteError::InvalidRequest(
                "Supply question answers",
            ));
        }
        let mut section = vec![prompt.to_owned()];
        section.extend(choices.iter().filter_map(Value::as_str).map(str::to_owned));
        sections.push(section.join("\n"));
    }
    Ok(sections.join("\n"))
}

/// Atomically admit an async-question follow-up, mirror an active radio answer,
/// and record the answer receipt. All filesystem and budget history preparation
/// is owned by the caller and happens before this writer transaction.
pub fn answer_async_question<V: Fn(&Value) -> bool>(
    transaction: &Transaction<'_>,
    input: AsyncQuestionAnswerInput<'_, V>,
) -> Result<AsyncQuestionAnswerResult, QuestionsWriteError> {
    let AsyncQuestionAnswerInput {
        id,
        data,
        text,
        admission,
        prepared,
        now,
        validate,
    } = input;
    let mut record = request(transaction, id, &validate)?;
    if record.get("method").and_then(Value::as_str) != Some("agent/asyncQuestion") {
        return Err(QuestionsWriteError::InvalidRequest("Not an async question"));
    }
    let signature = crate::provider_callbacks::answer_signature(data);
    if let Some(previous) = record.get("answerSignature").and_then(Value::as_str) {
        if previous != signature {
            return Err(QuestionsWriteError::Conflict(
                "This request already has a different answer",
            ));
        }
        if record.get("status").and_then(Value::as_str) == Some("answered") {
            return Ok(AsyncQuestionAnswerResult {
                response: json!({"status":"answered","replayed":true}),
                event_inserted: false,
            });
        }
        return Err(QuestionsWriteError::InvalidRequest(
            "Answer delivery is uncertain. Do not resend; inspect the agent conversation",
        ));
    }
    if record.get("status").and_then(Value::as_str) != Some(PENDING) {
        return Err(QuestionsWriteError::InvalidRequest(
            "Request is no longer pending",
        ));
    }
    let agent_id = record
        .get("agent")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_stored("runtime_requests", id))?;
    let agent = stored_agent(transaction, agent_id)?
        .ok_or(QuestionsWriteError::InvalidRequest("Unknown agent"))?;
    if !event_write::truthy(agent.extra.get("isLead")) {
        return Err(QuestionsWriteError::InvalidRequest(
            "Only the orchestrator can ask the user. The subagent must contact its orchestrator.",
        ));
    }
    if event_write::truthy(agent.extra.get("deletedAt"))
        || record.get("epoch").and_then(Value::as_i64) != Some(agent.epoch)
        || !event_write::truthy(agent.extra.get("autoWake"))
    {
        return Err(QuestionsWriteError::InvalidRequest(
            "This question belongs to a stopped turn",
        ));
    }
    let text = event_write::python_strip(text);
    let consumed = crate::radio_runtime::route_question_answer(
        transaction,
        prepared,
        &record,
        text,
        None,
        now,
    )?;
    let event_inserted = if consumed {
        false
    } else {
        let turn_id = record.get("turnId").and_then(Value::as_str);
        let accepted = coordination::send_system_followup_with_radio_answer(
            transaction,
            SystemFollowup {
                recipient_id: agent_id,
                id: &format!("{id}:answer"),
                text,
                delivery: studio_runtime_schema::delivery::DeliveryMoment::CurrentTurn,
                now,
            },
            turn_id,
            admission,
        )?;
        crate::radio_runtime::record_question_answer_followup(
            transaction,
            prepared,
            &record,
            text,
            now,
        )?;
        accepted.event_inserted
    };

    record["status"] = json!("answered");
    record["answerSignature"] = json!(signature);
    record["answeredBy"] = json!(USER);
    record["answeredAt"] = json!(now);
    record["decision"] = data.get("decision").cloned().unwrap_or(json!("answer"));
    record["deferred"] = json!(false);
    record["answerHistory"] = answer_history(&record, data)?;
    put_request(transaction, id, &record, &validate)?;
    Ok(AsyncQuestionAnswerResult {
        response: json!({"status":"answered"}),
        event_inserted,
    })
}

fn answer_history(record: &Value, data: &Value) -> Result<Value, QuestionsWriteError> {
    let questions = record
        .pointer("/params/questions")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            invalid_stored(
                "runtime_requests",
                record["id"].as_str().unwrap_or_default(),
            )
        })?;
    let answers = data.get("answers").and_then(Value::as_object).ok_or(
        QuestionsWriteError::InvalidRequest("Supply question answers"),
    )?;
    let mut history = Vec::with_capacity(questions.len());
    for question in questions {
        let question_id = question.get("id").and_then(Value::as_str).ok_or_else(|| {
            invalid_stored(
                "runtime_requests",
                record["id"].as_str().unwrap_or_default(),
            )
        })?;
        let values = answers
            .get(question_id)
            .and_then(|answer| answer.get("answers"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if values.iter().any(|value| !value.is_string()) {
            return Err(QuestionsWriteError::InvalidRequest(
                "Supply question answers",
            ));
        }
        history.push(json!({
            "id":question_id,
            "question":question.get("question").and_then(Value::as_str).unwrap_or(question_id),
            "isSecret":event_write::truthy(question.get("isSecret")),
            "answer":if event_write::truthy(question.get("isSecret")) { json!("[redacted]") } else { json!(values) }
        }));
    }
    Ok(Value::Array(history))
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
) -> Result<ComplaintWriteResult, QuestionsWriteError> {
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
        return Ok(ComplaintWriteResult {
            record: previous,
            event_inserted: false,
            wake_event_worker: false,
        });
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
    let event_inserted = event_write::enqueue_event(
        transaction,
        &lead,
        format!("complaint:{id}"),
        "complaint",
        message,
        now,
    )?;
    // Python's enqueue() fast-dispatches only for autoWake agents, while its
    // send eligibility excludes moveImportPending agents. Keep the event
    // durable, but do not wake the worker while either owner gate is closed.
    let wake_event_worker = event_inserted
        && event_write::truthy(lead.extra.get("autoWake"))
        && !event_write::recovery_pending(&lead);
    Ok(ComplaintWriteResult {
        record,
        event_inserted,
        wake_event_worker,
    })
}

pub fn respond_to_complaint(
    transaction: &Transaction<'_>,
    input: ComplaintResponseWrite<'_>,
    validate: impl Fn(&Value) -> bool,
) -> Result<ComplaintWriteResult, QuestionsWriteError> {
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
        return Ok(ComplaintWriteResult {
            record: saved,
            event_inserted: false,
            wake_event_worker: false,
        });
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
    let event_inserted = if !event_write::truthy(reporter.extra.get("deletedAt")) {
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
        )?
    } else {
        false
    };
    // Match Python enqueue/send gates: preserve the event, but do not wake
    // while autoWake is disabled or move import/recovery blocks sending.
    let wake_event_worker = event_inserted
        && event_write::truthy(reporter.extra.get("autoWake"))
        && !event_write::recovery_pending(&reporter);
    event_write::save_receipt(transaction, Some(&receipt_id), signature, &complaint)?;
    Ok(ComplaintWriteResult {
        record: complaint,
        event_inserted,
        wake_event_worker,
    })
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
    // The renderer reads requests as entities: publish the stored change with it.
    let view = crate::sync_views::project_persisted_record("request", record)
        .ok_or_else(|| invalid_stored("runtime_requests", id))?;
    SyncDocumentsRepository::put_entity(
        transaction,
        "request",
        id,
        view,
        record["status"] != PENDING,
    )?;
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
