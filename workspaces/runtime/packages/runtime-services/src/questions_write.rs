//! Atomic writes for durable questions and user complaints.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use studio_runtime_schema::approvals::{
    Answer, Approval, ApprovalId, ApprovalState, CommandId, Deferral, Hidden, Principal,
    QuestionAnswer, Request, Timestamp,
};
use studio_runtime_schema::complaints::ComplaintResponseStatus;
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, QuestionsWriteRepository, RuntimeStore, Transaction,
    approvals,
};
use thiserror::Error;
use uuid::Uuid;

use crate::approval_sync;
use crate::event_write::{self, EventWriteError};
use crate::{
    coordination::{self, CoordinationError, SystemFollowup},
    host_project_runtime::PreparedHost,
    message_admission::MessageAdmission,
    projects::ProjectsError,
};

const USER: &str = "user";
const PENDING: &str = "pending";

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
    Approval(#[from] studio_runtime_store::approvals::ApprovalError),
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
    pub status: ComplaintResponseStatus,
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

pub struct AsyncQuestionAnswerInput<'a> {
    pub id: &'a str,
    pub data: &'a Value,
    pub admission: &'a dyn MessageAdmission,
    pub prepared: &'a PreparedHost,
    pub now: f64,
}

pub async fn answer_async_question_in_store(
    store: std::sync::Arc<RuntimeStore>,
    id: &str,
    data: &Value,
    now: f64,
) -> Result<Option<AsyncQuestionAnswerResult>, QuestionsWriteError> {
    let id = id.to_owned();
    let data = data.clone();
    let id_for_preparation = id.clone();
    let read = store
        .read_async("runtime-services/questions_write", move |transaction| {
            let row = request(transaction, &id_for_preparation)?;
            if !matches!(row.request, Request::AgentQuestion { .. }) {
                return Ok::<_, QuestionsWriteError>(None);
            }
            let admission = crate::message_admission::MessageIngressAdmission::read(
                transaction,
                row.agent.as_str(),
            )?;
            Ok(Some(admission))
        })
        .await?;
    let Some(admission) = read else {
        return Ok(None);
    };
    let admission = admission.prepare().await?;
    let prepared = PreparedHost::load(&store).await?;
    let result = store
        .transaction_with_error(move |transaction| {
            answer_async_question(
                transaction,
                AsyncQuestionAnswerInput {
                    id: &id,
                    data: &data,
                    admission: &admission,
                    prepared: &prepared,
                    now,
                },
            )
        })
        .await?;
    Ok(Some(result))
}

/// Atomically admit an async-question follow-up, mirror an active radio answer,
/// and record the answer. All filesystem and budget history preparation
/// is owned by the caller and happens before this writer transaction.
pub fn answer_async_question(
    transaction: &Transaction<'_>,
    input: AsyncQuestionAnswerInput<'_>,
) -> Result<AsyncQuestionAnswerResult, QuestionsWriteError> {
    let AsyncQuestionAnswerInput {
        id,
        data,
        admission,
        prepared,
        now,
    } = input;
    let row = request(transaction, id)?;
    let Request::AgentQuestion { questions } = &row.request else {
        return Err(QuestionsWriteError::InvalidRequest("Not an async question"));
    };
    let answers = answered_questions(questions, data)?;
    // A secret answer is sent to the agent but stored redacted.
    let stored = Answer::Answers {
        answers: answers
            .iter()
            .map(|question| QuestionAnswer {
                id: question.id.clone(),
                answer: if question.secret {
<<<<<<< head
                    vec!["[redacted]".to_owned()]
                } else {
                    question.answers.clone()
                },
            })
            .collect(),
    };
    match &row.state {
        ApprovalState::Pending => {}
||||||| base
    }
    let signature = crate::provider_callbacks::answer_signature(data);
    if let Some(previous) = record.get("answerSignature").and_then(Value::as_str) {
        if previous != signature {
            return Err(QuestionsWriteError::Conflict(
                "This request already has a different answer",
=======
                    vec![QuestionAnswer::REDACTED.to_owned()]
                } else {
                    question.answers.clone()
                },
            })
            .collect(),
    };
    match &row.state {
        ApprovalState::Pending if row.hidden.is_none() => {}
>>>>>>> origin/feat/approval-entity
        ApprovalState::Answering { answer, .. }
        | ApprovalState::Answered { answer, .. }
        | ApprovalState::Unknown { answer, .. } => {
            if *answer != stored {
                return Err(QuestionsWriteError::Conflict(
                    "This request already has a different answer",
                ));
            }
            if matches!(row.state, ApprovalState::Answered { .. }) {
                return Ok(AsyncQuestionAnswerResult {
                    response: json!({"status":"answered","replayed":true}),
                    event_inserted: false,
                });
            }
            return Err(QuestionsWriteError::InvalidRequest(
                "Answer delivery is uncertain. Do not resend; inspect the agent conversation",
            ));
        }
<<<<<<< head
        ApprovalState::Closed { .. } => {
||||||| base
        if record.get("status").and_then(Value::as_str) == Some("answered") {
            return Ok(AsyncQuestionAnswerResult {
                response: json!({"status":"answered","replayed":true}),
                event_inserted: false,
            });
=======
        // Closed, or deleted while pending.
        ApprovalState::Pending | ApprovalState::Closed { .. } => {
>>>>>>> origin/feat/approval-entity
            return Err(QuestionsWriteError::InvalidRequest(
                "Request is no longer pending",
            ));
        }
    }
    let agent_id = row.agent.as_str();
    let agent = stored_agent(transaction, agent_id)?
        .ok_or(QuestionsWriteError::InvalidRequest("Unknown agent"))?;
    if !event_write::truthy(agent.extra.get("isLead")) {
        return Err(QuestionsWriteError::InvalidRequest(
            "Only the orchestrator can ask the user. The subagent must contact its orchestrator.",
        ));
    }
    if event_write::truthy(agent.extra.get("deletedAt"))
        || row.epoch.map(|epoch| epoch as i64) != Some(agent.epoch)
        || agent.auto_wake != Some(true)
    {
        return Err(QuestionsWriteError::InvalidRequest(
            "This question belongs to a stopped turn",
        ));
    }
    let text = answers
        .iter()
        .map(|question| {
            std::iter::once(question.prompt.as_str())
                .chain(question.answers.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = event_write::python_strip(&text);
    let route =
        crate::radio_runtime::route_question_answer(transaction, prepared, &row, text, None, now)?;
    let send_followup = |radio_answer_turn_id: Option<&str>| -> Result<bool, QuestionsWriteError> {
        let accepted = coordination::send_system_followup_with_radio_answer(
            transaction,
            SystemFollowup {
                recipient_id: agent_id,
                id: &format!("{id}:answer"),
                text,
                delivery: studio_runtime_schema::delivery::DeliveryMoment::CurrentTurn,
                now,
            },
            radio_answer_turn_id,
            admission,
        )?;
        crate::radio_runtime::record_question_answer_followup(
            transaction,
            prepared,
            &row,
            text,
            now,
        )?;
        Ok(accepted.event_inserted)
    };
    let event_inserted = match &route {
        crate::radio_runtime::QuestionAnswerRoute::ConsumedByRadio => false,
        crate::radio_runtime::QuestionAnswerRoute::OrdinaryFollowup => send_followup(None)?,
        crate::radio_runtime::QuestionAnswerRoute::RadioFollowup { turn_id } => {
            send_followup(Some(turn_id))?
        }
    };
    approvals::answer(transaction, &row.id, stored, CommandId(id.to_owned()))?;
    let mut answered =
        approvals::confirm_delivery(transaction, &row.id, Principal::Owner, Timestamp(now))?;
    if let Some(deferral) = answered.deferral.as_mut() {
        deferral.active = false;
        approvals::put(transaction, &answered)?;
    }
    approval_sync::publish(transaction, &answered)?;
    Ok(AsyncQuestionAnswerResult {
        response: json!({"status":"answered"}),
        event_inserted,
    })
}

/// Consumed leaves of an async answer.
struct AnsweredQuestion {
    id: String,
    prompt: String,
    secret: bool,
    answers: Vec<String>,
}

/// The body of the answer route for questions: one `answers` list per question id.
#[derive(serde::Deserialize)]
struct AnswerBody {
    answers: std::collections::BTreeMap<String, AnswerLeaf>,
}

/// Historical missing and null leaves mean no selected answers.
#[derive(serde::Deserialize)]
struct AnswerLeaf {
    answers: Option<Vec<String>>,
}

fn answered_questions(
    questions: &[studio_runtime_schema::approvals::Question],
    data: &Value,
) -> Result<Vec<AnsweredQuestion>, QuestionsWriteError> {
    let body: AnswerBody = serde_json::from_value(data.clone())
        .map_err(|_| QuestionsWriteError::InvalidRequest("Supply question answers"))?;
    Ok(questions
        .iter()
        .map(|question| AnsweredQuestion {
            id: question.id.clone(),
            prompt: if question.question.is_empty() {
                question.id.clone()
            } else {
                question.question.clone()
            },
            secret: question.is_secret,
            answers: body
                .answers
                .get(&question.id)
                .and_then(|leaf| leaf.answers.clone())
                .unwrap_or_default(),
        })
        .collect())
}

pub fn defer_question(
    transaction: &Transaction<'_>,
    id: &str,
    deferred: bool,
) -> Result<Value, QuestionsWriteError> {
    let mut row = request(transaction, id)?;
    if !is_question(&row) {
        return Err(QuestionsWriteError::InvalidRequest(
            "Only a question can be deferred",
        ));
    }
<<<<<<< head
    if !matches!(row.state, ApprovalState::Pending) {
||||||| base
    if record.get("status").and_then(Value::as_str) != Some(PENDING) {
=======
    if !matches!(row.state, ApprovalState::Pending) || row.hidden.is_some() {
>>>>>>> origin/feat/approval-entity
        return Err(QuestionsWriteError::InvalidRequest(
            "This question is no longer pending",
        ));
    }
    let agent = stored_agent(transaction, row.agent.as_str())?
        .ok_or(QuestionsWriteError::InvalidRequest("Unknown agent"))?;
    if event_write::truthy(agent.extra.get("deletedAt")) {
        return Err(QuestionsWriteError::InvalidRequest(
            "This conversation was deleted",
        ));
    }
    let previous = row.deferral.as_ref().is_some_and(|d| d.active);
    if previous != deferred {
        row.deferral = Some(Deferral {
            active: deferred,
            at: Timestamp(unix_time()?),
            by: Principal::Owner,
        });
        approvals::put(transaction, &row)?;
        approval_sync::publish(transaction, &row)?;
    }
    Ok(json!({"id":id,"status":PENDING,"deferred":deferred}))
}

pub fn delete_question(
    transaction: &Transaction<'_>,
    id: &str,
) -> Result<Value, QuestionsWriteError> {
    let mut row = request(transaction, id)?;
    let agent = stored_agent(transaction, row.agent.as_str())?
        .ok_or(QuestionsWriteError::InvalidRequest("Unknown agent"))?;
    if !delete_allowed(
        &row,
        event_write::truthy(agent.extra.get("deletedAt")),
        true,
    )? {
        return Ok(json!({"id":id,"status":"deleted"}));
    }
    hide(&mut row, unix_time()?);
    approvals::put(transaction, &row)?;
    approval_sync::publish(transaction, &row)?;
    Ok(json!({"id":id,"status":"deleted"}))
}

<<<<<<< head
/// Whether a question may be hidden. `false` means it is hidden already.
pub(crate) fn delete_allowed(
    row: &Approval,
||||||| base
pub(crate) fn state_only_delete_allowed(
    record: &Value,
=======
/// Hides an agent question. Returns None for any other id: provider cards are
/// deleted through the callback runtime.
pub fn delete_agent_question(
    transaction: &Transaction<'_>,
    id: &str,
) -> Result<Option<Value>, QuestionsWriteError> {
    match approvals::get(transaction, &ApprovalId(id.to_owned()))? {
        Some(row) if matches!(row.request, Request::AgentQuestion { .. }) => {
            delete_question(transaction, id).map(Some)
        }
        _ => Ok(None),
    }
}

/// Whether a question may be hidden. `false` means it is hidden already.
pub(crate) fn delete_allowed(
    row: &Approval,
    agent_deleted: bool,
    reject_pending_native: bool,
) -> Result<bool, QuestionsWriteError> {
    if !is_question(row) {
        return Err(QuestionsWriteError::InvalidRequest(
            "Only a question can be deleted",
        ));
    }
    if agent_deleted {
        return Err(QuestionsWriteError::InvalidRequest(
            "This agent was deleted",
        ));
    }
    if row.hidden.is_some() {
        return Ok(false);
    }
    if reject_pending_native
        && matches!(row.state, ApprovalState::Pending)
        && !matches!(row.request, Request::AgentQuestion { .. })
    {
        return Err(QuestionsWriteError::QuestionAnswerUnavailable);
    }
    if matches!(
        row.state,
        ApprovalState::Answering { .. } | ApprovalState::Unknown { .. }
    ) {
        return Err(QuestionsWriteError::InvalidRequest(
            "Question delivery is uncertain; wait for its receipt",
        ));
    }
    Ok(true)
}

/// History stays: a deleted question is hidden, its state is kept.
pub(crate) fn hide(row: &mut Approval, at: f64) {
    row.hidden = Some(Hidden {
        at: Timestamp(at),
        by: Principal::Owner,
    });
    if let Some(deferral) = row.deferral.as_mut() {
        deferral.active = false;
    }
}

// Value-based helpers of the provider card path. Removed with that path (cluster 4).
pub(crate) fn state_only_delete_allowed(
    record: &Value,
>>>>>>> origin/feat/approval-entity
    agent_deleted: bool,
    reject_pending_native: bool,
) -> Result<bool, QuestionsWriteError> {
<<<<<<< head
    if !is_question(row) {
||||||| base
    if !is_question(record) {
=======
    let is_question = match record.get("method").and_then(Value::as_str) {
        Some("agent/asyncQuestion" | "item/tool/requestUserInput") => true,
        Some("mcpServer/elicitation/request") => {
            record
                .get("params")
                .and_then(|params| params.get("mode"))
                .and_then(Value::as_str)
                == Some("form")
        }
        _ => false,
    };
    if !is_question {
>>>>>>> origin/feat/approval-entity
        return Err(QuestionsWriteError::InvalidRequest(
            "Only a question can be deleted",
        ));
    }
    if agent_deleted {
        return Err(QuestionsWriteError::InvalidRequest(
            "This agent was deleted",
        ));
    }
    if row.hidden.is_some() {
        return Ok(false);
    }
    if reject_pending_native
        && matches!(row.state, ApprovalState::Pending)
        && !matches!(row.request, Request::AgentQuestion { .. })
    {
        return Err(QuestionsWriteError::QuestionAnswerUnavailable);
    }
    if matches!(
        row.state,
        ApprovalState::Answering { .. } | ApprovalState::Unknown { .. }
    ) {
        return Err(QuestionsWriteError::InvalidRequest(
            "Question delivery is uncertain; wait for its receipt",
        ));
    }
    Ok(true)
}

/// History stays: a deleted question is hidden, its state is kept.
pub(crate) fn hide(row: &mut Approval, at: f64) {
    row.hidden = Some(Hidden {
        at: Timestamp(at),
        by: Principal::Owner,
    });
    if let Some(deferral) = row.deferral.as_mut() {
        deferral.active = false;
    }
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
    let wake_event_worker =
        event_inserted && lead.auto_wake == Some(true) && !event_write::recovery_pending(&lead);
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
    let now_ms = (unix_time()? * 1000.0) as i64;
    let admitted = match event_write::begin_request::<studio_runtime_store::ComplaintRecord>(
        transaction,
        event_write::RequestKey::new("complaint_response", &[complaint_id, USER], request_id)
            .raw(&receipt_id),
        signature,
        now_ms,
    )
    .map_err(EventWriteError::from)?
    {
        event_write::CallerAdmission::Replay(reply) => {
            return Ok(ComplaintWriteResult {
                record: reply.into_value(),
                event_inserted: false,
                wake_event_worker: false,
            });
        }
        event_write::CallerAdmission::Fresh(admitted) => admitted,
    };
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
        let stopped =
            event_write::truthy(lead.extra.get("deletedAt")) || lead.auto_wake != Some(true);
        let closing = match status {
            ComplaintResponseStatus::InProgress => false,
            ComplaintResponseStatus::Resolved | ComplaintResponseStatus::Declined => true,
        };
        if !closing || !(answered || stopped) {
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
    if text.is_empty() || text.chars().count() > 12_000 {
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
        && reporter.auto_wake == Some(true)
        && !event_write::recovery_pending(&reporter);
    event_write::settle_request(
        transaction,
        &admitted,
        Some(USER),
        &studio_runtime_store::ComplaintRecord::from_value(complaint.clone())?,
    )
    .map_err(EventWriteError::from)?;
    Ok(ComplaintWriteResult {
        record: complaint,
        event_inserted,
        wake_event_worker,
    })
}

fn request(transaction: &Transaction<'_>, id: &str) -> Result<Approval, QuestionsWriteError> {
    approvals::get(transaction, &ApprovalId(id.to_owned()))?
        .ok_or(QuestionsWriteError::InvalidRequest("Unknown question"))
}

fn is_question(row: &Approval) -> bool {
    match &row.request {
        Request::AgentQuestion { .. } | Request::Question { .. } => true,
        Request::Elicitation { .. } => {
            row.detail.params.0.get("mode").and_then(Value::as_str) == Some("form")
<<<<<<< head
||||||| base
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
=======
>>>>>>> origin/feat/approval-entity
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
    let complaint: studio_runtime_store::ComplaintRecord =
        serde_json::from_str(raw).map_err(|_| invalid_stored("runtime_complaints", id))?;
    if complaint.id() != id {
        return Err(invalid_stored("runtime_complaints", id));
    }
    let complaint = complaint.into_value();
    if !validate(&complaint) {
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
