use super::*;

pub fn peer_team_write(
    store: &RuntimeStore,
    body: &Value,
    default_account: &str,
    known_accounts: &[Value],
) -> Result<Value, ProjectsError> {
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if action == "radio" {
        return crate::radio_runtime::manage_http(store, body);
    }
    if action == "convert" {
        return crate::peer_conversion::convert(store, body);
    }
    if !matches!(action, "save" | "delete" | "move") {
        return Err(ProjectsError::Invalid("unknown peer team action"));
    }
    let path = project_key(body.get("path"), false)?;
    let revision = body
        .get("expected_revision")
        .and_then(Value::as_u64)
        .ok_or(ProjectsError::Invalid("peer team revision is invalid"))?;
    let raw_request = body
        .get("request_id")
        .and_then(Value::as_str)
        .ok_or(ProjectsError::Invalid("peer team request ID is required"))?;
    let request_id = python_strip(raw_request);
    if request_id.is_empty() || request_id.chars().count() > 255 {
        return Err(ProjectsError::Invalid("peer team request ID is invalid"));
    }
    let team_id = body
        .get("team_id")
        .and_then(Value::as_str)
        .map(|value| {
            Uuid::parse_str(value)
                .map(|id| id.to_string())
                .map_err(|_| ProjectsError::Invalid("peer team ID is invalid"))
        })
        .transpose()?;
    if (action != "move" || body.get("team_id").is_some_and(|value| !value.is_null()))
        && team_id.is_none()
    {
        return Err(ProjectsError::Invalid("peer team ID is invalid"));
    }
    let name = if action == "save" {
        let value = body
            .get("name")
            .and_then(Value::as_str)
            .ok_or(ProjectsError::Invalid("peer team name is invalid"))?;
        let value = python_strip(value);
        if value.is_empty() || value.chars().count() > 80 {
            return Err(ProjectsError::Invalid("peer team name is invalid"));
        }
        Some(value.to_owned())
    } else {
        None
    };
    let members = body
        .get("members")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if action == "save"
        && (members.len() < 2
            || members
                .iter()
                .any(|value| value.as_str().is_none_or(str::is_empty))
            || members
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                != members.len())
    {
        return Err(ProjectsError::Invalid(
            "select at least two different lead chats",
        ));
    }
    let signature =
        sha256_hex(python_json(&json!({"operation":"peer-team","body":body})).as_bytes());
    let key = format!("peer-team:{request_id}");
    let prepared = PreparedHost::load(store)?;
    prepared.recheck_paths()?;
    store.transaction_with(|tx| {
        prepared.recheck(tx)?;
        if let Some(result) = replay(tx, &key, &signature, "peer team")? { return Ok(result); }
        let mut project = match load_project(tx, &path)? {
            Some(project) => project,
            None if path.starts_with("project:") => return Err(ProjectsError::Invalid("select an existing project")),
            None => { let account = project_account(&prepared, tx, &path, default_account)?; new_project(&prepared, &path, &project_name(None, &path)?, &account, known_accounts)? }
        };
        let current = revision_of(Some(&project), "peerTeamsRevision", &path)?;
        if revision != current { return Err(ProjectsError::Conflict("Peer teams changed. Reload before saving")); }
        let raw_teams = project.get("peerTeams").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut previous_ids = raw_teams.iter().map(|team| {
            let value = team.get("id").and_then(Value::as_str)
                .ok_or_else(|| ProjectsError::InvalidStored { table: "runtime_projects", id: path.clone() })?;
            Uuid::parse_str(value).map(|uuid| uuid.to_string())
                .map_err(|_| ProjectsError::InvalidStored { table: "runtime_projects", id: path.clone() })
        }).collect::<Result<std::collections::HashSet<_>,_>>()?;
        let agents = project_agents(tx)?;
        let mut teams = live_teams(&raw_teams, &agents, &path, &prepared)?;
        let mut statement = tx.prepare("SELECT id FROM sync_entities WHERE collection='peerTeam' AND json_extract(payload,'$.value.projectPath')=?1")?;
        previous_ids.extend(statement.query_map([&path], |row| row.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?);
        drop(statement);
        match action {
            "save" => {
                let team_id = team_id.as_deref().ok_or(ProjectsError::Invalid("peer team ID is invalid"))?;
                for member in &members {
                    let id = member.as_str().unwrap_or_default();
                    if !agents.get(id).is_some_and(|agent| is_project_lead(agent, &path, &prepared)) { return Err(ProjectsError::Invalid("select live lead chats from this project only")); }
                }
                if teams.iter().any(|team| team.get("id").and_then(Value::as_str) != Some(team_id)
                    && team.get("members").and_then(Value::as_array).is_some_and(|existing| existing.iter().any(|member| members.contains(member)))) {
                    return Err(ProjectsError::Invalid("a chat already belongs to another peer team"));
                }
                let selected = json!({"id":team_id,"name":name,"members":members});
                if let Some(old) = teams.iter_mut().find(|team| team.get("id").and_then(Value::as_str) == Some(team_id)) { *old = selected; } else { teams.push(selected); }
            }
            "delete" => teams.retain(|team| team.get("id").and_then(Value::as_str) != team_id.as_deref()),
            "move" => {
                let member = python_strip(body.get("member").and_then(Value::as_str).ok_or(ProjectsError::Invalid("peer team member is required"))?);
                if member.is_empty() || member.chars().count() > 255 || !agents.get(member).is_some_and(|agent| is_project_lead(agent, &path, &prepared)) { return Err(ProjectsError::Invalid("select a live lead chat from this project only")); }
                let source = teams.iter().position(|team| team.get("members").and_then(Value::as_array).is_some_and(|items| items.iter().any(|item| item.as_str() == Some(member))));
                let target = team_id.as_deref().and_then(|id| teams.iter().position(|team| team.get("id").and_then(Value::as_str) == Some(id)));
                if team_id.is_some() && target.is_none() { return Err(ProjectsError::Invalid("the destination team is no longer available")); }
                if source == target { return Err(ProjectsError::Invalid("the chat already has this team membership")); }
                if let Some(index) = source
                    && let Some(list) = teams[index]["members"].as_array_mut()
                { list.retain(|item| item.as_str() != Some(member)); }
                if let Some(index) = target
                    && let Some(list) = teams[index]["members"].as_array_mut()
                { list.push(json!(member)); }
                teams.retain(|team| team.get("members").and_then(Value::as_array).is_some_and(|items| items.len() >= 2));
            }
            _ => return Err(ProjectsError::Invalid("unknown peer team action")),
        }
        project["peerTeams"] = json!(teams);
        project["peerTeamsRevision"] = json!(current + 1);
        project["updated"] = json!(now_seconds());
        ProjectsRepository::put_project(tx, &project)?;
        let team_values = project.get("peerTeams").and_then(Value::as_array).cloned().unwrap_or_default();
        let live = live_teams(&team_values, &agents, &path, &prepared)?;
        let mut live_ids = std::collections::HashSet::new();
        for team in &live {
            let id = team.get("id").and_then(Value::as_str).ok_or(ProjectsError::InvalidStored { table: "runtime_projects", id: path.clone() })?;
            live_ids.insert(id.to_owned());
            SyncDocumentsRepository::put_entity(tx, "peerTeam", id, peer_team_projection(team, &path, current + 1)?, false)?;
        }
        for id in previous_ids.difference(&live_ids) { SyncDocumentsRepository::put_entity(tx, "peerTeam", id, json!({}), true)?; }
        refresh_project_rooms(tx, &path)?;
        save_operation_receipt(tx, &key, &signature, &project)?;
        Ok(project)
    })
}

pub(crate) fn replay(
    tx: &studio_runtime_store::Transaction<'_>,
    key: &str,
    signature: &str,
    subject: &'static str,
) -> Result<Option<Value>, ProjectsError> {
    read_receipt(tx, Some(key), signature, "runtime_projects", key, |_| true).map_err(|error| {
        match error {
            EventWriteError::Conflict(_) => ProjectsError::Invalid(match subject {
                "sidebar" => "sidebar request ID was reused",
                _ => "peer team request ID was reused",
            }),
            EventWriteError::InvalidStoredObject { object, id } => {
                ProjectsError::InvalidStored { table: object, id }
            }
            EventWriteError::Store(error) => ProjectsError::Store(error),
            _ => ProjectsError::Invalid("operation receipt is invalid"),
        }
    })
}

pub(crate) fn save_operation_receipt(
    tx: &studio_runtime_store::Transaction<'_>,
    key: &str,
    signature: &str,
    result: &Value,
) -> Result<(), ProjectsError> {
    save_receipt(tx, Some(key), signature, result).map_err(|error| match error {
        EventWriteError::Store(error) => ProjectsError::Store(error),
        EventWriteError::InvalidStoredObject { object, id } => {
            ProjectsError::InvalidStored { table: object, id }
        }
        _ => ProjectsError::Invalid("operation receipt could not be saved"),
    })
}

pub(crate) fn project_agents(
    tx: &studio_runtime_store::Transaction<'_>,
) -> Result<std::collections::HashMap<String, Value>, ProjectsError> {
    ProjectsRepository::agents(tx)?
        .into_iter()
        .map(|(id, agent)| {
            check_id(&agent, "runtime_agents", &id)?;
            Ok((id, agent))
        })
        .collect()
}

pub(crate) fn is_project_lead(agent: &Value, path: &str, prepared: &PreparedHost) -> bool {
    agent.get("isLead").and_then(Value::as_bool) == Some(true)
        && agent.get("deletedAt").is_none_or(Value::is_null)
        && agent.get("parentId").is_none_or(Value::is_null)
        && agent.get("rootId").and_then(Value::as_str) == agent.get("id").and_then(Value::as_str)
        && agent
            .get("cwd")
            .and_then(Value::as_str)
            .is_some_and(|cwd| prepared.resolve(cwd).is_ok_and(|resolved| resolved == path))
}

pub(crate) fn live_teams(
    raw: &[Value],
    agents: &std::collections::HashMap<String, Value>,
    path: &str,
    prepared: &PreparedHost,
) -> Result<Vec<Value>, ProjectsError> {
    raw.iter()
        .map(|team| {
            let invalid = || ProjectsError::InvalidStored {
                table: "runtime_projects",
                id: path.to_owned(),
            };
            let id = team.get("id").and_then(Value::as_str).ok_or_else(invalid)?;
            let id = Uuid::parse_str(id).map_err(|_| invalid())?.to_string();
            let name = team
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(invalid)?;
            let members = team
                .get("members")
                .and_then(Value::as_array)
                .ok_or_else(invalid)?;
            let members = members
                .iter()
                .filter_map(Value::as_str)
                .filter(|member| {
                    agents
                        .get(*member)
                        .is_some_and(|agent| is_project_lead(agent, path, prepared))
                })
                .collect::<Vec<_>>();
            Ok((members.len() >= 2).then(|| json!({"id":id,"name":name,"members":members})))
        })
        .filter_map(|result| match result {
            Ok(Some(team)) => Some(Ok(team)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

pub(crate) fn peer_team_projection(
    team: &Value,
    path: &str,
    revision: u64,
) -> Result<Value, ProjectsError> {
    Ok(
        json!({"id":team.get("id").and_then(Value::as_str).ok_or(ProjectsError::Invalid("stored peer team is invalid"))?,
        "name":team.get("name").and_then(Value::as_str).ok_or(ProjectsError::Invalid("stored peer team is invalid"))?,
        "projectPath":path,"members":team.get("members").ok_or(ProjectsError::Invalid("stored peer team is invalid"))?,"revision":revision}),
    )
}

pub(crate) fn refresh_project_rooms(
    tx: &studio_runtime_store::Transaction<'_>,
    path: &str,
) -> Result<(), ProjectsError> {
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_rooms')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(());
    }
    let mut statement = tx.prepare("SELECT id FROM runtime_rooms WHERE json_extract(record,'$.kind')='private' AND json_extract(record,'$.projectPath')=?1")?;
    let ids = statement
        .query_map([path], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for id in ids {
        ProjectsRepository::refresh_private_room(tx, &id)?;
    }
    Ok(())
}

pub(super) fn reorder(store: &RuntimeStore, body: &Value) -> Result<Value, ProjectsError> {
    let request_id = python_strip(
        body.get("request_id")
            .and_then(Value::as_str)
            .ok_or(ProjectsError::Invalid("request ID is required"))?,
    );
    if request_id.is_empty() || request_id.chars().count() > 255 {
        return Err(ProjectsError::Invalid("request ID is invalid"));
    }
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
    let signature =
        sha256_hex(python_json(&json!({"operation":"sidebar-order","body":body})).as_bytes());
    let key = format!("sidebar-order:{request_id}");
    store.transaction_with(|tx| -> Result<Value, ProjectsError> {
        if let Some(result) = replay(tx, &key, &signature, "sidebar")? {
            return Ok(result);
        }
        let current = ProjectsRepository::sidebar_order(tx)?;
        let revision = current.get("revision").and_then(Value::as_u64).ok_or(
            ProjectsError::InvalidStored {
                table: "runtime_sidebar_order",
                id: "current".to_owned(),
            },
        )?;
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
        save_operation_receipt(tx, &key, &signature, &result)?;
        Ok(result)
    })
}
