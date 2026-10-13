//! Persisted context needed to build canonical renderer entity views.

use rusqlite::{OptionalExtension, Transaction};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::BufReader;
use std::path::Path;
use studio_runtime_store::StoreResult;

use crate::event_write::truthy;
use crate::sync_policy::{stored_records, table_exists};
use crate::sync_views::{AgentViewContext, RoomViewContext};

#[derive(Deserialize)]
struct LocalIdentity {
    #[serde(rename = "serverId")]
    server_id: String,
}

pub(crate) fn local_server_id(state_dir: &str) -> StoreResult<Option<String>> {
    let directory = Path::new(state_dir).join("multi-server");
    let directory_metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !directory_metadata.is_dir() || directory_metadata.file_type().is_symlink() {
        return Err(invalid_identity());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if directory_metadata.permissions().mode() & 0o077 != 0 {
            return Err(invalid_identity());
        }
    }

    let path = directory.join("identity.json");
    let path_metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !path_metadata.is_file()
        || path_metadata.file_type().is_symlink()
        || path_metadata.len() > 16_384
    {
        return Err(invalid_identity());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path_metadata.permissions().mode() & 0o077 != 0 {
            return Err(invalid_identity());
        }
    }
    let file = File::open(&path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid_identity());
    }
    let identity: LocalIdentity =
        serde_json::from_reader(BufReader::new(file)).map_err(|_| invalid_identity())?;
    Ok((!identity.server_id.is_empty()).then_some(identity.server_id))
}

fn invalid_identity() -> studio_runtime_store::StoreError {
    studio_runtime_store::StoreError::InvalidSyncObject {
        scope: "multi-server".to_owned(),
        id: "identity.json".to_owned(),
    }
}

pub(crate) fn agent_view_context(
    transaction: &Transaction<'_>,
    record: &Value,
    project_server_id: Option<&str>,
) -> StoreResult<AgentViewContext> {
    let id = record.get("id").and_then(Value::as_str).unwrap_or_default();
    let empty_lead = truthy(record.get("isLead"))
        && !truthy(record.get("deletedAt"))
        && !truthy(record.get("accountTransferId"))
        && record.get("status").and_then(Value::as_str) == Some("idle")
        && !truthy(record.get("threadId"))
        && !truthy(record.get("prompt"))
        && !exists_for_agent(transaction, "runtime_events", "agent", id)?
        && !exists_for_agent(transaction, "runtime_items", "agent", id)?
        && !transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_agents WHERE json_extract(record,'$.parentId')=?1)",
            [id],
            |row| row.get::<_, bool>(0),
        )?
        && !exists_for_agent(transaction, "runtime_monitors", "json_extract(record,'$.agent')", id)?;
    let mut latest: Option<(f64, String)> = None;
    let mut statement = transaction
        .prepare("SELECT record FROM runtime_work WHERE json_extract(record,'$.owner')=?1")?;
    let rows = statement
        .query_map([id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for raw in rows {
        let work: Value = serde_json::from_str(&raw)?;
        for result in work
            .get("results")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if result.get("agent").and_then(Value::as_str) != Some(id) {
                continue;
            }
            let Some(path) = result.get("resultFile").and_then(Value::as_str) else {
                continue;
            };
            if path.is_empty() {
                continue;
            }
            let created = result
                .get("created")
                .and_then(Value::as_f64)
                .unwrap_or_default();
            if latest.as_ref().is_none_or(|(prior, _)| created > *prior) {
                latest = Some((created, path.to_owned()));
            }
        }
    }
    Ok(AgentViewContext {
        project_server_id: project_server_id.map(str::to_owned),
        launcher_alive: true,
        empty_lead,
        latest_work_result_file: latest.map(|(_, path)| path),
    })
}

fn exists_for_agent(
    transaction: &Transaction<'_>,
    table: &str,
    agent_column: &str,
    agent_id: &str,
) -> StoreResult<bool> {
    if !table_exists(transaction, table)? {
        return Ok(false);
    }
    let sql = format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE {agent_column}=?1)");
    Ok(transaction.query_row(&sql, [agent_id], |row| row.get(0))?)
}

pub(crate) fn room_view_context(
    transaction: &Transaction<'_>,
    record: &Value,
    agents: &[Value],
) -> StoreResult<RoomViewContext> {
    let room_id = record.get("id").and_then(Value::as_str).unwrap_or_default();
    let mut peer_teams = Vec::new();
    if table_exists(transaction, "sync_entities")? {
        let mut statement = transaction.prepare(
            "SELECT payload FROM sync_entities WHERE collection='peerTeam' AND deleted=0",
        )?;
        for payload in statement.query_map([], |row| row.get::<_, Option<String>>(0))? {
            let Some(payload) = payload? else { continue };
            let Ok(payload) = serde_json::from_str::<Value>(&payload) else {
                continue;
            };
            if let Some(team) = payload.get("value") {
                peer_teams.push(team.clone());
            }
        }
    }
    let last_message = if table_exists(transaction, "runtime_chat_messages")? {
        transaction.query_row(
            "SELECT seq,text,created,sender FROM runtime_chat_messages WHERE room=?1 ORDER BY seq DESC LIMIT 1",
            [room_id],
            |row| {
                let text: String = row.get(1)?;
                Ok(json!({
                    "seq":row.get::<_, i64>(0)?,
                    "text":text.chars().take(180).collect::<String>(),
                    "created":row.get::<_, f64>(2)?,
                    "sender":row.get::<_, String>(3)?
                }))
            },
        ).optional()?
    } else {
        None
    };
    let federation = if record.get("kind").and_then(Value::as_str) == Some("federated")
        && table_exists(transaction, "runtime_federation_rooms")?
    {
        stored_records(transaction, "runtime_federation_rooms")?
            .into_iter()
            .find(|(id, _)| id == room_id)
            .and_then(|(_, raw)| serde_json::from_str::<Value>(&raw).ok())
    } else {
        None
    };
    Ok(RoomViewContext {
        agents: agents.to_vec(),
        peer_teams,
        last_message,
        federation,
    })
}
