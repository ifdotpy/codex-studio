//! Local managed Spawn and native Review child creation.
//!
//! Python parity sources: `codex_runtime.py::Runtime.spawn_agents` (8666–8862)
//! and `codex_agent_review.py::request` (114–260). Preparation and catalog
//! access happen before the writer; child admission, records, first-turn intents
//! and task assignment commit together. The caller releases returned wake
//! signals only after this transaction commits.

use crate::event_write;
use rusqlite::Transaction;
use serde_json::{Value, json};
use std::path::Path;
use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, RuntimeStore, StoreError};
use thiserror::Error;

const MAX_CHILDREN: usize = 64;
const MAX_NAME_CHARS: usize = 100;
const MAX_PROMPT_CHARS: usize = 32_000;

/// Host account facts and model admission. Production implementations resolve
/// model settings through the retained `AccountSessionPort` broker.
pub(crate) trait ManagedCreationEnvironment {
    fn account_identity(
        &self,
        account_key: &str,
    ) -> Result<crate::host_worker_create::AccountIdentity, crate::managed_host_tools::Error>;
    fn catalog(&self) -> &dyn crate::host_worker_create::CreationCatalog;
    fn prepare_workspace(
        &self,
        actor: &crate::managed_host_tools::Actor,
        args: &Value,
        request_id: &str,
    ) -> Result<
        Box<dyn crate::monitor_runtime::WorkspaceAdmission + Send + Sync>,
        crate::managed_host_tools::Error,
    >;
}

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error(transparent)]
    Managed(#[from] crate::managed_host_tools::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("Host child creation was refused")]
    Refused,
    #[error("Not implemented: requires {0}")]
    Unavailable(&'static str),
}

struct Draft {
    typed: AgentRecord,
    record: Value,
    id: String,
    prompt: String,
    task_id: Option<String>,
    intent: Value,
    worktree: bool,
    worker_base_main_ref: Option<String>,
    worker_base_behind_main: Option<i64>,
    epoch: i64,
    account: crate::host_worker_create::AccountIdentity,
    workspace_admission: Box<dyn crate::monitor_runtime::WorkspaceAdmission + Send + Sync>,
}

/// Create a local host batch using the P02 typed draft and recheck contract.
/// `request_id` is the callback's durable operation identity, so child ids and
/// initial-event ids remain stable across retries.
pub(crate) fn spawn_agents(
    store: &RuntimeStore,
    actor: &crate::managed_host_tools::Actor,
    args: &Value,
    request_id: &str,
    now: f64,
    environment: &dyn ManagedCreationEnvironment,
) -> Result<crate::managed_host_tools::OperationResult, Error> {
    let actor_record = actor.record();
    if actor_record.extra.get("role").and_then(Value::as_str) == Some("reviewer") {
        return Err(
            crate::managed_host_tools::Error::Invalid("Reviewers cannot create agents").into(),
        );
    }
    if actor_record
        .extra
        .get("executionMode")
        .and_then(Value::as_str)
        == Some("vm")
    {
        return Err(Error::Unavailable("layr/worker-creation"));
    }
    if !now.is_finite() || now < 0.0 || request_id.is_empty() {
        return Err(crate::managed_host_tools::Error::Invalid("Invalid spawn identity").into());
    }
    if args
        .get("server")
        .is_some_and(|value| event_write::truthy(Some(value)))
    {
        return Err(Error::Unavailable("multi-server/worker-call"));
    }
    let specs = args
        .get("agents")
        .and_then(Value::as_array)
        .filter(|items| (1..=MAX_CHILDREN).contains(&items.len()))
        .ok_or(crate::managed_host_tools::Error::Invalid(
            "Supply 1 to 64 agents",
        ))?;
    let parent_value = actor_record.value();
    let root_id = actor_record.root_id.0.as_str();
    let root = {
        let read = store.read()?;
        AgentRepository::get(&read, &AgentId::from(root_id))?
            .ok_or(crate::managed_host_tools::Error::Stale)?
    };
    event_write::assert_delegation(&root)
        .map_err(|_| crate::managed_host_tools::Error::Invalid("Delegation is disabled"))?;
    let root_value = root.value();
    let defaults = root_value
        .get("workerDefaults")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let parent_cwd = parent_value.get("cwd").and_then(Value::as_str).ok_or(
        crate::managed_host_tools::Error::Invalid("Select a project directory"),
    )?;
    let parent_account = parent_value
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let mut drafts = Vec::with_capacity(specs.len());
    let mut task_ids = std::collections::BTreeSet::new();

    for (index, spec) in specs.iter().enumerate() {
        if !spec.is_object() {
            return Err(crate::managed_host_tools::Error::Invalid(
                "Every worker needs a name, task and valid role",
            )
            .into());
        }
        let name = spec
            .get("name")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty() && text.trim().chars().count() <= MAX_NAME_CHARS)
            .ok_or(crate::managed_host_tools::Error::Invalid(
                "Every worker needs a name, task and valid role",
            ))?;
        let prompt = spec
            .get("prompt")
            .and_then(Value::as_str)
            .filter(|text| {
                !text.trim().is_empty() && text.trim().chars().count() <= MAX_PROMPT_CHARS
            })
            .ok_or(crate::managed_host_tools::Error::Invalid(
                "Every worker needs a name, task and valid role",
            ))?;
        let role = spec
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("implementer");
        if !matches!(role, "implementer" | "reviewer") {
            return Err(crate::managed_host_tools::Error::Invalid(
                "Every worker needs a name, task and valid role",
            )
            .into());
        }
        if let Some(task) = spec.get("task_id") {
            let task = task.as_str().filter(|value| !value.is_empty()).ok_or(
                crate::managed_host_tools::Error::Invalid("task_id must be a task id"),
            )?;
            if actor_record.extra.get("isLead").and_then(Value::as_bool) != Some(true) {
                return Err(crate::managed_host_tools::Error::Invalid(
                    "Only the lead can delegate an existing task; ask the lead to assign it",
                )
                .into());
            }
            if !task_ids.insert(task.to_owned()) {
                return Err(crate::managed_host_tools::Error::Invalid(
                    "Assign each task to one worker in a batch",
                )
                .into());
            }
        }
        let requested_base_ref = if let Some(base_ref) = spec.get("base_ref") {
            let value = base_ref
                .as_str()
                .filter(|value| !value.trim().is_empty() && value.chars().count() <= 1024)
                .ok_or(crate::managed_host_tools::Error::Invalid(
                    "base_ref must be a branch, tag, or commit",
                ))?;
            Some(value.trim())
        } else {
            None
        };
        if requested_base_ref.is_some()
            && spec.get("workspace").and_then(Value::as_str) == Some("shared")
        {
            return Err(crate::managed_host_tools::Error::Invalid(
                "base_ref cannot be used with workspace shared",
            )
            .into());
        }
        if let Some(base_ref) = requested_base_ref {
            if base_ref.is_empty() || base_ref.chars().count() > 1024 {
                return Err(crate::managed_host_tools::Error::Invalid(
                    "base_ref must be a branch, tag, or commit",
                )
                .into());
            }
        }
        if spec
            .get("server")
            .is_some_and(|value| event_write::truthy(Some(value)))
        {
            return Err(Error::Unavailable("multi-server/worker-call"));
        }
        if spec
            .get("cwd")
            .and_then(Value::as_str)
            .is_some_and(|value| value != parent_cwd)
        {
            return Err(Error::Unavailable(
                "workspace-state/alternate-child-directory",
            ));
        }
        let workspace = spec
            .get("workspace")
            .and_then(Value::as_str)
            .or_else(|| {
                if role == "reviewer" {
                    Some("shared")
                } else {
                    parent_value.get("workspaceMode").and_then(Value::as_str)
                }
            })
            .unwrap_or("worktree");
        let worktree = match workspace {
            "worktree" => true,
            "shared" => false,
            "image" => return Err(Error::Unavailable("workspace-state/image-workspace")),
            "layr" | "linux" => return Err(Error::Unavailable("workspace-state/vm-workspace")),
            _ => {
                return Err(
                    crate::managed_host_tools::Error::Invalid("Unknown workspace mode").into(),
                );
            }
        };
        if !worktree && spec.get("base_ref").is_some() {
            return Err(crate::managed_host_tools::Error::Invalid(
                "base_ref cannot be used with workspace shared",
            )
            .into());
        }
        let id = child_id(request_id, index);
        let account_key = spec
            .get("account")
            .and_then(Value::as_str)
            .or_else(|| defaults.get("accountKey").and_then(Value::as_str))
            .unwrap_or(parent_account);
        if defaults
            .get("accountKey")
            .and_then(Value::as_str)
            .is_some_and(|saved| saved != account_key)
        {
            return Err(crate::managed_host_tools::Error::Invalid(
                "Worker account differs from team defaults",
            )
            .into());
        }
        let account = environment.account_identity(account_key)?;
        let workspace_admission = environment.prepare_workspace(actor, spec, request_id)?;
        let provider = account.provider.as_deref().unwrap_or("codex");
        let model = spec
            .get("model")
            .and_then(Value::as_str)
            .or_else(|| defaults.get("model").and_then(Value::as_str))
            .or_else(|| root_value.get("model").and_then(Value::as_str))
            .unwrap_or(if provider == "claude" {
                "default"
            } else {
                "gpt-6-astra"
            });
        let effort = spec
            .get("effort")
            .and_then(Value::as_str)
            .or_else(|| defaults.get("effort").and_then(Value::as_str));
        let fast_mode = spec
            .get("fast_mode")
            .and_then(Value::as_bool)
            .or_else(|| defaults.get("fastMode").and_then(Value::as_bool))
            .unwrap_or(false);
        let daybreak = spec
            .get("daybreak_enabled")
            .and_then(Value::as_bool)
            .or_else(|| defaults.get("daybreakEnabled").and_then(Value::as_bool))
            .unwrap_or(false);
        let (base_ref, base_commit, main_ref, behind_main) = if worktree {
            crate::host_worker_create::resolve_worker_base(
                Path::new(parent_cwd),
                requested_base_ref,
            )
            .map_err(map_create_error)?
        } else {
            (String::new(), String::new(), None, None)
        };
        let body = json!({"account_key":account_key,"name":name,"model":model,"effort":effort,
            "prompt":prompt,"workspace_explicit":spec.get("workspace").is_some()});
        let project = {
            let read = store.read()?;
            studio_runtime_store::ProjectsRepository::get(&read, parent_cwd)?
                .map(|(_, project)| project)
        };
        let draft = crate::host_worker_create::prepare_work_item(
            &body,
            Path::new(parent_cwd),
            AgentId::from(id.as_str()),
            actor_record.root_id.clone(),
            actor_record.id.clone(),
            &base_ref,
            &base_commit,
            actor_record.epoch,
            parent_cwd,
            parent_account,
            parent_value.get("projectId"),
            parent_value.get("projectServerId"),
            project.as_ref(),
            &account,
            environment.catalog(),
            role,
            worktree,
            fast_mode,
            daybreak,
        )
        .map_err(map_create_error)?;
        let mut record = crate::host_worker_create::persisted_record(&draft)
            .map_err(map_create_error)?
            .value();
        record["provider"] = json!(provider);
        record["workerBaseMainRef"] = json!(main_ref.clone());
        record["workerBaseBehindMain"] = json!(behind_main);
        record["creationIntent"] = spec.clone();
        record["name"] = json!(name.trim());
        record["yoloMode"] = root_value.get("yoloMode").cloned().unwrap_or(json!(true));
        record["maxAgents"] = root_value.get("maxAgents").cloned().unwrap_or(json!(64));
        record["tokenBudget"] = root_value
            .get("tokenBudget")
            .cloned()
            .unwrap_or(Value::Null);
        drafts.push(Draft {
            typed: draft,
            record,
            id,
            prompt: prompt.trim().to_owned(),
            task_id: spec
                .get("task_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            intent: spec.clone(),
            worktree,
            worker_base_main_ref: main_ref,
            worker_base_behind_main: behind_main,
            epoch: actor_record.epoch,
            account,
            workspace_admission,
        });
    }

    store.transaction_with_error(|tx| -> Result<crate::managed_host_tools::OperationResult, crate::managed_host_tools::Error> {
        recheck_actor(tx, actor_record)?;
        let current_root = AgentRepository::get(tx, &actor_record.root_id)?
            .ok_or(crate::managed_host_tools::Error::Stale)?;
        event_write::assert_delegation(&current_root)
            .map_err(|_| crate::managed_host_tools::Error::Invalid("Delegation is disabled"))?;
        let max = current_root.extra.get("maxAgents").and_then(Value::as_i64).unwrap_or(64);
        let existing = drafts.iter().map(|draft| AgentRepository::get(tx, &AgentId::from(draft.id.as_str())))
            .collect::<Result<Vec<_>, _>>()?;
        let new_count = existing.iter().filter(|item| item.is_none()).count() as i64;
        if new_count != 0 && new_count != drafts.len() as i64 {
            return Err(crate::managed_host_tools::Error::Invalid("This request id has different content").into());
        }
        if active_agents(tx, &actor_record.root_id.0)? + new_count > max {
            return Err(crate::managed_host_tools::Error::Invalid("This batch exceeds the active agent limit; no workers were created").into());
        }
        for draft in &drafts {
            draft.workspace_admission.check(tx, actor_record)
                .map_err(|_| crate::managed_host_tools::Error::Unavailable("workspace-state/child-admission"))?;
            crate::host_worker_create::recheck_worker(tx, &draft.typed, draft.epoch, &draft.account)
                .map_err(map_recheck_error)?;
        }
        if !can_delegate(tx, actor_record, &current_root)? {
            return Err(crate::managed_host_tools::Error::Invalid("Delegation depth reached").into());
        }
        let mut children = Vec::with_capacity(drafts.len());
        let mut signals = Vec::new();
        for (draft, saved) in drafts.iter().zip(existing) {
            if let Some(saved) = saved {
                if saved.value().get("creationIntent") != Some(&draft.intent) {
                    return Err(crate::managed_host_tools::Error::Invalid("This request id has different content").into());
                }
                children.push(project_child(&saved.value(), draft.task_id.as_deref(), draft.worktree));
                continue;
            }
            if let Some(task_id) = &draft.task_id {
                crate::work_assignment_runtime::assign_in_transaction(tx, task_id, &draft.id, actor_record, now)
                    .map_err(|_| crate::managed_host_tools::Error::Invalid("Work item assignment was rejected"))?;
            }
            let mut text = draft.prompt.clone();
            if draft.worktree {
                text.push_str(&format!("\n\n[Studio worker base] Commit {} from ref {}.", draft.record["workerBaseCommit"].as_str().unwrap_or_default(), draft.record["workerBaseRef"].as_str().unwrap_or("HEAD")));
            }
            if let Some(task) = &draft.task_id {
                let work = studio_runtime_store::WorkItemsRepository::work_record(tx, task)?
                    .ok_or(crate::managed_host_tools::Error::Invalid("Unknown task_id in this team; no workers were created"))?;
                let title = work.get("title").and_then(Value::as_str).unwrap_or("Assigned task");
                text.push_str(&format!("\n\n[Studio task {task}] {title}\nYou own this task. Submit the result with the work item tools."));
            }
            let record = AgentRecord::from_value(draft.record.clone())
                .map_err(|_| crate::managed_host_tools::Error::Invalid("Invalid child agent record"))?;
            AgentRepository::put(tx, &record)?;
            let mut record_value = draft.record.clone();
            record_value["workerBaseMainRef"] = json!(draft.worker_base_main_ref);
            record_value["workerBaseBehindMain"] = json!(draft.worker_base_behind_main);
            crate::agents_create::publish_agent(tx, &record_value)?;
            let inserted = event_write::enqueue_event(tx, &record, format!("{}:initial", draft.id), "user", text, now)
                .map_err(|_| crate::managed_host_tools::Error::Invalid("Initial turn intent was rejected"))?;
            if !inserted {
                return Err(crate::managed_host_tools::Error::Invalid("This request id has different content").into());
            }
            children.push(project_child(&draft.record, draft.task_id.as_deref(), draft.worktree));
            signals.push(crate::managed_host_tools::WakeSignal::new("worker-driver"));
        }
        Ok(crate::managed_host_tools::OperationResult {
            value: json!({"requestId":request_id,"agents":children,"delivery":"Results wake you automatically. Finish your turn while waiting."}),
            signals,
        })
    }).map_err(Error::from)
}

fn child_id(request_id: &str, index: usize) -> String {
    uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("{request_id}:{index}").as_bytes(),
    )
    .to_string()
}

fn recheck_actor(
    tx: &Transaction<'_>,
    actor: &AgentRecord,
) -> Result<(), crate::managed_host_tools::Error> {
    let current =
        AgentRepository::get(tx, &actor.id)?.ok_or(crate::managed_host_tools::Error::Stale)?;
    if current.epoch != actor.epoch
        || current.root_id != actor.root_id
        || current.extra.get("accountKey") != actor.extra.get("accountKey")
        || current.extra.get("provider") != actor.extra.get("provider")
        || event_write::truthy(current.extra.get("deletedAt"))
        || !event_write::truthy(current.extra.get("autoWake"))
    {
        return Err(crate::managed_host_tools::Error::Stale);
    }
    Ok(())
}

fn active_agents(
    tx: &Transaction<'_>,
    root_id: &str,
) -> Result<i64, crate::managed_host_tools::Error> {
    let mut statement = tx.prepare("SELECT id,record FROM runtime_agents")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut active = 0_i64;
    for row in rows {
        let (id, raw) = row?;
        let value: Value = serde_json::from_str(&raw).map_err(|_| {
            crate::managed_host_tools::Error::InvalidStored {
                table: "runtime_agents",
                id,
                location: "/".to_owned(),
            }
        })?;
        if value.get("rootId").and_then(Value::as_str) != Some(root_id)
            || value
                .get("deletedAt")
                .is_some_and(|deleted| !deleted.is_null())
        {
            continue;
        }
        if !matches!(
            value.get("status").and_then(Value::as_str),
            Some("completed" | "failed" | "interrupted")
        ) && !(value["status"] == "paused" && value["autoWake"] == false)
        {
            active += 1;
        }
    }
    Ok(active)
}

fn can_delegate(
    tx: &Transaction<'_>,
    actor: &AgentRecord,
    root: &AgentRecord,
) -> Result<bool, crate::managed_host_tools::Error> {
    if actor.extra.get("role").and_then(Value::as_str) == Some("reviewer") {
        return Ok(false);
    }
    let limit = root
        .extra
        .get("workerDefaults")
        .and_then(|value| value.get("delegationDepth"))
        .and_then(Value::as_u64)
        .filter(|value| (1..=3).contains(value))
        .unwrap_or(1);
    let mut cursor = actor.clone();
    let mut depth = 0_u64;
    let mut visited = std::collections::BTreeSet::new();
    while cursor.id != root.id {
        if !visited.insert(cursor.id.0.clone()) {
            return Ok(false);
        }
        let Some(parent_id) = cursor.extra.get("parentId").and_then(Value::as_str) else {
            return Ok(false);
        };
        cursor = AgentRepository::get(tx, &AgentId::from(parent_id))?
            .ok_or(crate::managed_host_tools::Error::Stale)?;
        if cursor.root_id != root.id {
            return Ok(false);
        }
        depth += 1;
        if depth >= limit {
            return Ok(false);
        }
    }
    Ok(true)
}

fn project_child(record: &Value, task_id: Option<&str>, worktree: bool) -> Value {
    let mut output = serde_json::Map::new();
    for field in [
        "id",
        "name",
        "status",
        "model",
        "effort",
        "fastMode",
        "accountKey",
        "provider",
        "cwd",
    ] {
        output.insert(
            field.to_owned(),
            record.get(field).cloned().unwrap_or(Value::Null),
        );
    }
    output.insert("worktree".to_owned(), json!(worktree));
    output.insert("environment".to_owned(), json!("host"));
    output.insert(
        "workspace".to_owned(),
        record
            .get("workspaceMode")
            .cloned()
            .unwrap_or_else(|| json!(if worktree { "worktree" } else { "shared" })),
    );
    output.insert("workspaceBackend".to_owned(), Value::Null);
    if let Some(task) = task_id {
        output.insert("taskId".to_owned(), json!(task));
    }
    if worktree {
        output.insert(
            "baseRef".to_owned(),
            record.get("workerBaseRef").cloned().unwrap_or(Value::Null),
        );
        output.insert(
            "baseCommit".to_owned(),
            record
                .get("workerBaseCommit")
                .cloned()
                .unwrap_or(Value::Null),
        );
    }
    Value::Object(output)
}

fn map_create_error(error: crate::agents_create::CreateError) -> Error {
    match error {
        crate::agents_create::CreateError::CapabilityUnavailable(name) => Error::Unavailable(name),
        crate::agents_create::CreateError::Store(error) => Error::Store(error),
        crate::agents_create::CreateError::InvalidCreation(message) => {
            Error::Managed(crate::managed_host_tools::Error::Invalid(message))
        }
        _ => Error::Refused,
    }
}

fn map_recheck_error(
    error: crate::agents_create::MutationError,
) -> crate::managed_host_tools::Error {
    match error {
        crate::agents_create::MutationError::Store(error) => {
            crate::managed_host_tools::Error::Store(error)
        }
        _ => crate::managed_host_tools::Error::Stale,
    }
}

struct ReviewDraft {
    typed: AgentRecord,
    record: Value,
    epoch: i64,
    account: crate::host_worker_create::AccountIdentity,
    workspace_admission: Box<dyn crate::monitor_runtime::WorkspaceAdmission + Send + Sync>,
}

/// Reserve a native Codex review child. `nativeReview` is its durable first-turn
/// intent; the account worker claims it after the callback commits and wakes it.
pub(crate) fn request_review(
    store: &RuntimeStore,
    actor: &crate::managed_host_tools::Actor,
    args: &Value,
    request_id: &str,
    now: f64,
    environment: &dyn ManagedCreationEnvironment,
) -> Result<crate::managed_host_tools::OperationResult, Error> {
    let actor_record = actor.record();
    if actor_record
        .extra
        .get("executionMode")
        .and_then(Value::as_str)
        == Some("vm")
    {
        return Err(Error::Unavailable("layr-review/worker-driver"));
    }
    if actor_record
        .extra
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("codex")
        != "codex"
    {
        return Err(crate::managed_host_tools::Error::Invalid(
            "Native review requires a Codex agent",
        )
        .into());
    }
    if actor_record
        .extra
        .get("nativeReview")
        .is_some_and(|value| event_write::truthy(Some(value)))
    {
        return Err(crate::managed_host_tools::Error::Invalid(
            "A native reviewer cannot create another review",
        )
        .into());
    }
    if event_write::truthy(actor_record.extra.get("daybreakEnabled")) {
        return Err(crate::managed_host_tools::Error::Invalid(
            "Native review cannot select Daybreak. Delegate a review task to a subagent instead",
        )
        .into());
    }
    if !now.is_finite() || now < 0.0 || request_id.is_empty() {
        return Err(crate::managed_host_tools::Error::Invalid("Invalid review identity").into());
    }
    let parent_value = actor_record.value();
    let root = {
        let read = store.read()?;
        AgentRepository::get(&read, &actor_record.root_id)?
            .ok_or(crate::managed_host_tools::Error::Stale)?
    };
    let root_value = root.value();
    let directory = parent_value.get("cwd").and_then(Value::as_str).ok_or(
        crate::managed_host_tools::Error::Invalid("Select a project directory"),
    )?;
    let plan = crate::native_review::ReviewPlan::from_records(
        parent_value.clone(),
        root_value.clone(),
        args.clone(),
        request_id.to_owned(),
        directory.to_owned(),
    )
    .map_err(|_| crate::managed_host_tools::Error::Invalid("Invalid native review request"))?;
    let child_id = plan.spec().get("id").and_then(Value::as_str).ok_or(
        crate::managed_host_tools::Error::Invalid("Invalid native review request"),
    )?;
    if let Some(saved) = {
        let read = store.read()?;
        studio_runtime_store::ContextRepairRepository::agent(&read, child_id)?
    } {
        let value = plan.existing(&saved).map_err(|_| {
            crate::managed_host_tools::Error::Invalid("This request id has different content")
        })?;
        return Ok(crate::managed_host_tools::OperationResult {
            value,
            signals: Vec::new(),
        });
    }
    let account_key = parent_value
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let account = environment.account_identity(account_key)?;
    let workspace_admission = environment.prepare_workspace(actor, args, request_id)?;
    let spec = plan.spec();
    let model = spec.get("model").and_then(Value::as_str).ok_or(
        crate::managed_host_tools::Error::Invalid("Invalid native review model"),
    )?;
    let effort = spec.get("effort").and_then(Value::as_str);
    let fast_mode = spec
        .get("fast_mode")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (base_ref, base_commit, _, _) =
        crate::host_worker_create::resolve_worker_base(Path::new(directory), None)
            .map_err(map_create_error)?;
    let project = {
        let read = store.read()?;
        studio_runtime_store::ProjectsRepository::get(&read, directory)?.map(|(_, project)| project)
    };
    let body = json!({"account_key":account_key,"name":"Review","model":model,
        "effort":effort,"prompt":spec.get("prompt").and_then(Value::as_str).unwrap_or("")});
    let prepared = crate::host_worker_create::prepare_work_item(
        &body,
        Path::new(directory),
        AgentId::from(child_id),
        actor_record.root_id.clone(),
        actor_record.id.clone(),
        &base_ref,
        &base_commit,
        actor_record.epoch,
        directory,
        account_key,
        parent_value.get("projectId"),
        parent_value.get("projectServerId"),
        project.as_ref(),
        &account,
        environment.catalog(),
        "reviewer",
        false,
        fast_mode,
        false,
    )
    .map_err(map_create_error)?;
    let mut record = crate::host_worker_create::persisted_record(&prepared)
        .map_err(map_create_error)?
        .value();
    record["provider"] = json!("codex");
    record["yoloMode"] = json!(false);
    record["worktree"] = json!(false);
    record["workspaceMode"] = json!("shared");
    record["workerBaseRef"] = Value::Null;
    record["workerBaseCommit"] = Value::Null;
    record["creationIntent"] = spec.clone();
    record["maxAgents"] = root_value.get("maxAgents").cloned().unwrap_or(json!(64));
    record["tokenBudget"] = root_value
        .get("tokenBudget")
        .cloned()
        .unwrap_or(Value::Null);
    let review = ReviewDraft {
        typed: prepared,
        record,
        epoch: actor_record.epoch,
        account,
        workspace_admission,
    };
    let value = store
        .transaction_with_error(|tx| -> Result<Value, crate::managed_host_tools::Error> {
            recheck_actor(tx, actor_record)?;
            let creator = ReviewCreator {
                actor: actor_record,
                draft: &review,
            };
            let response =
                crate::native_review::reserve(tx, &plan, &creator).map_err(
                    |error| match error {
                        crate::native_actions::NativeError::Store(error) => {
                            crate::managed_host_tools::Error::Store(error)
                        }
                        crate::native_actions::NativeError::Unavailable(name) => {
                            crate::managed_host_tools::Error::Unavailable(name)
                        }
                        _ => crate::managed_host_tools::Error::Invalid(
                            "Native review creation was refused",
                        ),
                    },
                )?;
            let child = AgentRepository::get(tx, &AgentId::from(child_id))?
                .ok_or(crate::managed_host_tools::Error::Stale)?;
            crate::agents_create::publish_agent(tx, &child.value())?;
            Ok(response)
        })
        .map_err(Error::from)?;
    Ok(crate::managed_host_tools::OperationResult {
        value,
        signals: vec![crate::managed_host_tools::WakeSignal::new("worker-driver")],
    })
}

struct ReviewCreator<'a> {
    actor: &'a AgentRecord,
    draft: &'a ReviewDraft,
}
impl crate::native_review::ReviewCreator for ReviewCreator<'_> {
    fn child(
        &self,
        tx: &Transaction<'_>,
        actor: &Value,
        spec: &Value,
    ) -> crate::native_actions::Result<Value> {
        if actor.get("id") != Some(&Value::String(self.actor.id.0.clone()))
            || spec.get("id") != Some(&Value::String(self.draft.typed.id.0.clone()))
        {
            return Err(crate::native_actions::NativeError::Changed);
        }
        self.draft
            .workspace_admission
            .check(tx, self.actor)
            .map_err(|_| {
                crate::native_actions::NativeError::Unavailable("workspace-state/child-admission")
            })?;
        crate::host_worker_create::recheck_worker(
            tx,
            &self.draft.typed,
            self.draft.epoch,
            &self.draft.account,
        )
        .map_err(|_| crate::native_actions::NativeError::Changed)?;
        Ok(self.draft.record.clone())
    }
    fn receipt(
        &self,
        _: &Transaction<'_>,
        _: &str,
        _: &Value,
    ) -> crate::native_actions::Result<()> {
        Ok(())
    }
}
