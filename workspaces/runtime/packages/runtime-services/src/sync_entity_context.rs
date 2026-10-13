//! Persisted context needed to build canonical renderer entity views.

use rusqlite::{Connection, OptionalExtension, Transaction};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::BufReader;
use std::path::Path;
use studio_runtime_store::{StoreResult, system_reads as records};

use crate::event_write::truthy;
use crate::sync_policy::table_exists;
use crate::sync_views::{AgentViewContext, RoomViewContext};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalIdentity {
    pub(crate) server_id: String,
    pub(crate) label: String,
    pub(crate) public_key: String,
}

pub(crate) fn local_identity(state_dir: &str) -> StoreResult<Option<LocalIdentity>> {
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
    if identity.server_id.is_empty() || identity.label.is_empty() || identity.public_key.is_empty()
    {
        return Err(invalid_identity());
    }
    Ok(Some(identity))
}

pub(crate) fn local_server_id(state_dir: &str) -> StoreResult<Option<String>> {
    Ok(local_identity(state_dir)?.map(|identity| identity.server_id))
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

/// Schema-validated snapshot; only the database loader can construct its contents.
pub(crate) struct RoomDirectory {
    agents: Vec<Value>,
    projects: Vec<Value>,
    peers: Vec<Value>,
}
impl RoomDirectory {
    pub(crate) fn load(tx: &Transaction<'_>) -> Result<Self, studio_runtime_store::StoreError> {
        Self::resolve(
            records::all(tx, "runtime_agents")?,
            records::all(tx, "runtime_projects")?,
        )
    }
    /// Read checked database snapshots, then resolve paths with no transaction held.
    pub(crate) fn prepare(
        connection: &Connection,
    ) -> Result<Self, studio_runtime_store::StoreError> {
        if !connection.is_autocommit() {
            return Err(records::invalid(
                "runtime_projects",
                "room-path-preparation-in-transaction",
            ));
        }
        Self::resolve(
            records::all(connection, "runtime_agents")?,
            records::all(connection, "runtime_projects")?,
        )
    }
    fn resolve(agents: Vec<Value>, projects: Vec<Value>) -> StoreResult<Self> {
        let mut peers = Vec::new();
        for project in &projects {
            let Some(path) = project["path"].as_str().filter(|s| !s.is_empty()) else {
                continue;
            };
            let path = crate::system_reads::resolve_path(path).map_err(|_| {
                records::invalid(
                    "runtime_projects",
                    project["id"].as_str().unwrap_or_default(),
                )
            })?;
            for team in project["peerTeams"].as_array().into_iter().flatten() {
                let mut members = Vec::new();
                for id in team["members"].as_array().into_iter().flatten() {
                    if agents.iter().any(|a| {
                        a["id"] == *id
                            && a["rootId"] == *id
                            && a["isLead"] == true
                            && !truthy(a.get("deletedAt"))
                            && !truthy(a.get("parentId"))
                            && a["cwd"]
                                .as_str()
                                .and_then(|p| crate::system_reads::resolve_path(p).ok())
                                .as_ref()
                                == Some(&path)
                    }) && !members.contains(id)
                    {
                        members.push(id.clone());
                    }
                }
                if members.len() >= 2 {
                    let mut team = team.clone();
                    team["members"] = json!(members);
                    peers.push(team);
                }
            }
        }
        Ok(Self {
            agents,
            projects,
            peers,
        })
    }
    pub(crate) fn matches(&self, connection: &Connection) -> StoreResult<bool> {
        Ok(self.agents == records::all(connection, "runtime_agents")?
            && self.projects == records::all(connection, "runtime_projects")?)
    }
    pub(crate) fn agents(&self) -> &[Value] {
        &self.agents
    }
    pub(crate) fn projects(&self) -> &[Value] {
        &self.projects
    }
    pub(crate) fn for_room(
        &self,
        tx: &Connection,
        room: &Value,
        last: bool,
    ) -> Result<RoomViewContext, studio_runtime_store::StoreError> {
        let id = room["id"].as_str().unwrap_or_default();
        let last_message = if last && records::exists(tx, "runtime_chat_messages")? {
            let row=tx.query_row("SELECT id,seq,text,created,sender,deliveries FROM runtime_chat_messages WHERE room=?1 ORDER BY seq DESC LIMIT 1",[id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,f64>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?))).optional()?;
            row.map(|(key,seq,text,created,sender,deliveries)|{
                let deliveries:Value=serde_json::from_str(&deliveries).map_err(|_|records::invalid("runtime_chat_messages",&key))?;
                records::validate("runtime_chat_messages",&key,json!({"id":key,"seq":seq,"room":id,"text":text,"created":created,"sender":sender,"deliveries":deliveries}))?;
                Ok::<Value,studio_runtime_store::StoreError>(json!({"seq":seq,"text":text.chars().take(180).collect::<String>(),"created":created,"sender":sender}))
            }).transpose()?
        } else {
            None
        };
        Ok(RoomViewContext {
            agents: self.agents.clone(),
            peer_teams: self.peers.clone(),
            last_message,
            federation: if room["kind"] == "federated" {
                records::get(tx, "runtime_federation_rooms", id)?
            } else {
                None
            },
        })
    }
}

#[cfg(test)]
mod room_preparation_tests {
    use super::*;
    #[test]
    fn paths_are_prepared_without_transaction_and_snapshot_is_rechecked() {
        let root = tempfile::tempdir().expect("isolated state");
        let mut connection = Connection::open_in_memory().expect("database");
        connection.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT); CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT);").expect("tables");
        connection
            .execute(
                "INSERT INTO runtime_projects(id,record) VALUES ('project',?1)",
                [json!({"id":"project","path":root.path(),"name":"before"}).to_string()],
            )
            .expect("project");
        let prepared = RoomDirectory::prepare(&connection).expect("prepare outside transaction");
        connection.execute("UPDATE runtime_projects SET record=json_set(record,'$.name','after') WHERE id='project'", []).expect("project edit between prepare and transaction");
        let tx = connection.transaction().expect("read transaction");
        assert!(!prepared.matches(&tx).expect("check database preconditions"));
        assert!(
            RoomDirectory::prepare(&tx).is_err(),
            "filesystem preparation inside a transaction must refuse"
        );
    }
}
