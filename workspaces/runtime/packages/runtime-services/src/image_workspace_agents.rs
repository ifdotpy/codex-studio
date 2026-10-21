//! The agent-record side of image workspaces, ported from Python `codex_runtime.py`:
//! `prepare_locked` (write the mounted path back and send the ready notice),
//! `start_image_base` / `image_base_completed` / `complete_image_workspace`
//! (finish an agent in its read-only phase once its base image is ready), and
//! `resume_read_only_image_bases` (restart those builds at startup).
//!
//! The image lifecycle itself (base build, clone, attach) stays in `image_workspace`.
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, Connection, RuntimeStore, StoreError, Transaction,
    truthy,
};

use crate::event_write::{self, EventWriteError};
use crate::image_workspace::ImageWorkspaceError;

/// Message id prefix of the one ready notice per agent (`image-workspace-ready:<agent>`).
pub const READY_NOTICE_PREFIX: &str = "image-workspace-ready:";
/// Message id prefix of the one fallback notice per agent.
pub const FALLBACK_NOTICE_PREFIX: &str = "image-workspace-fallback:";
/// Message id prefix of the one failure notice per agent.
pub const ERROR_NOTICE_PREFIX: &str = "image-workspace-error:";
/// Characters of a failure kept in `imageWorkspaceError` (Python `str(error)[:1200]`).
const STORED_ERROR_CHARS: usize = 1200;
/// Characters of a failure quoted in a fallback notice (Python `str(error)[:800]`).
const NOTICE_ERROR_CHARS: usize = 800;
/// Time between base status checks while a read-only agent waits for its base.
pub const BASE_POLL: Duration = Duration::from_secs(1);

#[must_use]
pub fn ready_notice_id(agent_id: &str) -> String {
    format!("{READY_NOTICE_PREFIX}{agent_id}")
}

/// Python `prepare_locked`: the text of the ready notice for the copy at `cwd`.
#[must_use]
pub fn ready_notice_text(record: &Value, cwd: &str, now: f64) -> String {
    let copied_at = record["imageWorkspaceCreatedAt"]
        .as_f64()
        .filter(|at| *at != 0.0)
        .or_else(|| record["created"].as_f64())
        .unwrap_or(now);
    let taken_at = chrono::DateTime::from_timestamp(copied_at as i64, 0)
        .map(|time| time.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default();
    let source = record["imageWorkspaceRepo"]
        .as_str()
        .filter(|repo| !repo.is_empty())
        .unwrap_or("the user's folder");
    format!(
        "[Studio image workspace ready] Path: {cwd}. This is a copy of {source} taken at {taken_at}, including uncommitted changes. Write access is enabled."
    )
}

fn invalid(agent_id: &str) -> EventWriteError {
    EventWriteError::InvalidStoredObject {
        object: "runtime_agents",
        id: agent_id.to_owned(),
    }
}

fn put(transaction: &Transaction<'_>, value: Value) -> Result<Value, EventWriteError> {
    let id = value["id"].as_str().unwrap_or_default().to_owned();
    let record = AgentRecord::from_value(value).map_err(|_| invalid(&id))?;
    AgentRepository::put(transaction, &record)?;
    let value = record.value();
    crate::agents_create::publish_agent(transaction, &value)?;
    Ok(value)
}

/// A notice that commits with the record it describes.
struct Notice {
    id: String,
    text: String,
}

/// Remember the notice on the record (Python `_send_image_workspace_notice`).
fn mark_notice(value: &mut Value, notice: &Notice) {
    value["imageWorkspaceNoticeId"] = json!(notice.id);
    value["imageWorkspaceNoticeText"] = json!(notice.text);
    value["imageWorkspaceNoticeSent"] = json!(notice.id);
    if let Some(object) = value.as_object_mut() {
        object.remove("imageWorkspaceHandoffText");
        object.remove("imageWorkspaceNoticeError");
    }
}

/// Write the record, then queue its notice in the same transaction. The queue
/// write moves an idle auto-waking agent to `queued`, so it must follow the
/// record write; the stored record is returned.
fn put_with_notice(
    transaction: &Transaction<'_>,
    mut value: Value,
    notice: Option<Notice>,
    now: f64,
) -> Result<Value, EventWriteError> {
    if let Some(notice) = &notice {
        mark_notice(&mut value, notice);
    }
    let value = put(transaction, value)?;
    let Some(notice) = notice else {
        return Ok(value);
    };
    let id = value["id"].as_str().unwrap_or_default().to_owned();
    let record = AgentRecord::from_value(value.clone()).map_err(|_| invalid(&id))?;
    event_write::enqueue_event(
        transaction,
        &record,
        notice.id,
        "followup",
        notice.text,
        now,
    )?;
    Ok(
        AgentRepository::get(transaction, &AgentId::from(id.as_str()))?
            .map_or(value, |agent| agent.value()),
    )
}

/// Python `prepare_locked` after `ensure_mounted`: the record names the mounted turn
/// directory and mount, and the agent gets the ready notice unless one was sent.
/// Returns the current record, written only when something changed.
pub fn record_mounted(
    transaction: &Transaction<'_>,
    agent_id: &str,
    cwd: &Path,
    mount: &str,
    now: f64,
) -> Result<Option<Value>, EventWriteError> {
    let Some(agent) = AgentRepository::get(transaction, &AgentId::from(agent_id))? else {
        return Ok(None);
    };
    let mut value = agent.value();
    if truthy(value.get("deletedAt")) {
        return Ok(Some(value));
    }
    let cwd = cwd.to_str().ok_or_else(|| invalid(agent_id))?.to_owned();
    let mut changed = false;
    if value["cwd"] != cwd.as_str() {
        value["cwd"] = json!(cwd);
        changed = true;
    }
    if value["imageWorkspaceMount"] != mount {
        value["imageWorkspaceMount"] = json!(mount);
        changed = true;
    }
    let notice = (!truthy(value.get("imageWorkspaceNoticeSent"))).then(|| Notice {
        id: ready_notice_id(agent_id),
        text: ready_notice_text(&value, &cwd, now),
    });
    if !changed && notice.is_none() {
        return Ok(Some(value));
    }
    put_with_notice(transaction, value, notice, now).map(Some)
}

/// Python `complete_image_workspace` with a created copy, for a record the caller
/// already checked: the record names the copy and the agent gets the ready notice.
/// The create paths and the read-only completion share this one ready shape.
pub fn record_ready(
    transaction: &Transaction<'_>,
    mut value: Value,
    workspace: &Value,
    now: f64,
) -> Result<Value, EventWriteError> {
    let agent_id = value["id"].as_str().unwrap_or_default().to_owned();
    let repo = workspace["path"]
        .as_str()
        .ok_or_else(|| invalid(&agent_id))?;
    let mount = workspace["mount"]
        .as_str()
        .ok_or_else(|| invalid(&agent_id))?;
    let cwd = turn_cwd(Path::new(repo), value["imageWorkspaceSubpath"].as_str())
        .ok_or_else(|| invalid(&agent_id))?;
    let cwd = cwd.to_str().ok_or_else(|| invalid(&agent_id))?.to_owned();
    value["cwd"] = json!(cwd);
    value["branch"] = Value::Null;
    value["imageWorkspaceReady"] = json!(true);
    value["imageWorkspacePhase"] = json!("ready");
    value["imageWorkspaceBaseState"] = json!("ready");
    value["imageWorkspaceMount"] = json!(mount);
    value["imageWorkspaceCreatedAt"] = json!(now);
    let message_id = ready_notice_id(&agent_id);
    let notice = (value["imageWorkspaceNoticeSent"] != message_id.as_str()).then(|| Notice {
        text: ready_notice_text(&value, &cwd, now),
        id: message_id,
    });
    put_with_notice(transaction, value, notice, now)
}

/// The record still describes an image agent waiting in its read-only phase.
#[must_use]
pub fn read_only_pending(record: &Value) -> bool {
    !truthy(record.get("deletedAt"))
        && truthy(record.get("imageWorkspace"))
        && !truthy(record.get("imageWorkspaceReady"))
        && record["imageWorkspacePhase"] == "read_only"
}

/// The outcome of finishing one read-only agent, for callers and tests.
#[derive(Debug, Clone, PartialEq)]
pub enum Completion {
    /// The copy is mounted and the record is ready.
    Ready(Value),
    /// The base or copy failed; the record holds or falls back as in Python.
    Failed(Value),
    /// The agent changed meanwhile; nothing was written.
    Superseded,
}

/// Why a read-only agent did not get its copy. Python words the two cases apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// Python `complete_image_workspace` with a failed base status.
    Base(String),
    /// Python `complete_image_workspace` when `create_workspace` or its check failed.
    Setup(String),
}

impl Failure {
    fn message(&self) -> &str {
        match self {
            Self::Base(message) | Self::Setup(message) => message,
        }
    }
}

fn prefix(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

/// What `settle_ready` found in the store.
#[derive(Debug)]
enum Settled {
    Ready(Value),
    /// Another path already made the record ready; its copy is in use.
    AlreadyReady,
    /// The agent left its read-only phase; the new copy has no owner.
    Stale,
}

fn settle_ready(
    transaction: &Transaction<'_>,
    agent_id: &str,
    workspace: &Value,
    now: f64,
) -> Result<Settled, EventWriteError> {
    let Some(agent) = AgentRepository::get(transaction, &AgentId::from(agent_id))? else {
        return Ok(Settled::Stale);
    };
    let value = agent.value();
    if truthy(value.get("imageWorkspaceReady")) {
        return Ok(Settled::AlreadyReady);
    }
    if !read_only_pending(&value) {
        return Ok(Settled::Stale);
    }
    record_ready(transaction, value, workspace, now).map(Settled::Ready)
}

/// Python `hold_failed_explicit_image` and the implicit fallback of
/// `complete_image_workspace`: an explicit image request holds the agent paused;
/// an implicit one falls back to a Git worktree or the original folder.
pub fn settle_failed(
    transaction: &Transaction<'_>,
    agent_id: &str,
    failure: &Failure,
    now: f64,
) -> Result<Option<Value>, EventWriteError> {
    let Some(agent) = AgentRepository::get(transaction, &AgentId::from(agent_id))? else {
        return Ok(None);
    };
    let mut value = agent.value();
    if !read_only_pending(&value) {
        return Ok(None);
    }
    let stored = prefix(failure.message(), STORED_ERROR_CHARS);
    value["imageWorkspaceBaseState"] = json!("failed");
    value["imageWorkspaceError"] = json!(stored);
    let (event_id, message_id, notice) = if value["workspaceMode"] == "image"
        && value["workspaceModeExplicit"] == true
    {
        let notice = format!(
            "[Studio workspace error] The requested image workspace failed: {stored}. The worker remains read-only in the selected folder. Studio will not switch it to a worktree or shared folder."
        );
        value["imageWorkspacePhase"] = json!("failed");
        value["worktree"] = json!(false);
        value["worktreeWarning"] = Value::Null;
        value["status"] = json!("paused");
        value["autoWake"] = json!(false);
        value["error"] = json!(notice);
        (
            "image-workspace-error",
            format!("{ERROR_NOTICE_PREFIX}{agent_id}"),
            notice,
        )
    } else {
        let has_git = truthy(value.get("imageWorkspaceHasGit"));
        let fallback = if has_git {
            "Studio will create a Git worktree."
        } else {
            "Studio will use the original folder."
        };
        let quoted = prefix(failure.message(), NOTICE_ERROR_CHARS);
        let notice = match failure {
            Failure::Base(_) => format!(
                "[Studio workspace fallback] The image workspace could not start: {quoted}. {fallback} This work remains read-only until the next turn starts."
            ),
            Failure::Setup(_) => format!(
                "[Studio workspace fallback] Image workspace setup failed: {quoted}. {fallback}"
            ),
        };
        value["environment"] = json!("host");
        value["imageWorkspace"] = json!(false);
        value["imageWorkspacePhase"] = json!("fallback");
        value["workspaceBackend"] = Value::Null;
        value["worktree"] = json!(has_git);
        value["worktreeWarning"] = Value::Null;
        value["workspaceMode"] = json!(if has_git { "worktree" } else { "shared" });
        value["error"] = Value::Null;
        (
            "image-workspace-fallback",
            format!("{FALLBACK_NOTICE_PREFIX}{agent_id}"),
            notice,
        )
    };
    let queued = (value["imageWorkspaceNoticeSent"] != message_id.as_str()).then(|| Notice {
        id: message_id,
        text: notice.clone(),
    });
    let value = put_with_notice(transaction, value, queued, now)?;
    crate::lifecycle_effects::notify_parent_of_image_workspace(
        transaction,
        &value,
        event_id,
        &notice,
        now,
    )?;
    Ok(Some(value))
}

/// `<repo>/<subpath>`; `None` when the subpath leaves the repository.
#[must_use]
pub fn turn_cwd(repo: &Path, subpath: Option<&str>) -> Option<PathBuf> {
    use std::path::Component;
    let subpath = Path::new(subpath.unwrap_or("."));
    if subpath
        .components()
        .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return None;
    }
    Some(
        subpath
            .components()
            .filter(|part| matches!(part, Component::Normal(_)))
            .fold(repo.to_path_buf(), |path, part| path.join(part)),
    )
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or_default()
}

/// The image operations one completion uses. Production uses `image_workspace`;
/// tests substitute them so ordinary tests never create disk images.
#[derive(Clone, Copy)]
pub struct Steps {
    /// Start or continue the repository's base build and return its status
    /// (`state` is `ready`, `failed`, or still building). Python `start_base_build`.
    pub base: fn(&Path) -> Result<Value, ImageWorkspaceError>,
    /// Create and attach the agent's copy (`{"path", "mount"}`). Python `create_workspace`.
    pub create: fn(&Path, &str) -> Result<Value, ImageWorkspaceError>,
    /// Remove a copy that has no owner. Python `remove_image_workspace`.
    pub remove: fn(&str),
    /// Time between base status checks.
    pub poll: Duration,
}

async fn read_record(
    store: &Arc<RuntimeStore>,
    agent_id: &str,
) -> Result<Option<Value>, EventWriteError> {
    let read_id = AgentId::from(agent_id);
    store
        .read_async(
            "runtime-services/image_workspace_agents",
            move |connection| {
                Ok::<_, EventWriteError>(
                    AgentRepository::get(connection, &read_id)?.map(|agent| agent.value()),
                )
            },
        )
        .await
}

async fn fail(
    store: &Arc<RuntimeStore>,
    agent_id: &str,
    failure: Failure,
) -> Result<Completion, EventWriteError> {
    let id = agent_id.to_owned();
    let at = now();
    let settled = store
        .transaction_with(move |tx| settle_failed(tx, &id, &failure, at))
        .await?;
    Ok(settled.map_or(Completion::Superseded, Completion::Failed))
}

/// Finish one read-only agent: wait for its repository's base, create and attach
/// its copy, then record ready (or the Python failure outcome).
pub async fn complete(
    store: &Arc<RuntimeStore>,
    agent_id: &str,
    steps: Steps,
) -> Result<Completion, EventWriteError> {
    let Some(record) = read_record(store, agent_id).await? else {
        return Ok(Completion::Superseded);
    };
    if !read_only_pending(&record) {
        return Ok(Completion::Superseded);
    }
    let Some(repo) = record["imageWorkspaceRepo"]
        .as_str()
        .filter(|repo| !repo.is_empty())
        .map(PathBuf::from)
    else {
        return fail(
            store,
            agent_id,
            Failure::Setup("Image workspace repository is missing".to_owned()),
        )
        .await;
    };
    loop {
        let base_repo = repo.clone();
        let status = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/image_workspace_agents/base",
            move || (steps.base)(&base_repo),
        )
        .await
        .map_err(|_| invalid(agent_id))?;
        match status {
            Err(error) => return fail(store, agent_id, Failure::Base(error.to_string())).await,
            Ok(status) => match status["state"].as_str() {
                Some("ready") => break,
                Some("failed") => {
                    let message = status["error"]
                        .as_str()
                        .filter(|error| !error.is_empty())
                        .unwrap_or("Image workspace base build failed")
                        .to_owned();
                    return fail(store, agent_id, Failure::Base(message)).await;
                }
                _ => {}
            },
        }
        tokio::time::sleep(steps.poll).await;
        match read_record(store, agent_id).await? {
            Some(record) if read_only_pending(&record) => {}
            _ => return Ok(Completion::Superseded),
        }
    }
    let create_repo = repo.clone();
    let create_id = agent_id.to_owned();
    let created = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/image_workspace_agents/create",
        move || (steps.create)(&create_repo, &create_id),
    )
    .await
    .map_err(|_| invalid(agent_id))?;
    let checked = created
        .map_err(|error| error.to_string())
        .and_then(|workspace| {
            let cwd = workspace["path"].as_str().and_then(|path| {
                turn_cwd(Path::new(path), record["imageWorkspaceSubpath"].as_str())
            });
            match cwd {
                Some(cwd) if cwd.is_dir() => Ok(workspace),
                _ => Err("Image workspace folder is missing".to_owned()),
            }
        });
    let workspace = match checked {
        Ok(workspace) => workspace,
        Err(message) => {
            (steps.remove)(agent_id);
            return fail(store, agent_id, Failure::Setup(message)).await;
        }
    };
    let id = agent_id.to_owned();
    let at = now();
    match store
        .transaction_with(move |tx| settle_ready(tx, &id, &workspace, at))
        .await?
    {
        Settled::Ready(value) => Ok(Completion::Ready(value)),
        Settled::AlreadyReady => Ok(Completion::Superseded),
        Settled::Stale => {
            (steps.remove)(agent_id);
            Ok(Completion::Superseded)
        }
    }
}

/// The scheduler wake. A completion writes the ready or failed record and its
/// notice outside any turn, so the scheduler must look again afterwards.
static SCHEDULER_WAKE: OnceLock<Arc<tokio::sync::Notify>> = OnceLock::new();

/// Install the scheduler wake once, before any completion starts.
pub fn install_wake(wake: Arc<tokio::sync::Notify>) {
    if SCHEDULER_WAKE.set(wake).is_err() {
        eprintln!("the image workspace scheduler wake was already installed");
    }
}

fn wake_scheduler() {
    if let Some(wake) = SCHEDULER_WAKE.get() {
        wake.notify_one();
    }
}

fn in_flight() -> &'static Mutex<BTreeSet<String>> {
    static AGENTS: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    AGENTS.get_or_init(Mutex::default)
}

fn production_remove(agent_id: &str) {
    if let Err(error) = crate::image_workspace::remove_workspace(agent_id, true) {
        eprintln!("image workspace cleanup for {agent_id} failed: {error}");
    }
}

/// The production image operations.
pub const PRODUCTION: Steps = Steps {
    base: crate::image_workspace::start_base_build,
    create: crate::image_workspace::create_workspace,
    remove: production_remove,
    poll: BASE_POLL,
};

/// Python `start_image_base` for one agent: finish it in the background, at most
/// one completion per agent at a time.
pub fn start(store: &Arc<RuntimeStore>, agent_id: &str) {
    start_with(store, agent_id, PRODUCTION);
}

/// [`start`] with substitute image operations.
pub fn start_with(store: &Arc<RuntimeStore>, agent_id: &str, steps: Steps) {
    {
        let mut agents = in_flight()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !agents.insert(agent_id.to_owned()) {
            return;
        }
    }
    let store = Arc::clone(store);
    let agent_id = agent_id.to_owned();
    tokio::spawn(async move {
        match complete(&store, &agent_id, steps).await {
            Ok(Completion::Ready(_)) => wake_scheduler(),
            Ok(Completion::Failed(record)) => {
                eprintln!(
                    "image workspace for {agent_id} failed: {}",
                    record["imageWorkspaceError"].as_str().unwrap_or_default()
                );
                wake_scheduler();
            }
            Ok(Completion::Superseded) => {}
            Err(error) => eprintln!("image workspace completion for {agent_id} failed: {error}"),
        }
        in_flight()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&agent_id);
    });
}

/// The agents still in their read-only phase, in id order.
pub fn read_only_agents(db: &Connection) -> Result<Vec<String>, StoreError> {
    let mut statement = db.prepare(
        "SELECT id,record FROM runtime_agents \
         WHERE json_extract(record,'$.imageWorkspacePhase')='read_only' ORDER BY id",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|(_, raw)| {
            serde_json::from_str::<Value>(raw).is_ok_and(|record| read_only_pending(&record))
        })
        .map(|(id, _)| id)
        .collect())
}

/// Python `resume_read_only_image_bases`: restart the completion of every agent
/// that was still in its read-only phase when the backend stopped.
pub async fn resume_read_only(store: &Arc<RuntimeStore>) -> Result<usize, StoreError> {
    resume_read_only_with(store, PRODUCTION).await
}

/// [`resume_read_only`] with substitute image operations.
pub async fn resume_read_only_with(
    store: &Arc<RuntimeStore>,
    steps: Steps,
) -> Result<usize, StoreError> {
    let agents = store
        .read_async("runtime-services/image_workspace_agents/resume", |db| {
            read_only_agents(db)
        })
        .await?;
    for agent in &agents {
        start_with(store, agent, steps);
    }
    Ok(agents.len())
}

#[cfg(test)]
#[path = "../tests/image_workspace_agents/completion.rs"]
mod tests;
