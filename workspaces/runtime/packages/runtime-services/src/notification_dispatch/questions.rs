//! Async questions and exact native resolution, within the delivery transaction.
use super::notices::ordered_text;
use super::snapshot::checked;
use super::{NotificationDispatchError, stored};
use serde::Deserialize;
use serde_json::{Value, json};
use studio_runtime_schema::complaint_record::{
    ComplaintRecipient, ComplaintSource, ComplaintStatus,
};
use studio_runtime_store::CachedSql;
use studio_runtime_store::system_reads as records;
use studio_runtime_store::{
    AgentId, AgentRepository, ComplaintRecord, RequestRecord, SyncDocumentsRepository, Table,
    Transaction,
};
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Question {
    title: String,
    options: Option<Vec<Value>>,
    #[serde(default, alias = "multi_select")]
    multi_select: bool,
    #[serde(default)]
    is_secret: bool,
}
fn fields(value: &Value) -> Result<Vec<Value>, NotificationDispatchError> {
    Vec::<Question>::deserialize(value).map_err(|_| NotificationDispatchError::invalid())?.into_iter().enumerate().map(|(index,q)| {
        let options=q.options.unwrap_or_default().into_iter().map(|option|match option {
            Value::String(label)=>Ok(json!({"label":label})),
            Value::Object(ref object) if object.get("label").is_some_and(Value::is_string)=>Ok(option),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_)=>Err(NotificationDispatchError::invalid()),
        }).collect::<Result<Vec<_>,_>>()?;
        Ok(json!({"id":index.to_string(),"question":q.title,"options":options,"multiSelect":q.multi_select,"isSecret":q.is_secret}))
    }).collect()
}

pub(crate) fn submit_agent_complaint(
    tx: &Transaction<'_>,
    agent: &Value,
    request_id: &str,
    text: &str,
    at: f64,
) -> Result<ComplaintRecord, NotificationDispatchError> {
    let agent_id = agent["id"]
        .as_str()
        .ok_or(NotificationDispatchError::invalid())?;
    let lead_id = agent["rootId"]
        .as_str()
        .ok_or_else(|| stored("runtime_agents", agent_id))?;
    let lead = AgentRepository::get(tx, &AgentId::from(lead_id))?
        .ok_or_else(|| stored("runtime_agents", lead_id))?;
    if lead.is_lead != Some(true) || lead.is_deleted() {
        return Err(NotificationDispatchError::invalid());
    }
    let text = text.trim();
    if text.is_empty() || text.chars().count() > 12_000 {
        return Err(NotificationDispatchError::invalid());
    }
    let complaint_id = Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("complaint:{request_id}").as_bytes(),
    )
    .to_string();
    if let Some(previous) = Table::<ComplaintRecord>::new(tx).get(&complaint_id)? {
        if previous.text() != Some(text)
            || previous.author() != Some(agent_id)
            || previous.lead_id() != Some(lead_id)
        {
            return Err(NotificationDispatchError::ConflictingInput);
        }
        return Ok(previous);
    }
    let recipient = if agent_id == lead_id {
        ComplaintRecipient::User
    } else {
        ComplaintRecipient::Lead
    };
    let typed = ComplaintRecord {
        id: complaint_id.clone(),
        lead_id: Some(lead_id.to_owned()),
        author: Some(agent_id.to_owned()),
        text: Some(text.to_owned()),
        recipient,
        version: Some(1),
        status: ComplaintStatus::Open,
        created: Some(at),
        updated: Some(at),
        read_at: None,
        responses: Vec::new(),
        source_type: None,
        title: None,
    };
    let record = typed.clone().into_value();
    checked("runtime_complaints", &record)?;
    Table::<ComplaintRecord>::new(tx).put(&typed)?;
    let context = crate::sync_views::ComplaintViewContext {
        agents: &[
            serde::Deserialize::deserialize(agent.clone())?,
            serde::Deserialize::deserialize(lead.value())?,
        ],
    };
    let view = crate::sync_views::complaint_entity_view(&record, &context)
        .ok_or_else(|| stored("runtime_complaints", &complaint_id))?;
    SyncDocumentsRepository::put_entity(tx, "complaint", &complaint_id, view, false)?;
    if recipient == ComplaintRecipient::Lead {
        let complaint = ordered_text(
            &json!({"complaint_id":complaint_id,"author":agent_id,"author_name":agent.get("name").unwrap_or(&agent["id"]),"text":text,"status":"open","created":at}),
            [
                "complaint_id",
                "author",
                "author_name",
                "text",
                "status",
                "created",
            ]
            .map(str::to_owned),
        );
        let message = format!(
            "{{\"complaints\": [{complaint}], \"required\": {}}}",
            json!(
                "Respond to each complaint with orchestration_complaint action=respond before finishing. The full complaint is in this message. No separate read call is required."
            )
        );
        let inserted = crate::event_write::enqueue_event(
            tx,
            &lead,
            format!("complaint:{complaint_id}"),
            "complaint",
            message,
            at,
        )
        .map_err(|_| stored("runtime_events", &complaint_id))?;
        if inserted && lead.auto_wake == Some(false) {
            tx.execute_cached("UPDATE runtime_event_meta SET record=json_set(record,'$.timing.fastSkipReason','autoWake') WHERE id=?1", [format!("complaint:{complaint_id}")]).map_err(studio_runtime_store::StoreError::from)?;
        }
    }
    Ok(typed)
}

/// Python `send_to_user`: the lead submits a complaint to itself, which is
/// addressed to the user, and the record is saved again as an ordinary message.
pub(super) fn submit_user_message(
    tx: &Transaction<'_>,
    lead: &Value,
    request_id: &str,
    text: &str,
    at: f64,
) -> Result<ComplaintRecord, NotificationDispatchError> {
    if lead["id"] != lead["rootId"] {
        return Err(NotificationDispatchError::invalid());
    }
    let mut message = submit_agent_complaint(tx, lead, request_id, text, at)?;
    message.source_type = Some(ComplaintSource::Message);
    save_complaint_projection(tx, &message)?;
    Ok(message)
}

/// Transaction-aware implementation of Runtime.complaint read/respond actions.
/// The callback caller owns commit; the response event is durable in this write.
pub(crate) fn agent_complaint_action(
    tx: &Transaction<'_>,
    actor: &Value,
    args: &Value,
    request_id: &str,
    at: f64,
) -> Result<Value, NotificationDispatchError> {
    let actor_id = actor["id"]
        .as_str()
        .ok_or(NotificationDispatchError::invalid())?;
    let root_id = actor["rootId"]
        .as_str()
        .ok_or_else(|| stored("runtime_agents", actor_id))?;
    let root = AgentRepository::get(tx, &AgentId::from(root_id))?
        .ok_or_else(|| stored("runtime_agents", root_id))?;
    if root.is_lead != Some(true) || root.deleted_at.is_some() {
        return Err(NotificationDispatchError::invalid());
    }
    match args["action"].as_str() {
        Some("submit") => {
            let text = args["text"]
                .as_str()
                .ok_or(NotificationDispatchError::invalid())?;
            submit_agent_complaint(tx, actor, request_id, text, at).map(ComplaintRecord::into_value)
        }
        Some("read") => {
            if args.get("complaint_id").is_some()
                && !args["complaint_id"].is_null()
                && !args["complaint_id"].is_string()
            {
                return Err(NotificationDispatchError::invalid());
            }
            let requested = args["complaint_id"].as_str();
            let mut complaints = Table::<ComplaintRecord>::new(tx).list()?;
            complaints.retain(|complaint| {
                complaint.lead_id() == Some(root_id)
                    && requested.is_none_or(|id| complaint.id == id)
            });
            if requested.is_some() && complaints.is_empty() {
                return Err(NotificationDispatchError::invalid());
            }
            complaints.sort_by(|left, right| {
                needs_agent_response(right)
                    .cmp(&needs_agent_response(left))
                    .then_with(|| {
                        right
                            .created
                            .unwrap_or_default()
                            .total_cmp(&left.created.unwrap_or_default())
                    })
            });
            let total = complaints.len();
            for complaint in complaints.iter_mut().take(50) {
                if actor_id == root_id
                    && complaint.recipient == ComplaintRecipient::Lead
                    && complaint.read_at.is_none()
                {
                    complaint.read_at = Some(at);
                    save_complaint_projection(tx, complaint)?;
                }
            }
            complaints.truncate(50);
            Ok(json!({
                "complaints":complaints,
                "remaining":total.saturating_sub(50),
                "instruction":"Respond only to entries assigned to the lead. Entries assigned to user wait for the user. Do not poll the book."
            }))
        }
        Some("respond") => {
            if actor_id != root_id {
                return Err(NotificationDispatchError::invalid());
            }
            let id = args["complaint_id"]
                .as_str()
                .ok_or(NotificationDispatchError::invalid())?;
            let raw = studio_runtime_store::QuestionsWriteRepository::complaint(tx, id)?
                .ok_or(NotificationDispatchError::invalid())?;
            let mut complaint: ComplaintRecord =
                serde_json::from_str(&raw).map_err(|_| stored("runtime_complaints", id))?;
            if complaint.id() != id {
                return Err(stored("runtime_complaints", id));
            }
            if complaint.lead_id() != Some(actor_id)
                || complaint.recipient == ComplaintRecipient::User
            {
                return Err(NotificationDispatchError::invalid());
            }
            let text = args["text"]
                .as_str()
                .ok_or(NotificationDispatchError::invalid())?
                .trim();
            let status = studio_runtime_schema::complaints::ComplaintResponseStatus::deserialize(
                &args["status"],
            )
            .map_err(|_| NotificationDispatchError::invalid())?;
            if text.is_empty() || text.chars().count() > 12_000 {
                return Err(NotificationDispatchError::invalid());
            }
            if let Some(existing) = complaint
                .responses
                .iter()
                .find(|response| response.id == request_id)
            {
                if existing.text != text || existing.status != status {
                    return Err(NotificationDispatchError::ConflictingInput);
                }
                return Ok(complaint.into_value());
            }
            let response = studio_runtime_schema::complaint_record::ComplaintResponse {
                id: request_id.to_owned(),
                author: actor_id.to_owned(),
                text: text.to_owned(),
                status,
                at,
            };
            complaint.responses.push(response.clone());
            complaint.status = match status {
                studio_runtime_schema::complaints::ComplaintResponseStatus::InProgress => {
                    ComplaintStatus::InProgress
                }
                studio_runtime_schema::complaints::ComplaintResponseStatus::Resolved => {
                    ComplaintStatus::Resolved
                }
                studio_runtime_schema::complaints::ComplaintResponseStatus::Declined => {
                    ComplaintStatus::Declined
                }
            };
            complaint.updated = Some(at);
            if complaint.read_at.is_none() {
                complaint.read_at = Some(at);
            }
            complaint.version = Some(complaint.version.unwrap_or(0) + 1);
            save_complaint_projection(tx, &complaint)?;
            reconcile_complaint_notifications(tx, actor_id)?;
            let author = complaint.author().unwrap_or_default();
            if author != "user" && author != actor_id {
                let reporter = AgentRepository::get(tx, &AgentId::from(author))?
                    .ok_or_else(|| stored("runtime_agents", author))?;
                if !reporter.is_deleted() {
                    let event = json!({"complaint_id":id,"lead":actor_id,"response":response});
                    crate::event_write::enqueue_event(
                        tx,
                        &reporter,
                        format!("complaint-response:{request_id}"),
                        "complaint_response",
                        crate::event_write::python_json(&event),
                        at,
                    )
                    .map_err(|_| stored("runtime_events", id))?;
                }
            }
            Ok(complaint.into_value())
        }
        _ => Err(NotificationDispatchError::invalid()),
    }
}

fn needs_agent_response(complaint: &ComplaintRecord) -> bool {
    if matches!(
        complaint.status,
        ComplaintStatus::Resolved | ComplaintStatus::Declined
    ) {
        return false;
    }
    let responsible = if complaint.recipient == ComplaintRecipient::User {
        "user"
    } else {
        complaint.lead_id().unwrap_or_default()
    };
    !complaint
        .responses
        .iter()
        .any(|response| response.author == responsible)
}

fn reconcile_complaint_notifications(
    tx: &Transaction<'_>,
    lead_id: &str,
) -> Result<(), NotificationDispatchError> {
    let pending = {
        let mut statement = tx.prepare(
            "SELECT id,text FROM runtime_events WHERE agent=?1 AND kind='complaint' AND status='pending'",
        ).map_err(studio_runtime_store::StoreError::from)?;
        statement
            .query_map([lead_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(studio_runtime_store::StoreError::from)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(studio_runtime_store::StoreError::from)?
    };
    for (event_id, text) in pending {
        let Ok(payload) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(entries) = payload.get("complaints").and_then(Value::as_array) else {
            continue;
        };
        if entries.is_empty() {
            continue;
        }
        let mut all_answered = true;
        for entry in entries {
            let Some(id) = entry.get("complaint_id").and_then(Value::as_str) else {
                all_answered = false;
                break;
            };
            let Some(raw) = studio_runtime_store::QuestionsWriteRepository::complaint(tx, id)?
            else {
                all_answered = false;
                break;
            };
            let Ok(complaint) = serde_json::from_str::<ComplaintRecord>(&raw) else {
                all_answered = false;
                break;
            };
            if complaint.id() != id
                || complaint.lead_id() != Some(lead_id)
                || needs_agent_response(&complaint)
            {
                all_answered = false;
                break;
            }
        }
        if all_answered {
            tx.execute_cached(
                "UPDATE runtime_events SET status='stored_only',error=?1 WHERE id=?2 AND status='pending'",
                rusqlite::params!["Every referenced complaint already has a response", event_id],
            ).map_err(studio_runtime_store::StoreError::from)?;
        }
    }
    Ok(())
}

fn save_complaint_projection(
    tx: &Transaction<'_>,
    complaint: &ComplaintRecord,
) -> Result<(), NotificationDispatchError> {
    let id = complaint.id();
    Table::<ComplaintRecord>::new(tx).put(complaint)?;
    let agents: Vec<_> = records::all(tx, "runtime_agents")?
        .into_iter()
        .map(serde::Deserialize::deserialize)
        .collect::<Result<_, _>>()?;
    let context = crate::sync_views::ComplaintViewContext { agents: &agents };
    let view = crate::sync_views::complaint_entity_view(&complaint.clone().into_value(), &context)
        .ok_or_else(|| stored("runtime_complaints", id))?;
    SyncDocumentsRepository::put_entity(tx, "complaint", id, view, false)?;
    Ok(())
}

pub(super) fn completed(
    tx: &Transaction<'_>,
    agent: &Value,
    params: &Value,
    at: f64,
) -> Result<(), NotificationDispatchError> {
    let questions = fields(&params["item"]["questions"])?;
    let id = crate::questions_reads::async_question_id(
        agent["id"].as_str().unwrap_or_default(),
        params["item"]["id"].as_str().unwrap_or_default(),
    );
    if Table::<RequestRecord>::new(tx).get(&id)?.is_some() {
        return Ok(());
    }
    if agent["isLead"] == true {
        let record = json!({"id":id,"method":"agent/asyncQuestion","agent":agent["id"],"epoch":agent["epoch"],"turnId":params.get("turnId").filter(|v|!v.is_null() && **v != "").unwrap_or(&agent["turnId"]),"params":{"questions":questions},"status":"pending","createdAt":at});
        put_request(tx, &record)?;
    } else {
        let text = format!(
            "Questions for the orchestrator:\n[{}]",
            questions
                .iter()
                .map(|q| ordered_text(
                    q,
                    ["id", "question", "options", "multiSelect", "isSecret"].map(str::to_owned)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
        submit_agent_complaint(tx, agent, &id, &text, at)?;
    }
    Ok(())
}

pub(super) fn resolved(
    tx: &Transaction<'_>,
    agent: &mut Value,
    params: &Value,
    account: &str,
    generation: u64,
    at: f64,
) -> Result<(), NotificationDispatchError> {
    use studio_runtime_store::{RecordOrder, RecordPredicate};
    let id = agent["id"]
        .as_str()
        .ok_or(NotificationDispatchError::invalid())?
        .to_owned();
    let records = Table::<RequestRecord>::new(tx).list_matching(
        Some(RecordPredicate::JsonAgentEquals(&id)),
        RecordOrder::RowId,
    )?;
    let mut pending = false;
    for record in records {
        let connection_id = record.connection_id().map(str::to_owned);
        let mut status = record.status().map(str::to_owned);
        let mut record = record.into_value();
        // A card names its provider generation: the Rust account worker stores
        // it under `supervisor`; Python named the connection after it.
        let same_generation = record
            .pointer("/supervisor/generation")
            .and_then(Value::as_u64)
            == Some(generation)
            || connection_id.as_deref() == Some(format!("connection-{generation}").as_str());
        if record["rpcId"] == params["requestId"]
            && record.get("accountKey").unwrap_or(&json!("default")) == account
            && same_generation
            && record["params"]["threadId"] == params["threadId"]
            && matches!(
                status.as_deref(),
                Some("pending" | "answering" | "uncertain" | "blocked")
            )
        {
            record["status"] = json!("resolved");
            status = Some("resolved".into());
            record["resolvedAt"] = json!(at);
            put_request(tx, &record)?;
        }
        pending |= status.as_deref() == Some("pending");
    }
    if agent["status"] == "approval"
        && !pending
        && agent["inFlight"] == true
        && agent["autoWake"] == true
    {
        agent["status"] = json!("running");
    }
    Ok(())
}
fn put_request(tx: &Transaction<'_>, record: &Value) -> Result<(), NotificationDispatchError> {
    let typed = RequestRecord::from_value(record.clone()).map_err(|_| {
        stored(
            "runtime_requests",
            record["id"].as_str().unwrap_or("<missing>"),
        )
    })?;
    Table::new(tx).put(&typed)?;
    let id = record["id"]
        .as_str()
        .ok_or(NotificationDispatchError::invalid())?;
    let view = crate::sync_views::project_persisted_record("request", record)
        .ok_or_else(|| stored("runtime_requests", id))?;
    SyncDocumentsRepository::put_entity(tx, "request", id, view, record["status"] != "pending")?;
    Ok(())
}
