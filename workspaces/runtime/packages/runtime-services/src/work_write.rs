//! Atomic work-item writes that need no worker or filesystem driver.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, Transaction, WorkItemsRepository,
};

use crate::event_write::{self, EventWriteError as Error};

static GENERATED_WORK_ID: AtomicU64 = AtomicU64::new(0);

pub struct CreateWork {
    pub actor_id: String,
    pub title: String,
    pub description: String,
    pub owner: Option<String>,
    pub status: Option<String>,
    pub dependencies: Vec<String>,
}

pub enum WorkMutation {
    Update {
        task_id: String,
        version: Option<i64>,
        title: Option<String>,
        description: Option<String>,
        owner: Option<Option<String>>,
        status: Option<String>,
        dependencies: Option<Vec<String>>,
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

pub enum WorkWrite {
    Create(CreateWork),
    Mutate(WorkMutation),
}

pub struct WorkWriteResult {
    pub record: Value,
    pub event_inserted: bool,
    pub changed: bool,
}

pub fn write(
    tx: &Transaction<'_>,
    agent_id: &str,
    request_id: Option<&str>,
    signature: &str,
    command: WorkWrite,
    validate: impl Fn(&Value) -> bool,
) -> Result<WorkWriteResult, Error> {
    let agent = event_write::checked_agent(tx, agent_id)?;
    if let Some(record) = event_write::read_receipt(
        tx,
        request_id,
        signature,
        "runtime_work",
        request_id.unwrap_or(agent_id),
        &validate,
    )? {
        return Ok(WorkWriteResult {
            record,
            event_inserted: false,
            changed: false,
        });
    }
    match command {
        WorkWrite::Create(input) => create(tx, &agent, input, request_id, signature, &validate),
        WorkWrite::Mutate(input) => mutate(tx, &agent, input, request_id, signature, &validate),
    }
}

fn create(
    tx: &Transaction<'_>,
    agent: &AgentRecord,
    input: CreateWork,
    request_id: Option<&str>,
    signature: &str,
    validate: &impl Fn(&Value) -> bool,
) -> Result<WorkWriteResult, Error> {
    let title = text(&input.title, false)?;
    let description = text(&input.description, true)?;
    let status = input.status.as_deref().unwrap_or("ready");
    if status == "accepted" {
        return Err(Error::InvalidRequest(
            "Cannot edit accepted work. Create a follow-up task with this task ID and the new evidence",
        ));
    }
    if status == "review" {
        return Err(Error::InvalidRequest(
            "Work under review cannot be reassigned; reject its result before editing",
        ));
    }
    if !matches!(status, "ready" | "blocked") {
        return Err(Error::InvalidRequest("Choose ready or blocked"));
    }
    let owner = input.owner.as_deref();
    check_owner(tx, agent, owner)?;
    let id = work_id(event_write::unix_time()?);
    let dependencies = unique(input.dependencies);
    validate_dependencies(tx, &agent.root_id.0, &id, &dependencies, validate)?;
    let work = json!({
        "id": id, "rootId": agent.root_id.0, "title": title, "description": description,
        "owner": owner, "dependencies": dependencies, "status": status,
        "created": event_write::unix_time()?, "updated": event_write::unix_time()?, "version": 1,
        "results": [], "decisions": [], "createdBy": input.actor_id,
    });
    if !validate(&work) {
        return Err(invalid("runtime_work", &id));
    }
    let view = single_view(tx, &work, validate)?;
    assignment_needed(agent, owner, true, is_ready(&view))?;
    WorkItemsRepository::put_work(tx, &id, &work)?;
    event_write::save_receipt(tx, request_id, signature, &view)?;
    Ok(WorkWriteResult {
        record: view,
        event_inserted: false,
        changed: true,
    })
}

fn mutate(
    tx: &Transaction<'_>,
    agent: &AgentRecord,
    mutation: WorkMutation,
    request_id: Option<&str>,
    signature: &str,
    validate: &impl Fn(&Value) -> bool,
) -> Result<WorkWriteResult, Error> {
    let (task_id, expected) = match &mutation {
        WorkMutation::Update {
            task_id, version, ..
        }
        | WorkMutation::Cancel {
            task_id, version, ..
        }
        | WorkMutation::Reject {
            task_id, version, ..
        } => (task_id.clone(), *version),
    };
    let encoded = WorkItemsRepository::work_record(tx, &task_id)?
        .ok_or(Error::InvalidRequest("Unknown work item"))?;
    let mut work: Value =
        serde_json::from_str(&encoded).map_err(|_| invalid("runtime_work", &task_id))?;
    if work["id"] != task_id || work["rootId"] != agent.root_id.0 || !validate(&work) {
        return Err(invalid("runtime_work", &task_id));
    }
    let version = work["version"]
        .as_i64()
        .ok_or_else(|| invalid("runtime_work", &task_id))?;
    if expected.is_some_and(|expected| expected != version) {
        return Err(Error::Conflict(
            "This work item changed. Reload before editing",
        ));
    }
    let status = work["status"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", &task_id))?;
    let owner = work["owner"].as_str().map(str::to_owned);
    let previous_ready = is_ready(&single_view(tx, &work, validate)?);
    let mut wake = false;

    match mutation {
        WorkMutation::Update {
            title,
            description,
            owner: requested_owner,
            status: requested_status,
            dependencies,
            ..
        } => {
            if status == "accepted" {
                return Err(Error::InvalidRequest(
                    "Cannot edit accepted work. Create a follow-up task with this task ID and the new evidence",
                ));
            }
            if status == "review" {
                return Err(Error::InvalidRequest(
                    "Work under review cannot be reassigned; reject its result before editing",
                ));
            }
            if let Some(title) = title {
                work["title"] = json!(text(&title, false)?);
            }
            if let Some(description) = description {
                work["description"] = json!(text(&description, true)?);
            }
            if let Some(status) = requested_status {
                if !matches!(status.as_str(), "ready" | "blocked") {
                    return Err(Error::InvalidRequest("Choose ready or blocked"));
                }
                work["status"] = json!(status);
            }
            if let Some(dependencies) = dependencies {
                let dependencies = unique(dependencies);
                validate_dependencies(tx, &agent.root_id.0, &task_id, &dependencies, validate)?;
                work["dependencies"] = json!(dependencies);
            }
            let next_owner = requested_owner.map(|owner| owner.unwrap_or_default());
            if let Some(owner) = next_owner {
                let owner = (!owner.is_empty()).then_some(owner);
                check_owner(tx, agent, owner.as_deref())?;
                work["owner"] = owner.map_or(Value::Null, Value::String);
            }
            if let Some(owner) = work["owner"].as_str()
                && owner != agent.root_id.0
            {
                event_write::assert_delegation(&root_agent(tx, agent)?)?;
            }
            let view = single_view(tx, &work, validate)?;
            let current_owner = work["owner"].as_str();
            let changed = current_owner != owner.as_deref();
            let ready = is_ready(&view);
            assignment_needed(agent, current_owner, changed || !previous_ready, ready)?;
            if let Some(previous_owner) = owner.as_deref()
                && (changed || previous_ready != ready)
            {
                WorkItemsRepository::retire_pending_work_ready(
                    tx,
                    previous_owner,
                    &task_id,
                    "The work assignment or readiness changed before delivery",
                )?;
            }
        }
        WorkMutation::Cancel { reason, .. } => {
            let reason = text(&reason, false)?;
            if status == "cancelled" {
                let view = single_view(tx, &work, validate)?;
                event_write::save_receipt(tx, request_id, signature, &view)?;
                return Ok(WorkWriteResult {
                    record: view,
                    event_inserted: false,
                    changed: false,
                });
            }
            if status == "accepted" {
                return Err(Error::InvalidRequest("Cannot cancel accepted work"));
            }
            push_decision(
                &mut work,
                json!({"decision":"cancel", "reason":reason, "by":agent.id.0, "owner":owner, "created":event_write::unix_time()?}),
            )?;
            work["status"] = json!("cancelled");
            work["owner"] = Value::Null;
            if let Some(owner_id) = owner.as_deref() {
                WorkItemsRepository::retire_pending_work_ready(
                    tx,
                    owner_id,
                    &task_id,
                    "The task was cancelled before delivery",
                )?;
                if let Some(recipient) = AgentRepository::get(tx, &AgentId::from(owner_id))?
                    && live_recipient(&recipient)
                {
                    wake |= event_write::enqueue_recovery_event(
                        tx,
                        &recipient,
                        format!("work-decision:{task_id}:cancel"),
                        "work_decision",
                        json!({"task":task_id,"decision":"cancel","reason":reason}).to_string(),
                        event_write::unix_time()?,
                    )?;
                }
            }
        }
        WorkMutation::Reject { reason, .. } => {
            if status == "accepted" {
                return Err(Error::InvalidRequest(
                    "This work is already accepted. Create a follow-up task with this task ID and the new evidence",
                ));
            }
            if status != "review" {
                return Err(Error::InvalidRequest("Submit a result before review"));
            }
            let reason = text(&reason, false)?;
            let result_id = work["results"]
                .as_array()
                .and_then(|v| v.last())
                .and_then(|v| v["id"].as_str())
                .ok_or_else(|| invalid("runtime_work", &task_id))?
                .to_owned();
            if let Some(owner_id) = owner.as_deref() {
                if owner_id != agent.root_id.0 {
                    event_write::assert_delegation(&root_agent(tx, agent)?)?;
                }
                let recipient = AgentRepository::get(tx, &AgentId::from(owner_id))?
                    .ok_or_else(|| invalid("runtime_agents", owner_id))?;
                wake |= event_write::enqueue_recovery_event(
                    tx,
                    &recipient,
                    format!("work-decision:{task_id}:{version}"),
                    "work_decision",
                    json!({"task":task_id,"decision":"reject","reason":reason}).to_string(),
                    event_write::unix_time()?,
                )?;
            }
            WorkItemsRepository::close_review_event(tx, &agent.root_id.0, &result_id)?;
            push_decision(
                &mut work,
                json!({"resultId":result_id,"decision":"reject","reason":reason,"by":"user","created":event_write::unix_time()?}),
            )?;
            work["status"] = json!("ready");
        }
    }

    work["version"] = json!(version + 1);
    work["updated"] = json!(event_write::unix_time()?);
    if !validate(&work) {
        return Err(invalid("runtime_work", &task_id));
    }
    WorkItemsRepository::update_work(tx, &task_id, &work)?;
    let view = single_view(tx, &work, validate)?;
    event_write::save_receipt(tx, request_id, signature, &view)?;
    Ok(WorkWriteResult {
        record: view,
        event_inserted: wake,
        changed: true,
    })
}

fn push_decision(work: &mut Value, decision: Value) -> Result<(), Error> {
    let id = work["id"].as_str().unwrap_or("<work>").to_owned();
    work["decisions"]
        .as_array_mut()
        .ok_or_else(|| invalid("runtime_work", &id))?
        .push(decision);
    Ok(())
}

fn single_view(
    tx: &Transaction<'_>,
    work: &Value,
    validate: &impl Fn(&Value) -> bool,
) -> Result<Value, Error> {
    let mut statuses = BTreeMap::from([(
        work["id"].as_str().unwrap_or_default().to_owned(),
        work["status"].as_str().unwrap_or_default().to_owned(),
    )]);
    for dependency in work["dependencies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if let Some(encoded) = WorkItemsRepository::work_record(tx, dependency)? {
            let value: Value =
                serde_json::from_str(&encoded).map_err(|_| invalid("runtime_work", dependency))?;
            if value["rootId"] == work["rootId"] {
                if !validate(&value) {
                    return Err(invalid("runtime_work", dependency));
                }
                statuses.insert(
                    dependency.to_owned(),
                    value["status"].as_str().unwrap_or_default().to_owned(),
                );
            }
        }
    }
    Ok(crate::work_policy::work_item_view(work, &statuses))
}

fn validate_dependencies(
    tx: &Transaction<'_>,
    root: &str,
    task: &str,
    deps: &[String],
    validate: &impl Fn(&Value) -> bool,
) -> Result<(), Error> {
    if deps.iter().any(|id| id == task) {
        return Err(Error::InvalidRequest("Task dependencies contain a cycle"));
    }
    for id in deps {
        let encoded = WorkItemsRepository::work_record(tx, id)?
            .ok_or(Error::InvalidRequest("Dependency is outside this team"))?;
        let item: Value =
            serde_json::from_str(&encoded).map_err(|_| invalid("runtime_work", id))?;
        if item["rootId"] != root || !validate(&item) {
            return Err(Error::InvalidRequest("Dependency is outside this team"));
        }
    }
    let mut active = vec![task.to_owned()];
    let mut visited = BTreeMap::new();
    for id in deps {
        dependency_visit(tx, root, id, &mut active, &mut visited, validate)?;
    }
    Ok(())
}

fn dependency_visit(
    tx: &Transaction<'_>,
    root: &str,
    task: &str,
    active: &mut Vec<String>,
    visited: &mut BTreeMap<String, ()>,
    validate: &impl Fn(&Value) -> bool,
) -> Result<(), Error> {
    if active.iter().any(|id| id == task) {
        return Err(Error::InvalidRequest("Task dependencies contain a cycle"));
    }
    if visited.contains_key(task) {
        return Ok(());
    }
    let encoded = WorkItemsRepository::work_record(tx, task)?
        .ok_or(Error::InvalidRequest("Dependency is outside this team"))?;
    let item: Value = serde_json::from_str(&encoded).map_err(|_| invalid("runtime_work", task))?;
    if item["rootId"] != root || !validate(&item) {
        return Err(Error::InvalidRequest("Dependency is outside this team"));
    }
    active.push(task.to_owned());
    for dependency in item["dependencies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        dependency_visit(tx, root, dependency, active, visited, validate)?;
    }
    active.pop();
    visited.insert(task.to_owned(), ());
    Ok(())
}

fn unique(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeMap::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone(), ()).is_none())
        .collect()
}

fn check_owner(
    tx: &Transaction<'_>,
    agent: &AgentRecord,
    owner: Option<&str>,
) -> Result<(), Error> {
    if let Some(owner) = owner {
        let candidate = event_write::checked_agent(tx, owner)?;
        if candidate.root_id != agent.root_id {
            return Err(Error::InvalidRequest("Owner belongs to another team"));
        }
        if owner != agent.root_id.0 {
            event_write::assert_delegation(&root_agent(tx, agent)?)?;
        }
    }
    Ok(())
}

fn assignment_needed(
    agent: &AgentRecord,
    owner: Option<&str>,
    changed: bool,
    ready: bool,
) -> Result<(), Error> {
    if owner.is_some_and(|owner| owner != agent.root_id.0) && ready && changed {
        return Err(Error::WorkerAssignmentUnavailable);
    }
    Ok(())
}

fn root_agent(tx: &Transaction<'_>, agent: &AgentRecord) -> Result<AgentRecord, Error> {
    if agent.id == agent.root_id {
        return Ok(agent.clone());
    }
    AgentRepository::get(tx, &agent.root_id)?.ok_or(Error::InvalidRequest("Invalid request"))
}

fn live_recipient(agent: &AgentRecord) -> bool {
    !event_write::truthy(agent.extra.get("deletedAt"))
        && !matches!(
            agent.status,
            studio_runtime_store::AgentStatus::Completed
                | studio_runtime_store::AgentStatus::Failed
        )
}

fn is_ready(view: &Value) -> bool {
    matches!(view["status"].as_str(), Some("ready" | "running"))
        && view["blockedBy"].as_array().is_some_and(Vec::is_empty)
}

fn text(value: &str, empty: bool) -> Result<String, Error> {
    let value = value.trim();
    if !empty && value.is_empty() {
        return Err(Error::InvalidRequest("Invalid request"));
    }
    Ok(value.to_owned())
}

fn work_id(created: f64) -> String {
    format!(
        "work-{}-{}",
        (created * 1_000_000_000.0) as u128,
        GENERATED_WORK_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn invalid(object: &'static str, id: &str) -> Error {
    Error::InvalidStoredObject {
        object,
        id: id.to_owned(),
    }
}
