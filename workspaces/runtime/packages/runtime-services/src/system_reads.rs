//! Local filesystem reads used by the system routes.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use studio_runtime_store::Connection;
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryEntry {
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryListing {
    pub path: String,
    pub parent: Option<String>,
    pub directories: Vec<DirectoryEntry>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum SystemReadError {
    #[error("This directory is unavailable")]
    UnavailableDirectory,
    #[error("Permission denied")]
    PermissionDenied,
}

pub fn ui_summary(
    connection: &Connection,
    account_snapshot: Option<crate::account_policy::AccountsSnapshot>,
) -> Result<Value, SystemReadError> {
    let accounts = account_snapshot.map_or_else(Vec::new, |snapshot| {
        let default = snapshot.default_account_key.as_str();
        snapshot.accounts.into_iter().filter_map(|account| {
            let provider = account.get("provider").and_then(Value::as_str).unwrap_or("codex");
            if provider != "codex" && provider != "claude" { return None; }
            Some(json!({
                "provider":provider,
                "email":account.get("email").and_then(Value::as_str),
                "plan":account.get("plan").and_then(Value::as_str),
                "status":if truthy(account.get("disconnected")) {"signedOut"} else {account.get("status").and_then(Value::as_str).unwrap_or("error")},
                "label":account.get("label").and_then(Value::as_str).unwrap_or("Account"),
                "isDefault":account.get("id").and_then(Value::as_str)==default,
            }))
        }).collect()
    });
    let mut result = json!({
        "ready":false,"busy":false,"system":operating_system(),"agentsRunning":0,
        "projects":[],"chats":[],"alerts":[],"accounts":accounts,
    });
    let has_table: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='sync_entities')",
        [], |row| row.get(0),
    ).map_err(|_| SystemReadError::UnavailableDirectory)?;
    if !has_table {
        return Ok(result);
    }
    let mut statement = connection.prepare(
        "SELECT collection,payload FROM sync_entities WHERE deleted=0 AND collection IN ('agent','project','room','request','complaint') ORDER BY collection,id",
    ).map_err(|_| SystemReadError::UnavailableDirectory)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|_| SystemReadError::UnavailableDirectory)?;
    let mut values: std::collections::BTreeMap<String, Vec<Value>> =
        std::collections::BTreeMap::new();
    for row in rows {
        let (collection, payload) = row.map_err(|_| SystemReadError::UnavailableDirectory)?;
        if let Ok(value) = serde_json::from_str::<Value>(&payload)
            && let Some(value) = value.get("value").filter(|value| value.is_object())
        {
            values.entry(collection).or_default().push(value.clone());
        }
    }
    result["ready"] = Value::Bool(true);
    let agents = values
        .get("agent")
        .into_iter()
        .flatten()
        .filter(|agent| !truthy(agent.get("deletedAt")))
        .filter_map(|agent| {
            agent
                .get("id")
                .and_then(Value::as_str)
                .map(|id| (id.to_owned(), agent.clone()))
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    result["agentsRunning"] = json!(
        agents
            .values()
            .filter(|agent| truthy(agent.get("inFlight")))
            .count()
    );
    let mut projects = std::collections::BTreeMap::new();
    for project in values.get("project").into_iter().flatten() {
        if let Some(path) = project
            .get("path")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
        {
            let mut value = json!({"path":path,"name":project.get("name").and_then(Value::as_str).unwrap_or(path)});
            for key in ["id", "homeServerId", "locations", "projectAliases"] {
                if let Some(field) = project.get(key) {
                    value[key] = field.clone();
                }
            }
            projects.insert(path.to_owned(), value);
        }
    }
    let team_members = values
        .get("project")
        .into_iter()
        .flatten()
        .flat_map(|project| {
            project
                .get("peerTeams")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .flat_map(|team| {
            team.get("members")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<std::collections::BTreeSet<_>>();
    let mut chats = Vec::new();
    let mut alerts = Vec::new();
    for (id, agent) in &agents {
        let in_flight = truthy(agent.get("inFlight"));
        result["busy"] = json!(result["busy"].as_bool().unwrap_or(false) || in_flight);
        if agent.get("source").and_then(Value::as_str) != Some("managed")
            || agent.get("isLead").and_then(Value::as_bool) != Some(true)
            || truthy(agent.get("sharedRoomId"))
            || truthy(agent.get("remoteAnchor"))
        {
            continue;
        }
        let path = agent
            .get("hostProjectPath")
            .or_else(|| agent.get("cwd"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let project_exists = values.get("project").into_iter().flatten().any(|project| {
            let project_id = agent.get("projectId").filter(|value| !value.is_null());
            let by_id = project_id.is_some()
                && project.get("id") == project_id
                && project.get("homeServerId") == agent.get("projectServerId");
            let by_location = project
                .get("locations")
                .and_then(Value::as_array)
                .is_some_and(|locations| {
                    locations.iter().any(|location| {
                        location.get("path").and_then(Value::as_str) == Some(path)
                            && location.get("serverId") == agent.get("serverId")
                    })
                });
            by_id || by_location
        });
        if !project_exists {
            projects.entry(path.to_owned()).or_insert_with(
                || json!({"path":path,"name":if path.is_empty(){"Other chats"}else{path}}),
            );
        }
        let turn = agent.get("lastCompletedTurn").and_then(Value::as_str);
        let read = agent.get("readState");
        let unread = truthy(agent.get("threadId"))
            && truthy(agent.get("lastCompletedTurn"))
            && turn.is_some()
            && agent.get("lastCompletedTurnStatus").and_then(Value::as_str) == Some("completed")
            && !(read
                .and_then(|value| value.get("read"))
                .and_then(Value::as_bool)
                == Some(true)
                && read.and_then(|value| value.get("threadId")) == agent.get("threadId")
                && read.and_then(|value| value.get("turnId")) == agent.get("lastCompletedTurn"));
        chats.push(json!({
            "id":id,"name":agent.get("name").and_then(Value::as_str).filter(|name|!name.is_empty()).unwrap_or("Chat"),
            "path":path,"archived":truthy(agent.get("archived")),"status":agent.get("status").and_then(Value::as_str).unwrap_or(""),
            "unread":unread,"serverId":agent.get("serverId"),"projectId":agent.get("projectId"),
            "projectServerId":agent.get("projectServerId"),"provider":agent.get("provider"),
            "updated":as_number(agent.get("updated")),"created":as_number(agent.get("created")),
            "inFlight":in_flight,"pinned":truthy(agent.get("pinned")),"team":team_members.contains(id)
        }));
        if in_flight {
            continue;
        }
        if turn.is_some()
            && agent.get("status").and_then(Value::as_str) == Some("completed")
            && agent.get("lastCompletedTurnStatus").and_then(Value::as_str) == Some("completed")
        {
            push_alert(
                &mut alerts,
                format!("completed:{id}:{}", turn.unwrap_or_default()),
                agent,
                "Reply ready",
                agent
                    .get("tail")
                    .and_then(Value::as_str)
                    .unwrap_or("Open the chat to read the reply."),
                None,
            );
        } else if matches!(
            agent.get("status").and_then(Value::as_str),
            Some("failed" | "interrupted")
        ) {
            let error = agent
                .get("error")
                .and_then(|value| value.get("message"))
                .or_else(|| agent.get("error"));
            push_alert(
                &mut alerts,
                format!(
                    "failed:{id}:{}",
                    turn.or_else(|| agent.get("turnId").and_then(Value::as_str))
                        .unwrap_or_else(|| agent
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown"))
                ),
                agent,
                "Work stopped",
                error
                    .and_then(Value::as_str)
                    .unwrap_or("Open the chat to check the error."),
                None,
            );
        }
    }
    for request in values.get("request").into_iter().flatten() {
        if request.get("status").and_then(Value::as_str) != Some("pending")
            || truthy(request.get("deferred"))
        {
            continue;
        }
        let id = request.get("id").and_then(Value::as_str).unwrap_or("");
        let agent = request.get("agent").and_then(Value::as_str).unwrap_or("");
        let question = request
            .get("params")
            .and_then(|value| value.get("questions"))
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("question"))
            .and_then(Value::as_str);
        if let Some(owner) = agents.get(agent) {
            push_alert(
                &mut alerts,
                format!("request:{id}"),
                owner,
                if question.is_some() {
                    "Question for you"
                } else {
                    "Your approval is needed"
                },
                question.unwrap_or("Open the request to review it."),
                Some(id),
            );
        }
    }
    for complaint in values.get("complaint").into_iter().flatten() {
        if !truthy(complaint.get("needsUserResponse")) {
            continue;
        }
        let agent = complaint
            .get("leadId")
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Some(owner) = agents.get(agent) {
            push_alert(
                &mut alerts,
                format!(
                    "complaint:{}:{}",
                    complaint.get("id").and_then(Value::as_str).unwrap_or(""),
                    complaint
                        .get("version")
                        .and_then(Value::as_i64)
                        .unwrap_or(0)
                ),
                owner,
                "Message for you",
                complaint
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("Open the message to read it."),
                complaint.get("id").and_then(Value::as_str),
            );
        }
    }
    for room in values.get("room").into_iter().flatten() {
        if !truthy(room.get("radio")) || truthy(room.get("userHidden")) {
            continue;
        }
        let path = room
            .get("projectPath")
            .and_then(Value::as_str)
            .unwrap_or("");
        projects.entry(path.to_owned()).or_insert_with(
            || json!({"path":path,"name":if path.is_empty(){"Other chats"}else{path}}),
        );
        let unread = room
            .get("members")
            .and_then(Value::as_array)
            .is_some_and(|members| {
                members.iter().filter_map(Value::as_str).any(|id| {
                    agents.get(id).is_some_and(|agent| {
                        truthy(agent.get("lastCompletedTurn"))
                            && agent.get("lastCompletedTurnStatus").and_then(Value::as_str)
                                == Some("completed")
                    })
                })
            });
        chats.push(json!({"id":room.get("id"),"name":room.get("name").and_then(Value::as_str).unwrap_or("Shared chat"),
            "path":path,"archived":false,"status":"","unread":unread}));
    }
    result["projects"] = Value::Array(projects.into_values().collect());
    result["chats"] = Value::Array(chats);
    result["alerts"] = Value::Array(alerts);
    Ok(result)
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|value| value != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(values)) => !values.is_empty(),
        Some(Value::Object(values)) => !values.is_empty(),
    }
}
fn as_number(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64)
}
fn operating_system() -> &'static str {
    if cfg!(target_os = "macos") {
        "Darwin"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "Linux"
    }
}
fn push_alert(
    alerts: &mut Vec<Value>,
    id: String,
    agent: &Value,
    title: &str,
    body: &str,
    item: Option<&str>,
) {
    alerts.push(json!({"id":id,"title":format!("{}: {}",agent.get("name").and_then(Value::as_str).unwrap_or("Chat"),title).chars().take(160).collect::<String>(),
        "body":body.chars().take(2000).collect::<String>(),"target":{"agentId":agent.get("id"),"itemId":item}}));
}

pub fn directories(path: Option<&str>) -> Result<DirectoryListing, SystemReadError> {
    let raw = path.unwrap_or("");
    let raw = if raw.is_empty() {
        std::env::current_dir().map_err(|_| SystemReadError::UnavailableDirectory)?
    } else {
        expand_home(raw)?
    };
    let directory = resolve_non_strict(&raw)?;
    match fs::metadata(&directory) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(SystemReadError::UnavailableDirectory),
        Err(error) => return Err(directory_io_error(error)),
    }

    let mut directories = Vec::new();
    for entry in fs::read_dir(&directory).map_err(directory_io_error)? {
        let entry = entry.map_err(directory_io_error)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        match entry.file_type().map_err(directory_io_error)?.is_dir() {
            true => {}
            false => match fs::metadata(entry.path()) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(directory_io_error(error)),
            },
        }
        directories.push(DirectoryEntry {
            name,
            path: entry.path().to_string_lossy().into_owned(),
        });
    }
    directories.sort_by_key(|entry| entry.name.to_lowercase());
    directories.truncate(500);

    Ok(DirectoryListing {
        path: directory.to_string_lossy().into_owned(),
        parent: parent(&directory).map(|path| path.to_string_lossy().into_owned()),
        directories,
    })
}

fn parent(directory: &Path) -> Option<PathBuf> {
    let parent = directory.parent()?;
    (parent != directory).then(|| parent.to_path_buf())
}

fn expand_home(path: &str) -> Result<PathBuf, SystemReadError> {
    let Some(rest) = path.strip_prefix('~') else {
        return Ok(PathBuf::from(path));
    };
    let (user, suffix) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    let home = if user.is_empty() {
        current_home()?
    } else {
        named_home(user)?.ok_or(SystemReadError::UnavailableDirectory)?
    };
    Ok(home.join(suffix.trim_start_matches('/')))
}

#[cfg(unix)]
fn current_home() -> Result<PathBuf, SystemReadError> {
    current_home_for(std::env::var_os("HOME"))
}

#[cfg(unix)]
fn current_home_for(home: Option<std::ffi::OsString>) -> Result<PathBuf, SystemReadError> {
    if let Some(home) = home.filter(|home| !home.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    use nix::unistd::{Uid, User};
    User::from_uid(Uid::current())
        .map_err(|_| SystemReadError::UnavailableDirectory)?
        .map(|user| user.dir)
        .ok_or(SystemReadError::UnavailableDirectory)
}

#[cfg(not(unix))]
fn current_home() -> Result<PathBuf, SystemReadError> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .ok_or(SystemReadError::UnavailableDirectory)
}

#[cfg(unix)]
fn named_home(user: &str) -> Result<Option<PathBuf>, SystemReadError> {
    nix::unistd::User::from_name(user)
        .map(|user| user.map(|user| user.dir))
        .map_err(|_| SystemReadError::UnavailableDirectory)
}

#[cfg(not(unix))]
fn named_home(_user: &str) -> Result<Option<PathBuf>, SystemReadError> {
    Ok(None)
}

fn resolve_non_strict(path: &Path) -> Result<PathBuf, SystemReadError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(directory_io_error)?
            .join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
            std::path::Component::RootDir => resolved.push(component.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            std::path::Component::Normal(name) => {
                let candidate = resolved.join(name);
                match fs::symlink_metadata(&candidate) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        resolved = fs::canonicalize(candidate).map_err(directory_io_error)?;
                    }
                    Ok(_) => resolved.push(name),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        resolved.push(name);
                    }
                    Err(error) => return Err(directory_io_error(error)),
                }
            }
        }
    }
    Ok(resolved)
}

fn directory_io_error(error: std::io::Error) -> SystemReadError {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        SystemReadError::PermissionDenied
    } else {
        SystemReadError::UnavailableDirectory
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_visible_directories_in_case_insensitive_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = std::env::temp_dir().join(format!("system-reads-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root)?;
        }
        fs::create_dir_all(root.join("zeta"))?;
        fs::create_dir_all(root.join("Alpha"))?;
        fs::create_dir_all(root.join(".hidden"))?;
        fs::write(root.join("file.txt"), "file")?;

        let path = root
            .to_str()
            .ok_or_else(|| std::io::Error::other("temporary directory path is not UTF-8"))?;
        let listing = directories(Some(path))?;
        assert_eq!(
            listing
                .directories
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["Alpha", "zeta"]
        );
        let parent = root
            .parent()
            .and_then(Path::to_str)
            .ok_or_else(|| std::io::Error::other("temporary directory has no UTF-8 parent"))?;
        assert_eq!(listing.parent.as_deref(), Some(parent));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn rejects_a_missing_directory() {
        assert_eq!(
            directories(Some("/definitely/not/a/studio/directory")),
            Err(SystemReadError::UnavailableDirectory)
        );
    }

    #[cfg(unix)]
    #[test]
    fn empty_home_uses_current_user_and_unknown_tilde_user_is_rejected() {
        use nix::unistd::{Uid, User};
        let expected = User::from_uid(Uid::current())
            .expect("uid lookup succeeds")
            .expect("current user exists")
            .dir;
        assert_eq!(current_home_for(None), Ok(expected.clone()));
        assert_eq!(expand_home("~//tmp"), Ok(expected.join("tmp")));
        assert_eq!(
            expand_home("~studio-user-that-does-not-exist"),
            Err(SystemReadError::UnavailableDirectory)
        );
    }

    #[test]
    fn permission_errors_keep_their_category() {
        assert_eq!(
            directory_io_error(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            SystemReadError::PermissionDenied
        );
    }
}
