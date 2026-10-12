//! Atomic work-item writes supported without worker assignment services.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, AgentStatus, EventWriteRepository, StoreError,
    Transaction, WorkItemsRepository,
};
use thiserror::Error;

static GENERATED_WORK_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum WorkWriteError {
    #[error("invalid request: {0}")]
    InvalidRequest(&'static str),
    #[error("{0}")]
    Conflict(&'static str),
    #[error("invalid stored object {object}/{id}")]
    InvalidStoredObject { object: &'static str, id: String },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    EventWrite(#[from] crate::event_write::EventWriteError),
    #[error("system clock is before the Unix epoch")]
    Clock,
}

pub struct CreateWork {
    pub agent_id: String,
    pub actor_id: String,
    pub request_id: Option<String>,
    pub title: String,
    pub description: String,
    pub signature: String,
}

#[derive(Debug, PartialEq)]
pub struct WorkWriteResult {
    pub record: Value,
    pub event_inserted: bool,
}

pub enum WorkMutation {
    Update {
        task_id: String,
        version: Option<i64>,
        title: Option<String>,
        description: Option<String>,
    },
    Reject {
        task_id: String,
        version: Option<i64>,
        reason: String,
    },
    Cancel {
        task_id: String,
        version: Option<i64>,
        reason: String,
    },
}

pub fn create(
    transaction: &Transaction<'_>,
    input: CreateWork,
    validate: impl Fn(&Value) -> bool,
) -> Result<WorkWriteResult, WorkWriteError> {
    let agent = AgentRepository::get(transaction, &AgentId::from(input.agent_id.as_str()))?
        .ok_or(WorkWriteError::InvalidRequest("Invalid request"))?;
    if agent.id.0 != input.agent_id {
        return Err(invalid_stored("runtime_agents", &input.agent_id));
    }
    if agent
        .extra
        .get("deletedAt")
        .is_some_and(|value| !value.is_null() && value != false && value != 0 && value != "")
    {
        return Err(WorkWriteError::InvalidRequest("This agent was deleted"));
    }
    let title = text_field(&input.title, 160, false)?;
    let description = text_field(&input.description, 64_000, true)?;
    let receipt_id = input.request_id.as_deref().filter(|id| !id.is_empty());
    if let Some(request_id) = receipt_id
        && let Some(receipt) = EventWriteRepository::receipt(transaction, request_id)?
    {
        if receipt.signature != input.signature {
            return Err(WorkWriteError::Conflict(
                "This request id has different content",
            ));
        }
        let saved: Value = serde_json::from_str(&receipt.result)
            .map_err(|_| invalid_stored("runtime_operation_receipts", request_id))?;
        if !validate(&saved) {
            return Err(invalid_stored(
                "runtime_work",
                saved["id"].as_str().unwrap_or(request_id),
            ));
        }
        return Ok(WorkWriteResult {
            record: saved,
            event_inserted: false,
        });
    }

    let created = unix_time()?;
    let id = generated_work_id(created);
    let stored = json!({
        "id": id,
        "rootId": agent.root_id.0,
        "title": title,
        "description": description,
        "owner": null,
        "dependencies": [],
        "status": "ready",
        "created": created,
        "updated": created,
        "version": 1,
        "results": [],
        "decisions": [],
        "createdBy": input.actor_id,
    });
    if !validate(&stored) {
        return Err(invalid_stored("runtime_work", &id));
    }
    let mut result = stored.clone();
    result["blockedBy"] = json!([]);
    result["displayStatus"] = json!("ready");
    if !validate(&result) {
        return Err(invalid_stored("runtime_work", &id));
    }

    WorkItemsRepository::put_work(transaction, &id, &stored)?;
    if let Some(request_id) = receipt_id {
        EventWriteRepository::put_receipt(
            transaction,
            request_id,
            &input.signature,
            &serde_json::to_string(&result).map_err(StoreError::from)?,
        )?;
    }
    Ok(WorkWriteResult {
        record: result,
        event_inserted: false,
    })
}

pub fn mutate(
    transaction: &Transaction<'_>,
    agent_id: &str,
    request_id: Option<&str>,
    signature: &str,
    mutation: WorkMutation,
    validate: impl Fn(&Value) -> bool,
) -> Result<WorkWriteResult, WorkWriteError> {
    let agent = checked_agent(transaction, agent_id)?;
    if let Some(request_id) = request_id.filter(|id| !id.is_empty())
        && let Some(receipt) = EventWriteRepository::receipt(transaction, request_id)?
    {
        if receipt.signature != signature {
            return Err(WorkWriteError::Conflict(
                "This request id has different content",
            ));
        }
        let saved: Value = serde_json::from_str(&receipt.result)
            .map_err(|_| invalid_stored("runtime_operation_receipts", request_id))?;
        if !validate(&saved) {
            return Err(invalid_stored(
                "runtime_work",
                saved["id"].as_str().unwrap_or(request_id),
            ));
        }
        return Ok(WorkWriteResult {
            record: saved,
            event_inserted: false,
        });
    }

    let (task_id, expected_version) = match &mutation {
        WorkMutation::Update {
            task_id, version, ..
        }
        | WorkMutation::Reject {
            task_id, version, ..
        }
        | WorkMutation::Cancel {
            task_id, version, ..
        } => (task_id.clone(), *version),
    };
    let encoded = WorkItemsRepository::work_record(transaction, &task_id)?
        .ok_or(WorkWriteError::InvalidRequest("Unknown work item"))?;
    let mut work: Value =
        serde_json::from_str(&encoded).map_err(|_| invalid_stored("runtime_work", &task_id))?;
    if work.get("id").and_then(Value::as_str) != Some(task_id.as_str())
        || work.get("rootId").and_then(Value::as_str) != Some(agent.root_id.0.as_str())
        || !validate(&work)
    {
        return Err(invalid_stored("runtime_work", &task_id));
    }
    let actual_version = work
        .get("version")
        .and_then(Value::as_i64)
        .ok_or_else(|| invalid_stored("runtime_work", &task_id))?;
    if expected_version.is_some_and(|version| version != actual_version) {
        return Err(WorkWriteError::Conflict(
            "This work item changed. Reload before editing",
        ));
    }
    let status = work
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| invalid_stored("runtime_work", &task_id))?;
    let owner = work.get("owner").and_then(Value::as_str).map(str::to_owned);

    let mut event_inserted = false;
    match mutation {
        WorkMutation::Update {
            title, description, ..
        } => {
            if status == "accepted" {
                return Err(WorkWriteError::InvalidRequest(
                    "Cannot edit accepted work. Create a follow-up task with this task ID and the new evidence",
                ));
            }
            if status == "review" {
                return Err(WorkWriteError::InvalidRequest(
                    "Work under review cannot be reassigned; reject its result before editing",
                ));
            }
            if let Some(title) = title {
                work["title"] = Value::String(text_field(&title, 160, false)?);
            }
            if let Some(description) = description {
                work["description"] = Value::String(text_field(&description, 64_000, true)?);
            }
        }
        WorkMutation::Cancel { reason, .. } => {
            if status == "cancelled" {
                let view = work_view(transaction, &agent.root_id.0, &work, &validate)?;
                save_receipt(transaction, request_id, signature, &view)?;
                return Ok(WorkWriteResult {
                    record: view,
                    event_inserted: false,
                });
            }
            if status == "accepted" {
                return Err(WorkWriteError::InvalidRequest(
                    "Cannot cancel accepted work",
                ));
            }
            let reason = text_field(&reason, 32_000, false)?;
            let mut decisions = work["decisions"]
                .as_array()
                .cloned()
                .ok_or_else(|| invalid_stored("runtime_work", &task_id))?;
            decisions.push(json!({
                "decision": "cancel", "reason": reason, "by": agent.id.0,
                "owner": owner, "created": unix_time()?
            }));
            work["decisions"] = Value::Array(decisions);
            work["status"] = json!("cancelled");
            work["owner"] = Value::Null;
            if let Some(owner_id) = owner.as_deref()
                && let Some(recipient) =
                    AgentRepository::get(transaction, &AgentId::from(owner_id))?
                && is_live_event_recipient(&recipient)
            {
                let reason_json = json!({"task": task_id, "decision": "cancel", "reason": reason});
                event_inserted |= crate::event_write::enqueue_event(
                    transaction,
                    &recipient,
                    format!("work-decision:{task_id}:cancel"),
                    "work_decision",
                    reason_json.to_string(),
                    unix_time()?,
                )?;
            }
        }
        WorkMutation::Reject { reason, .. } => {
            if status == "accepted" {
                return Err(WorkWriteError::InvalidRequest(
                    "This work is already accepted. Create a follow-up task with this task ID and the new evidence",
                ));
            }
            let results = work["results"]
                .as_array()
                .filter(|results| !results.is_empty())
                .ok_or(WorkWriteError::InvalidRequest(
                    "Submit a result before review",
                ))?;
            if status != "review" {
                return Err(WorkWriteError::InvalidRequest(
                    "Submit a result before review",
                ));
            }
            let reason = text_field(&reason, 32_000, false)?;
            let latest = results
                .last()
                .ok_or_else(|| invalid_stored("runtime_work", &task_id))?;
            let result_id = latest
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_stored("runtime_work", &task_id))?
                .to_owned();
            let mut decisions = work["decisions"]
                .as_array()
                .cloned()
                .ok_or_else(|| invalid_stored("runtime_work", &task_id))?;
            decisions.push(json!({
                "resultId": result_id, "decision": "reject", "reason": reason,
                "by": "user", "created": unix_time()?
            }));
            work["decisions"] = Value::Array(decisions);
            work["status"] = json!("ready");
            WorkItemsRepository::close_review_event(transaction, &agent.root_id.0, &result_id)?;
            if let Some(owner_id) = owner.as_deref()
                && owner_id != agent.id.0
            {
                let recipient = AgentRepository::get(transaction, &AgentId::from(owner_id))?
                    .ok_or_else(|| invalid_stored("runtime_agents", owner_id))?;
                let payload = json!({"task": task_id, "decision": "reject", "reason": reason});
                event_inserted |= crate::event_write::enqueue_event(
                    transaction,
                    &recipient,
                    format!("work-decision:{task_id}:{actual_version}"),
                    "work_decision",
                    payload.to_string(),
                    unix_time()?,
                )?;
            }
        }
    }

    work["version"] = json!(actual_version + 1);
    work["updated"] = json!(unix_time()?);
    if !validate(&work) {
        return Err(invalid_stored("runtime_work", &task_id));
    }
    WorkItemsRepository::update_work(transaction, &task_id, &work)?;
    let view = work_view(transaction, &agent.root_id.0, &work, &validate)?;
    save_receipt(transaction, request_id, signature, &view)?;
    Ok(WorkWriteResult {
        record: view,
        event_inserted,
    })
}

fn checked_agent(
    transaction: &Transaction<'_>,
    agent_id: &str,
) -> Result<studio_runtime_store::AgentRecord, WorkWriteError> {
    let agent = AgentRepository::get(transaction, &AgentId::from(agent_id))?
        .ok_or(WorkWriteError::InvalidRequest("Invalid request"))?;
    if agent.id.0 != agent_id {
        return Err(invalid_stored("runtime_agents", agent_id));
    }
    Ok(agent)
}

fn work_view(
    transaction: &Transaction<'_>,
    root_id: &str,
    work: &Value,
    validate: &impl Fn(&Value) -> bool,
) -> Result<Value, WorkWriteError> {
    let records = WorkItemsRepository::work(transaction)?
        .into_iter()
        .map(|row| {
            let value: Value = serde_json::from_str(
                row.record
                    .as_deref()
                    .ok_or_else(|| invalid_stored("runtime_work", &row.id))?,
            )
            .map_err(|_| invalid_stored("runtime_work", &row.id))?;
            if value.get("id").and_then(Value::as_str) != Some(row.id.as_str()) || !validate(&value)
            {
                return Err(invalid_stored("runtime_work", &row.id));
            }
            Ok(value)
        })
        .collect::<Result<Vec<_>, WorkWriteError>>()?;
    let list = crate::work_policy::work_list(root_id, &records);
    let id = work
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_stored("runtime_work", "<work>"))?;
    list["items"]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|item| item.get("id").and_then(Value::as_str) == Some(id))
        })
        .cloned()
        .ok_or_else(|| invalid_stored("runtime_work", id))
}

fn save_receipt(
    transaction: &Transaction<'_>,
    request_id: Option<&str>,
    signature: &str,
    value: &Value,
) -> Result<(), WorkWriteError> {
    if let Some(request_id) = request_id.filter(|id| !id.is_empty()) {
        EventWriteRepository::put_receipt(
            transaction,
            request_id,
            signature,
            &serde_json::to_string(value).map_err(StoreError::from)?,
        )?;
    }
    Ok(())
}

fn is_live_event_recipient(agent: &AgentRecord) -> bool {
    let deleted = agent
        .extra
        .get("deletedAt")
        .is_some_and(|value| match value {
            Value::Null | Value::Bool(false) => false,
            Value::Bool(true) => true,
            Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
            Value::String(value) => !value.is_empty(),
            _ => true,
        });
    !deleted && !matches!(agent.status, AgentStatus::Completed | AgentStatus::Failed)
}

fn text_field(value: &str, maximum: usize, empty: bool) -> Result<String, WorkWriteError> {
    let trimmed = value.trim();
    if value.chars().count() > maximum || (!empty && trimmed.is_empty()) {
        return Err(WorkWriteError::InvalidRequest("Invalid request"));
    }
    Ok(trimmed.to_owned())
}

fn unix_time() -> Result<f64, WorkWriteError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .map_err(|_| WorkWriteError::Clock)
}

fn generated_work_id(created: f64) -> String {
    let sequence = GENERATED_WORK_ID.fetch_add(1, Ordering::Relaxed);
    format!("work-{}-{sequence}", (created * 1_000_000_000.0) as u128)
}

fn invalid_stored(object: &'static str, id: &str) -> WorkWriteError {
    WorkWriteError::InvalidStoredObject {
        object,
        id: id.to_owned(),
    }
}
