//! Decisions for local project records and sidebar state.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use studio_runtime_store::{
    ProjectsRepository, RuntimeStore, StoreError, SyncDocumentsRepository, sha256_hex,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProjectsError {
    #[error("invalid request: {0}")]
    Invalid(&'static str),
    #[error("conflict: {0}")]
    Conflict(&'static str),
    #[error("not implemented: requires {0}")]
    Requires(&'static str),
    #[error("invalid stored project {0}")]
    InvalidStored(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub fn list(store: &RuntimeStore) -> Result<Value, ProjectsError> {
    let connection = store.read()?;
    let rows = ProjectsRepository::list(&connection)?;
    let mut items = Vec::with_capacity(rows.len());
    for (id, record) in rows {
        if record.get("id").and_then(Value::as_str) != Some(id.as_str()) {
            return Err(ProjectsError::InvalidStored(id));
        }
        items.push(record);
    }
    Ok(json!({"items": items}))
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
    if body
        .get("server")
        .and_then(Value::as_str)
        .is_some_and(|server| !server.is_empty())
    {
        return Err(ProjectsError::Requires(
            "cross-server-driver/project-registration",
        ));
    }
    if matches!(action, "add_location" | "remove_location") {
        return Err(ProjectsError::Requires(
            "cross-server-driver/project-location-write",
        ));
    }
    if action == "set_worker_environment" {
        return set_worker_environment(store, body);
    }
    if matches!(
        action,
        "rename" | "add_folder" | "rename_folder" | "remove_folder"
    ) {
        return Err(ProjectsError::Requires(
            "workspace-state/project-organization",
        ));
    }
    if action == "set_accounts" {
        return set_accounts(store, body, known_accounts);
    }
    if action == "reorder" {
        return reorder(store, body);
    }
    let path = project_key(body.get("path"), action == "register")?;
    if action == "remove" {
        return store.transaction_with(|tx| -> Result<Value, ProjectsError> {
            let removed = ProjectsRepository::remove(tx, &path)?;
            Ok(json!({"id": path, "removed": removed}))
        });
    }
    if !matches!(action, "register" | "set_account" | "set_worker_base") {
        return Err(ProjectsError::Invalid("unknown project action"));
    }

    let expected_revision = body.get("expected_revision").and_then(Value::as_u64);
    if matches!(action, "set_account" | "set_worker_base") && expected_revision.is_none() {
        return Err(ProjectsError::Invalid(
            "supply the current project revision",
        ));
    }
    let explicit_account = body.get("account_key").and_then(Value::as_str);
    if action == "set_account"
        && !known_accounts
            .iter()
            .any(|account| account.get("id").and_then(Value::as_str) == explicit_account)
    {
        return Err(ProjectsError::Invalid("select an existing account"));
    }
    let base_ref = body.get("base_ref");
    if action == "set_worker_base"
        && base_ref.is_some_and(|value| {
            !value.is_null()
                && value
                    .as_str()
                    .is_none_or(|text| text.trim().chars().count() > 1024)
        })
    {
        return Err(ProjectsError::Invalid("worker base ref is invalid"));
    }

    store.transaction_with(|tx| {
        let row = ProjectsRepository::get(tx, &path)?;
        let mut project = row.map(|(_, value)| value);
        if action == "register"
            && let Some(project) = &project
        {
            return Ok(project.clone());
        }
        let current = project
            .as_ref()
            .and_then(|value| value.get("accountRevision"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let current_base = project
            .as_ref()
            .and_then(|value| value.get("workerBaseRevision"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if action == "set_account"
            && project.as_ref().is_some_and(|project| {
                project.get("accountKey").and_then(Value::as_str) == explicit_account
                    && expected_revision.is_some_and(|revision| {
                        revision == current || revision.saturating_add(1) == current
                    })
            })
        {
            return Ok(project.clone().unwrap_or(Value::Null));
        }
        if action == "set_account" && expected_revision != Some(current) {
            return Err(ProjectsError::Conflict(
                "Project account changed. Reload it before saving",
            ));
        }
        if action == "set_worker_base" && expected_revision != Some(current_base) {
            return Err(ProjectsError::Conflict(
                "Worker base changed. Reload it before saving",
            ));
        }
        let account = explicit_account.unwrap_or(default_account);
        if project.is_none() {
            let name = body
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty() && name.chars().count() <= 255)
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    Path::new(&path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .filter(|name| !name.is_empty())
                        .unwrap_or(&path)
                        .to_owned()
                });
            project = Some(json!({"id":path,"path":path,"name":name,"created":now_seconds()}));
        }
        let project = project
            .as_mut()
            .ok_or(ProjectsError::Invalid("invalid project"))?;
        if action == "set_worker_base" {
            let value = base_ref
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| Value::String(value.to_owned()))
                .unwrap_or(Value::Null);
            project["workerBaseRef"] = value;
            project["workerBaseRevision"] = json!(current_base.saturating_add(1));
        } else {
            if !known_accounts
                .iter()
                .any(|candidate| candidate.get("id").and_then(Value::as_str) == Some(account))
            {
                return Err(ProjectsError::Invalid("select an existing account"));
            }
            if action == "set_account"
                && known_accounts.iter().any(|candidate| {
                    candidate.get("id").and_then(Value::as_str) == Some(account)
                        && candidate.get("disconnected").and_then(Value::as_bool) == Some(true)
                })
            {
                return Err(ProjectsError::Invalid(
                    "reconnect this account before selecting it",
                ));
            }
            project["accountKey"] = json!(account);
            project["accountRevision"] = json!(current.saturating_add(1));
        }
        ProjectsRepository::put_project(tx, project)?;
        Ok(project.clone())
    })
}

fn set_accounts(
    store: &RuntimeStore,
    body: &Value,
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
        .map(|key| {
            key.as_str()
                .map(str::to_owned)
                .ok_or(ProjectsError::Invalid("project account key is invalid"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    keys.sort();
    keys.dedup();
    let default = body
        .get("account_key")
        .and_then(Value::as_str)
        .ok_or(ProjectsError::Invalid("select a default project account"))?;
    if !keys.iter().any(|key| key == default)
        || keys.iter().any(|key| {
            !known_accounts.iter().any(|account| {
                account.get("id").and_then(Value::as_str) == Some(key)
                    && account.get("status").and_then(Value::as_str) == Some("ready")
            })
        })
    {
        return Err(ProjectsError::Invalid("select ready project accounts"));
    }
    let expected = body
        .get("expected_revision")
        .and_then(Value::as_u64)
        .ok_or(ProjectsError::Invalid(
            "project account revision is invalid",
        ))?;
    store.transaction_with(|tx| -> Result<Value, ProjectsError> {
        let row = ProjectsRepository::get(tx, &path)?;
        let mut project = row.map(|(_, value)| value);
        let current = project
            .as_ref()
            .and_then(|project| project.get("accountRevision"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if project.as_ref().is_some_and(|project| {
            project.get("accountKey").and_then(Value::as_str) == Some(default)
                && project.get("accountKeys").and_then(Value::as_array)
                    == Some(&keys.iter().cloned().map(Value::String).collect::<Vec<_>>())
                && (expected == current || expected.saturating_add(1) == current)
        }) {
            return project.ok_or(ProjectsError::Invalid("select an existing project"));
        }
        if current != expected {
            return Err(ProjectsError::Conflict(
                "Project accounts changed. Reload before saving",
            ));
        }
        if project.is_none() {
            let name = Path::new(&path)
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .unwrap_or(&path);
            project = Some(json!({"id":path,"path":path,"name":name,"created":now_seconds()}));
        }
        let project = project
            .as_mut()
            .ok_or(ProjectsError::Invalid("invalid project"))?;
        project["accountKey"] = json!(default);
        project["accountKeys"] = json!(keys);
        project["accountRevision"] = json!(current.saturating_add(1));
        ProjectsRepository::put_project(tx, project)?;
        Ok(project.clone())
    })
}

fn set_worker_environment(store: &RuntimeStore, body: &Value) -> Result<Value, ProjectsError> {
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
    store.transaction_with(|tx| -> Result<Value, ProjectsError> {
        let row = ProjectsRepository::get(tx, &path)?;
        let mut project = row.map(|(_, value)| value);
        let current = project
            .as_ref()
            .and_then(|project| project.get("workerEnvironmentRevision"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if project.as_ref().is_some_and(|project| {
            project.get("workerEnvironment").and_then(Value::as_str) == Some(environment)
                && (expected == current || expected.saturating_add(1) == current)
        }) {
            return project.ok_or(ProjectsError::Invalid("invalid project"));
        }
        if current != expected {
            return Err(ProjectsError::Conflict(
                "Worker environment changed. Reload it before saving",
            ));
        }
        if project.is_none() {
            let name = Path::new(&path)
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .unwrap_or(&path);
            project = Some(json!({"id":path,"path":path,"name":name,"created":now_seconds()}));
        }
        let project = project.ok_or(ProjectsError::Invalid("invalid project"))?;
        let mut project = project;
        project["workerEnvironment"] = json!(environment);
        project["workerEnvironmentRevision"] = json!(current.saturating_add(1));
        project["updated"] = json!(now_seconds());
        ProjectsRepository::put_project(tx, &project)?;
        Ok(project)
    })
}

pub fn peer_team_write(
    store: &RuntimeStore,
    body: &Value,
    default_account: &str,
) -> Result<Value, ProjectsError> {
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .ok_or(ProjectsError::Invalid("peer team action is required"))?;
    if action == "radio" {
        return Err(ProjectsError::Requires("workspace-state/peer-radio"));
    }
    if action == "convert" {
        return Err(ProjectsError::Requires(
            "cross-server-driver/peer-team-transport",
        ));
    }
    if !matches!(action, "save" | "delete" | "move") {
        return Err(ProjectsError::Invalid("unknown peer team action"));
    }
    let path = project_key(body.get("path"), false)?;
    let revision = body
        .get("expected_revision")
        .and_then(Value::as_u64)
        .ok_or(ProjectsError::Invalid("peer team revision is invalid"))?;
    let request_id = body
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(ProjectsError::Invalid("peer team request ID is required"))?;
    let team_id = body.get("team_id").and_then(Value::as_str);
    if (action != "move" || team_id.is_some()) && !team_id.is_some_and(valid_uuid) {
        return Err(ProjectsError::Invalid("peer team ID is invalid"));
    }
    let name = body.get("name").and_then(Value::as_str);
    let members = body.get("members").and_then(Value::as_array);
    if action == "save"
        && (name.is_none_or(|name| name.trim().is_empty() || name.chars().count() > 80)
            || members.is_none_or(|members| {
                members.len() < 2
                    || members
                        .iter()
                        .any(|id| id.as_str().is_none_or(str::is_empty))
                    || members
                        .iter()
                        .collect::<std::collections::HashSet<_>>()
                        .len()
                        != members.len()
            }))
    {
        return Err(ProjectsError::Invalid("peer team fields are invalid"));
    }
    let signature_value = json!({"operation":"peer-team","body":body});
    let signature = sha256_hex(
        serde_json::to_string(&signature_value)
            .map_err(|_| ProjectsError::Invalid("peer team request is invalid"))?
            .as_bytes(),
    );
    let key = format!("peer-team:{request_id}");
    store
        .transaction_with(|tx| -> Result<Value, ProjectsError> {
            if let Some((prior_signature, prior_result)) = ProjectsRepository::receipt(tx, &key)? {
                if prior_signature != signature {
                    return Err(ProjectsError::Invalid("peer team request ID was reused"));
                }
                return Ok(prior_result);
            }

            let project_row = ProjectsRepository::get(tx, &path)?;
            let mut project = match project_row {
                Some((_, project)) => project,
                None => json!({
                    "id": path, "path": path,
                    "name": Path::new(&path).file_name().and_then(|name| name.to_str()).filter(|name| !name.is_empty()).unwrap_or(&path),
                    "created": now_seconds(), "accountKey": default_account, "accountRevision": 1
                }),
            };
            if project.get("id").and_then(Value::as_str) != Some(path.as_str()) {
                return Err(ProjectsError::InvalidStored(path.clone()));
            }
            let current = project
                .get("peerTeamsRevision")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if current != revision {
                return Err(ProjectsError::Conflict(
                    "Peer teams changed. Reload before saving",
                ));
            }
            let mut teams = project
                .get("peerTeams")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let prior_ids = teams
                .iter()
                .filter_map(|team| team.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect::<std::collections::HashSet<_>>();
            let agents = project_agents(tx, &path)?;
            match action {
                "save" => {
                    let members = members.ok_or(ProjectsError::Invalid("peer team members are required"))?;
                    for member in members.iter().filter_map(Value::as_str) {
                        if !agents.get(member).is_some_and(|agent| is_project_lead(agent, &path)) {
                            return Err(ProjectsError::Invalid("select live lead chats from this project"));
                        }
                    }
                    let team_id = team_id.ok_or(ProjectsError::Invalid("peer team ID is required"))?;
                    let member_ids = members.clone();
                    let mut selected = json!({"id": team_id, "name": name, "members": member_ids});
                    if let Some(previous) = teams.iter().find(|team| team.get("id").and_then(Value::as_str) == Some(team_id))
                        && let (Some(previous), Some(selected)) =
                            (previous.as_object(), selected.as_object_mut())
                    {
                        for (field, value) in previous {
                            selected.entry(field.clone()).or_insert_with(|| value.clone());
                        }
                    }
                    if teams.iter().any(|team| team.get("id").and_then(Value::as_str) != Some(team_id)
                        && team.get("members").and_then(Value::as_array).is_some_and(|other| other.iter().any(|member| members.contains(member)))) {
                        return Err(ProjectsError::Invalid("a chat already belongs to another peer team"));
                    }
                    if let Some(existing) = teams.iter_mut().find(|team| team.get("id").and_then(Value::as_str) == Some(team_id)) {
                        *existing = selected;
                    } else {
                        teams.push(selected);
                    }
                }
                "delete" => teams.retain(|team| team.get("id").and_then(Value::as_str) != team_id),
                "move" => {
                    let member = body.get("member").and_then(Value::as_str).ok_or(ProjectsError::Invalid("peer team member is required"))?;
                    if !agents.get(member).is_some_and(|agent| is_project_lead(agent, &path)) {
                        return Err(ProjectsError::Invalid("select a live lead chat from this project"));
                    }
                    let source = teams.iter().position(|team| team.get("members").and_then(Value::as_array).is_some_and(|members| members.iter().any(|id| id.as_str() == Some(member))));
                    let target = team_id.and_then(|id| teams.iter().position(|team| team.get("id").and_then(Value::as_str) == Some(id)));
                    if team_id.is_some() && target.is_none() {
                        return Err(ProjectsError::Invalid("peer team destination is unavailable"));
                    }
                    if source == target {
                        return Err(ProjectsError::Invalid("chat already has this team membership"));
                    }
                    if let Some(index) = source
                        && let Some(members) = teams[index].get_mut("members").and_then(Value::as_array_mut)
                    {
                        members.retain(|id| id.as_str() != Some(member));
                    }
                    if let Some(index) = target
                        && let Some(members) = teams[index].get_mut("members").and_then(Value::as_array_mut)
                    {
                        members.push(Value::String(member.to_owned()));
                    }
                    teams.retain(|team| team.get("members").and_then(Value::as_array).is_some_and(|members| members.len() >= 2));
                }
                _ => unreachable!(),
            }
            project["peerTeams"] = Value::Array(teams.clone());
            project["peerTeamsRevision"] = json!(current.saturating_add(1));
            project["updated"] = json!(now_seconds());
            ProjectsRepository::put_project(tx, &project)?;
            let now = teams
                .iter()
                .filter_map(|team| peer_team_projection(team, &path, current + 1))
                .collect::<Vec<_>>();
            let live_ids = now
                .iter()
                .filter_map(|team| team.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect::<std::collections::HashSet<_>>();
            for team in &now {
                let id = team.get("id").and_then(Value::as_str).ok_or(ProjectsError::Invalid("peer team ID is invalid"))?;
                SyncDocumentsRepository::put_entity(tx, "peerTeam", id, team.clone(), false)?;
            }
            for old_id in prior_ids.difference(&live_ids) {
                SyncDocumentsRepository::put_entity(tx, "peerTeam", old_id, json!({}), true)?;
            }
            ProjectsRepository::save_receipt(tx, &key, &signature, &project)?;
            Ok(project)
        })
}

fn project_agents(
    tx: &studio_runtime_store::Transaction<'_>,
    path: &str,
) -> Result<std::collections::HashMap<String, Value>, ProjectsError> {
    let mut agents = std::collections::HashMap::new();
    for (id, agent) in ProjectsRepository::agents(tx)? {
        if agent.get("id").and_then(Value::as_str) != Some(id.as_str()) {
            return Err(ProjectsError::InvalidStored(id));
        }
        if is_project_lead(&agent, path) {
            agents.insert(id, agent);
        }
    }
    Ok(agents)
}

fn is_project_lead(agent: &Value, path: &str) -> bool {
    agent.get("isLead").and_then(Value::as_bool) == Some(true)
        && agent.get("deletedAt").is_none_or(Value::is_null)
        && agent.get("parentId").is_none_or(Value::is_null)
        && agent.get("rootId").and_then(Value::as_str) == agent.get("id").and_then(Value::as_str)
        && agent.get("cwd").and_then(Value::as_str).is_some_and(|cwd| {
            Path::new(cwd)
                .canonicalize()
                .map(|resolved| resolved == Path::new(path))
                .unwrap_or(false)
        })
}

fn peer_team_projection(team: &Value, path: &str, revision: u64) -> Option<Value> {
    let members = team.get("members")?.as_array()?;
    let mut result = json!({
        "id": team.get("id")?.as_str()?,
        "name": team.get("name")?.as_str()?,
        "projectPath": path,
        "members": members,
        "revision": revision,
    });
    result.as_object_mut()?.retain(|_, value| !value.is_null());
    Some(result)
}

fn valid_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
}

fn reorder(store: &RuntimeStore, body: &Value) -> Result<Value, ProjectsError> {
    let request_id = body
        .get("request_id")
        .and_then(Value::as_str)
        .ok_or(ProjectsError::Invalid("request ID is required"))?;
    let expected = body
        .get("expected_revision")
        .and_then(Value::as_u64)
        .ok_or(ProjectsError::Invalid("sidebar revision is invalid"))?;
    let groups = body
        .get("groups")
        .filter(|value| value.is_object())
        .ok_or(ProjectsError::Invalid("sidebar groups are invalid"))?;
    let Some(group_map) = groups.as_object() else {
        return Err(ProjectsError::Invalid("sidebar groups are invalid"));
    };
    let total = group_map
        .values()
        .filter_map(Value::as_array)
        .map(Vec::len)
        .sum::<usize>();
    if group_map.len() > 500
        || total > 10_000
        || serde_json::to_string(groups).map_or(true, |value| value.len() > 5_000_000)
        || group_map.iter().any(|(key, items)| {
            key.chars().count() > 4096
                || items.as_array().is_none_or(|items| {
                    items.len() > 10_000
                        || items.iter().any(|item| {
                            item.as_str().is_none_or(|item| item.chars().count() > 4096)
                        })
                        || items.iter().collect::<std::collections::HashSet<_>>().len()
                            != items.len()
                })
        })
    {
        return Err(ProjectsError::Invalid("sidebar order is invalid"));
    }
    let signature = sha256_hex(
        serde_json::to_string(&json!({"operation":"sidebar-order","body":body}))
            .map_err(|_| ProjectsError::Invalid("sidebar request is invalid"))?
            .as_bytes(),
    );
    let key = format!("sidebar-order:{request_id}");
    store.transaction_with(|tx| -> Result<Value, ProjectsError> {
        if let Some((old_signature, result)) = ProjectsRepository::receipt(tx, &key)? {
            if old_signature != signature {
                return Err(ProjectsError::Invalid("sidebar request ID was reused"));
            }
            return Ok(result);
        }
        let current = ProjectsRepository::sidebar_order(tx)?;
        let revision = current.get("revision").and_then(Value::as_u64).unwrap_or(0);
        if revision != expected {
            return Err(ProjectsError::Conflict(
                "Sidebar order changed. Refreshed from the server",
            ));
        }
        if current
            .get("groups")
            .is_some_and(|groups| !groups.is_null())
            && body.get("migration").and_then(Value::as_bool) == Some(true)
        {
            return Err(ProjectsError::Conflict(
                "Sidebar order changed. Refreshed from the server",
            ));
        }
        let result = json!({"revision": revision.saturating_add(1), "groups": groups});
        ProjectsRepository::put_sidebar_order(tx, &result)?;
        ProjectsRepository::save_receipt(tx, &key, &signature, &result)?;
        Ok(result)
    })
}

fn project_key(value: Option<&Value>, require_directory: bool) -> Result<String, ProjectsError> {
    let value = value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(ProjectsError::Invalid("project path is required"))?;
    if value.starts_with("project:") {
        return Err(ProjectsError::Requires("workspace-state/logical-projects"));
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
    let canonical = match expanded.canonicalize() {
        Ok(canonical) => canonical,
        Err(_) if !require_directory => {
            let absolute = if expanded.is_absolute() {
                expanded
            } else {
                std::env::current_dir()
                    .map_err(|_| ProjectsError::Invalid("project path is invalid"))?
                    .join(expanded)
            };
            normalize_path(&absolute)
        }
        Err(_) => {
            return Err(ProjectsError::Invalid(
                "select an existing project directory",
            ));
        }
    };
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

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}
