//! Transactional chat organization effects from `/api/organization`.

use rusqlite::OptionalExtension;
use serde_json::{Map, Value, json};
use studio_runtime_store::{AgentRecord, AgentRepository, Transaction};

use crate::event_write::{self, EventWriteError};

pub fn apply(tx: &Transaction<'_>, body: &Value) -> Result<Value, EventWriteError> {
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .ok_or(EventWriteError::InvalidRequest("Invalid request"))?;
    let mut agent = event_write::checked_agent(tx, id)?;
    let allowed = [
        "id",
        "read_state",
        "project_folder",
        "project_path",
        "expected_revision",
        "expected_folder",
        "pinned",
        "archived",
        "project",
    ];
    if body
        .as_object()
        .is_none_or(|o| o.keys().any(|key| !allowed.contains(&key.as_str())))
    {
        return Err(EventWriteError::InvalidRequest(
            "Unknown chat organization field",
        ));
    }
    if let Some(read) = body.get("read_state") {
        return apply_read_state(tx, agent, body, read);
    }
    if body.get("project_folder").is_some() {
        if agent.extra.get("isLead").and_then(Value::as_bool) != Some(true)
            || body.get("project_path").and_then(Value::as_str)
                != agent.extra.get("cwd").and_then(Value::as_str)
        {
            return Err(EventWriteError::InvalidRequest(
                "Select a chat in this project",
            ));
        }
        let folder = body
            .get("project_folder")
            .ok_or(EventWriteError::InvalidRequest("Select a project folder"))?;
        let desired = if folder.is_null() {
            None
        } else {
            let folder_id = folder
                .as_str()
                .ok_or(EventWriteError::InvalidRequest("Select a project folder"))?;
            let path = agent
                .extra
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let project = tx
                .query_row(
                    "SELECT record FROM runtime_projects WHERE id=?1",
                    [path],
                    |r| r.get::<_, String>(0),
                )
                .optional()
                .map_err(studio_runtime_store::StoreError::from)?
                .unwrap_or_else(|| "{}".to_owned());
            let project: Value = serde_json::from_str(&project).map_err(|_| {
                EventWriteError::InvalidStoredObject {
                    object: "runtime_projects",
                    id: path.to_owned(),
                }
            })?;
            let exists = project
                .get("folders")
                .and_then(Value::as_array)
                .is_some_and(|folders| {
                    folders
                        .iter()
                        .any(|f| f.get("id").and_then(Value::as_str) == Some(folder_id))
                });
            if !exists {
                return Err(EventWriteError::InvalidRequest(
                    "This folder is not in the selected project",
                ));
            }
            Some(folder_id.to_owned())
        };
        let current = agent
            .extra
            .get("projectFolderRevision")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let revision = body
            .get("expected_revision")
            .and_then(Value::as_i64)
            .filter(|v| *v >= 0)
            .ok_or(EventWriteError::InvalidRequest(
                "Supply the current chat folder revision",
            ))?;
        let current_folder = agent
            .extra
            .get("projectFolder")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let replay = current_folder == desired && (revision == current || revision == current - 1);
        if !replay
            && (revision != current
                || body.get("expected_folder").and_then(Value::as_str) != current_folder.as_deref())
        {
            return Err(EventWriteError::InvalidRequest(
                "The chat folder changed. Reload it before moving",
            ));
        }
        if current_folder != desired {
            let revision = current
                .checked_add(1)
                .ok_or(EventWriteError::InvalidRequest("The chat folder revision is invalid"))?;
            agent.extra.insert("projectFolderRevision".into(), json!(revision));
        }
        match desired {
            Some(folder) => {
                agent.extra.insert("projectFolder".into(), json!(folder));
            }
            None => {
                agent.extra.insert("projectFolder".into(), Value::Null);
            }
        }
    }
    for field in ["pinned", "archived"] {
        if let Some(value) = body.get(field) {
            let enabled = value.as_bool().ok_or(EventWriteError::InvalidRequest(
                "Supply a boolean for organization setting",
            ))?;
            if field == "archived"
                && enabled
                && (agent.extra.get("inFlight").and_then(Value::as_bool) == Some(true)
                    || active_monitor(tx, id)?)
            {
                return Err(EventWriteError::InvalidRequest(
                    "Stop active work before archiving this conversation",
                ));
            }
            agent.extra.insert(field.into(), json!(enabled));
        }
    }
    if let Some(project) = body.get("project") {
        let value = project
            .as_str()
            .ok_or(EventWriteError::InvalidRequest("Supply a project name"))?;
        if value.chars().count() > 100 {
            return Err(EventWriteError::InvalidRequest("Supply a project name"));
        }
        agent.extra.insert("project".into(), json!(value.trim()));
    }
    AgentRepository::put(tx, &agent)?;
    Ok(agent.value())
}

fn active_monitor(tx: &Transaction<'_>, id: &str) -> Result<bool, EventWriteError> {
    Ok(tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_monitors WHERE json_extract(record,'$.agent')=?1 AND json_extract(record,'$.status') IN ('running','approval','starting'))", [id], |r| r.get::<_, bool>(0)).map_err(studio_runtime_store::StoreError::from)?)
}

fn apply_read_state(
    tx: &Transaction<'_>,
    mut agent: AgentRecord,
    body: &Value,
    read: &Value,
) -> Result<Value, EventWriteError> {
    if body.as_object().is_none_or(|o| {
        o.keys()
            .any(|k| !["id", "read_state"].contains(&k.as_str()))
    }) {
        return Err(EventWriteError::InvalidRequest(
            "Change read state separately from other chat settings",
        ));
    }
    if body.get("id").is_some_and(|value| value.as_str() != Some(agent.id.0.as_str())) {
        return Err(EventWriteError::InvalidRequest("The chat identity changed"));
    }
    let fields = ["thread_id", "turn_id", "read", "expected_revision"];
    if read
        .as_object()
        .is_none_or(|o| o.len() != 4 || o.keys().any(|k| !fields.contains(&k.as_str())))
    {
        return Err(EventWriteError::InvalidRequest(
            "Supply the result identity, read state, and revision",
        ));
    }
    let thread = read
        .get("thread_id")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or(EventWriteError::InvalidRequest(
            "Supply a nonempty result identity",
        ))?;
    let turn = read
        .get("turn_id")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or(EventWriteError::InvalidRequest(
            "Supply a nonempty result identity",
        ))?;
    let read_value =
        read.get("read")
            .and_then(Value::as_bool)
            .ok_or(EventWriteError::InvalidRequest(
                "Supply a boolean read state",
            ))?;
    let revision = read
        .get("expected_revision")
        .and_then(Value::as_i64)
        .filter(|v| *v >= 0)
        .ok_or(EventWriteError::InvalidRequest(
            "Supply the current read state revision",
        ))?;
    if agent.extra.get("threadId").and_then(Value::as_str) != Some(thread)
        || agent.extra.get("lastCompletedTurn").and_then(Value::as_str) != Some(turn)
        || agent
            .extra
            .get("lastCompletedTurnStatus")
            .and_then(Value::as_str)
            != Some("completed")
    {
        return Err(EventWriteError::InvalidRequest(
            "The completed result changed. Reload the chat",
        ));
    }
    let previous = agent
        .extra
        .get("readState")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let current = match previous.get("revision") {
        None => 0,
        Some(value) => value
            .as_i64()
            .filter(|revision| *revision >= 0)
            .ok_or(EventWriteError::InvalidRequest(
                "The saved read state revision is invalid",
            ))?,
    };
    let same = previous.get("threadId").and_then(Value::as_str) == Some(thread)
        && previous.get("turnId").and_then(Value::as_str) == Some(turn)
        && previous.get("read").and_then(Value::as_bool) == Some(read_value);
    if same && (revision == current || revision == current - 1) {
        return Ok(agent.value());
    }
    if revision != current {
        return Err(EventWriteError::InvalidRequest(
            "The read state changed. Reload the chat",
        ));
    }
    let mut state = Map::new();
    state.insert("threadId".into(), json!(thread));
    state.insert("turnId".into(), json!(turn));
    state.insert("read".into(), json!(read_value));
    state.insert("revision".into(), json!(current + 1));
    agent.extra.insert("readState".into(), Value::Object(state));
    AgentRepository::put(tx, &agent)?;
    Ok(agent.value())
}
