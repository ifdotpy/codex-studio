//! Typed host-agent drafts shared by creation and assignment adapters.
//!
//! Drafting is read-only and happens before the caller's transaction. The caller
//! owns durable intent, filesystem effects, and insertion; `recheck` validates
//! the project and parent identities again inside that transaction.
//!
//! Registered Python difference: Rust `str::trim` does not strip U+001C–U+001F,
//! while Python `str.strip()` does. Names and prompts ending or beginning with
//! those characters keep them in Rust.

use crate::agents_create::{CreateError, build_agent_record};
use serde_json::{Value, json};
use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;
use studio_runtime_schema;
use studio_runtime_schema::agent_limits_generated::{
    DEFAULT_MAX_TEAM_AGENTS, DEFAULT_SUBAGENT_CONCURRENCY,
};
use studio_runtime_store::{AgentId, AgentRecord, AgentStatus, ProjectsRepository, Transaction};

const MAX_ACCOUNT_KEY_CHARS: usize = 255;
const COMMIT_HEX_LENGTHS: [usize; 2] = [40, 64];

/// Freeze the parent repository's current commit before the caller opens a writer.
pub fn freeze_base(path: &Path) -> Result<(String, String), CreateError> {
    resolve_worker_base(path, None).map(|(reference, commit, _, _)| (reference, commit))
}

/// Resolve a requested Git ref and the available main-branch lag metadata before
/// entering the writer, matching Python's worker-base selection contract.
pub fn resolve_worker_base(
    path: &Path,
    requested: Option<&str>,
) -> Result<(String, String, Option<String>, Option<i64>), CreateError> {
    let selected = requested.unwrap_or("HEAD").trim();
    if selected.is_empty() || selected.chars().count() > 1024 {
        return Err(CreateError::InvalidCreation("base ref is invalid"));
    }
    let commit = git_value(
        path,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{selected}^{{commit}}"),
        ],
    )?;
    let main_candidates = ["main@{upstream}", "origin/main", "main"];
    let mut main = None;
    for candidate in main_candidates {
        if let Ok(value) = git_value(
            path,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &format!("{candidate}^{{commit}}"),
            ],
        ) {
            main = Some((candidate.to_owned(), value));
            break;
        }
    }
    let behind = main.as_ref().and_then(|(_, target)| {
        if target == &commit {
            return None;
        }
        let count = git_value(
            path,
            &["rev-list", "--count", &format!("{commit}..{target}")],
        )
        .ok()?;
        count.parse::<i64>().ok()
    });
    validate_base(Some(selected), Some(&commit))?;
    Ok((
        selected.to_owned(),
        commit,
        main.map(|(reference, _)| reference),
        behind,
    ))
}

fn git_value(path: &Path, args: &[&str]) -> Result<String, CreateError> {
    let output = crate::bounded_child::run_without_git_environment(crate::bounded_child::Request {
        program: OsStr::new("git"),
        cwd: path.as_os_str(),
        args,
        environment: &[("GIT_TERMINAL_PROMPT", "0"), ("GIT_CONFIG_NOSYSTEM", "1")],
        input: None,
        stdout_limit: 256,
        stderr_limit: 256,
        timeout: Duration::from_secs(10),
    })
    .map_err(|_| CreateError::BaseUnresolved)?;
    let value = String::from_utf8(output.stdout.bytes)
        .map_err(|_| CreateError::InvalidCreation("base commit is invalid"))?
        .trim()
        .to_owned();
    if value.is_empty() {
        return Err(CreateError::InvalidCreation("base commit is invalid"));
    }
    Ok(value)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountIdentity {
    pub provider: studio_runtime_providers::ProviderKind,
    pub home: Option<String>,
    pub account_id: Option<String>,
    pub email: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionSettings {
    pub model: String,
    pub effort: Option<String>,
    pub native_effort: String,
    pub fast_mode: bool,
    pub daybreak_enabled: bool,
    pub cyber_access_program: String,
}

/// The P18 adapter resolves only a fresh, account-bound catalog snapshot.
/// Implementations must not fall back to stale data for admission.
pub trait CreationCatalog: Send + Sync {
    fn resolve(
        &self,
        account_key: &str,
        account: &AccountIdentity,
        model: &str,
        effort: Option<&str>,
        fast_mode: bool,
        daybreak_enabled: bool,
    ) -> Result<ExecutionSettings, CreateError>;
}

/// Prepare two radio lead drafts. No rows or filesystem paths are created here.
/// Explicit model settings require the P18 catalog adapter; simple defaults are
/// available without a provider request.
pub fn prepare(
    body: &Value,
    path: &Path,
    ids: [AgentId; 2],
    accounts: &[AccountIdentity; 2],
    catalog: &dyn CreationCatalog,
) -> Result<[AgentRecord; 2], CreateError> {
    let path = canonical_directory(path)?;
    let name = required_string_value(body, "name")?;
    if name.trim().is_empty() || name.chars().count() > 80 {
        return Err(CreateError::InvalidCreation("chat name is invalid"));
    }
    let participants = body
        .get("participants")
        .and_then(Value::as_array)
        .filter(|participants| participants.len() == 2)
        .ok_or(CreateError::InvalidCreation("participants"))?;
    if ids[0] == ids[1] {
        return Err(CreateError::InvalidCreation("participant ids"));
    }
    let created = crate::event_write::unix_time().map_err(|_| CreateError::Internal)?;
    let mut drafts = Vec::with_capacity(2);
    for index in 0..2 {
        let participant = &participants[index];
        if participant.as_object().is_none_or(|object| {
            object
                .keys()
                .any(|key| !["account_key", "model", "effort"].contains(&key.as_str()))
        }) {
            return Err(CreateError::InvalidCreation("participant"));
        }
        let account_key = required_string_value(participant, "account_key")?;
        let model = required_string_value(participant, "model")?;
        let effort = optional_string(participant, "effort")?;
        if account_key.chars().count() > MAX_ACCOUNT_KEY_CHARS
            || model.chars().count() > MAX_ACCOUNT_KEY_CHARS
        {
            return Err(CreateError::InvalidCreation("account key is invalid"));
        }
        let settings =
            catalog.resolve(account_key, &accounts[index], model, effort, false, false)?;
        let mut value = build_agent_record(
            ids[index].as_ref(),
            &format!("{model} ({})", index + 1),
            "",
            &path,
            account_key,
            accounts[index].provider.key(),
            true,
            "orchestrator",
            true,
            "idle",
            "worktree",
            Some(DEFAULT_SUBAGENT_CONCURRENCY),
            DEFAULT_MAX_TEAM_AGENTS,
            Value::Null,
            true,
            created,
            None,
        );
        value["needsTitle"] = json!(false);
        value["model"] = json!(settings.model);
        value["effort"] = json!(settings.effort);
        value["fastMode"] = json!(settings.fast_mode);
        value["daybreakEnabled"] = json!(settings.daybreak_enabled);
        value["cyberAccessProgram"] = json!(settings.cyber_access_program);
        value["worktree"] = json!(false);
        value["workerBaseRef"] = Value::Null;
        value["workerBaseCommit"] = Value::Null;
        value["_p02CreationIdentity"] = identity_value(&accounts[index]);
        drafts.push(checked_agent(value, &ids[index])?);
    }
    drafts.try_into().map_err(|_| CreateError::Internal)
}

/// Prepare an assigned host worker with the work item's frozen Git identity.
/// The assignment owner inserts the returned record in its own transaction.
#[allow(clippy::too_many_arguments)]
pub fn prepare_work_item(
    body: &Value,
    path: &Path,
    id: AgentId,
    root_id: AgentId,
    parent_id: AgentId,
    base_ref: &str,
    base_commit: &str,
    parent_epoch: i64,
    parent_cwd: &str,
    parent_project_path: &str,
    parent_account_key: &str,
    parent_project_id: Option<&Value>,
    parent_project_server_id: Option<&Value>,
    project_binding: Option<&Value>,
    account: &AccountIdentity,
    catalog: &dyn CreationCatalog,
    role: &str,
    worktree: bool,
    fast_mode: bool,
    daybreak_enabled: bool,
) -> Result<AgentRecord, CreateError> {
    if parent_epoch < 0 {
        return Err(CreateError::InvalidCreation("parent epoch"));
    }
    if worktree {
        validate_base(Some(base_ref), Some(base_commit))?;
    }
    let path = canonical_directory(path)?;
    let account_key = required_string_value(body, "account_key")?;
    let name = required_string_value(body, "name")?.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(CreateError::InvalidCreation("name"));
    }
    let model = required_string_value(body, "model")?;
    if !matches!(role, "implementer" | "reviewer") {
        return Err(CreateError::InvalidCreation("role"));
    }
    let settings = catalog.resolve(
        account_key,
        account,
        model,
        optional_string(body, "effort")?,
        fast_mode,
        daybreak_enabled,
    )?;
    let created = crate::event_write::unix_time().map_err(|_| CreateError::Internal)?;
    let mut draft = build_agent_record(
        id.as_ref(),
        name,
        body.get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim(),
        &path,
        account_key,
        account.provider.key(),
        true,
        role,
        false,
        "queued",
        "worktree",
        None,
        DEFAULT_MAX_TEAM_AGENTS,
        Value::Null,
        false,
        created,
        None,
    );
    draft["rootId"] = json!(root_id.0);
    draft["parentId"] = json!(parent_id.0);
    draft["isLead"] = json!(false);
    draft["role"] = json!(role);
    draft["status"] = json!("queued");
    draft["prompt"] = body
        .get("prompt")
        .filter(|value| value.is_string())
        .and_then(Value::as_str)
        .map(str::trim)
        .map(str::to_owned)
        .map(Value::String)
        .ok_or(CreateError::InvalidCreation("prompt"))?;
    if draft["prompt"]
        .as_str()
        .is_none_or(|prompt| prompt.trim().is_empty() || prompt.chars().count() > 32_000)
    {
        return Err(CreateError::InvalidCreation("prompt"));
    }
    draft["autoWake"] = json!(true);
    if let Some(object) = draft.as_object_mut() {
        object.remove("concurrency");
        object.remove("maxAgentsExplicit");
    }
    // The public create request does not carry trusted repository metadata.
    // Python populates these flags only from its internal `_workspaceInGit`
    // and `_imageWorkspaceHasGit` inputs, which are absent for HTTP creation.
    let workspace_in_git = body
        .get("_workspaceInGit")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let image_workspace_has_git = body
        .get("_imageWorkspaceHasGit")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    draft["workspaceInGit"] = json!(workspace_in_git);
    draft["imageWorkspaceHasGit"] = json!(image_workspace_has_git);
    draft["worktree"] = json!(worktree);
    if !worktree {
        draft["workspaceMode"] = json!("shared");
    }
    draft["workerBaseRef"] = if worktree {
        json!(base_ref)
    } else {
        Value::Null
    };
    draft["workerBaseCommit"] = if worktree {
        json!(base_commit)
    } else {
        Value::Null
    };
    draft["model"] = json!(settings.model);
    draft["effort"] = json!(settings.effort);
    draft["nativeEffort"] = json!(settings.native_effort);
    draft["fastMode"] = json!(settings.fast_mode);
    draft["daybreakEnabled"] = json!(settings.daybreak_enabled);
    draft["workspaceModeExplicit"] =
        json!(body.get("workspace_explicit").and_then(Value::as_bool) == Some(true));
    draft["cyberAccessProgram"] = json!(settings.cyber_access_program);
    draft["_p02CreationIdentity"] = identity_value(account);
    draft["_p02ParentCwd"] = json!(parent_cwd);
    draft["_p02ParentProjectPath"] = json!(parent_project_path);
    draft["_p02ParentAccountKey"] = json!(parent_account_key);
    draft["_p02ParentProjectId"] = parent_project_id.cloned().unwrap_or(Value::Null);
    draft["_p02ParentProjectServerId"] = parent_project_server_id.cloned().unwrap_or(Value::Null);
    draft["_p02ProjectBinding"] = project_binding.cloned().unwrap_or(Value::Null);
    checked_agent(draft, &id)
}

/// Recheck durable project and parent identity inside the caller's transaction.
/// Account selection is checked against the current project binding, not a
/// snapshot taken before the transaction began.
pub fn recheck(
    transaction: &Transaction<'_>,
    drafts: &[AgentRecord; 2],
    accounts: &[AccountIdentity; 2],
) -> Result<(), CreateError> {
    for (index, draft) in drafts.iter().enumerate() {
        recheck_record(transaction, draft)?;
        let frozen = field(draft, "_p02CreationIdentity").ok_or(CreateError::IdentityChanged)?;
        let observed = identity_value(&accounts[index]);
        if frozen != &observed
            || field(draft, "provider").and_then(Value::as_str)
                != Some(accounts[index].provider.key())
        {
            return Err(CreateError::IdentityChanged);
        }
    }
    let cwd = required_string(&drafts[0], "cwd")?;
    if let Some((_, project)) = ProjectsRepository::get(transaction, cwd)?
        && project
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            != field(&drafts[0], "accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
    {
        return Err(CreateError::IdentityChanged);
    }
    if drafts[0].id == drafts[1].id
        || field(&drafts[0], "cwd") != field(&drafts[1], "cwd")
        || field(&drafts[0], "isLead").and_then(Value::as_bool) != Some(true)
        || field(&drafts[1], "isLead").and_then(Value::as_bool) != Some(true)
    {
        return Err(CreateError::IdentityChanged);
    }
    Ok(())
}

/// Recheck one work-item draft; the assignment owner calls this in its insert
/// transaction after it has revalidated its own work-item receipt and owner.
pub fn recheck_worker(
    transaction: &Transaction<'_>,
    draft: &AgentRecord,
    parent_epoch: i64,
    account: &AccountIdentity,
) -> Result<(), CreateError> {
    recheck_record(transaction, draft)?;
    let observed = identity_value(account);
    if field(draft, "_p02CreationIdentity") != Some(&observed)
        || field(draft, "provider").and_then(Value::as_str) != Some(account.provider.key())
    {
        return Err(CreateError::IdentityChanged);
    }
    recheck_parent_epoch(transaction, draft, parent_epoch)?;
    let parent_id = field(draft, "parentId")
        .and_then(Value::as_str)
        .ok_or(CreateError::IdentityChanged)?;
    let parent =
        studio_runtime_store::AgentRepository::get(transaction, &AgentId::from(parent_id))?
            .ok_or(CreateError::IdentityChanged)?;
    let frozen_cwd = field(draft, "_p02ParentCwd");
    let frozen_account = field(draft, "_p02ParentAccountKey");
    if field(&parent, "cwd") != frozen_cwd
        || field(&parent, "accountKey") != frozen_account
        || field(&parent, "projectId").and_then(Value::as_str)
            != field(draft, "_p02ParentProjectId").and_then(Value::as_str)
        || field(&parent, "projectServerId").and_then(Value::as_str)
            != field(draft, "_p02ParentProjectServerId").and_then(Value::as_str)
    {
        return Err(CreateError::IdentityChanged);
    }
    let _guest_cwd = frozen_cwd
        .and_then(Value::as_str)
        .ok_or(CreateError::IdentityChanged)?;
    let project_path = field(draft, "_p02ParentProjectPath")
        .and_then(Value::as_str)
        .ok_or(CreateError::IdentityChanged)?;
    if field(&parent, "executionMode").and_then(Value::as_str) == Some("vm")
        && field(&parent, "hostProjectPath").and_then(Value::as_str) != Some(project_path)
    {
        return Err(CreateError::IdentityChanged);
    }
    let frozen_project = field(draft, "_p02ProjectBinding")
        .filter(|value| !value.is_null())
        .cloned();
    let current_project =
        ProjectsRepository::get(transaction, project_path)?.map(|(_, value)| value);
    if current_project != frozen_project {
        return Err(CreateError::IdentityChanged);
    }
    Ok(())
}

/// Remove P02's transient account observation before storing the checked record.
/// Call only after `recheck` or `recheck_worker` succeeds inside the writer transaction.
pub fn persisted_record(draft: &AgentRecord) -> Result<AgentRecord, CreateError> {
    let mut value = draft.value();
    let fields = value.as_object_mut().ok_or(CreateError::Internal)?;
    fields.retain(|key, _| !key.starts_with("_p02"));
    checked_agent(value, &draft.id)
}

fn recheck_parent_epoch(
    transaction: &Transaction<'_>,
    draft: &AgentRecord,
    expected_epoch: i64,
) -> Result<(), CreateError> {
    let Some(parent_id) = field(draft, "parentId").and_then(Value::as_str) else {
        return Err(CreateError::IdentityChanged);
    };
    let Some(parent) =
        studio_runtime_store::AgentRepository::get(transaction, &AgentId::from(parent_id))?
    else {
        return Err(CreateError::IdentityChanged);
    };
    if expected_epoch < 0
        || parent.epoch != expected_epoch
        || field(&parent, "deletedAt").is_some_and(|value| !value.is_null())
        || field(&parent, "autoWake").and_then(Value::as_bool) == Some(false)
        || parent.root_id != draft.root_id
    {
        return Err(CreateError::IdentityChanged);
    }
    let root = studio_runtime_store::AgentRepository::get(transaction, &draft.root_id)?
        .ok_or(CreateError::IdentityChanged)?;
    if field(&root, "isLead").and_then(Value::as_bool) != Some(true)
        || field(&root, "deletedAt").is_some_and(|value| !value.is_null())
        || field(&root, "autoWake").and_then(Value::as_bool) == Some(false)
        || field(&root, "concurrency").and_then(Value::as_i64) == Some(0)
    {
        return Err(CreateError::IdentityChanged);
    }
    let max_agents = field(&root, "maxAgents")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_MAX_TEAM_AGENTS);
    let active = studio_runtime_store::AgentIndexedReads::active_team_members(
        transaction,
        &draft.root_id.0,
    )?;
    if active >= max_agents {
        return Err(CreateError::InvalidCreation("team active agent limit"));
    }
    Ok(())
}

fn recheck_record(transaction: &Transaction<'_>, draft: &AgentRecord) -> Result<(), CreateError> {
    validate_agent(draft)?;
    if let Some(parent_id) = field(draft, "parentId").and_then(Value::as_str) {
        let Some(parent) =
            studio_runtime_store::AgentRepository::get(transaction, &AgentId::from(parent_id))?
        else {
            return Err(CreateError::IdentityChanged);
        };
        if field(&parent, "deletedAt").is_some_and(|value| !value.is_null())
            || field(&parent, "autoWake").and_then(Value::as_bool) == Some(false)
            || parent.root_id != draft.root_id
        {
            return Err(CreateError::IdentityChanged);
        }
    }
    Ok(())
}

fn checked_agent(value: Value, id: &AgentId) -> Result<AgentRecord, CreateError> {
    if !studio_runtime_schema::validate("PersistedAgentRecord", &value)
        .map_err(|_| CreateError::Internal)?
    {
        return Err(CreateError::InvalidStoredObject {
            table: "runtime_agents",
            id: id.0.clone(),
        });
    }
    let record = AgentRecord::from_value(value).map_err(|_| CreateError::InvalidStoredObject {
        table: "runtime_agents",
        id: id.0.clone(),
    })?;
    if record.id != *id
        || record.status != AgentStatus::Idle && record.status != AgentStatus::Queued
    {
        return Err(CreateError::InvalidStoredObject {
            table: "runtime_agents",
            id: id.0.clone(),
        });
    }
    validate_agent(&record)?;
    Ok(record)
}

fn validate_agent(record: &AgentRecord) -> Result<(), CreateError> {
    if !studio_runtime_schema::validate("PersistedAgentRecord", &record.value())
        .map_err(|_| CreateError::Internal)?
    {
        return Err(CreateError::InvalidStoredObject {
            table: "runtime_agents",
            id: record.id.0.clone(),
        });
    }
    for field in ["accountKey", "provider", "cwd", "workspaceMode", "model"] {
        required_string(record, field)?;
    }
    if record.epoch < 0 {
        return Err(CreateError::InvalidStoredObject {
            table: "runtime_agents",
            id: record.id.0.clone(),
        });
    }
    Ok(())
}

fn field<'a>(record: &'a AgentRecord, key: &str) -> Option<&'a Value> {
    record.extra.get(key)
}

fn required_string<'a>(record: &'a AgentRecord, key: &'static str) -> Result<&'a str, CreateError> {
    field(record, key)
        .and_then(Value::as_str)
        .ok_or(CreateError::InvalidCreation(key))
}

fn required_string_value<'a>(value: &'a Value, key: &'static str) -> Result<&'a str, CreateError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::trim)
        .ok_or(CreateError::InvalidCreation(key))
}

fn identity_value(identity: &AccountIdentity) -> Value {
    json!({
        "provider": identity.provider,
        "home": identity.home,
        "accountId": identity.account_id,
        "email": identity.email,
    })
}

fn optional_string<'a>(
    value: &'a Value,
    key: &'static str,
) -> Result<Option<&'a str>, CreateError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(CreateError::InvalidCreation(key)),
    }
}

fn canonical_directory(path: &Path) -> Result<String, CreateError> {
    crate::agents_create::canonical_project_directory(path)
        .map_err(|_| CreateError::InvalidCreation("path"))
}

fn validate_base(base_ref: Option<&str>, base_commit: Option<&str>) -> Result<(), CreateError> {
    match (base_ref, base_commit) {
        (None, None) => Ok(()),
        (Some(reference), Some(commit))
            if !reference.trim().is_empty()
                && reference.chars().count() <= 255
                && COMMIT_HEX_LENGTHS.contains(&commit.len())
                && commit.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
        {
            Ok(())
        }
        _ => Err(CreateError::InvalidCreation("frozen base")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::tempdir;

    const IDS: [&str; 2] = [
        "00000000-0000-4000-8000-000000000001",
        "00000000-0000-4000-8000-000000000002",
    ];

    struct Catalog;

    impl CreationCatalog for Catalog {
        fn resolve(
            &self,
            _account_key: &str,
            _account: &AccountIdentity,
            model: &str,
            effort: Option<&str>,
            fast_mode: bool,
            daybreak_enabled: bool,
        ) -> Result<ExecutionSettings, CreateError> {
            Ok(ExecutionSettings {
                model: model.to_owned(),
                effort: Some(effort.unwrap_or("medium").to_owned()),
                native_effort: effort.unwrap_or("medium").to_owned(),
                fast_mode,
                daybreak_enabled,
                cyber_access_program: "standard".to_owned(),
            })
        }
    }

    fn accounts() -> [AccountIdentity; 2] {
        [
            AccountIdentity {
                provider: studio_runtime_providers::ProviderKind::Codex,
                home: Some("/home/one".to_owned()),
                account_id: Some("account-one".to_owned()),
                email: Some("one@example.test".to_owned()),
            },
            AccountIdentity {
                provider: studio_runtime_providers::ProviderKind::Codex,
                home: Some("/home/two".to_owned()),
                account_id: Some("account-two".to_owned()),
                email: Some("two@example.test".to_owned()),
            },
        ]
    }

    fn body() -> Value {
        json!({"name":"Pair","participants":[
            {"account_key":"one","model":"model-one"},
            {"account_key":"two","model":"model-two","effort":"high"}
        ]})
    }

    struct WorkerDraftFixture {
        database: Connection,
        draft: AgentRecord,
        account: AccountIdentity,
        parent_id: AgentId,
        cwd: String,
    }

    fn worker_draft_fixture(name: &str, prompt: &str) -> WorkerDraftFixture {
        worker_draft_fixture_with_options(name, prompt, "implementer", true, false, false)
    }

    fn worker_draft_fixture_with_options(
        name: &str,
        prompt: &str,
        role: &str,
        worktree: bool,
        fast_mode: bool,
        daybreak_enabled: bool,
    ) -> WorkerDraftFixture {
        let directory = tempdir().expect("temporary worker project");
        let cwd = directory.path().canonicalize().expect("canonical project");
        let cwd = cwd.to_str().expect("UTF-8 test path").to_owned();
        let parent_id = AgentId::from(IDS[0]);
        let child_id = AgentId::from(IDS[1]);
        let account = accounts()[0].clone();
        let database = Connection::open_in_memory().expect("open worker database");
        database
            .execute_batch(
                "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT NOT NULL);",
            )
            .expect("worker tables");
        let mut parent = build_agent_record(
            parent_id.as_ref(),
            "Parent",
            "",
            &cwd,
            "parent-account",
            "codex",
            true,
            "orchestrator",
            true,
            "idle",
            "worktree",
            Some(DEFAULT_SUBAGENT_CONCURRENCY),
            DEFAULT_MAX_TEAM_AGENTS,
            Value::Null,
            true,
            1.0,
            None,
        );
        parent["autoWake"] = json!(true);
        parent["maxAgents"] = json!(DEFAULT_MAX_TEAM_AGENTS);
        database
            .execute(
                "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                rusqlite::params![parent_id.0, parent.to_string()],
            )
            .expect("save parent");
        let project = json!({
            "id":cwd,"path":cwd,"accountKey":"parent-account",
            "projectId":"project-one","projectServerId":"server-one"
        });
        database
            .execute(
                "INSERT INTO runtime_projects(id,record) VALUES(?1,?2)",
                rusqlite::params![cwd, project.to_string()],
            )
            .expect("save project binding");
        let body = json!({
            "account_key":"worker-account","name":name,
            "model":"model-one","prompt":prompt
        });
        let draft = prepare_work_item(
            &body,
            Path::new(&cwd),
            child_id,
            parent_id.clone(),
            parent_id.clone(),
            "HEAD",
            &"a".repeat(40),
            0,
            &cwd,
            &cwd,
            "parent-account",
            None,
            None,
            Some(&project),
            &account,
            &Catalog,
            role,
            worktree,
            fast_mode,
            daybreak_enabled,
        )
        .expect("typed worker draft");
        WorkerDraftFixture {
            database,
            draft,
            account,
            parent_id,
            cwd,
        }
    }

    #[test]
    fn work_item_options_set_role_workspace_and_execution_settings() {
        let fixture = worker_draft_fixture_with_options(
            "Review",
            "Inspect the change",
            "reviewer",
            false,
            true,
            false,
        );
        assert_eq!(
            field(&fixture.draft, "role").and_then(Value::as_str),
            Some("reviewer")
        );
        assert_eq!(
            field(&fixture.draft, "worktree").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            field(&fixture.draft, "workspaceMode").and_then(Value::as_str),
            Some("shared")
        );
        assert_eq!(field(&fixture.draft, "workerBaseRef"), Some(&Value::Null));
        assert_eq!(
            field(&fixture.draft, "fastMode").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            field(&fixture.draft, "daybreakEnabled").and_then(Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn review_target_directory_can_differ_from_parent_workspace() {
        let mut fixture = worker_draft_fixture("Review", "Inspect the change");
        let target = tempfile::tempdir().expect("target repository directory");
        let project = ProjectsRepository::get(&fixture.database, &fixture.cwd)
            .expect("parent project lookup")
            .expect("parent project exists")
            .1;
        let body = json!({"account_key":"worker-account","name":"Review",
            "model":"model-one","prompt":"Inspect the change"});
        let canonical_target = target
            .path()
            .canonicalize()
            .expect("canonical review target directory");
        let draft = prepare_work_item(
            &body,
            target.path(),
            AgentId::from("review-child"),
            fixture.parent_id.clone(),
            fixture.parent_id.clone(),
            "",
            "",
            0,
            &fixture.cwd,
            &fixture.cwd,
            "parent-account",
            None,
            None,
            Some(&project),
            &fixture.account,
            &Catalog,
            "reviewer",
            false,
            false,
            false,
        )
        .expect("review uses its validated target directory");
        assert_eq!(
            field(&draft, "cwd").and_then(Value::as_str),
            canonical_target.to_str()
        );
        assert_eq!(
            field(&draft, "_p02ParentCwd").and_then(Value::as_str),
            Some(fixture.cwd.as_str())
        );
        let tx = fixture.database.transaction().expect("recheck transaction");
        recheck_worker(&tx, &draft, 0, &fixture.account)
            .expect("parent identity is rechecked against the stored parent cwd");
    }

    #[test]
    fn vm_worker_rechecks_guest_cwd_and_host_project_path_separately() {
        let mut fixture = worker_draft_fixture("Implement", "Inspect the project");
        let host_project = tempdir().expect("host project directory");
        let host_path = host_project
            .path()
            .canonicalize()
            .expect("canonical host project")
            .to_string_lossy()
            .into_owned();
        let guest_cwd = "/vm/projects/example";
        let mut parent: Value = fixture
            .database
            .query_row(
                "SELECT record FROM runtime_agents WHERE id=?1",
                [&fixture.parent_id.0],
                |row| row.get::<_, String>(0),
            )
            .map(|raw| serde_json::from_str(&raw).expect("parent JSON"))
            .expect("parent row");
        parent["cwd"] = json!(guest_cwd);
        parent["hostProjectPath"] = json!(host_path);
        parent["executionMode"] = json!("vm");
        fixture
            .database
            .execute(
                "UPDATE runtime_agents SET record=?1 WHERE id=?2",
                rusqlite::params![parent.to_string(), fixture.parent_id.0],
            )
            .expect("store VM parent identity");

        let mut project = ProjectsRepository::get(&fixture.database, &fixture.cwd)
            .expect("original project lookup")
            .expect("original project exists")
            .1;
        fixture
            .database
            .execute("DELETE FROM runtime_projects WHERE id=?1", [&fixture.cwd])
            .expect("move project fixture");
        project["id"] = json!(host_path);
        project["path"] = json!(host_path);
        fixture
            .database
            .execute(
                "INSERT INTO runtime_projects(id,record) VALUES(?1,?2)",
                rusqlite::params![host_path, project.to_string()],
            )
            .expect("save host-side project identity");

        let body = json!({"account_key":"worker-account","name":"VM worker",
            "model":"model-one","prompt":"Inspect the project"});
        let draft = prepare_work_item(
            &body,
            Path::new(&host_path),
            AgentId::from("vm-child"),
            fixture.parent_id.clone(),
            fixture.parent_id.clone(),
            "HEAD",
            &"a".repeat(40),
            0,
            guest_cwd,
            &host_path,
            "parent-account",
            None,
            None,
            Some(&project),
            &fixture.account,
            &Catalog,
            "implementer",
            true,
            false,
            false,
        )
        .expect("prepare VM worker draft");
        assert_eq!(
            field(&draft, "_p02ParentCwd").and_then(Value::as_str),
            Some(guest_cwd)
        );
        assert_eq!(
            field(&draft, "_p02ParentProjectPath").and_then(Value::as_str),
            Some(host_path.as_str())
        );
        let tx = fixture.database.transaction().expect("recheck transaction");
        recheck_worker(&tx, &draft, 0, &fixture.account)
            .expect("guest parent identity and host project binding remain independently stable");
        drop(tx);

        for (field, changed) in [
            ("cwd", "/vm/projects/replaced"),
            ("hostProjectPath", "/host/projects/replaced"),
        ] {
            let mut changed_parent: Value = fixture
                .database
                .query_row(
                    "SELECT record FROM runtime_agents WHERE id=?1",
                    [&fixture.parent_id.0],
                    |row| row.get::<_, String>(0),
                )
                .map(|raw| serde_json::from_str(&raw).expect("parent JSON"))
                .expect("parent row");
            changed_parent[field] = json!(changed);
            fixture
                .database
                .execute(
                    "UPDATE runtime_agents SET record=?1 WHERE id=?2",
                    rusqlite::params![changed_parent.to_string(), fixture.parent_id.0],
                )
                .expect("change parent identity");
            let tx = fixture.database.transaction().expect("identity recheck");
            assert!(matches!(
                recheck_worker(&tx, &draft, 0, &fixture.account),
                Err(CreateError::IdentityChanged)
            ));
            drop(tx);
            changed_parent[field] = if field == "cwd" {
                json!(guest_cwd)
            } else {
                json!(host_path)
            };
            fixture
                .database
                .execute(
                    "UPDATE runtime_agents SET record=?1 WHERE id=?2",
                    rusqlite::params![changed_parent.to_string(), fixture.parent_id.0],
                )
                .expect("restore parent identity");
        }
    }

    #[test]
    fn rust_trim_keeps_python_strip_only_edge_controls() {
        let value = "\u{1c}edge\u{1c}";
        let fixture = worker_draft_fixture(value, value);
        assert_eq!(
            field(&fixture.draft, "name").and_then(Value::as_str),
            Some(value)
        );
        assert_eq!(
            field(&fixture.draft, "prompt").and_then(Value::as_str),
            Some(value)
        );
    }

    fn assert_worker_recheck_refuses_without_writes(fixture: &mut WorkerDraftFixture) {
        let tx = fixture.database.transaction().expect("transaction");
        assert!(matches!(
            recheck_worker(&tx, &fixture.draft, 0, &fixture.account),
            Err(CreateError::IdentityChanged)
        ));
        drop(tx);
        let counts: (i64, i64) = fixture
            .database
            .query_row(
                "SELECT (SELECT COUNT(*) FROM runtime_agents),
                        (SELECT COUNT(*) FROM runtime_projects)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("count state after refused recheck");
        assert_eq!(counts, (1, 1));
    }

    #[test]
    fn radio_creation_builds_checked_typed_drafts_with_a_frozen_base() {
        let directory = tempdir().expect("temporary project directory");
        let ids = [AgentId::from(IDS[0]), AgentId::from(IDS[1])];
        let accounts = accounts();
        let drafts =
            prepare(&body(), directory.path(), ids, &accounts, &Catalog).expect("checked drafts");
        assert!(field(&drafts[0], "_p02CreationIdentity").is_some());
        let persisted = persisted_record(&drafts[0]).expect("persistable typed record");
        assert!(field(&persisted, "_p02CreationIdentity").is_none());
        for (draft, id) in drafts.iter().zip(IDS) {
            assert_eq!(draft.id.0, id);
            assert_eq!(draft.root_id.0, id);
            assert_eq!(draft.epoch, 0);
            assert_eq!(draft.status, AgentStatus::Idle);
            assert_eq!(field(draft, "parentId"), Some(&Value::Null));
            assert_eq!(
                field(draft, "accountKey").and_then(Value::as_str),
                Some(if id == IDS[0] { "one" } else { "two" })
            );
            assert_eq!(
                field(draft, "cwd").and_then(Value::as_str),
                directory
                    .path()
                    .canonicalize()
                    .expect("canonical temporary directory")
                    .to_str()
            );
            assert_eq!(field(draft, "workerBaseRef"), Some(&Value::Null));
            assert_eq!(
                field(draft, "model").and_then(Value::as_str),
                Some(if id == IDS[0] {
                    "model-one"
                } else {
                    "model-two"
                })
            );
            assert_eq!(
                field(draft, "needsTitle").and_then(Value::as_bool),
                Some(false)
            );
        }
    }

    #[test]
    fn malformed_participant_is_refused_before_any_store_access() {
        let directory = tempdir().expect("temporary project directory");
        let result = prepare(
            &json!({"name":"Pair","participants":[{"account_key":"one","model":"m"}]}),
            directory.path(),
            [AgentId::from(IDS[0]), AgentId::from(IDS[1])],
            &accounts(),
            &Catalog,
        );
        assert!(matches!(
            result,
            Err(CreateError::InvalidCreation("participants"))
        ));
    }

    #[test]
    fn changed_account_observation_is_refused_before_project_lookup() {
        let directory = tempdir().expect("temporary project directory");
        let observed = accounts();
        let drafts = prepare(
            &body(),
            directory.path(),
            [AgentId::from(IDS[0]), AgentId::from(IDS[1])],
            &observed,
            &Catalog,
        )
        .expect("checked drafts");
        let mut changed = observed.clone();
        changed[1].email = Some("new@example.test".to_owned());
        let database = Connection::open_in_memory().expect("open transaction database");
        database
            .execute_batch(
                "CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT NOT NULL);",
            )
            .expect("project table");
        let tx = database.unchecked_transaction().expect("begin transaction");
        let result = recheck(&tx, &drafts, &changed);
        assert!(matches!(result, Err(CreateError::IdentityChanged)));
        assert!(recheck(&tx, &drafts, &observed).is_ok());
    }

    #[test]
    fn project_account_is_rechecked_inside_the_transaction_without_writes_on_change() {
        let project = tempdir().expect("temporary project directory");
        let mut database = Connection::open_in_memory().expect("open transaction database");
        database
            .execute_batch(
                "CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT NOT NULL);",
            )
            .expect("project table");
        let accounts = accounts();
        let drafts = prepare(
            &body(),
            project.path(),
            [AgentId::from(IDS[0]), AgentId::from(IDS[1])],
            &accounts,
            &Catalog,
        )
        .expect("checked drafts");
        let path = field(&drafts[0], "cwd")
            .and_then(Value::as_str)
            .expect("draft cwd");
        database
            .execute(
                "INSERT INTO runtime_projects(id,record) VALUES(?1,?2)",
                rusqlite::params![
                    path,
                    json!({"id":path,"path":path,"accountKey":"other"}).to_string()
                ],
            )
            .expect("seed changed project binding");
        let before = database
            .query_row(
                "SELECT record FROM runtime_projects WHERE id=?1",
                [path],
                |row| row.get::<_, String>(0),
            )
            .expect("saved project");
        let tx = database.transaction().expect("begin recheck transaction");
        let result = recheck(&tx, &drafts, &accounts);
        assert!(matches!(result, Err(CreateError::IdentityChanged)));
        drop(tx);
        let after = database
            .query_row(
                "SELECT record FROM runtime_projects WHERE id=?1",
                [path],
                |row| row.get::<_, String>(0),
            )
            .expect("saved project after recheck");
        assert_eq!(before, after);
    }

    #[test]
    fn worker_recheck_refuses_parent_cwd_changes() {
        let mut fixture = worker_draft_fixture("Named worker", "Do the task");
        fixture
            .database
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.cwd','/stale') WHERE id=?1",
                [&fixture.parent_id.0],
            )
            .expect("change parent cwd");
        assert_worker_recheck_refuses_without_writes(&mut fixture);
    }

    #[test]
    fn worker_recheck_refuses_parent_account_changes() {
        let mut fixture = worker_draft_fixture("Named worker", "Do the task");
        fixture
            .database
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.accountKey','changed-account') WHERE id=?1",
                [&fixture.parent_id.0],
            )
            .expect("change parent account");
        assert_worker_recheck_refuses_without_writes(&mut fixture);
    }

    #[test]
    fn worker_recheck_refuses_parent_project_binding_changes() {
        let mut fixture = worker_draft_fixture("Named worker", "Do the task");
        fixture
            .database
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.projectId','changed-project') WHERE id=?1",
                [&fixture.parent_id.0],
            )
            .expect("change parent project binding");
        assert_worker_recheck_refuses_without_writes(&mut fixture);
    }

    #[test]
    fn worker_recheck_refuses_parent_project_server_changes() {
        let mut fixture = worker_draft_fixture("Named worker", "Do the task");
        fixture
            .database
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.projectServerId','changed-server') WHERE id=?1",
                [&fixture.parent_id.0],
            )
            .expect("change parent project server");
        assert_worker_recheck_refuses_without_writes(&mut fixture);
    }

    #[test]
    fn worker_recheck_refuses_project_binding_changes() {
        let mut fixture = worker_draft_fixture("Named worker", "Do the task");
        fixture
            .database
            .execute(
                "UPDATE runtime_projects SET record=json_set(record,'$.projectId','changed-project') WHERE id=?1",
                [&fixture.cwd],
            )
            .expect("change project binding");
        assert_worker_recheck_refuses_without_writes(&mut fixture);
    }

    #[test]
    fn frozen_base_reads_head_from_a_disposable_repository_before_the_write() {
        let directory = tempdir().expect("temporary repository");
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(directory.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .expect("git process")
        };
        assert!(run(&["init", "--quiet"]).status.success());
        assert!(run(&["config", "user.name", "P02 test"]).status.success());
        assert!(
            run(&["config", "user.email", "p02@example.test"])
                .status
                .success()
        );
        std::fs::write(directory.path().join("README"), "frozen base\n").expect("repository file");
        assert!(run(&["add", "README"]).status.success());
        assert!(run(&["commit", "--quiet", "-m", "base"]).status.success());

        let (reference, commit) = freeze_base(directory.path()).expect("frozen HEAD");
        assert_eq!(reference, "HEAD");
        assert!(COMMIT_HEX_LENGTHS.contains(&commit.len()));
        assert!(commit.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}
