//! Async questions and exact native resolution, within the delivery transaction.
use super::notices::ordered_text;
use super::snapshot::checked;
use super::{NotificationDispatchError, stored};
use serde::Deserialize;
use serde_json::{Value, json};
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
        let lead_id = agent["rootId"]
            .as_str()
            .ok_or_else(|| stored("runtime_agents", &id))?;
        let lead = AgentRepository::get(tx, &AgentId::from(lead_id))?
            .ok_or_else(|| stored("runtime_agents", lead_id))?;
        let complaint_id =
            Uuid::new_v5(&Uuid::NAMESPACE_URL, format!("complaint:{id}").as_bytes()).to_string();
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
        if let Some(previous) = Table::<ComplaintRecord>::new(tx).get(&complaint_id)? {
            let value = previous.value();
            if value["text"] != text || value["author"] != agent["id"] || value["leadId"] != lead_id
            {
                return Err(NotificationDispatchError::ConflictingInput);
            }
            return Ok(());
        }
        let record = json!({"id":complaint_id,"leadId":lead_id,"author":agent["id"],"text":text.trim(),"recipient":"lead","version":1,"status":"open","created":at,"updated":at,"readAt":null,"responses":[]});
        checked("runtime_complaints", &record)?;
        Table::<ComplaintRecord>::new(tx).put(&ComplaintRecord::from_value(record.clone())?)?;
        let context = crate::sync_views::ComplaintViewContext {
            agents: vec![agent.clone(), json!(lead)],
        };
        let view = crate::sync_views::complaint_entity_view(&record, &context)
            .ok_or_else(|| stored("runtime_complaints", &complaint_id))?;
        SyncDocumentsRepository::put_entity(tx, "complaint", &complaint_id, view, false)?;
        let complaint = ordered_text(
            &json!({"complaint_id":complaint_id,"author":agent["id"],"author_name":agent.get("name").unwrap_or(&agent["id"]),"text":record["text"],"status":"open","created":at}),
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
        crate::event_write::enqueue_event(
            tx,
            &lead,
            format!("complaint:{complaint_id}"),
            "complaint",
            message,
            at,
        )
        .map_err(|_| stored("runtime_events", &complaint_id))?;
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
