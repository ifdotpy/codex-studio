//! Local filesystem reads used by the system routes.

use std::fs;
use std::path::{Path, PathBuf};

use crate::event_write::truthy;
use serde_json::{Value, json};
use studio_runtime_store::{StoreError, system_reads as records};
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
    if let Some(home) = home {
        return Ok(if home.is_empty() {
            PathBuf::from("/")
        } else {
            PathBuf::from(home)
        });
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

pub(crate) fn resolve_path(path: &str) -> Result<PathBuf, SystemReadError> {
    resolve_non_strict(&expand_home(path)?)
}

fn resolve_non_strict(path: &Path) -> Result<PathBuf, SystemReadError> {
    resolve_links(path, &mut 0)
}

fn resolve_links(path: &Path, links: &mut usize) -> Result<PathBuf, SystemReadError> {
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
                        *links += 1;
                        if *links > 40 {
                            return Err(SystemReadError::UnavailableDirectory);
                        }
                        let target = fs::read_link(candidate).map_err(directory_io_error)?;
                        let target = if target.is_absolute() {
                            target
                        } else {
                            resolved.join(target)
                        };
                        resolved = resolve_links(&target, links)?;
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

pub fn ui_summary(
    connection: &rusqlite::Transaction<'_>,
    account_snapshot: Option<crate::account_policy::AccountsSnapshot>,
) -> Result<Value, StoreError> {
    let accounts=account_snapshot.map_or_else(Vec::new,|snapshot|snapshot.accounts.iter().filter_map(|account|{
        let provider=account["provider"].as_str().unwrap_or("codex");
        matches!(provider,"codex"|"claude").then(||json!({"provider":provider,"email":account["email"],"plan":account["plan"],"status":if truthy(account.get("disconnected")){"signedOut"}else{account["status"].as_str().unwrap_or("error")},"label":account["label"].as_str().unwrap_or("Account"),"isDefault":account["id"]==snapshot.default_account_key}))
    }).collect::<Vec<_>>());
    let system = if cfg!(target_os = "macos") {
        "Darwin"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "Linux"
    };
    let mut result = json!({"ready":false,"busy":false,"system":system,"agentsRunning":0,"projects":[],"chats":[],"alerts":[],"accounts":accounts});
    if !records::exists(connection, "sync_entities")? {
        return Ok(result);
    }
    let mut statement=connection.prepare("SELECT collection,id,payload FROM sync_entities WHERE deleted=0 AND collection IN ('agent','project','room','request','complaint') ORDER BY collection,id")?;
    let mut values = std::collections::BTreeMap::<String, Vec<Value>>::new();
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (collection, id, raw) = row?;
        let value = records::decode("sync_entities", &id, &raw)?;
        values
            .entry(collection)
            .or_default()
            .push(value["value"].clone());
    }
    let rows = |name: &str| values.get(name).map(Vec::as_slice).unwrap_or(&[]);
    let agents = rows("agent")
        .iter()
        .filter(|a| !truthy(a.get("deletedAt")))
        .map(|a| (a["id"].as_str().unwrap_or_default(), a))
        .collect::<std::collections::BTreeMap<_, _>>();
    let unread = agents
        .iter()
        .filter(|(_, a)| {
            truthy(a.get("threadId"))
                && truthy(a.get("lastCompletedTurn"))
                && a["lastCompletedTurnStatus"] == "completed"
                && !(a["readState"]["read"] == true
                    && a["readState"]["threadId"] == a["threadId"]
                    && a["readState"]["turnId"] == a["lastCompletedTurn"])
        })
        .map(|(id, _)| *id)
        .collect::<std::collections::BTreeSet<_>>();
    let running = agents
        .values()
        .filter(|a| truthy(a.get("inFlight")))
        .count();
    let mut projects = Vec::new();
    for project in rows("project").iter().filter(|p| truthy(p.get("path"))) {
        let mut p = json!({});
        copy_fields(
            &mut p,
            project,
            "id homeServerId locations projectAliases path name",
            false,
        );
        if let Some(index) = projects.iter().position(|v: &Value| v["path"] == p["path"]) {
            projects[index] = p;
        } else {
            projects.push(p);
        }
    }
    let team_members = rows("project")
        .iter()
        .flat_map(|p| p["peerTeams"].as_array().into_iter().flatten())
        .flat_map(|team| team["members"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let mut chats = Vec::new();
    let mut alerts = Vec::new();
    for (id, agent) in &agents {
        if agent["source"] == "managed"
            && truthy(agent.get("isLead"))
            && !truthy(agent.get("sharedRoomId"))
            && !truthy(agent.get("remoteAnchor"))
        {
            let path = fallback(agent, "hostProjectPath", fallback(agent, "cwd", ""));
            if !projects.iter().any(|p| {
                (truthy(p.get("id"))
                    && p["id"] == agent["projectId"]
                    && p["homeServerId"] == agent["projectServerId"])
                    || p["locations"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|l| l["path"] == path && l["serverId"] == agent["serverId"])
            }) {
                ensure_project(&mut projects, path);
            }
            let mut chat = json!({"id":id,"name":fallback(agent,"name","Chat"),"path":path,"archived":truthy(agent.get("archived")),"status":fallback(agent,"status",""),"unread":unread.contains(id),"inFlight":truthy(agent.get("inFlight")),"pinned":truthy(agent.get("pinned")),"team":team_members.contains(id)});
            copy_fields(
                &mut chat,
                agent,
                "serverId projectId projectServerId provider created updated",
                true,
            );
            chats.push(chat);
        }
        if !truthy(agent.get("isLead")) || truthy(agent.get("inFlight")) {
            continue;
        }
        let turn = fallback(agent, "lastCompletedTurn", "");
        let stopped = matches!(agent["status"].as_str(), Some("failed" | "interrupted"));
        let completed = agent["status"] == "completed"
            && !turn.is_empty()
            && agent["lastCompletedTurnStatus"] == "completed";
        if stopped || completed {
            let (kind, title) = if stopped {
                ("failed", "Work stopped")
            } else {
                ("completed", "Reply ready")
            };
            let body = if stopped {
                agent["error"]
                    .get("message")
                    .unwrap_or(&agent["error"])
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("Open the chat to check the error.")
            } else {
                fallback(agent, "tail", "Open the chat to read the reply.")
            };
            let turn = fallback(
                agent,
                "lastCompletedTurn",
                fallback(agent, "turnId", fallback(agent, "status", "")),
            );
            alert(
                &mut alerts,
                format!("{kind}:{id}:{turn}"),
                agent,
                title,
                body,
                None,
            );
        }
    }
    for request in rows("request")
        .iter()
        .filter(|r| r["status"] == "pending" && !truthy(r.get("deferred")))
    {
        let question = request["params"]["questions"]
            .as_array()
            .and_then(|q| q.first())
            .and_then(|q| q["question"].as_str())
            .filter(|q| !q.is_empty());
        if let Some(agent) = agents.get(request["agent"].as_str().unwrap_or_default()) {
            alert(
                &mut alerts,
                format!("request:{}", fallback(request, "id", "")),
                agent,
                if question.is_some() {
                    "Question for you"
                } else {
                    "Your approval is needed"
                },
                question.unwrap_or("Open the request to review it."),
                request["id"].as_str(),
            );
        }
    }
    for complaint in rows("complaint")
        .iter()
        .filter(|c| truthy(c.get("needsUserResponse")))
    {
        if let Some(agent) = agents.get(complaint["leadId"].as_str().unwrap_or_default()) {
            alert(
                &mut alerts,
                format!(
                    "complaint:{}:{}",
                    fallback(complaint, "id", ""),
                    complaint["version"].as_i64().unwrap_or(0)
                ),
                agent,
                "Message for you",
                fallback(complaint, "title", "Open the message to read it."),
                complaint["id"].as_str(),
            );
        }
    }
    for room in rows("room")
        .iter()
        .filter(|r| truthy(r.get("radio")) && !truthy(r.get("userHidden")))
    {
        let path = fallback(room, "projectPath", "");
        ensure_project(&mut projects, path);
        chats.push(json!({"id":room["id"],"name":fallback(room,"name","Shared chat"),"path":path,"archived":false,"status":"","unread":room["members"].as_array().into_iter().flatten().filter_map(Value::as_str).any(|id|unread.contains(id))}));
    }
    result["ready"] = json!(true);
    result["busy"] = json!(running > 0);
    result["agentsRunning"] = json!(running);
    result["projects"] = json!(projects);
    result["chats"] = json!(chats);
    result["alerts"] = json!(alerts);
    Ok(result)
}
fn copy_fields(target: &mut Value, source: &Value, keys: &str, include_missing: bool) {
    for key in keys.split_whitespace() {
        if include_missing || source.get(key).is_some() {
            target[key] = source[key].clone();
        }
    }
}
fn fallback<'a>(value: &'a Value, key: &str, default: &'a str) -> &'a str {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(default)
}
fn ensure_project(projects: &mut Vec<Value>, path: &str) {
    if !projects.iter().any(|p| p["path"] == path) {
        projects.push(json!({"path":path,"name":if path.is_empty(){"Other chats"}else{path}}));
    }
}
fn alert(
    alerts: &mut Vec<Value>,
    id: String,
    agent: &Value,
    title: &str,
    body: &str,
    item: Option<&str>,
) {
    if truthy(agent.get("remoteAnchor")) {
        return;
    }
    alerts.push(json!({"id":id,"title":format!("{}: {title}",fallback(agent,"name","Chat")).chars().take(160).collect::<String>(),"body":body.chars().take(2000).collect::<String>(),"target":{"agentId":agent["id"],"itemId":item}}));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn missing_and_empty_home_follow_expanduser_without_environment_mutation() {
        assert_eq!(
            current_home_for(Some("".into())).expect("fixture operation succeeds"),
            PathBuf::from("/")
        );
        assert_eq!(
            current_home_for(Some("/fixture".into())).expect("fixture operation succeeds"),
            PathBuf::from("/fixture")
        );
        let user = nix::unistd::User::from_uid(nix::unistd::Uid::current())
            .expect("fixture operation succeeds")
            .expect("fixture operation succeeds");
        assert_eq!(
            current_home_for(None).expect("fixture operation succeeds"),
            user.dir
        );
        assert_eq!(
            named_home(&user.name).expect("fixture operation succeeds"),
            Some(user.dir)
        );
    }

    #[test]
    fn account_projection_preserves_provider_order_and_empty_labels() {
        let mut connection =
            rusqlite::Connection::open_in_memory().expect("fixture operation succeeds");
        let snapshot = crate::account_policy::AccountsSnapshot {
            accounts: vec![
                json!({"id":"z","provider":"claude","status":"","label":"","email":null,"plan":null}),
                json!({"id":"a","provider":"codex","disconnected":true,"label":"A"}),
                json!({"id":"other","provider":"unsupported"}),
            ],
            archived_accounts: vec![],
            default_account_key: json!("a"),
            logins: vec![],
            known_account_keys: Default::default(),
        };
        let tx = connection.transaction().expect("read snapshot opens");
        let value = ui_summary(&tx, Some(snapshot)).expect("fixture operation succeeds");
        assert_eq!(
            value["accounts"],
            json!([
                {"provider":"claude","status":"","label":"","email":null,"plan":null,"isDefault":false},
                {"provider":"codex","status":"signedOut","label":"A","email":null,"plan":null,"isDefault":true}
            ])
        );
    }
}
