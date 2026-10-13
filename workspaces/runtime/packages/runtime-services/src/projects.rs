//! Decisions for local project records and sidebar state.

use crate::event_write::{EventWriteError, python_json, python_strip, read_receipt, save_receipt};
use crate::host_project_runtime::PreparedHost;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use studio_runtime_store::{
    ProjectsRepository, RuntimeStore, StoreError, SyncDocumentsRepository, sha256_hex,
};
use thiserror::Error;
use uuid::Uuid;

pub(crate) mod operations;
pub use operations::peer_team_write;
use operations::reorder;

#[derive(Debug, Error)]
pub enum ProjectsError {
    #[error("invalid request: {0}")]
    Invalid(&'static str),
    #[error("conflict: {0}")]
    Conflict(&'static str),
    #[error("not implemented: requires {0}")]
    Requires(&'static str),
    #[error("invalid stored {table} record {id}")]
    InvalidStored { table: &'static str, id: String },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

pub fn list(store: &RuntimeStore) -> Result<Value, ProjectsError> {
    let connection = store.read()?;
    let rows = ProjectsRepository::list(&connection)?;
    let mut items = Vec::with_capacity(rows.len());
    for (id, record) in rows {
        check_id(&record, "runtime_projects", &id)?;
        items.push(record);
    }
    Ok(json!({"items": items}))
}

pub fn write_dependency(body: &Value) -> Option<&'static str> {
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("register");
    if action == "register"
        && body.get("server").is_some_and(|server| {
            !server.is_null() && server.as_str().is_some_and(|value| !value.is_empty())
        })
    {
        Some("cross-server-driver/project-registration")
    } else if matches!(action, "add_location" | "remove_location") {
        Some("cross-server-driver/project-location-write")
    } else if action == "set_worker_environment"
        && body.get("environment").and_then(Value::as_str) == Some("linux")
    {
        Some("workspace-state/worker-environment")
    } else {
        None
    }
}

pub fn write(
    store: &RuntimeStore,
    body: &Value,
    default_account: &str,
    known_accounts: &[Value],
) -> Result<Value, ProjectsError> {
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("register");
    if let Some(dependency) = write_dependency(body) {
        return Err(ProjectsError::Requires(dependency));
    }
    if action == "set_accounts" {
        return set_accounts(store, body, default_account, known_accounts);
    }
    if action == "set_worker_environment" {
        return set_worker_environment(store, body, default_account, known_accounts);
    }
    if action == "reorder" {
        return reorder(store, body);
    }
    if matches!(
        action,
        "rename" | "add_folder" | "rename_folder" | "remove_folder"
    ) {
        return organize(store, body, default_account, known_accounts);
    }
    if !matches!(
        action,
        "register" | "remove" | "set_account" | "set_worker_base"
    ) {
        return Err(ProjectsError::Invalid("unknown project action"));
    }
    let path = project_key(body.get("path"), action == "register")?;
    let revision = body.get("expected_revision").and_then(Value::as_u64);
    let account = body.get("account_key").and_then(Value::as_str);
    let base = body
        .get("base_ref")
        .and_then(Value::as_str)
        .map(python_strip)
        .filter(|s| !s.is_empty());
    if action == "set_account"
        && !known_accounts
            .iter()
            .any(|row| row.get("id").and_then(Value::as_str) == account)
    {
        return Err(ProjectsError::Invalid("select an existing account"));
    }
    let prepared = PreparedHost::load(store)?;
    prepared.recheck_paths()?;
    store.transaction_with(|tx| {
        prepared.recheck(tx)?;
        let mut project = load_project(tx, &path)?;
        if path.starts_with("project:") && project.is_none() {
            return Err(ProjectsError::Invalid("select an existing project"));
        }
        if action == "remove" {
            let removed = ProjectsRepository::remove(tx, &path)?;
            return Ok(json!({"id":path,"removed":removed}));
        }
        if action == "register"
            && let Some(project) = project
        {
            return Ok(project);
        }
        let account_revision = revision_of(project.as_ref(), "accountRevision", &path)?;
        let base_revision = revision_of(project.as_ref(), "workerBaseRevision", &path)?;
        if action == "set_account" {
            if let Some(project) = &project
                && project.get("accountKey").and_then(Value::as_str) == account
                && revision.is_some_and(|value| {
                    value == account_revision || value.saturating_add(1) == account_revision
                })
            {
                return Ok(project.clone());
            }
            if revision != Some(account_revision) {
                return Err(ProjectsError::Conflict(
                    "Project account changed. Reload it before saving",
                ));
            }
            if known_accounts.iter().any(|row| {
                row.get("id").and_then(Value::as_str) == account
                    && row.get("disconnected").and_then(Value::as_bool) == Some(true)
            }) {
                return Err(ProjectsError::Invalid(
                    "reconnect this account before selecting it",
                ));
            }
        }
        if action == "set_worker_base" {
            if project
                .as_ref()
                .is_some_and(|row| row.get("workerBaseRef").and_then(Value::as_str) == base)
                && revision.is_some_and(|value| {
                    value == base_revision || value.saturating_add(1) == base_revision
                })
            {
                return Ok(project.clone().unwrap_or(Value::Null));
            }
            if revision != Some(base_revision) {
                return Err(ProjectsError::Conflict(
                    "Worker base changed. Reload it before saving",
                ));
            }
        }
        if project.is_none() {
            let name = project_name(body.get("name"), &path)?;
            let chosen = account.map(str::to_owned).unwrap_or(project_account(
                &prepared,
                tx,
                &path,
                default_account,
            )?);
            project = Some(new_project(
                &prepared,
                &path,
                &name,
                &chosen,
                known_accounts,
            )?);
        }
        let project = project
            .as_mut()
            .ok_or(ProjectsError::Invalid("project is unavailable"))?;
        if action == "set_worker_base" {
            project["workerBaseRef"] = base.map_or(Value::Null, |value| json!(value));
            project["workerBaseRevision"] = json!(base_revision + 1);
        } else {
            let selected = account
                .or_else(|| project["accountKey"].as_str())
                .unwrap_or(default_account)
                .to_owned();
            project["accountKey"] = json!(selected);
            project["accountRevision"] = json!(account_revision + 1);
            if let Some(keys) = project
                .get("accountKeys")
                .and_then(Value::as_array)
                .filter(|v| !v.is_empty())
            {
                let mut keys = keys
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                if !keys.iter().any(|key| key == &selected) {
                    keys.push(selected.to_owned());
                    keys.sort();
                }
                project["accountKeys"] = json!(keys);
            }
        }
        ProjectsRepository::put_project(tx, project)?;
        Ok(project.clone())
    })
}

fn check_id(value: &Value, table: &'static str, id: &str) -> Result<(), ProjectsError> {
    studio_runtime_store::system_reads::validate(table, id, value.clone())?;
    if value.get("id").and_then(Value::as_str) != Some(id) {
        return Err(ProjectsError::InvalidStored {
            table,
            id: id.to_owned(),
        });
    }
    Ok(())
}

fn folder_fold(value: &str) -> String {
    value.to_lowercase()
}

fn load_project(
    tx: &studio_runtime_store::Transaction<'_>,
    id: &str,
) -> Result<Option<Value>, ProjectsError> {
    ProjectsRepository::get(tx, id)?
        .map(|(key, value)| {
            check_id(&value, "runtime_projects", &key)?;
            Ok(value)
        })
        .transpose()
}

fn revision_of(project: Option<&Value>, field: &str, id: &str) -> Result<u64, ProjectsError> {
    match project.and_then(|value| value.get(field)) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => value.as_u64().ok_or_else(|| ProjectsError::InvalidStored {
            table: "runtime_projects",
            id: id.to_owned(),
        }),
    }
}

fn project_name(value: Option<&Value>, path: &str) -> Result<String, ProjectsError> {
    let Some(value) = value else {
        return Ok(Path::new(path)
            .file_name()
            .and_then(|part| part.to_str())
            .filter(|part| !part.is_empty())
            .unwrap_or(path)
            .to_owned());
    };
    let name = value
        .as_str()
        .ok_or(ProjectsError::Invalid("project name is invalid"))?;
    let name = python_strip(name);
    if name.is_empty() || name.chars().count() > 255 {
        return Err(ProjectsError::Invalid("project name is invalid"));
    }
    Ok(name.to_owned())
}

fn new_project(
    prepared: &PreparedHost,
    path: &str,
    name: &str,
    account: &str,
    accounts: &[Value],
) -> Result<Value, ProjectsError> {
    if !accounts
        .iter()
        .any(|row| row.get("id").and_then(Value::as_str) == Some(account))
    {
        return Err(ProjectsError::Invalid("select an existing account"));
    }
    let server = prepared.server()?;
    Ok(
        json!({"id":path,"path":path,"name":name,"created":now_seconds(),"accountKey":account,"accountRevision":1,
        "homeServerId":server,"locations":[{"serverId":server,"path":path,"projectId":path}],"locationsRevision":0}),
    )
}

fn project_account(
    prepared: &PreparedHost,
    tx: &studio_runtime_store::Transaction<'_>,
    path: &str,
    fallback: &str,
) -> Result<String, ProjectsError> {
    if path.starts_with("project:") {
        let project =
            load_project(tx, path)?.ok_or(ProjectsError::Invalid("select an existing project"))?;
        return Ok(project
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or(fallback)
            .to_owned());
    }
    let target = Path::new(path);
    let mut best: Option<(usize, String)> = None;
    for (id, project) in ProjectsRepository::list(tx)? {
        check_id(&project, "runtime_projects", &id)?;
        let Some(candidate) = project
            .get("path")
            .and_then(Value::as_str)
            .filter(|value| !value.starts_with("project:"))
        else {
            continue;
        };
        let candidate = prepared.resolve(candidate)?;
        if target.starts_with(candidate) {
            let depth = Path::new(&candidate).components().count();
            if best.as_ref().is_none_or(|(current, _)| depth > *current)
                && let Some(account) = project.get("accountKey").and_then(Value::as_str)
            {
                best = Some((depth, account.to_owned()));
            }
        }
    }
    Ok(best
        .map(|(_, account)| account)
        .unwrap_or_else(|| fallback.to_owned()))
}

fn set_accounts(
    store: &RuntimeStore,
    body: &Value,
    _default_account: &str,
    known_accounts: &[Value],
) -> Result<Value, ProjectsError> {
    let path = project_key(body.get("path"), true)?;
    let keys_value = body
        .get("account_keys")
        .and_then(Value::as_array)
        .filter(|keys| !keys.is_empty())
        .ok_or(ProjectsError::Invalid(
            "select at least one project account",
        ))?;
    let mut keys = keys_value
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    keys.sort();
    keys.dedup();
    let default = body
        .get("account_key")
        .and_then(Value::as_str)
        .ok_or(ProjectsError::Invalid("select a default project account"))?;
    if !keys.iter().any(|key| key == default) {
        return Err(ProjectsError::Invalid(
            "the default must be a selected project account",
        ));
    }
    let selected = keys
        .iter()
        .map(|key| {
            known_accounts
                .iter()
                .find(|row| row.get("id").and_then(Value::as_str) == Some(key))
                .map(|row| (key.as_str(), row))
                .ok_or(ProjectsError::Invalid("select existing project accounts"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if selected.iter().any(|(_, row)| {
        row.get("status").and_then(Value::as_str) != Some("ready")
            && row.get("canAttemptNativeProof").and_then(Value::as_bool) != Some(true)
    }) {
        return Err(ProjectsError::Invalid(
            "sign in to the selected project accounts first",
        ));
    }
    let expected = body
        .get("expected_revision")
        .and_then(Value::as_u64)
        .ok_or(ProjectsError::Invalid(
            "project account revision is invalid",
        ))?;
    let prepared = PreparedHost::load(store)?;
    prepared.recheck_paths()?;
    store.transaction_with(|tx| -> Result<Value, ProjectsError> {
        prepared.recheck(tx)?;
        let mut project = load_project(tx, &path)?;
        if path.starts_with("project:") && project.is_none() { return Err(ProjectsError::Invalid("select an existing project")); }
        let current = revision_of(project.as_ref(), "accountRevision", &path)?;
        let current_keys = project.as_ref().and_then(|row| row.get("accountKeys")).and_then(Value::as_array).cloned().unwrap_or_default();
        let existing_keys = if current_keys.is_empty() { project.as_ref().and_then(|row| row.get("accountKey")).cloned().into_iter().collect() } else { current_keys };
        if project.as_ref().is_some_and(|row| row.get("accountKey").and_then(Value::as_str) == Some(default)
            && row.get("accountKeys").and_then(Value::as_array) == Some(&keys.iter().cloned().map(Value::String).collect::<Vec<_>>())
            && (expected == current || expected.saturating_add(1) == current)) { return Ok(project.clone().unwrap_or(Value::Null)); }
        for (key, account) in &selected {
            if account.get("deleted").and_then(Value::as_bool) == Some(true) { return Err(ProjectsError::Invalid("deleted accounts cannot be added to a project")); }
            if account.get("disconnected").and_then(Value::as_bool) == Some(true)
                && (*key == default || !existing_keys.iter().any(|value| value.as_str() == Some(key))) {
                return Err(ProjectsError::Invalid("reconnect this account before adding it to a project or selecting it as the default"));
            }
        }
        if expected != current { return Err(ProjectsError::Conflict("Project accounts changed. Reload before saving")); }
        if project.is_none() {
            let name = project_name(None, &path)?;
            project = Some(new_project(&prepared, &path, &name, default, known_accounts)?);
        }
        let project = project.as_mut().ok_or(ProjectsError::Invalid("project is unavailable"))?;
        project["accountKey"] = json!(default);
        project["accountKeys"] = json!(keys);
        project["accountRevision"] = json!(current.saturating_add(1));
        ProjectsRepository::put_project(tx, project)?;
        Ok(project.clone())
    })
}

fn set_worker_environment(
    store: &RuntimeStore,
    body: &Value,
    default_account: &str,
    known_accounts: &[Value],
) -> Result<Value, ProjectsError> {
    let environment = body
        .get("environment")
        .and_then(Value::as_str)
        .filter(|environment| matches!(*environment, "host" | "linux"))
        .ok_or(ProjectsError::Invalid("worker environment is invalid"))?;
    if environment == "linux" {
        return Err(ProjectsError::Requires(
            "workspace-state/worker-environment",
        ));
    }
    let path = project_key(body.get("path"), true)?;
    let expected = body
        .get("expected_revision")
        .and_then(Value::as_u64)
        .ok_or(ProjectsError::Invalid(
            "worker environment revision is invalid",
        ))?;
    let prepared = PreparedHost::load(store)?;
    prepared.recheck_paths()?;
    store.transaction_with(|tx| -> Result<Value, ProjectsError> {
        prepared.recheck(tx)?;
        let mut project = load_project(tx, &path)?;
        if path.starts_with("project:") && project.is_none() {
            return Err(ProjectsError::Invalid("select an existing project"));
        }
        let current = revision_of(project.as_ref(), "workerEnvironmentRevision", &path)?;
        if project.as_ref().is_some_and(|row| {
            row.get("workerEnvironment")
                .and_then(Value::as_str)
                .unwrap_or("host")
                == environment
                && (expected == current || expected.saturating_add(1) == current)
        }) {
            return Ok(project.clone().unwrap_or(Value::Null));
        }
        if current != expected {
            return Err(ProjectsError::Conflict(
                "Worker environment changed. Reload it before saving",
            ));
        }
        if project.is_none() {
            let account = project_account(&prepared, tx, &path, default_account)?;
            project = Some(new_project(
                &prepared,
                &path,
                &project_name(None, &path)?,
                &account,
                known_accounts,
            )?);
        }
        let project = project.ok_or(ProjectsError::Invalid("project is unavailable"))?;
        let mut project = project;
        project["workerEnvironment"] = json!(environment);
        project["workerEnvironmentRevision"] = json!(current + 1);
        ProjectsRepository::put_project(tx, &project)?;
        Ok(project)
    })
}

fn organize(
    store: &RuntimeStore,
    body: &Value,
    default_account: &str,
    accounts: &[Value],
) -> Result<Value, ProjectsError> {
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let path = project_key(body.get("path"), false)?;
    let expected = body
        .get("expected_revision")
        .and_then(Value::as_u64)
        .ok_or(ProjectsError::Invalid(
            "supply the current project revision",
        ))?;
    let name = if action == "remove_folder" {
        None
    } else {
        let value = body
            .get("name")
            .ok_or(ProjectsError::Invalid("name is required"))?;
        Some(project_name(Some(value), "")?)
    };
    let folder_id = if action == "rename" {
        None
    } else {
        let raw = body
            .get("folder_id")
            .and_then(Value::as_str)
            .ok_or(ProjectsError::Invalid("supply a folder ID"))?;
        Some(
            Uuid::parse_str(raw)
                .map_err(|_| ProjectsError::Invalid("supply a folder ID"))?
                .to_string(),
        )
    };
    let prepared = PreparedHost::load(store)?;
    prepared.recheck_paths()?;
    store.transaction_with(|tx| {
        prepared.recheck(tx)?;
        let mut project = match load_project(tx, &path)? {
            Some(project) => project,
            None if path.starts_with("project:") => return Err(ProjectsError::Invalid("select an existing project")),
            None => {
                let account = project_account(&prepared, tx, &path, default_account)?;
                let project = new_project(&prepared, &path, &project_name(None, &path)?, &account, accounts)?;
                ProjectsRepository::put_project(tx, &project)?;
                project
            }
        };
        let revision = revision_of(Some(&project), "organizationRevision", &path)?;
        let mut folders = project.get("folders").and_then(Value::as_array).cloned().unwrap_or_default();
        let folder = folder_id.as_deref().and_then(|id| folders.iter().position(|item| item.get("id").and_then(Value::as_str) == Some(id)));
        if action == "rename" && project.get("name").and_then(Value::as_str) == name.as_deref() { return Ok(project); }
        if action == "add_folder" && let Some(index) = folder {
            let item = &folders[index];
            if item.get("name").and_then(Value::as_str) == name.as_deref()
                && item.get("parentId").and_then(Value::as_str) == body.get("parent_id").and_then(Value::as_str) { return Ok(project); }
            return Err(ProjectsError::Invalid("this folder ID has different settings"));
        }
        if action == "rename_folder" && folder.is_some_and(|i| folders[i].get("name").and_then(Value::as_str) == name.as_deref()) { return Ok(project); }
        if action == "remove_folder" && folder.is_none() { return Ok(project); }
        if expected != revision { return Err(ProjectsError::Conflict("Project changed. Reload it before saving")); }
        match action {
            "rename" => project["name"] = json!(name),
            "add_folder" | "rename_folder" => {
                let id = folder_id.as_deref().ok_or(ProjectsError::Invalid("supply a folder ID"))?;
                if action == "rename_folder" && folder.is_none() { return Err(ProjectsError::Invalid("this folder no longer exists")); }
                let parent = if action == "add_folder" {
                    let parent = body.get("parent_id").and_then(Value::as_str);
                    if parent.is_some_and(|parent| !folders.iter().any(|item| item.get("id").and_then(Value::as_str) == Some(parent))) {
                        return Err(ProjectsError::Invalid("this folder is not in the selected project"));
                    }
                    parent.map(str::to_owned)
                } else { folders[folder.unwrap_or(0)].get("parentId").and_then(Value::as_str).map(str::to_owned) };
                let new_name = name.as_deref().unwrap_or_default();
                if folders.iter().enumerate().any(|(index, item)| Some(index) != folder
                    && item.get("parentId").and_then(Value::as_str) == parent.as_deref()
                    // Named compatibility difference: Rust lowercase is not Python casefold
                    // for expansion folds such as ß -> ss. ASCII and ordinary Unicode
                    // lowercase match; this is covered by the explicit corpus case.
                    && item.get("name").and_then(Value::as_str).is_some_and(|old| folder_fold(old) == folder_fold(new_name))) {
                    return Err(ProjectsError::Invalid("a folder with this name already exists here"));
                }
                if let Some(index) = folder { folders[index]["name"] = json!(new_name); }
                else { folders.push(json!({"id":id,"name":new_name,"parentId":parent})); }
            }
            "remove_folder" => {
                let id = folder_id.as_deref().unwrap_or_default();
                let used_by_child = folders.iter().any(|item| item.get("parentId").and_then(Value::as_str) == Some(id));
                let agent_table: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_agents')", [], |row| row.get(0))?;
                let used_by_agent = agent_table && tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_agents WHERE (json_extract(record,'$.cwd')=?1 OR json_extract(record,'$.projectId')=?1) AND json_extract(record,'$.projectFolder')=?2 AND json_extract(record,'$.deletedAt') IS NULL)", rusqlite::params![path,id], |row| row.get::<_,bool>(0))?;
                if used_by_child || used_by_agent { return Err(ProjectsError::Invalid("this folder still contains chats or subfolders")); }
                folders.retain(|item| item.get("id").and_then(Value::as_str) != Some(id));
            }
            _ => return Err(ProjectsError::Invalid("unknown organization action")),
        }
        if action != "rename" { project["folders"] = json!(folders); }
        project["organizationRevision"] = json!(revision + 1);
        project["updated"] = json!(now_seconds());
        ProjectsRepository::put_project(tx, &project)?;
        Ok(project)
    })
}

pub(crate) fn project_key(
    value: Option<&Value>,
    require_directory: bool,
) -> Result<String, ProjectsError> {
    let value = value
        .and_then(Value::as_str)
        .ok_or(ProjectsError::Invalid("project path is required"))?;
    let validated = python_strip(value);
    if validated.starts_with("project:") {
        let parsed = Uuid::parse_str(validated.strip_prefix("project:").unwrap_or_default())
            .map_err(|_| ProjectsError::Invalid("invalid project ID"))?;
        return Ok(format!("project:{parsed}"));
    }
    if value.is_empty()
        || python_strip(value).is_empty()
        || validated.chars().count() > 4096
        || value.contains('\0')
    {
        return Err(ProjectsError::Invalid("project path is invalid"));
    }
    let expanded = if value == "~" {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(value))
    } else if let Some(rest) = value.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("~"))
            .join(rest)
    } else {
        PathBuf::from(value)
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map_err(|_| ProjectsError::Invalid("project path is invalid"))?
            .join(expanded)
    };
    let canonical = resolve_nonstrict(&absolute)?;
    if require_directory && !canonical.is_dir() {
        return Err(ProjectsError::Invalid(
            "select an existing project directory",
        ));
    }
    Ok(canonical.to_string_lossy().into_owned())
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        use std::path::Component;
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

pub(crate) fn resolve_nonstrict(path: &Path) -> Result<PathBuf, ProjectsError> {
    let mut ancestor = path.to_path_buf();
    let mut suffix = Vec::new();
    loop {
        match ancestor.canonicalize() {
            Ok(mut resolved) => {
                for part in suffix.into_iter().rev() {
                    resolved.push(part);
                }
                return Ok(normalize_path(&resolved));
            }
            Err(_) => {
                if ancestor.components().next_back() == Some(std::path::Component::ParentDir) {
                    suffix.push(std::ffi::OsString::from(".."));
                    ancestor.pop();
                    continue;
                }
                let part = ancestor
                    .file_name()
                    .ok_or(ProjectsError::Invalid("project path is invalid"))?
                    .to_os_string();
                suffix.push(part);
                if !ancestor.pop() {
                    return Err(ProjectsError::Invalid("project path is invalid"));
                }
            }
        }
    }
}

pub(crate) fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

#[cfg(test)]
mod compatibility_tests {
    #[test]
    fn python_casefold_expansion_is_a_registered_difference() {
        assert_ne!(super::folder_fold("straße"), super::folder_fold("STRASSE"));
    }

    #[test]
    fn profile_normalization_matches_python_for_missing_parent_components() {
        let home = tempfile::tempdir().expect("temporary home");
        let requested = home.path().join("fixtures/../profile");
        assert_eq!(
            super::resolve_nonstrict(&requested).expect("normalized profile"),
            home.path().join("profile")
        );
    }
}
