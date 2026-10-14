//! Async questions and exact native resolution, within the delivery transaction.
use super::notices::ordered_text;
use super::snapshot::checked;
use super::{NotificationDispatchError, stored};
use serde::Deserialize;
use serde_json::{Value, json};
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
    let questions: Vec<Question> = serde_json::from_value(value.clone())
        .map_err(|_| NotificationDispatchError::InvalidNotification)?;
    questions.into_iter().enumerate().map(|(index,q)| {
        let options=q.options.unwrap_or_default().into_iter().map(|option|match option {
            Value::String(label)=>Ok(json!({"label":label})),
            Value::Object(ref object) if object.get("label").is_some_and(Value::is_string)=>Ok(option),
            _=>Err(NotificationDispatchError::InvalidNotification),
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
) -> Result<Value, NotificationDispatchError> {
    let agent_id = agent["id"]
        .as_str()
        .ok_or(NotificationDispatchError::InvalidNotification)?;
    let lead_id = agent["rootId"]
        .as_str()
        .ok_or_else(|| stored("runtime_agents", agent_id))?;
    let lead = AgentRepository::get(tx, &AgentId::from(lead_id))?
        .ok_or_else(|| stored("runtime_agents", lead_id))?;
    if !lead
        .extra
        .get("isLead")
        .is_some_and(|value| value == &Value::Bool(true))
        || studio_runtime_store::truthy(lead.extra.get("deletedAt"))
    {
        return Err(NotificationDispatchError::InvalidNotification);
    }
    let text = text.trim();
    if text.is_empty() || text.chars().count() > 12_000 {
        return Err(NotificationDispatchError::InvalidNotification);
    }
    let complaint_id = Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("complaint:{request_id}").as_bytes(),
    )
    .to_string();
    if let Some(previous) = Table::<ComplaintRecord>::new(tx).get(&complaint_id)? {
        let previous = previous.value();
        if previous["text"] != text
            || previous["author"] != agent_id
            || previous["leadId"] != lead_id
        {
            return Err(NotificationDispatchError::ConflictingInput);
        }
        return Ok(previous.clone());
    }
    let recipient = if agent_id == lead_id { "user" } else { "lead" };
    let record = json!({"id":complaint_id,"leadId":lead_id,"author":agent_id,"text":text,"recipient":recipient,"version":1,"status":"open","created":at,"updated":at,"readAt":null,"responses":[]});
    checked("runtime_complaints", &record)?;
    Table::<ComplaintRecord>::new(tx).put(&ComplaintRecord::from_value(record.clone())?)?;
    let context = crate::sync_views::ComplaintViewContext {
        agents: vec![agent.clone(), serde_json::to_value(&lead)?],
    };
    let view = crate::sync_views::complaint_entity_view(&record, &context)
        .ok_or_else(|| stored("runtime_complaints", &complaint_id))?;
    SyncDocumentsRepository::put_entity(tx, "complaint", &complaint_id, view, false)?;
    if recipient == "lead" {
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
        if inserted && lead.extra.get("autoWake") == Some(&json!(false)) {
            tx.execute("UPDATE runtime_event_meta SET record=json_set(record,'$.timing.fastSkipReason','autoWake') WHERE id=?1", [format!("complaint:{complaint_id}")]).map_err(studio_runtime_store::StoreError::from)?;
        }
    }
    Ok(record)
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
        .ok_or(NotificationDispatchError::InvalidNotification)?;
    let root_id = actor["rootId"]
        .as_str()
        .ok_or_else(|| stored("runtime_agents", actor_id))?;
    let root = AgentRepository::get(tx, &AgentId::from(root_id))?
        .ok_or_else(|| stored("runtime_agents", root_id))?;
    if root.extra.get("isLead") != Some(&Value::Bool(true))
        || root
            .extra
            .get("deletedAt")
            .is_some_and(|deleted_at| !deleted_at.is_null())
    {
        return Err(NotificationDispatchError::InvalidNotification);
    }
    match args["action"].as_str() {
        Some("submit") => {
            let text = args["text"]
                .as_str()
                .ok_or(NotificationDispatchError::InvalidNotification)?;
            submit_agent_complaint(tx, actor, request_id, text, at)
        }
        Some("read") => {
            if args.get("complaint_id").is_some()
                && !args["complaint_id"].is_null()
                && !args["complaint_id"].is_string()
            {
                return Err(NotificationDispatchError::InvalidNotification);
            }
            let requested = args["complaint_id"].as_str();
            let mut complaints = records::all(tx, "runtime_complaints")?;
            complaints.retain(|complaint| {
                complaint["leadId"] == root_id && requested.is_none_or(|id| complaint["id"] == id)
            });
            if requested.is_some() && complaints.is_empty() {
                return Err(NotificationDispatchError::InvalidNotification);
            }
            complaints.sort_by(|left, right| {
                needs_agent_response(right)
                    .cmp(&needs_agent_response(left))
                    .then_with(|| {
                        right["created"]
                            .as_f64()
                            .unwrap_or_default()
                            .total_cmp(&left["created"].as_f64().unwrap_or_default())
                    })
            });
            let total = complaints.len();
            for complaint in complaints.iter_mut().take(50) {
                if actor_id == root_id
                    && complaint["recipient"] == "lead"
                    && complaint["readAt"].is_null()
                {
                    complaint["readAt"] = json!(at);
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
                return Err(NotificationDispatchError::InvalidNotification);
            }
            let id = args["complaint_id"]
                .as_str()
                .ok_or(NotificationDispatchError::InvalidNotification)?;
            let raw = studio_runtime_store::QuestionsWriteRepository::complaint(tx, id)?
                .ok_or(NotificationDispatchError::InvalidNotification)?;
            let mut complaint: Value =
                serde_json::from_str(&raw).map_err(|_| stored("runtime_complaints", id))?;
            if complaint["leadId"] != actor_id || complaint["recipient"] == "user" {
                return Err(NotificationDispatchError::InvalidNotification);
            }
            let text = args["text"]
                .as_str()
                .ok_or(NotificationDispatchError::InvalidNotification)?
                .trim();
            let status = args["status"]
                .as_str()
                .filter(|status| matches!(*status, "in_progress" | "resolved" | "declined"))
                .ok_or(NotificationDispatchError::InvalidNotification)?;
            if text.is_empty() || text.chars().count() > 12_000 {
                return Err(NotificationDispatchError::InvalidNotification);
            }
            let responses = complaint["responses"]
                .as_array_mut()
                .ok_or_else(|| stored("runtime_complaints", id))?;
            if let Some(existing) = responses
                .iter()
                .find(|response| response["id"] == request_id)
            {
                if existing["text"] != text || existing["status"] != status {
                    return Err(NotificationDispatchError::ConflictingInput);
                }
                return Ok(complaint);
            }
            let response =
                json!({"id":request_id,"author":actor_id,"text":text,"status":status,"at":at});
            responses.push(response.clone());
            complaint["status"] = json!(status);
            complaint["updated"] = json!(at);
            if complaint["readAt"].is_null() {
                complaint["readAt"] = json!(at);
            }
            complaint["version"] = json!(complaint["version"].as_i64().unwrap_or(0) + 1);
            save_complaint_projection(tx, &complaint)?;
            reconcile_complaint_notifications(tx, actor_id)?;
            let author = complaint["author"].as_str().unwrap_or_default();
            if author != "user" && author != actor_id {
                let reporter = AgentRepository::get(tx, &AgentId::from(author))?
                    .ok_or_else(|| stored("runtime_agents", author))?;
                if !studio_runtime_store::truthy(reporter.extra.get("deletedAt")) {
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
            Ok(complaint)
        }
        _ => Err(NotificationDispatchError::InvalidNotification),
    }
}

fn needs_agent_response(complaint: &Value) -> bool {
    if matches!(complaint["status"].as_str(), Some("resolved" | "declined")) {
        return false;
    }
    let responsible = if complaint["recipient"] == "user" {
        "user"
    } else {
        complaint["leadId"].as_str().unwrap_or_default()
    };
    !complaint["responses"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|response| response["author"] == responsible)
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
            let Ok(complaint) = serde_json::from_str::<Value>(&raw) else {
                all_answered = false;
                break;
            };
            if complaint["leadId"] != lead_id || needs_agent_response(&complaint) {
                all_answered = false;
                break;
            }
        }
        if all_answered {
            tx.execute(
                "UPDATE runtime_events SET status='stored_only',error=?1 WHERE id=?2 AND status='pending'",
                rusqlite::params!["Every referenced complaint already has a response", event_id],
            ).map_err(studio_runtime_store::StoreError::from)?;
        }
    }
    Ok(())
}

fn save_complaint_projection(
    tx: &Transaction<'_>,
    complaint: &Value,
) -> Result<(), NotificationDispatchError> {
    let id = complaint["id"]
        .as_str()
        .ok_or(NotificationDispatchError::InvalidNotification)?;
    let typed = ComplaintRecord::from_value(complaint.clone())?;
    Table::<ComplaintRecord>::new(tx).put(&typed)?;
    let agents = records::all(tx, "runtime_agents")?;
    let context = crate::sync_views::ComplaintViewContext { agents };
    let view = crate::sync_views::complaint_entity_view(complaint, &context)
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
    let id = format!(
        "{}:question:{}",
        agent["id"].as_str().unwrap_or_default(),
        params["item"]["id"].as_str().unwrap_or_default()
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
        .ok_or(NotificationDispatchError::InvalidNotification)?
        .to_owned();
    let records = Table::<RequestRecord>::new(tx).list_matching(
        Some(RecordPredicate::JsonAgentEquals(&id)),
        RecordOrder::RowId,
    )?;
    let mut pending = false;
    for record in records {
        let mut record = record.value().clone();
        if record["rpcId"] == params["requestId"]
            && record.get("accountKey").unwrap_or(&json!("default")) == account
            && record["connectionId"] == format!("connection-{generation}")
            && record["params"]["threadId"] == params["threadId"]
            && matches!(
                record["status"].as_str(),
                Some("pending" | "answering" | "uncertain" | "blocked")
            )
        {
            record["status"] = json!("resolved");
            record["resolvedAt"] = json!(at);
            put_request(tx, &record)?;
        }
        pending |= record["status"] == "pending";
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
    checked("runtime_requests", record)?;
    let typed = RequestRecord::from_value(record.clone())?;
    Table::new(tx).put(&typed)?;
    let id = record["id"]
        .as_str()
        .ok_or(NotificationDispatchError::InvalidNotification)?;
    let view = crate::sync_views::project_persisted_record("request", record)
        .ok_or_else(|| stored("runtime_requests", id))?;
    SyncDocumentsRepository::put_entity(tx, "request", id, view, record["status"] != "pending")?;
    Ok(())
}
