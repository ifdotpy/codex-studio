//! Atomic work-item writes that need no worker or filesystem driver.

use std::collections::BTreeMap;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::io::{Read, Write};
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

pub struct WorkActorResult {
    pub record: Value,
    pub event_inserted: bool,
}

pub struct ResultArtifact {
    state_dir: PathBuf,
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
    write_internal(tx, agent_id, None, request_id, signature, command)
}

fn write_internal(
    tx: &Transaction<'_>,
    agent_id: &str,
    actor_id: Option<&str>,
    request_id: Option<&str>,
    signature: &str,
    command: WorkWrite,
) -> Result<WorkWriteResult, WorkWriteError> {
    #[cfg(not(unix))]
    if matches!(&command, WorkWrite::Submit(_)) {
        return Err(WorkWriteError::NotImplemented(
            "workspace-state/submission-files",
        ));
    }

    let agent = event_write::checked_agent(tx, agent_id)?;
    let actor = if let Some(actor_id) = actor_id {
        let actor = event_write::checked_agent(tx, actor_id)?;
        if actor.root_id != agent.root_id {
            return Err(Error::InvalidRequest("Actor belongs to another team").into());
        }
        if !event_write::truthy(actor.extra.get("autoWake")) {
            return Err(Error::InvalidRequest("The caller is stopped").into());
        }
        Some(actor)
    } else {
        None
    };
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
            if let Some(actor) = &actor
                && (actor.id != actor.root_id || agent.id != agent.root_id)
            {
                return Err(Error::InvalidRequest("Only the lead can create work").into());
            }
            create(
                tx,
                &agent,
                input,
                actor_id.unwrap_or(&agent.id.0),
                request_id,
                signature,
            )
            .map_err(Into::into)
        }
        WorkWrite::Mutate(input) => {
            if let Some(actor) = &actor {
                authorize_mutation(tx, &agent, actor, &input)?;
            }
            mutate(tx, &agent, input, actor_id, request_id, signature).map_err(Into::into)
        }
        WorkWrite::Claim(input) => claim(tx, &agent, input, actor_id, request_id, signature),
        WorkWrite::Submit(input) => submit(tx, &agent, input, actor_id, request_id, signature),
        WorkWrite::Accept(input) => accept(tx, &agent, input, actor_id, request_id, signature),
    }
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
    let command = match command {
        WorkActorWrite::Create(input) => WorkWrite::Create(input),
        WorkActorWrite::Mutate(input) => WorkWrite::Mutate(input),
    };
    let result = write_internal(tx, agent_id, Some(actor_id), request_id, signature, command)
        .map_err(|error| match error {
            WorkWriteError::Event(error) => error,
            WorkWriteError::Store(error) => Error::Store(error),
            WorkWriteError::NotImplemented(_) => Error::InvalidRequest("Invalid request"),
        })?;
    Ok(WorkActorResult {
        record: result.record,
        event_inserted: result.event_inserted,
    })
}

/// Apply caller provenance to all work actions introduced by this slice.
pub fn write_action_as_agent(
    tx: &Transaction<'_>,
    agent_id: &str,
    actor_id: &str,
    request_id: Option<&str>,
    signature: &str,
    command: WorkWrite,
) -> Result<WorkWriteResult, WorkWriteError> {
    write_internal(tx, agent_id, Some(actor_id), request_id, signature, command)
}

fn authorize_mutation(
    tx: &Transaction<'_>,
    selected: &AgentRecord,
    actor: &AgentRecord,
    mutation: &WorkMutation,
) -> Result<(), Error> {
    match mutation {
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
                && work.get("createdBy").and_then(Value::as_str) != Some(actor.id.0.as_str())
            {
                return Err(Error::InvalidRequest(
                    "Only the lead or task creator can cancel this task",
                ));
            }
        }
    }
    if selected.root_id != actor.root_id {
        return Err(Error::InvalidRequest("Actor belongs to another team"));
    }
    Ok(())
}

/// Run the filesystem half after the transaction commits. A same-identity retry
/// recreates a missing artifact and rejects a conflicting file.
pub fn write_result_artifact(artifact: &ResultArtifact) -> Result<(), std::io::Error> {
    #[cfg(not(unix))]
    {
        let _ = artifact;
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "safe result artifact writes require directory-relative filesystem operations",
        ));
    }
    #[cfg(unix)]
    write_result_artifact_at(artifact)
}

#[cfg(unix)]
fn write_result_artifact_at(artifact: &ResultArtifact) -> Result<(), std::io::Error> {
    use rustix::fs::{AtFlags, Mode, OFlags, linkat, mkdirat, openat};

    let root = fs::canonicalize(&artifact.state_dir)?;
    let relative = artifact
        .path
        .strip_prefix(&artifact.state_dir)
        .map_err(|_| std::io::Error::other("invalid result artifact path"))?;
    let parent_path = relative
        .parent()
        .ok_or_else(|| std::io::Error::other("invalid result artifact path"))?;
    let mut parent = fs::File::open("/")?;
    for component in root.components() {
        let std::path::Component::Normal(name) = component else {
            if matches!(component, std::path::Component::RootDir) {
                continue;
            }
            return Err(std::io::Error::other("invalid result artifact root"));
        };
        parent = openat(
            &parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map(fs::File::from)
        .map_err(std::io::Error::from)?;
    }
    for component in parent_path.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(std::io::Error::other("invalid result artifact path"));
        };
        let open_directory = || {
            openat(
                &parent,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map(fs::File::from)
        };
        match open_directory() {
            Ok(directory) => parent = directory,
            Err(error) if error == rustix::io::Errno::NOENT => {
                match mkdirat(&parent, name, Mode::from_raw_mode(0o700)) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => return Err(error.into()),
                }
                parent = open_directory().map_err(std::io::Error::from)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let file_name = relative
        .file_name()
        .ok_or_else(|| std::io::Error::other("invalid result artifact path"))?;
    let mut temporary_file = None;
    let mut temporary_name = String::new();
    for _ in 0..32 {
        temporary_name = format!(
            ".result-{}-{}.tmp",
            std::process::id(),
            GENERATED_WORK_ID.fetch_add(1, Ordering::Relaxed)
        );
        match openat(
            &parent,
            temporary_name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(file) => {
                temporary_file = Some(fs::File::from(file));
                break;
            }
            Err(error) if error == rustix::io::Errno::EXIST => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let mut temporary_file = temporary_file
        .ok_or_else(|| std::io::Error::other("could not allocate result artifact temporary"))?;
    let mut temporary_guard = TemporaryArtifact {
        parent: &parent,
        name: Some(temporary_name.clone()),
    };
    temporary_file.write_all(artifact.body.as_bytes())?;
    temporary_file.sync_all()?;
    drop(temporary_file);
    let result = match linkat(
        &parent,
        temporary_name.as_str(),
        &parent,
        file_name,
        AtFlags::empty(),
    ) {
        Ok(()) => Ok(()),
        Err(error) if error == rustix::io::Errno::EXIST => {
            let existing = openat(
                &parent,
                file_name,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map(fs::File::from)
            .map_err(std::io::Error::from)?;
            if !existing.metadata()?.is_file()
                || existing.metadata()?.len() > artifact.body.len() as u64
            {
                Err(std::io::Error::other("saved task result file is invalid"))
            } else {
                let mut saved = Vec::with_capacity(artifact.body.len());
                existing
                    .take(artifact.body.len() as u64 + 1)
                    .read_to_end(&mut saved)?;
                if saved == artifact.body.as_bytes() {
                    Ok(())
                } else {
                    Err(std::io::Error::other(
                        "saved task result file differs from its committed record",
                    ))
                }
            }
        }
        Err(error) => Err(error.into()),
    };
    result?;
    temporary_guard.cleanup()?;
    Ok(())
}

#[cfg(unix)]
struct TemporaryArtifact<'a> {
    parent: &'a fs::File,
    name: Option<String>,
}

#[cfg(unix)]
impl TemporaryArtifact<'_> {
    fn cleanup(&mut self) -> Result<(), std::io::Error> {
        let Some(name) = self.name.as_deref() else {
            return Ok(());
        };
        rustix::fs::unlinkat(self.parent, name, rustix::fs::AtFlags::empty())
            .map_err(std::io::Error::from)?;
        self.name.take();
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for TemporaryArtifact<'_> {
    fn drop(&mut self) {
        if let Some(name) = self.name.take() {
            match rustix::fs::unlinkat(self.parent, name.as_str(), rustix::fs::AtFlags::empty()) {
                Ok(()) | Err(_) => {}
            }
        }
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
    for snapshot in agents {
        let Some(worker) = AgentRepository::get(tx, &snapshot.id)? else {
            continue;
        };
        if worker.root_id != snapshot.root_id
            || worker.epoch != snapshot.epoch
            || worker.status != snapshot.status
        {
            continue;
        }
        let deleted = worker
            .extra
            .get("deletedAt")
            .is_some_and(|value| event_write::truthy(Some(value)));
        if worker.id == worker.root_id
            || (!deleted && worker.status.as_str() != "failed")
            || (!deleted && continuation_work_pending(&worker))
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
            if let Some(root) = root.filter(|root| {
                !root
                    .extra
                    .get("deletedAt")
                    .is_some_and(|value| event_write::truthy(Some(value)))
            }) {
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
    if !agent
        .extra
        .get("autoWake")
        .is_some_and(|value| event_write::truthy(Some(value)))
        || agent
            .extra
            .get("deletedAt")
            .is_some_and(|value| event_write::truthy(Some(value)))
    {
        return false;
    }
    let default_account = json!("default");
    let account = agent.extra.get("accountKey").unwrap_or(&default_account);
    let thread = agent.extra.get("threadId").and_then(Value::as_str);
    let epoch = agent.epoch;
    let last_turn = agent.extra.get("lastCompletedTurn").and_then(Value::as_str);

    let matches = |record: &Value, statuses: &[&str], turn_field: &str| {
        let record_account = record.get("accountKey").unwrap_or(&default_account);
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
            && restart
                .get("autoWake")
                .is_some_and(|value| event_write::truthy(Some(value)))
            && restart.get("accountKey").unwrap_or(&default_account) == account
            && restart.get("threadId").and_then(Value::as_str) == thread
            && thread.is_some_and(|thread| !thread.is_empty())
            && restart.get("epoch").and_then(Value::as_i64) == Some(epoch))
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
    let result_text = python_bounded_text(&input.result, 32000)?;
    let checks_text = python_bounded_text(&input.checks, 32000)?;
    let revision_text = python_bounded_text(&input.revision, 200)?;
    let mut result = json!({
        "id": result_id,
        "agent": submitter_id,
        "text": result_text,
        "checks": checks_text,
        "revision": revision_text,
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
    Ok((
        updated,
        ResultArtifact {
            state_dir: input.state_dir.clone(),
            path,
            body,
        },
    ))
}

fn decide_accept(
    snapshot: WorkSnapshot<'_>,
    reason: &str,
    by: &str,
    now: f64,
) -> Result<Value, Error> {
    let record = snapshot.record;
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
    if crate::event_write::python_strip(reason).is_empty() {
        return Err(Error::InvalidRequest("Field must not be empty"));
    }
    push_decision(
        &mut updated,
        json!({"resultId":result_id,"decision":"accept","reason":text(reason, false)?,"by":by,"created":now}),
    )?;
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
        state_dir: state_dir.to_owned(),
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
    actor_id: &str,
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
        "results": [], "decisions": [], "createdBy": actor_id,
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
    actor_id: Option<&str>,
    request_id: Option<&str>,
    signature: &str,
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
                json!({"decision":"cancel", "reason":reason, "by":actor_id.unwrap_or(&agent.id.0), "owner":owner, "created":event_write::unix_time()?}),
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
                if actor_id != Some(owner_id) {
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
                json!({"resultId":result_id,"decision":"reject","reason":reason,"by":actor_id.unwrap_or("user"),"created":event_write::unix_time()?}),
            )?;
            work["status"] = json!("ready");
        }
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
    actor_id: Option<&str>,
    request_id: Option<&str>,
    signature: &str,
) -> Result<WorkWriteResult, WorkWriteError> {
    let work = load_work(tx, &agent.root_id.0, &input.task_id, input.version)?;
    let claimant = event_write::checked_agent(tx, actor_id.unwrap_or(&input.claimant_id))?;
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
    actor_id: Option<&str>,
    request_id: Option<&str>,
    signature: &str,
) -> Result<WorkWriteResult, WorkWriteError> {
    if input.files.len() > 50 || input.files.iter().any(|path| path.chars().count() > 4096) {
        return Err(Error::InvalidRequest("Supply up to 50 file paths").into());
    }
    let work = load_work(tx, &agent.root_id.0, &input.task_id, input.version)?;
    if actor_id.is_some_and(|actor| work["owner"].as_str() != Some(actor)) {
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
    if actor_id != Some(agent.root_id.0.as_str()) {
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
    actor_id: Option<&str>,
    request_id: Option<&str>,
    signature: &str,
) -> Result<WorkWriteResult, WorkWriteError> {
    if actor_id.is_some_and(|actor| actor != agent.root_id.0) {
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
    if crate::event_write::python_strip(&input.reason).is_empty() {
        return Err(Error::InvalidRequest("Field must not be empty").into());
    }
    let root = AgentRepository::get(tx, &agent.root_id)?
        .ok_or_else(|| invalid("runtime_agents", &agent.root_id.0))?;
    let updated = decide_accept(
        WorkSnapshot {
            record: &work,
            dependency_statuses: &statuses,
        },
        &input.reason,
        actor_id.unwrap_or("user"),
        now,
    )?;
    let result_id = updated["decisions"]
        .as_array()
        .and_then(|decisions| decisions.last())
        .and_then(|decision| decision.get("resultId"))
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("runtime_work", &input.task_id))?
        .to_owned();
    let mut ready_dependents = Vec::new();
    for row in WorkItemsRepository::dependent_work(tx, &agent.root_id.0, &input.task_id)? {
        let dependent = WorkItemsRepository::work_record(tx, &row.id)?
            .ok_or_else(|| invalid("runtime_work", &row.id))?;
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
        event_write::refuse_remote(&recipient)?;
        ready_dependents.push((recipient, dependent));
    }
    WorkItemsRepository::close_review_event(tx, &agent.root_id.0, &result_id)?;
    let mut event_inserted = false;
    if let Some(owner_id) = work["owner"].as_str()
        && owner_id != actor_id.unwrap_or("")
        && (actor_id.is_some() || owner_id == agent.root_id.0)
    {
        event_inserted |= event_write::enqueue_recovery_event(
            tx,
            &root,
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
    for (recipient, dependent) in ready_dependents {
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
    let statuses = dependency_statuses(tx, work)?;
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

fn python_bounded_text(value: &str, maximum: usize) -> Result<String, Error> {
    let value = event_write::python_strip(value);
    if value.is_empty() || value.chars().count() > maximum {
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

#[cfg(test)]
mod artifact_tests {
    use super::{ResultArtifact, write_result_artifact};
    use std::fs;

    #[cfg(unix)]
    #[test]
    fn replay_of_fifo_is_bounded_and_does_not_leave_a_temporary_file() {
        use rustix::fs::{Mode, mkfifoat};
        use std::sync::mpsc;
        use std::time::Duration;

        let temporary = tempfile::tempdir().expect("temporary root opens");
        let state = temporary.path().join("state");
        let parent = state.join("results/task");
        fs::create_dir_all(&parent).expect("artifact parent creates");
        let directory = fs::File::open(&parent).expect("artifact parent opens");
        mkfifoat(&directory, "result.md", Mode::from_raw_mode(0o600))
            .expect("FIFO artifact creates");
        let artifact = ResultArtifact {
            state_dir: state,
            path: parent.join("result.md"),
            body: "committed result".to_owned(),
        };
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            drop(sender.send(write_result_artifact(&artifact)));
        });
        receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("FIFO replay returns without blocking")
            .expect_err("FIFO is not a valid result artifact");
        let entries: Vec<_> = fs::read_dir(&parent)
            .expect("artifact parent lists")
            .map(|entry| entry.expect("directory entry reads").file_name())
            .collect();
        assert_eq!(entries, [std::ffi::OsString::from("result.md")]);
    }

    #[cfg(unix)]
    #[test]
    fn failed_artifact_replay_removes_its_temporary_file() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary root opens");
        let state = temporary.path().join("state");
        let parent = state.join("results/task");
        let outside = temporary.path().join("outside.md");
        fs::create_dir_all(&parent).expect("artifact parent creates");
        fs::write(&outside, "foreign content").expect("outside file writes");
        symlink(&outside, parent.join("result.md")).expect("artifact symlink creates");
        let artifact = ResultArtifact {
            state_dir: state,
            path: parent.join("result.md"),
            body: "committed result".to_owned(),
        };

        assert!(write_result_artifact(&artifact).is_err());
        let entries: Vec<_> = fs::read_dir(&parent)
            .expect("artifact parent lists")
            .map(|entry| entry.expect("directory entry reads").file_name())
            .collect();
        assert_eq!(entries, [std::ffi::OsString::from("result.md")]);
    }

    #[cfg(unix)]
    #[test]
    fn result_artifact_writer_is_anchored_to_open_directory_handles() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary root opens");
        let state = temporary.path().join("state");
        let outside = temporary.path().join("outside");
        fs::create_dir_all(&state).expect("state directory creates");
        fs::create_dir_all(&outside).expect("outside directory creates");
        let artifact_path = state.join("results/task/result-1.md");
        let artifact = ResultArtifact {
            state_dir: state.clone(),
            path: artifact_path.clone(),
            body: "committed result".to_owned(),
        };
        write_result_artifact(&artifact).expect("safe artifact writes");
        assert_eq!(
            fs::read(&artifact_path).expect("artifact reads"),
            b"committed result"
        );

        fs::remove_dir_all(state.join("results")).expect("result tree removes");
        symlink(&outside, state.join("results")).expect("outside symlink creates");
        let escaped = ResultArtifact {
            state_dir: state,
            path: artifact_path,
            body: "must stay contained".to_owned(),
        };
        assert!(write_result_artifact(&escaped).is_err());
        assert!(!outside.join("task/result-1.md").exists());
    }
}

#[cfg(test)]
mod actor_tests {
    #[cfg(not(unix))]
    use super::SubmitWork;
    use super::{
        AcceptWork, ClaimWork, CreateWork, WorkActorWrite, WorkMutation, WorkWrite, WorkWriteError,
        write_action_as_agent, write_as_agent,
    };
    use rusqlite::Connection;
    use serde_json::{Value, json};
    use studio_runtime_store::WorkItemsRepository;

    fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("memory SQLite opens");
        connection.execute_batch(
            "CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,created REAL,epoch INTEGER,turn_id TEXT,error TEXT);
             CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL);
             CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,agent TEXT,account TEXT,epoch INTEGER,thread TEXT,turn TEXT,created REAL,record TEXT);",
        ).expect("work tables create");
        for agent in [
            json!({"id":"lead","rootId":"lead","epoch":1,"status":"running","autoWake":true,"inFlight":true,"agentMode":"multi","concurrency":8}),
            json!({"id":"worker","rootId":"lead","epoch":2,"status":"running","autoWake":true,"inFlight":true,"agentMode":"multi","concurrency":8}),
        ] {
            connection
                .execute(
                    "INSERT INTO runtime_agents VALUES (?1,?2)",
                    rusqlite::params![agent["id"].as_str().expect("id"), agent.to_string()],
                )
                .expect("agent row inserts");
        }
        connection
    }

    #[cfg(not(unix))]
    #[test]
    fn submit_refuses_before_database_access_without_directory_relative_operations() {
        let mut connection = Connection::open_in_memory().expect("memory SQLite opens");
        let tx = connection.transaction().expect("transaction starts");
        let result = super::write(
            &tx,
            "lead",
            None,
            "submit-unsupported-platform",
            WorkWrite::Submit(SubmitWork {
                task_id: "task".to_owned(),
                version: Some(1),
                result: "result".to_owned(),
                checks: "checks".to_owned(),
                revision: "abc1234".to_owned(),
                files: vec![],
                state_dir: std::env::temp_dir(),
            }),
        );
        assert!(matches!(
            result,
            Err(WorkWriteError::NotImplemented(
                "workspace-state/submission-files"
            ))
        ));
    }

    fn insert_work(connection: &Connection, work: &Value) {
        connection
            .execute(
                "INSERT INTO runtime_work VALUES (?1,?2)",
                rusqlite::params![work["id"].as_str().expect("work id"), work.to_string()],
            )
            .expect("work row inserts");
    }

    fn events(connection: &Connection) -> Vec<(String, String, String)> {
        let mut statement = connection
            .prepare("SELECT id,agent,kind FROM runtime_events ORDER BY id")
            .expect("events query prepares");
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("events query runs")
            .map(|row| row.expect("event row reads"))
            .collect()
    }

    fn work(id: &str, owner: Option<&str>, status: &str, results: Value) -> Value {
        json!({"id":id,"rootId":"lead","owner":owner,"status":status,"title":id,
            "description":"","created":1.0,"updated":1.0,"version":2,
            "dependencies":[],"results":results,"decisions":[],"createdBy":"worker"})
    }

    #[test]
    fn actor_claim_overrides_the_selected_payload_claimant() {
        let mut connection = database();
        insert_work(&connection, &work("claim", None, "ready", json!([])));
        let tx = connection.transaction().expect("transaction starts");
        let result = write_action_as_agent(
            &tx,
            "lead",
            "worker",
            None,
            "claim-as-worker",
            WorkWrite::Claim(ClaimWork {
                task_id: "claim".to_owned(),
                version: Some(2),
                claimant_id: "lead".to_owned(),
            }),
        )
        .expect("actor claim succeeds");
        assert_eq!(result.record["owner"], "worker");
        tx.commit().expect("transaction commits");
    }

    #[test]
    fn actor_create_rejects_a_nonlead_selected_agent_without_writing() {
        let mut connection = database();
        let tx = connection.transaction().expect("transaction starts");
        let result = write_action_as_agent(
            &tx,
            "worker",
            "lead",
            None,
            "create-with-worker-selected",
            WorkWrite::Create(CreateWork {
                title: "must be refused".to_owned(),
                description: String::new(),
                owner: None,
                status: None,
                dependencies: Vec::new(),
            }),
        );
        assert!(matches!(result, Err(WorkWriteError::Event(_))));
        let count: i64 = tx
            .query_row("SELECT count(*) FROM runtime_work", [], |row| row.get(0))
            .expect("work count reads");
        assert_eq!(count, 0);
        tx.rollback().expect("transaction rolls back");
    }

    #[test]
    fn nonlead_actor_cannot_change_assignments_through_actor_api() {
        let mut connection = database();
        insert_work(
            &connection,
            &work("unauthorized-update", Some("worker"), "running", json!([])),
        );
        let before = WorkItemsRepository::work_record(&connection, "unauthorized-update")
            .expect("stored work reads")
            .expect("work exists");
        let tx = connection.transaction().expect("transaction starts");
        let result = write_as_agent(
            &tx,
            "lead",
            "worker",
            None,
            "worker-assignment-update",
            WorkActorWrite::Mutate(WorkMutation::Update {
                task_id: "unauthorized-update".to_owned(),
                version: Some(2),
                title: None,
                description: None,
                owner: Some(Some("lead".to_owned())),
                status: None,
                dependencies: None,
            }),
        );
        assert!(result.is_err());
        let after = WorkItemsRepository::work_record(&tx, "unauthorized-update")
            .expect("stored work reads")
            .expect("work exists");
        assert_eq!(after, before);
        tx.rollback().expect("transaction rolls back");
    }

    #[test]
    fn actor_owner_cancel_notifies_the_owner_itself() {
        let mut connection = database();
        insert_work(
            &connection,
            &work("cancel", Some("worker"), "running", json!([])),
        );
        let tx = connection.transaction().expect("transaction starts");
        let result = write_action_as_agent(
            &tx,
            "lead",
            "worker",
            None,
            "cancel-as-owner",
            WorkWrite::Mutate(WorkMutation::Cancel {
                task_id: "cancel".to_owned(),
                version: Some(2),
                reason: "stop".to_owned(),
            }),
        )
        .expect("owner cancellation succeeds");
        assert_eq!(result.record["status"], "cancelled");
        assert_eq!(
            events(&tx),
            vec![(
                "work-decision:cancel:cancel".to_owned(),
                "worker".to_owned(),
                "work_decision".to_owned(),
            )]
        );
        tx.commit().expect("transaction commits");
    }

    #[test]
    fn lead_actor_cancelling_worker_owned_work_notifies_that_worker() {
        let mut connection = database();
        insert_work(
            &connection,
            &work("lead-cancel", Some("worker"), "running", json!([])),
        );
        let tx = connection.transaction().expect("transaction starts");
        let result = write_action_as_agent(
            &tx,
            "lead",
            "lead",
            None,
            "cancel-worker-as-lead",
            WorkWrite::Mutate(WorkMutation::Cancel {
                task_id: "lead-cancel".to_owned(),
                version: Some(2),
                reason: "stop".to_owned(),
            }),
        )
        .expect("lead cancellation succeeds");
        assert_eq!(result.record["decisions"][0]["by"], "lead");
        assert_eq!(
            events(&tx),
            vec![(
                "work-decision:lead-cancel:cancel".into(),
                "worker".into(),
                "work_decision".into(),
            )]
        );
        tx.commit().expect("transaction commits");
    }

    #[test]
    fn lead_actor_accepting_its_own_result_does_not_self_notify() {
        let mut connection = database();
        insert_work(
            &connection,
            &work(
                "accept",
                Some("lead"),
                "review",
                json!([
                    {"id":"result-1","agent":"lead","text":"done","checks":"checked","revision":"abc1234","files":[]}
                ]),
            ),
        );
        let tx = connection.transaction().expect("transaction starts");
        let result = write_action_as_agent(
            &tx,
            "lead",
            "lead",
            None,
            "accept-as-lead",
            WorkWrite::Accept(AcceptWork {
                task_id: "accept".to_owned(),
                version: Some(2),
                reason: "verified".to_owned(),
            }),
        )
        .expect("lead acceptance succeeds");
        assert_eq!(result.record["status"], "accepted");
        let count: i64 = tx
            .query_row(
                "SELECT count(*) FROM runtime_events WHERE kind='work_decision'",
                [],
                |row| row.get(0),
            )
            .expect("decision event count reads");
        assert_eq!(count, 0);
        tx.commit().expect("transaction commits");
    }

    #[test]
    fn lead_actor_rejecting_its_own_result_does_not_self_notify() {
        let mut connection = database();
        insert_work(
            &connection,
            &work(
                "reject-own",
                Some("lead"),
                "review",
                json!([{"id":"result-own","agent":"lead","text":"done","checks":"checked","revision":"abc1234","files":[]}]),
            ),
        );
        let tx = connection.transaction().expect("transaction starts");
        let result = write_action_as_agent(
            &tx,
            "lead",
            "lead",
            None,
            "reject-own-as-lead",
            WorkWrite::Mutate(WorkMutation::Reject {
                task_id: "reject-own".to_owned(),
                version: Some(2),
                reason: "needs changes".to_owned(),
            }),
        )
        .expect("lead rejection succeeds");
        assert_eq!(result.record["decisions"][0]["by"], "lead");
        let count: i64 = tx
            .query_row(
                "SELECT count(*) FROM runtime_events WHERE kind='work_decision'",
                [],
                |row| row.get(0),
            )
            .expect("decision event count reads");
        assert_eq!(count, 0);
        tx.commit().expect("transaction commits");
    }

    #[test]
    fn lead_actor_rejecting_worker_result_notifies_worker() {
        let mut connection = database();
        insert_work(
            &connection,
            &work(
                "reject-worker",
                Some("worker"),
                "review",
                json!([{"id":"result-worker","agent":"worker","text":"done","checks":"checked","revision":"abc1234","files":[]}]),
            ),
        );
        let tx = connection.transaction().expect("transaction starts");
        let result = write_action_as_agent(
            &tx,
            "lead",
            "lead",
            None,
            "reject-worker-as-lead",
            WorkWrite::Mutate(WorkMutation::Reject {
                task_id: "reject-worker".to_owned(),
                version: Some(2),
                reason: "needs changes".to_owned(),
            }),
        )
        .expect("lead rejection succeeds");
        assert_eq!(result.record["decisions"][0]["by"], "lead");
        assert_eq!(
            events(&tx),
            vec![(
                "work-decision:reject-worker:2".into(),
                "worker".into(),
                "work_decision".into(),
            )]
        );
        tx.commit().expect("transaction commits");
    }

    #[test]
    fn actor_submit_records_worker_and_notifies_the_lead() {
        let mut connection = database();
        insert_work(
            &connection,
            &work("submit", Some("worker"), "running", json!([])),
        );
        let tx = connection.transaction().expect("transaction starts");
        let result = write_action_as_agent(
            &tx,
            "lead",
            "worker",
            None,
            "submit-as-worker",
            WorkWrite::Submit(super::SubmitWork {
                task_id: "submit".to_owned(),
                version: Some(2),
                result: "done".to_owned(),
                checks: "checked".to_owned(),
                revision: "abc1234".to_owned(),
                files: vec![],
                state_dir: std::env::temp_dir(),
            }),
        )
        .expect("worker submission succeeds");
        assert_eq!(result.record["results"][0]["agent"], "worker");
        assert_eq!(
            events(&tx),
            vec![(
                format!(
                    "work-result:{}",
                    result.record["results"][0]["id"]
                        .as_str()
                        .expect("result ID")
                ),
                "lead".to_owned(),
                "work_review".to_owned(),
            )]
        );
        tx.commit().expect("transaction commits");
    }

    #[test]
    fn lead_actor_submitting_its_own_work_does_not_create_a_review_event() {
        let mut connection = database();
        insert_work(
            &connection,
            &work("lead-submit", Some("lead"), "running", json!([])),
        );
        let tx = connection.transaction().expect("transaction starts");
        let result = write_action_as_agent(
            &tx,
            "lead",
            "lead",
            None,
            "submit-as-lead",
            WorkWrite::Submit(super::SubmitWork {
                task_id: "lead-submit".to_owned(),
                version: Some(2),
                result: "done".to_owned(),
                checks: "checked".to_owned(),
                revision: "abc1234".to_owned(),
                files: vec![],
                state_dir: std::env::temp_dir(),
            }),
        )
        .expect("lead submission succeeds");
        assert_eq!(result.record["results"][0]["agent"], "lead");
        let count: i64 = tx
            .query_row(
                "SELECT count(*) FROM runtime_events WHERE kind='work_review'",
                [],
                |row| row.get(0),
            )
            .expect("review event count reads");
        assert_eq!(count, 0);
        tx.commit().expect("transaction commits");
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

        let mut explicit_null = agent.value();
        explicit_null["accountKey"] = Value::Null;
        explicit_null["capacityRetry"]
            .as_object_mut()
            .expect("capacity retry is an object")
            .remove("accountKey");
        let explicit_null = AgentRecord::from_value(explicit_null).expect("worker record parses");
        assert!(!continuation_work_pending(&explicit_null));
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
