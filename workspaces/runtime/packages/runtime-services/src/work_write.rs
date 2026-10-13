//! Atomic work-item writes that need no worker or filesystem driver.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, Transaction, WorkItemsRepository,
};

use crate::event_write::{self, EventWriteError as Error};
use thiserror::Error;

static GENERATED_WORK_ID: AtomicU64 = AtomicU64::new(0);

pub struct CreateWork {
    pub actor_id: String,
    pub title: String,
    pub description: String,
    pub owner: Option<String>,
    pub status: Option<String>,
    pub dependencies: Vec<String>,
}

pub enum WorkActorWrite {
    Create(CreateWork),
    Mutate(WorkMutation),
}

pub struct WorkActorResult {
    pub record: Value,
    pub event_inserted: bool,
}

/// Execute a work action with a persisted agent as the acting principal.
pub fn write_as_agent(
    tx: &Transaction<'_>,
    agent_id: &str,
    actor_id: &str,
    request_id: Option<&str>,
    signature: &str,
    command: WorkActorWrite,
) -> Result<WorkActorResult, Error> {
    let selected = event_write::checked_agent(tx, agent_id)?;
    let actor = event_write::checked_agent(tx, actor_id)?;
    if selected.root_id != actor.root_id {
        return Err(Error::InvalidRequest("Actor belongs to another team"));
    }
    if !event_write::truthy(actor.extra.get("autoWake")) {
        return Err(Error::InvalidRequest("The caller is stopped"));
    }
    if let Some(record) = event_write::read_receipt(
        tx,
        request_id,
        signature,
        "runtime_work",
        request_id.unwrap_or(agent_id),
        |value| {
            value
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| WorkItemsRepository::validate_work(tx, id, value).is_ok())
        },
    )? {
        return Ok(WorkActorResult {
            record,
            event_inserted: false,
        });
    }
    let result = match command {
        WorkActorWrite::Create(mut input) => {
            if actor.id != actor.root_id {
                return Err(Error::InvalidRequest(
                    "Only the lead can change work assignments",
                ));
            }
            input.actor_id = actor_id.to_owned();
            create(tx, &selected, input, request_id, signature)?
        }
        WorkActorWrite::Mutate(action) => {
            match &action {
                WorkMutation::Update { .. } | WorkMutation::Reject { .. } => {
                    if actor.id != actor.root_id {
                        return Err(Error::InvalidRequest(
                            "Only the lead can change work assignments",
                        ));
                    }
                }
                WorkMutation::Cancel { task_id, .. } => {
                    let work = WorkItemsRepository::work_record(tx, task_id)?
                        .ok_or(Error::InvalidRequest("Unknown work item"))?;
                    if actor.id != actor.root_id
                        && work.get("createdBy").and_then(Value::as_str) != Some(actor_id)
                    {
                        return Err(Error::InvalidRequest(
                            "Only the lead or task creator can cancel this task",
                        ));
                    }
                }
                WorkMutation::ByAgent { .. } => {
                    return Err(Error::InvalidRequest("Invalid request"));
                }
            }
            mutate(
                tx,
                &selected,
                WorkMutation::ByAgent {
                    actor_id: actor_id.to_owned(),
                    action: Box::new(action),
                },
                request_id,
                signature,
            )?
        }
    };
    Ok(WorkActorResult {
        record: result.record,
        event_inserted: result.event_inserted,
    })
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
    ByAgent {
        actor_id: String,
        action: Box<WorkMutation>,
    },
    Cancel {
        task_id: String,
        version: Option<i64>,
        reason: String,
    },
}

pub struct SubmitWork {
    pub task_id: String,
    pub version: Option<i64>,
    pub result: String,
    pub checks: String,
    pub revision: String,
    pub files: Vec<String>,
    pub state_dir: PathBuf,
}

pub struct ClaimWork {
    pub task_id: String,
    pub version: Option<i64>,
    pub claimant_id: String,
}

pub struct AcceptWork {
    pub task_id: String,
    pub version: Option<i64>,
    pub reason: String,
}

pub enum WorkWrite {
    Create(CreateWork),
    Mutate(WorkMutation),
    Claim(ClaimWork),
    Submit(SubmitWork),
    Accept(AcceptWork),
}

pub struct WorkWriteResult {
    pub record: Value,
    pub event_inserted: bool,
    pub artifact: Option<ResultArtifact>,
}

pub struct ResultArtifact {
    path: PathBuf,
    body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseFailedWorkResult {
    pub task_ids: Vec<String>,
    pub event_inserted: bool,
}

#[derive(Debug, Error)]
pub enum WorkWriteError {
    #[error(transparent)]
    Event(#[from] Error),
    #[error(transparent)]
    Store(#[from] studio_runtime_store::StoreError),
    #[error("Not implemented: requires {0}")]
    NotImplemented(&'static str),
}

pub fn write(
    tx: &Transaction<'_>,
    agent_id: &str,
    request_id: Option<&str>,
    signature: &str,
    command: WorkWrite,
) -> Result<WorkWriteResult, WorkWriteError> {
    let agent = event_write::checked_agent(tx, agent_id)?;
    if let Some(record) = event_write::read_receipt(
        tx,
        request_id,
        signature,
        "runtime_work",
        request_id.unwrap_or(agent_id),
        |value| {
            value
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| WorkItemsRepository::validate_work(tx, id, value).is_ok())
        },
    )? {
        return Ok(WorkWriteResult {
            artifact: if matches!(&command, WorkWrite::Submit(_)) {
                let WorkWrite::Submit(input) = &command else {
                    return Err(WorkWriteError::Event(Error::InvalidRequest(
                        "Invalid request",
                    )));
                };
                Some(result_artifact(&record, &input.state_dir)?)
            } else {
                None
            },
            record,
            event_inserted: false,
        });
    }
    match command {
        WorkWrite::Create(input) => {
            create(tx, &agent, input, request_id, signature).map_err(Into::into)
        }
        WorkWrite::Mutate(input) => {
            mutate(tx, &agent, input, request_id, signature).map_err(Into::into)
        }
        WorkWrite::Claim(input) => claim(tx, &agent, input, request_id, signature),
        WorkWrite::Submit(input) => submit(tx, &agent, input, request_id, signature),
        WorkWrite::Accept(input) => accept(tx, &agent, input, request_id, signature),
    }
}

/// Run the filesystem half after the transaction commits. A same-identity retry
/// recreates a missing artifact and rejects a conflicting file.
pub fn write_result_artifact(artifact: &ResultArtifact) -> Result<(), std::io::Error> {
    let parent = artifact
        .path
        .parent()
        .ok_or_else(|| std::io::Error::other("invalid result artifact path"))?;
    if !parent.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700).create(parent)?;
        }
        #[cfg(not(unix))]
        fs::create_dir_all(parent)?;
    }
    let mut temporary = tempfile::Builder::new()
        .prefix(".result-")
        .tempfile_in(parent)?;
    temporary.write_all(artifact.body.as_bytes())?;
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(&artifact.path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            if fs::read_to_string(&artifact.path)? == artifact.body {
                Ok(())
            } else {
                Err(std::io::Error::other(
                    "saved task result file differs from its committed record",
                ))
            }
        }
        Err(error) => Err(error.error),
    }
}

/// Release persisted claims for failed or deleted workers. Callers own cadence and
/// invoke this in the same transaction that commits the resulting recovery events.
pub fn release_failed_work(
    tx: &Transaction<'_>,
    agents: &[AgentRecord],
    now: f64,
) -> Result<ReleaseFailedWorkResult, Error> {
    let mut released_ids = Vec::new();
    let mut event_inserted = false;
    for worker in agents {
        let deleted = worker.extra.get("deletedAt").is_some_and(truthy);
        if worker.id == worker.root_id
            || (!deleted && worker.status.as_str() != "failed")
            || (!deleted && continuation_work_pending(worker))
        {
            continue;
        }
        let reason = if deleted {
            "The worker was deleted".to_owned()
        } else {
            worker
                .extra
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("The worker failed")
                .to_owned()
        };
        for released in WorkItemsRepository::release_failed_work(tx, &worker.id.0, &reason, now)? {
            let id = released["id"]
                .as_str()
                .ok_or_else(|| invalid("runtime_work", "<invalid>"))?;
            let root = AgentRepository::get(tx, &worker.root_id)?;
            if let Some(root) = root.filter(|root| !root.extra.get("deletedAt").is_some_and(truthy))
            {
                let owner = released["releases"]
                    .as_array()
                    .and_then(|history| history.last())
                    .and_then(|release| release.get("agent"))
                    .and_then(Value::as_str)
                    .unwrap_or(&worker.id.0);
                let reason = released["releases"]
                    .as_array()
                    .and_then(|history| history.last())
                    .and_then(|release| release.get("reason"))
                    .cloned()
                    .unwrap_or_else(|| json!("The worker failed"));
                event_inserted |= event_write::enqueue_recovery_event(
                    tx,
                    &root,
                    format!(
                        "work-released:{id}:{}",
                        released["version"].as_i64().unwrap_or_default()
                    ),
                    "work_released",
                    json!({"task":id,"title":released["title"],"agent":owner,"reason":reason})
                        .to_string(),
                    now,
                )?;
            }
            released_ids.push(id.to_owned());
        }
    }
    Ok(ReleaseFailedWorkResult {
        task_ids: released_ids,
        event_inserted,
    })
}

fn continuation_work_pending(agent: &AgentRecord) -> bool {
    if !agent.extra.get("autoWake").is_some_and(truthy)
        || agent.extra.get("deletedAt").is_some_and(truthy)
    {
        return false;
    }
    let account = agent
        .extra
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let thread = agent.extra.get("threadId").and_then(Value::as_str);
    let epoch = agent.epoch;
    let last_turn = agent.extra.get("lastCompletedTurn").and_then(Value::as_str);

    let matches = |record: &Value, statuses: &[&str], turn_field: &str| {
        let record_account = record
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default");
        record
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| statuses.contains(&status))
            && record_account == account
            && record.get("threadId").and_then(Value::as_str) == thread
            && thread.is_some_and(|thread| !thread.is_empty())
            && record.get("epoch").and_then(Value::as_i64) == Some(epoch)
            && record.get(turn_field).and_then(Value::as_str) == last_turn
            && last_turn.is_some_and(|turn| !turn.is_empty())
    };

    let capacity = agent.extra.get("capacityRetry").unwrap_or(&Value::Null);
    let usage = agent.extra.get("usageResume").unwrap_or(&Value::Null);
    let restart = agent.extra.get("restartRecovery").unwrap_or(&Value::Null);
    matches(capacity, &["scheduled", "starting", "unknown"], "turnId")
        || matches(usage, &["scheduled", "started", "unknown"], "turnId")
        || (restart.get("stage").and_then(Value::as_str) == Some("pending")
            && restart.get("autoWake").is_some_and(truthy)
            && restart
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
                == account
            && restart.get("threadId").and_then(Value::as_str) == thread
            && thread.is_some_and(|thread| !thread.is_empty())
            && restart.get("epoch").and_then(Value::as_i64) == Some(epoch))
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

struct WorkSnapshot<'a> {
    record: &'a Value,
    dependency_statuses: &'a BTreeMap<String, String>,
}

fn decide_claim(snapshot: WorkSnapshot<'_>, claimant: &str, now: f64) -> Result<Value, Error> {
    let record = snapshot.record;
    let id = record["id"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", "<invalid>"))?;
    let status = record["status"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", id))?;
    if !matches!(status, "ready" | "running")
        || !blocked_by(record, snapshot.dependency_statuses).is_empty()
    {
        return Err(Error::InvalidRequest("This work item is not ready"));
    }
    if record["owner"]
        .as_str()
        .is_some_and(|owner| owner != claimant)
    {
        return Err(Error::InvalidRequest("Another agent owns this work item"));
    }
    let mut updated = record.clone();
    updated["owner"] = json!(claimant);
    updated["status"] = json!("running");
    advance(&mut updated, now)?;
    Ok(updated)
}

fn decide_submit(
    snapshot: WorkSnapshot<'_>,
    submitter: &AgentRecord,
    input: &SubmitWork,
    identity: Option<(String, Option<String>)>,
    now: f64,
    result_id: String,
) -> Result<(Value, ResultArtifact), Error> {
    let record = snapshot.record;
    let id = record["id"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", "<invalid>"))?;
    if !matches!(
        record["status"].as_str(),
        Some("running" | "ready" | "blocked" | "review")
    ) {
        return Err(Error::InvalidRequest("This result is already accepted"));
    }
    if !blocked_by(record, snapshot.dependency_statuses).is_empty() {
        return Err(Error::InvalidRequest("Dependencies have not been accepted"));
    }
    let title = record["title"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", id))?;
    let submitter_id = submitter.id.0.as_str();
    let path = result_path(&input.state_dir, id, &result_id)?;
    let mut result = json!({
        "id": result_id,
        "agent": submitter_id,
        "text": bounded_text(&input.result, 32000)?,
        "checks": bounded_text(&input.checks, 32000)?,
        "revision": bounded_text(&input.revision, 200)?,
        "files": input.files,
        "created": now,
        "resultFile": path.to_string_lossy(),
    });
    if let Some(identity) = identity {
        result["runId"] = json!(identity.0);
        result["attemptId"] = json!(identity.1);
    }
    let mut updated = record.clone();
    updated["results"]
        .as_array_mut()
        .ok_or_else(|| invalid("runtime_work", id))?
        .push(result.clone());
    updated["status"] = json!("review");
    advance(&mut updated, now)?;
    let body = result_artifact_body(title, &result)?;
    Ok((updated, ResultArtifact { path, body }))
}

fn decide_accept(
    snapshot: WorkSnapshot<'_>,
    reason: &str,
    by: &str,
    now: f64,
) -> Result<Value, Error> {
    let record = snapshot.record;
    let id = record["id"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", "<invalid>"))?;
    if record["status"] == "accepted" {
        return Err(Error::InvalidRequest(
            "This work is already accepted. Create a follow-up task with this task ID and the new evidence",
        ));
    }
    let result_id = record["results"]
        .as_array()
        .and_then(|results| results.last())
        .and_then(|result| result.get("id"))
        .and_then(Value::as_str)
        .ok_or(Error::InvalidRequest("Submit a result before review"))?;
    if record["status"] != "review" {
        return Err(Error::InvalidRequest("Submit a result before review"));
    }
    let mut updated = record.clone();
    updated["decisions"]
        .as_array_mut()
        .ok_or_else(|| invalid("runtime_work", id))?
        .push(json!({"resultId":result_id,"decision":"accept","reason":text(reason, false)?,"by":by,"created":now}));
    updated["status"] = json!("accepted");
    updated["archive"] =
        json!({"status":"skipped","reason":"The task owner is the lead or is unassigned"});
    advance(&mut updated, now)?;
    let _ = snapshot.dependency_statuses;
    Ok(updated)
}

fn advance(record: &mut Value, now: f64) -> Result<(), Error> {
    let id = record["id"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", "<invalid>"))?;
    let version = record["version"]
        .as_i64()
        .ok_or_else(|| invalid("runtime_work", id))?;
    let next = version
        .checked_add(1)
        .ok_or_else(|| invalid("runtime_work", id))?;
    record["version"] = json!(next);
    record["updated"] = json!(now);
    Ok(())
}

fn blocked_by(record: &Value, statuses: &BTreeMap<String, String>) -> Vec<String> {
    record["dependencies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|dependency| statuses.get(*dependency).map(String::as_str) != Some("accepted"))
        .map(str::to_owned)
        .collect()
}

fn result_path(state_dir: &Path, task_id: &str, result_id: &str) -> Result<PathBuf, Error> {
    if !task_id
        .chars()
        .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_'))
        || !result_id
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_'))
    {
        return Err(invalid("runtime_work", task_id));
    }
    Ok(state_dir
        .join("results")
        .join(task_id)
        .join(format!("{result_id}.md")))
}

fn result_artifact(record: &Value, state_dir: &Path) -> Result<ResultArtifact, Error> {
    let result = record["results"]
        .as_array()
        .and_then(|results| results.last())
        .ok_or_else(|| invalid("runtime_work", record["id"].as_str().unwrap_or("<invalid>")))?;
    let task_id = record["id"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", "<invalid>"))?;
    let result_id = result["id"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", task_id))?;
    let path = result["resultFile"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", task_id))?;
    let expected_path = result_path(state_dir, task_id, result_id)?;
    if Path::new(path) != expected_path {
        return Err(invalid("runtime_work", task_id));
    }
    Ok(ResultArtifact {
        path: PathBuf::from(path),
        body: result_artifact_body(
            record["title"]
                .as_str()
                .ok_or_else(|| invalid("runtime_work", task_id))?,
            result,
        )?,
    })
}

fn result_artifact_body(title: &str, result: &Value) -> Result<String, Error> {
    let id = result["id"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", "<invalid>"))?;
    let text = result["text"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", id))?;
    let checks = result["checks"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", id))?;
    let revision = result["revision"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", id))?;
    let files = result["files"]
        .as_array()
        .ok_or_else(|| invalid("runtime_work", id))?
        .iter()
        .map(|value| value.as_str().map(|value| format!("- `{value}`")))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| invalid("runtime_work", id))?;
    Ok(format!(
        "# {title}\n\nResult ID: `{id}`\n\n## Result\n\n{text}\n\n## Checks\n\n{checks}\n\n## Revision\n\n{revision}\n\n## Files\n\n{}\n",
        if files.is_empty() {
            "None".to_owned()
        } else {
            files.join("\n")
        },
    ))
}

fn create(
    tx: &Transaction<'_>,
    agent: &AgentRecord,
    input: CreateWork,
    request_id: Option<&str>,
    signature: &str,
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
    validate_dependencies(tx, &agent.root_id.0, &id, &dependencies)?;
    let work = json!({
        "id": id, "rootId": agent.root_id.0, "title": title, "description": description,
        "owner": owner, "dependencies": dependencies, "status": status,
        "created": event_write::unix_time()?, "updated": event_write::unix_time()?, "version": 1,
        "results": [], "decisions": [], "createdBy": input.actor_id,
    });
    let view = single_view(tx, &work)?;
    assignment_needed(agent, owner, true, is_ready(&view))?;
    if !WorkItemsRepository::put_work(tx, &id, &work)? {
        return Err(Error::Conflict("Work item already exists"));
    }
    event_write::save_receipt(tx, request_id, signature, &view)?;
    Ok(WorkWriteResult {
        record: view,
        event_inserted: false,
        artifact: None,
    })
}

fn mutate(
    tx: &Transaction<'_>,
    agent: &AgentRecord,
    mutation: WorkMutation,
    request_id: Option<&str>,
    signature: &str,
) -> Result<WorkWriteResult, Error> {
    let (mutation, actor_id) = match mutation {
        WorkMutation::ByAgent { actor_id, action } => (*action, Some(actor_id)),
        mutation => (mutation, None),
    };
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
        WorkMutation::ByAgent { .. } => return Err(Error::InvalidRequest("Invalid request")),
    };
    let mut work = WorkItemsRepository::work_record(tx, &task_id)?
        .ok_or(Error::InvalidRequest("Unknown work item"))?;
    if work["rootId"] != agent.root_id.0 {
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
    let previous_ready = is_ready(&single_view(tx, &work)?);
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
                validate_dependencies(tx, &agent.root_id.0, &task_id, &dependencies)?;
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
            let view = single_view(tx, &work)?;
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
                let view = single_view(tx, &work)?;
                event_write::save_receipt(tx, request_id, signature, &view)?;
                return Ok(WorkWriteResult {
                    record: view,
                    event_inserted: false,
                    artifact: None,
                });
            }
            if status == "accepted" {
                return Err(Error::InvalidRequest("Cannot cancel accepted work"));
            }
            push_decision(
                &mut work,
                json!({"decision":"cancel", "reason":reason, "by":actor_id.as_deref().unwrap_or(&agent.id.0), "owner":owner, "created":event_write::unix_time()?}),
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
                if actor_id.as_deref() != Some(owner_id)
                    && let Some(recipient) = AgentRepository::get(tx, &AgentId::from(owner_id))?
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
                if actor_id.as_deref() != Some(owner_id) {
                    wake |= event_write::enqueue_recovery_event(
                        tx,
                        &recipient,
                        format!("work-decision:{task_id}:{version}"),
                        "work_decision",
                        json!({"task":task_id,"decision":"reject","reason":reason}).to_string(),
                        event_write::unix_time()?,
                    )?;
                }
            }
            WorkItemsRepository::close_review_event(tx, &agent.root_id.0, &result_id)?;
            push_decision(
                &mut work,
                json!({"resultId":result_id,"decision":"reject","reason":reason,"by":actor_id.as_deref().unwrap_or("user"),"created":event_write::unix_time()?}),
            )?;
            work["status"] = json!("ready");
        }
        WorkMutation::ByAgent { .. } => return Err(Error::InvalidRequest("Invalid request")),
    }

    work["version"] = json!(version + 1);
    work["updated"] = json!(event_write::unix_time()?);
    WorkItemsRepository::update_work(tx, &task_id, &work)?;
    let view = single_view(tx, &work)?;
    event_write::save_receipt(tx, request_id, signature, &view)?;
    Ok(WorkWriteResult {
        record: view,
        event_inserted: wake,
        artifact: None,
    })
}

fn claim(
    tx: &Transaction<'_>,
    agent: &AgentRecord,
    input: ClaimWork,
    request_id: Option<&str>,
    signature: &str,
) -> Result<WorkWriteResult, WorkWriteError> {
    let work = load_work(tx, &agent.root_id.0, &input.task_id, input.version)?;
    let claimant = event_write::checked_agent(tx, &input.claimant_id)?;
    if claimant.root_id != agent.root_id {
        return Err(Error::InvalidRequest("Claimant belongs to another team").into());
    }
    if claimant.id != agent.root_id && work["owner"].as_str() != Some(claimant.id.0.as_str()) {
        event_write::assert_delegation(&root_agent(tx, agent)?)?;
    }
    let statuses = dependency_statuses(tx, &work)?;
    let updated = decide_claim(
        WorkSnapshot {
            record: &work,
            dependency_statuses: &statuses,
        },
        &claimant.id.0,
        event_write::unix_time()?,
    )?;
    WorkItemsRepository::update_work(tx, &input.task_id, &updated)?;
    let record = single_view(tx, &updated)?;
    event_write::save_receipt(tx, request_id, signature, &record)?;
    Ok(WorkWriteResult {
        record,
        event_inserted: false,
        artifact: None,
    })
}

fn submit(
    tx: &Transaction<'_>,
    agent: &AgentRecord,
    input: SubmitWork,
    request_id: Option<&str>,
    signature: &str,
) -> Result<WorkWriteResult, WorkWriteError> {
    if input.files.len() > 50
        || input
            .files
            .iter()
            .any(|path| bounded_text(path, 4096).is_err())
    {
        return Err(Error::InvalidRequest("Supply up to 50 file paths").into());
    }
    let work = load_work(tx, &agent.root_id.0, &input.task_id, input.version)?;
    if !matches!(
        work["status"].as_str(),
        Some("running" | "ready" | "blocked" | "review")
    ) {
        return Err(Error::InvalidRequest("This result is already accepted").into());
    }
    if work["owner"]
        .as_str()
        .is_some_and(|owner| owner != agent.id.0)
    {
        return Err(Error::InvalidRequest("Only the assigned worker can submit its result").into());
    }
    let submitter_id = work["owner"].as_str().unwrap_or(&agent.id.0);
    let submitter = event_write::checked_agent(tx, submitter_id)?;
    if submitter.root_id != agent.root_id {
        return Err(invalid("runtime_agents", submitter_id).into());
    }
    if submitter.extra.get("executionMode").and_then(Value::as_str) == Some("vm") {
        return Err(WorkWriteError::NotImplemented(
            "workspace-state/submission-files",
        ));
    }
    event_write::refuse_remote(&submitter)?;
    let statuses = dependency_statuses(tx, &work)?;
    let identity = WorkItemsRepository::submission_identity(tx, &submitter)?;
    let result_id = new_id()?;
    let now = event_write::unix_time()?;
    let (updated, artifact) = decide_submit(
        WorkSnapshot {
            record: &work,
            dependency_statuses: &statuses,
        },
        &submitter,
        &input,
        identity,
        now,
        result_id,
    )?;
    let result = updated["results"]
        .as_array()
        .and_then(|results| results.last())
        .cloned()
        .ok_or_else(|| invalid("runtime_work", &input.task_id))?;
    let mut event_inserted = false;
    if submitter.id != agent.root_id {
        let lead = AgentRepository::get(tx, &agent.root_id)?
            .ok_or_else(|| invalid("runtime_agents", &agent.root_id.0))?;
        let result_id = result["id"]
            .as_str()
            .ok_or_else(|| invalid("runtime_work", &input.task_id))?;
        event_inserted |= event_write::enqueue_recovery_event(
            tx,
            &lead,
            format!("work-result:{result_id}"),
            "work_review",
            json!({"task":input.task_id,"title":work["title"],"result":result}).to_string(),
            now,
        )?;
    }
    WorkItemsRepository::update_work(tx, &input.task_id, &updated)?;
    let record = single_view(tx, &updated)?;
    event_write::save_receipt(tx, request_id, signature, &record)?;
    Ok(WorkWriteResult {
        record,
        event_inserted,
        artifact: Some(artifact),
    })
}

fn accept(
    tx: &Transaction<'_>,
    agent: &AgentRecord,
    input: AcceptWork,
    request_id: Option<&str>,
    signature: &str,
) -> Result<WorkWriteResult, WorkWriteError> {
    if agent.id != agent.root_id {
        return Err(Error::InvalidRequest("Only the lead can accept or reject a result").into());
    }
    let work = load_work(tx, &agent.root_id.0, &input.task_id, input.version)?;
    if work["owner"]
        .as_str()
        .is_some_and(|owner| owner != agent.root_id.0)
    {
        return Err(WorkWriteError::NotImplemented(
            "workspace-state/agent-archive",
        ));
    }
    let statuses = dependency_statuses(tx, &work)?;
    let now = event_write::unix_time()?;
    let updated = decide_accept(
        WorkSnapshot {
            record: &work,
            dependency_statuses: &statuses,
        },
        &input.reason,
        "user",
        now,
    )?;
    let result_id = updated["decisions"]
        .as_array()
        .and_then(|decisions| decisions.last())
        .and_then(|decision| decision.get("resultId"))
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("runtime_work", &input.task_id))?
        .to_owned();
    WorkItemsRepository::close_review_event(tx, &agent.root_id.0, &result_id)?;
    let mut event_inserted = false;
    if work["owner"].as_str() == Some(agent.root_id.0.as_str()) {
        event_inserted |= event_write::enqueue_recovery_event(
            tx,
            agent,
            format!(
                "work-decision:{}:{}",
                input.task_id,
                work["version"].as_i64().unwrap_or_default()
            ),
            "work_decision",
            json!({"task":input.task_id,"decision":"accept","reason":input.reason}).to_string(),
            now,
        )?;
    }
    for row in WorkItemsRepository::work(tx)? {
        let dependent = row
            .record
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .ok_or_else(|| invalid("runtime_work", &row.id))?;
        if dependent["rootId"] != agent.root_id.0
            || !dependent["dependencies"]
                .as_array()
                .is_some_and(|dependencies| {
                    dependencies
                        .iter()
                        .any(|id| id.as_str() == Some(&input.task_id))
                })
        {
            continue;
        }
        let mut dep_statuses = dependency_statuses(tx, &dependent)?;
        dep_statuses.insert(input.task_id.clone(), "accepted".to_owned());
        if !blocked_by(&dependent, &dep_statuses).is_empty() {
            continue;
        }
        let Some(owner_id) = dependent["owner"].as_str() else {
            continue;
        };
        let recipient = AgentRepository::get(tx, &AgentId::from(owner_id))?
            .ok_or_else(|| invalid("runtime_agents", owner_id))?;
        event_inserted |= event_write::enqueue_recovery_event(
            tx,
            &recipient,
            format!(
                "work-ready:{}:{}",
                dependent["id"].as_str().unwrap_or_default(),
                input.task_id
            ),
            "work_ready",
            json!({
                "task":dependent["id"],"title":dependent["title"],
                "description":dependent.get("description").cloned().unwrap_or_else(|| json!(""))
            })
            .to_string(),
            now,
        )?;
    }
    WorkItemsRepository::update_work(tx, &input.task_id, &updated)?;
    let mut record = single_view(tx, &updated)?;
    record["archivePending"] = json!(false);
    event_write::save_receipt(tx, request_id, signature, &record)?;
    Ok(WorkWriteResult {
        record,
        event_inserted,
        artifact: None,
    })
}

fn load_work(
    tx: &Transaction<'_>,
    root: &str,
    task_id: &str,
    expected_version: Option<i64>,
) -> Result<Value, Error> {
    let work = WorkItemsRepository::work_record(tx, task_id)?
        .ok_or(Error::InvalidRequest("Unknown work item"))?;
    if work["rootId"] != root {
        return Err(invalid("runtime_work", task_id));
    }
    let version = work["version"]
        .as_i64()
        .ok_or_else(|| invalid("runtime_work", task_id))?;
    if expected_version.is_some_and(|expected| expected != version) {
        return Err(Error::Conflict(
            "This work item changed. Reload before editing",
        ));
    }
    Ok(work)
}

fn dependency_statuses(
    tx: &Transaction<'_>,
    work: &Value,
) -> Result<BTreeMap<String, String>, Error> {
    let id = work["id"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", "<invalid>"))?;
    let status = work["status"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", id))?;
    let mut statuses = BTreeMap::from([(id.to_owned(), status.to_owned())]);
    for dependency in work["dependencies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if let Some(value) = WorkItemsRepository::work_record(tx, dependency)?
            && value["rootId"] == work["rootId"]
        {
            let status = value["status"]
                .as_str()
                .ok_or_else(|| invalid("runtime_work", dependency))?;
            statuses.insert(dependency.to_owned(), status.to_owned());
        }
    }
    Ok(statuses)
}

fn new_id() -> Result<String, Error> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| {
        Error::Store(studio_runtime_store::StoreError::StateDirectory(
            std::io::Error::other("secure random generator failed"),
        ))
    })?;
    Ok(uuid::Builder::from_random_bytes(bytes)
        .into_uuid()
        .to_string())
}

fn push_decision(work: &mut Value, decision: Value) -> Result<(), Error> {
    let id = work["id"].as_str().unwrap_or("<work>").to_owned();
    work["decisions"]
        .as_array_mut()
        .ok_or_else(|| invalid("runtime_work", &id))?
        .push(decision);
    Ok(())
}

fn single_view(tx: &Transaction<'_>, work: &Value) -> Result<Value, Error> {
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
        if let Some(value) = WorkItemsRepository::work_record(tx, dependency)?
            && value["rootId"] == work["rootId"]
        {
            statuses.insert(
                dependency.to_owned(),
                value["status"].as_str().unwrap_or_default().to_owned(),
            );
        }
    }
    Ok(crate::work_policy::work_item_view(work, &statuses))
}

fn validate_dependencies(
    tx: &Transaction<'_>,
    root: &str,
    task: &str,
    deps: &[String],
) -> Result<(), Error> {
    if deps.iter().any(|id| id == task) {
        return Err(Error::InvalidRequest("Task dependencies contain a cycle"));
    }
    for id in deps {
        let item = WorkItemsRepository::work_record(tx, id)?
            .ok_or(Error::InvalidRequest("Dependency is outside this team"))?;
        if item["rootId"] != root {
            return Err(Error::InvalidRequest("Dependency is outside this team"));
        }
    }
    let mut active = vec![task.to_owned()];
    let mut visited = BTreeMap::new();
    for id in deps {
        dependency_visit(tx, root, id, &mut active, &mut visited)?;
    }
    Ok(())
}

fn dependency_visit(
    tx: &Transaction<'_>,
    root: &str,
    task: &str,
    active: &mut Vec<String>,
    visited: &mut BTreeMap<String, ()>,
) -> Result<(), Error> {
    if active.iter().any(|id| id == task) {
        return Err(Error::InvalidRequest("Task dependencies contain a cycle"));
    }
    if visited.contains_key(task) {
        return Ok(());
    }
    let item = WorkItemsRepository::work_record(tx, task)?
        .ok_or(Error::InvalidRequest("Dependency is outside this team"))?;
    if item["rootId"] != root {
        return Err(Error::InvalidRequest("Dependency is outside this team"));
    }
    active.push(task.to_owned());
    for dependency in item["dependencies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        dependency_visit(tx, root, dependency, active, visited)?;
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

fn bounded_text(value: &str, maximum: usize) -> Result<String, Error> {
    let value = text(value, false)?;
    if value.chars().count() > maximum {
        return Err(Error::InvalidRequest("Invalid request"));
    }
    Ok(value)
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

#[cfg(test)]
mod release_tests {
    use super::{continuation_work_pending, release_failed_work};
    use rusqlite::Connection;
    use serde_json::{Value, json};
    use studio_runtime_store::AgentRecord;

    #[test]
    fn continuation_identity_only_preserves_an_exact_pending_retry() {
        let agent = AgentRecord::from_value(json!({
            "id":"worker","rootId":"lead","epoch":7,"status":"failed",
            "autoWake":true,"accountKey":"acct","threadId":"thread",
            "lastCompletedTurn":"turn-3",
            "capacityRetry":{"status":"starting","accountKey":"acct","threadId":"thread","epoch":7,"turnId":"turn-3"}
        }))
        .expect("worker record parses");
        assert!(continuation_work_pending(&agent));

        let mut unrelated = agent.value();
        unrelated["capacityRetry"]["turnId"] = json!("older-turn");
        let unrelated = AgentRecord::from_value(unrelated).expect("worker record parses");
        assert!(!continuation_work_pending(&unrelated));
    }

    #[test]
    fn service_releases_failed_worker_claim_and_emits_recovery_event() {
        let mut connection = Connection::open_in_memory().expect("memory SQLite opens");
        connection.execute_batch(
            "CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,created REAL,epoch INTEGER,turn_id TEXT,error TEXT);
             CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT NOT NULL);",
        ).expect("service tables create");
        for agent in [
            json!({"id":"lead","rootId":"lead","epoch":1,"status":"paused","autoWake":false}),
            json!({"id":"worker","rootId":"lead","epoch":2,"status":"failed","parentId":"lead","autoWake":false,"error":"worker exited"}),
        ] {
            connection
                .execute(
                    "INSERT INTO runtime_agents(id,record) VALUES (?1,?2)",
                    rusqlite::params![agent["id"].as_str().unwrap_or_default(), agent.to_string()],
                )
                .expect("agent inserts");
        }
        let work = json!({
            "id":"task","rootId":"lead","owner":"worker","status":"running",
            "title":"Task","created":1.0,"updated":1.0,"version":2,
            "dependencies":[],"results":[],"decisions":[]
        });
        let blocked_work = json!({
            "id":"blocked-task","rootId":"lead","owner":"worker","status":"blocked",
            "title":"Blocked task","created":1.0,"updated":1.0,"version":2,
            "dependencies":[],"results":[],"decisions":[]
        });
        connection
            .execute(
                "INSERT INTO runtime_work(id,record) VALUES ('task',?1),('blocked-task',?2)",
                rusqlite::params![work.to_string(), blocked_work.to_string()],
            )
            .expect("work items insert");
        let failed = AgentRecord::from_value(json!({
            "id":"worker","rootId":"lead","epoch":2,"status":"failed",
            "parentId":"lead","autoWake":false,"error":"worker exited"
        }))
        .expect("worker record parses");

        let tx = connection.transaction().expect("transaction starts");
        let released = release_failed_work(&tx, &[failed], 10.0).expect("claims release");
        let mut released_ids = released.task_ids.clone();
        released_ids.sort();
        assert_eq!(released_ids, ["blocked-task", "task"]);
        assert!(released.event_inserted);
        let stored: Value = serde_json::from_str(
            &tx.query_row(
                "SELECT record FROM runtime_work WHERE id='task'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("released record reads"),
        )
        .expect("released JSON parses");
        assert_eq!(stored["owner"], Value::Null);
        assert_eq!(stored["status"], "ready");
        assert_eq!(stored["releases"][0]["reason"], "worker exited");
        let blocked: Value = serde_json::from_str(
            &tx.query_row(
                "SELECT record FROM runtime_work WHERE id='blocked-task'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("blocked release reads"),
        )
        .expect("blocked work JSON parses");
        assert_eq!(blocked["owner"], Value::Null);
        assert_eq!(blocked["status"], "blocked");
        assert_eq!(blocked["version"], 3);
        let event = tx
            .query_row(
                "SELECT kind,text FROM runtime_events WHERE id='work-released:task:3'",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .expect("recovery event is persisted");
        assert_eq!(event.0, "work_released");
        assert!(event.1.contains("worker exited"));
        tx.commit().expect("transaction commits");
    }
}
