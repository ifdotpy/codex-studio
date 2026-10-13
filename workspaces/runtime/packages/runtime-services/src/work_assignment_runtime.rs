//! Transactional work-item assignment used by the worker creation path.

use serde_json::Value;
use studio_runtime_store::{AgentRecord, Transaction, WorkItemsRepository};

use crate::event_write::{self, EventWriteError};
use crate::work_write::WorkWriteError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment {
    pub task_id: String,
    pub owner_id: String,
    pub version: i64,
    pub changed: bool,
}

/// Assign a newly created child to an existing work item under spawn rules.
///
/// The caller inserts the child and invokes this in the same transaction as
/// the spawn receipt and initial event. Replaying the same pair is read-only;
/// assigning an item already owned by a different child fails closed.
pub fn assign_in_transaction(
    tx: &Transaction<'_>,
    task_id: &str,
    owner_id: &str,
    actor: &AgentRecord,
    now: f64,
) -> Result<Assignment, WorkWriteError> {
    if !now.is_finite() {
        return Err(EventWriteError::InvalidRequest("Invalid request").into());
    }

    let current_actor = event_write::checked_agent(tx, &actor.id.0)?;
    if current_actor.id != current_actor.root_id
        || !event_write::truthy(current_actor.extra.get("isLead"))
        || current_actor.extra.get("role").and_then(Value::as_str) == Some("reviewer")
        || current_actor.epoch != actor.epoch
        || !event_write::truthy(current_actor.extra.get("autoWake"))
    {
        return Err(
            EventWriteError::InvalidRequest("Only the active lead can delegate a task").into(),
        );
    }

    let child = event_write::checked_agent(tx, owner_id)?;
    if child.root_id != current_actor.root_id
        || child.extra.get("parentId").and_then(Value::as_str) != Some(current_actor.id.0.as_str())
    {
        return Err(EventWriteError::InvalidRequest("Worker belongs to another team").into());
    }

    let mut work = WorkItemsRepository::work_record(tx, task_id)?
        .ok_or(EventWriteError::InvalidRequest("Unknown work item"))?;
    if work.get("rootId").and_then(Value::as_str) != Some(current_actor.root_id.0.as_str()) {
        return Err(EventWriteError::InvalidRequest("Invalid request").into());
    }
    let saved_owner = work.get("owner").and_then(Value::as_str);
    let version = work
        .get("version")
        .and_then(Value::as_i64)
        .ok_or_else(|| invalid_work(task_id))?;
    if saved_owner == Some(owner_id) {
        return Ok(Assignment {
            task_id: task_id.to_owned(),
            owner_id: owner_id.to_owned(),
            version,
            changed: false,
        });
    }
    if saved_owner.is_some() {
        return Err(EventWriteError::Conflict("Another agent owns this work item").into());
    }
    if matches!(
        work.get("status").and_then(Value::as_str),
        Some("accepted" | "review")
    ) {
        return Err(EventWriteError::InvalidRequest(
            "Task is accepted or under review and cannot be assigned",
        )
        .into());
    }
    let next_version = version
        .checked_add(1)
        .ok_or_else(|| invalid_work(task_id))?;

    work["owner"] = Value::String(owner_id.to_owned());
    work["version"] = Value::from(next_version);
    work["updated"] = serde_json::Number::from_f64(now)
        .map(Value::Number)
        .ok_or_else(|| invalid_work(task_id))?;
    WorkItemsRepository::update_work(tx, task_id, &work)?;

    Ok(Assignment {
        task_id: task_id.to_owned(),
        owner_id: owner_id.to_owned(),
        version: next_version,
        changed: true,
    })
}

fn invalid_work(task_id: &str) -> WorkWriteError {
    EventWriteError::InvalidStoredObject {
        object: "runtime_work",
        id: task_id.to_owned(),
    }
    .into()
}
