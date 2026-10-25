//! Atomic deletion of a registered project lifetime. The lifecycle owner stages
//! every stop; this owner selects project membership and retires its navigation.
use crate::{
    agent_lifecycle_runtime::{self, LifecyclePorts},
    agent_management::{Action, ManagementError},
};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use studio_runtime_store::{
    ProjectsRepository, RuntimeStore, StoreError, SyncDocumentsRepository, Transaction,
};
use thiserror::Error;

/// Internal command supplied by the generated HTTP request. No caller-supplied
/// agent lists or paths participate in membership selection.
pub struct Request {
    pub id: String,
    pub request_id: String,
    pub expected_workspace_id: String,
    pub expected_project_seq: i64,
}
#[derive(Debug, Error)]
pub enum Error {
    #[error("The project no longer exists")]
    NotFound,
    #[error("The project belongs to a different workspace")]
    Workspace,
    #[error("The project changed; confirm deletion again using current state")]
    Sequence,
    #[error("This request identity has different content")]
    RequestConflict,
    #[error("Project deletion requires one resolved owning server")]
    Remote,
    #[error("Project membership is ambiguous or malformed")]
    Membership,
    #[error(transparent)]
    Lifecycle(#[from] ManagementError),
    #[error(transparent)]
    Store(#[from] StoreError),
}
impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.into())
    }
}

pub async fn execute(
    store: Arc<RuntimeStore>,
    request: Request,
    now: f64,
    ports: Arc<dyn LifecyclePorts>,
) -> Result<Value, Error> {
    let adapter = ports.clone();
    let (reply, applied) = store
        .transaction_with(move |tx| delete_in_transaction(tx, &request, now, adapter.as_ref()))
        .await?;
    if applied && let Err(error) = ports.committed().await {
        // A failed wake cannot change the committed receipt. Existing durable
        // lifecycle owners drain the same intents after restart.
        eprintln!("Project deletion committed; effect delivery remains pending: {error}");
    }
    Ok(reply)
}
fn delete_in_transaction(
    tx: &Transaction<'_>,
    request: &Request,
    now: f64,
    ports: &dyn LifecyclePorts,
) -> Result<(Value, bool), Error> {
    let workspace = SyncDocumentsRepository::workspace_id(tx)?;
    let server = RuntimeStore::transaction_local_server_id(tx)?
        .filter(|s| !s.is_empty())
        .ok_or(Error::Remote)?;
    // The authenticated owner API is the only ingress; paired agents have no
    // project-delete authority. Include actual server/workspace in the receipt.
    let signature = json!([
        "owner-api",
        server,
        workspace,
        request.id,
        request.request_id,
        request.expected_workspace_id,
        request.expected_project_seq
    ])
    .to_string();
    let receipt_id = format!("project-delete:{}", request.request_id);
    if let Some((saved, result)) = ProjectsRepository::receipt(tx, &receipt_id)? {
        return if saved == signature {
            Ok((result, false))
        } else {
            Err(Error::RequestConflict)
        };
    }
    if workspace != request.expected_workspace_id {
        return Err(Error::Workspace);
    }
    let (_, project) = ProjectsRepository::get(tx, &request.id)?.ok_or(Error::NotFound)?;
    if project.get("id").and_then(Value::as_str) != Some(request.id.as_str()) {
        return Err(Error::Membership);
    }
    let seq: Option<i64> = tx
        .query_row(
            "SELECT seq FROM sync_entities WHERE collection='project' AND id=?1 AND deleted=0",
            [&request.id],
            |row| row.get(0),
        )
        .optional()?;
    if seq != Some(request.expected_project_seq) {
        return Err(Error::Sequence);
    }
    let (project_ids, paths) = project_scope(tx, &project, &server)?;
    let agents = ProjectsRepository::agents(tx)?
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    let selected = membership(&agents, &project_ids, &server, &paths)?;
    refuse_pending_remote(tx, &project_ids, &paths, &selected)?;
    if studio_runtime_store::AgentMoveRepository::retains_agent_ownership(tx, &selected)? {
        return Err(Error::Remote);
    }
    let roots = selected
        .iter()
        .filter(|id| {
            agents[*id]
                .get("parentId")
                .and_then(Value::as_str)
                .is_none_or(|parent| !selected.contains(parent))
        })
        .cloned()
        .collect::<Vec<_>>();
    // Each selected tree goes through the existing transaction-only lifecycle
    // owner, under this ONE writer. Bound orphans are roots for this purpose.
    for root in roots {
        agent_lifecycle_runtime::execute_in_transaction(
            tx,
            &Action::Delete { target: root },
            now,
            ports,
        )?;
    }
    let rooms = retire_rooms(tx, &project_ids, &paths, &selected, now)?;
    for id in &project_ids {
        ProjectsRepository::remove(tx, id)?;
        prune_sidebar(tx, id, &paths, &selected)?;
    }
    let reply = json!({"id":request.id,"request_id":request.request_id,"outcome":"deleted","deletedAgentIds":selected,"deletedRoomIds":rooms});
    ProjectsRepository::save_receipt(tx, &receipt_id, &signature, &reply)?;
    Ok((reply, true))
}
fn project_scope(
    tx: &Transaction<'_>,
    project: &Value,
    server: &str,
) -> Result<(BTreeSet<String>, BTreeSet<String>), Error> {
    let mut ids = BTreeSet::from([project
        .get("id")
        .and_then(Value::as_str)
        .ok_or(Error::Membership)?
        .to_owned()]);
    let records = ProjectsRepository::list(tx)?;
    let mut paths = BTreeSet::new();
    loop {
        let count = ids.len();
        for (id, record) in &records {
            let linked = ids.contains(id)
                || record
                    .get("projectAliases")
                    .and_then(Value::as_array)
                    .is_some_and(|aliases| {
                        aliases.iter().any(|alias| {
                            alias.get("serverId").and_then(Value::as_str) == Some(server)
                                && alias
                                    .get("projectId")
                                    .and_then(Value::as_str)
                                    .is_some_and(|id| ids.contains(id))
                        })
                    });
            if !linked {
                continue;
            }
            paths.extend(project_paths(record, server)?);
            ids.insert(id.clone());
            for alias in record
                .get("projectAliases")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                ids.insert(
                    alias
                        .get("projectId")
                        .and_then(Value::as_str)
                        .ok_or(Error::Membership)?
                        .to_owned(),
                );
            }
            for location in record
                .get("locations")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(id) = location.get("projectId").and_then(Value::as_str) {
                    ids.insert(id.to_owned());
                }
            }
        }
        if count == ids.len() {
            break;
        }
    }
    // Exact path fallback is only safe when no other registered lifetime owns
    // the same saved local location without the explicit alias relationship.
    for (id, record) in &records {
        if !ids.contains(id)
            && record
                .get("locations")
                .and_then(Value::as_array)
                .is_some_and(|locations| {
                    locations.iter().any(|location| {
                        location.get("serverId").and_then(Value::as_str) == Some(server)
                            && location
                                .get("path")
                                .and_then(Value::as_str)
                                .is_some_and(|path| paths.contains(path))
                    })
                })
        {
            return Err(Error::Membership);
        }
    }
    Ok((ids, paths))
}
fn project_paths(project: &Value, server: &str) -> Result<BTreeSet<String>, Error> {
    if project.get("homeServerId").and_then(Value::as_str) != Some(server) {
        return Err(Error::Remote);
    }
    let mut paths = BTreeSet::new();
    let locations = project
        .get("locations")
        .and_then(Value::as_array)
        .ok_or(Error::Remote)?;
    for location in locations {
        if location.get("serverId").and_then(Value::as_str) != Some(server) {
            return Err(Error::Remote);
        }
        let path = location
            .get("path")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
            .ok_or(Error::Membership)?;
        paths.insert(path.to_owned());
    }
    if paths.is_empty() {
        return Err(Error::Remote);
    }
    if let Some(aliases) = project.get("projectAliases").filter(|a| !a.is_null()) {
        for alias in aliases.as_array().ok_or(Error::Membership)? {
            if alias.get("serverId").and_then(Value::as_str) != Some(server)
                || alias
                    .get("projectId")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
            {
                return Err(Error::Remote);
            }
        }
    }
    if ["relocation", "relocationPending", "movedTo"]
        .iter()
        .any(|key| studio_runtime_store::truthy(project.get(*key)))
    {
        return Err(Error::Remote);
    }
    Ok(paths)
}
fn refuse_pending_remote(
    tx: &Transaction<'_>,
    projects: &BTreeSet<String>,
    paths: &BTreeSet<String>,
    agents: &BTreeSet<String>,
) -> Result<(), Error> {
    let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_server_outbox')",[],|row|row.get(0))?;
    if !exists {
        return Ok(());
    }
    let mut stmt =
        tx.prepare_cached("SELECT id FROM runtime_server_outbox WHERE state != 'complete'")?;
    let ids = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    for operation in ids {
        let value = studio_runtime_store::ProjectLocationsRepository::outbox(tx, &operation)?
            .ok_or(Error::Remote)?;
        let payload = &value["body"]["payload"];
        if payload
            .get("project")
            .and_then(Value::as_str)
            .is_some_and(|id| projects.contains(id))
            || payload
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| paths.contains(path))
            || payload
                .get("agent")
                .and_then(Value::as_str)
                .is_some_and(|id| agents.contains(id))
        {
            return Err(Error::Remote);
        }
    }
    Ok(())
}
fn binding(agent: &Value) -> Result<Option<(&str, &str)>, Error> {
    let project = agent.get("projectId").filter(|v| !v.is_null());
    let server = agent.get("projectServerId").filter(|v| !v.is_null());
    match (project, server) {
        (None, None) => Ok(None),
        (Some(project), Some(server)) => Ok(Some((
            project
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or(Error::Membership)?,
            server
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or(Error::Membership)?,
        ))),
        _ => Err(Error::Membership),
    }
}
fn membership(
    agents: &BTreeMap<String, Value>,
    projects: &BTreeSet<String>,
    server: &str,
    paths: &BTreeSet<String>,
) -> Result<BTreeSet<String>, Error> {
    let mut selected = BTreeSet::new();
    for (id, agent) in agents {
        let owned = match binding(agent) {
            Ok(Some((p, s))) => projects.contains(p) && s == server,
            Ok(None) => {
                agent.get("parentId").is_none_or(Value::is_null)
                    && agent
                        .get("hostProjectPath")
                        .filter(|v| !v.is_null())
                        .or_else(|| agent.get("cwd"))
                        .and_then(Value::as_str)
                        .is_some_and(|p| paths.contains(p))
            }
            Err(_) => {
                // Refuse malformed records that can belong to this project;
                // unrelated projects do not become a global deletion blocker.
                if agent
                    .get("projectId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| projects.contains(id))
                    || agent
                        .get("cwd")
                        .and_then(Value::as_str)
                        .is_some_and(|p| paths.contains(p))
                {
                    return Err(Error::Membership);
                }
                false
            }
        };
        if owned {
            selected.insert(id.clone());
        }
    }
    loop {
        let count = selected.len();
        for (id, agent) in agents {
            if agent
                .get("parentId")
                .and_then(Value::as_str)
                .is_some_and(|p| selected.contains(p))
                || agent
                    .get("rootId")
                    .and_then(Value::as_str)
                    .is_some_and(|root| selected.contains(root))
            {
                selected.insert(id.clone());
            }
        }
        if count == selected.len() {
            break;
        }
    }
    for id in &selected {
        let agent = &agents[id];
        if agent.get("id").and_then(Value::as_str) != Some(id.as_str())
            || agent
                .get("rootId")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        {
            return Err(Error::Membership);
        }
        if agent
            .get("rootId")
            .and_then(Value::as_str)
            .is_some_and(|root| agents.contains_key(root) && !selected.contains(root))
        {
            return Err(Error::Membership);
        }
        if let Some((p, s)) = binding(agent)?
            && (!projects.contains(p) || s != server)
        {
            return Err(Error::Membership);
        }
        if [
            "movedTo",
            "remoteWorker",
            "remoteParent",
            "remoteOrigin",
            "relocation",
            "teleportPending",
        ]
        .iter()
        .any(|key| studio_runtime_store::truthy(agent.get(*key)))
        {
            return Err(Error::Remote);
        }
        for field in ["serverId", "localServerId"] {
            if agent
                .get(field)
                .filter(|v| !v.is_null())
                .is_some_and(|v| v.as_str() != Some(server))
            {
                return Err(Error::Remote);
            }
        }
        let mut chain = BTreeSet::new();
        let mut current = id.as_str();
        while let Some(record) = agents.get(current) {
            if !chain.insert(current) {
                return Err(Error::Membership);
            }
            if current != id && !selected.contains(current) {
                return Err(Error::Membership);
            }
            let Some(parent) = record.get("parentId").filter(|v| !v.is_null()) else {
                break;
            };
            current = parent
                .as_str()
                .filter(|p| !p.is_empty())
                .ok_or(Error::Membership)?;
        }
    }
    Ok(selected)
}
fn retire_rooms(
    tx: &Transaction<'_>,
    projects: &BTreeSet<String>,
    paths: &BTreeSet<String>,
    agents: &BTreeSet<String>,
    now: f64,
) -> Result<BTreeSet<String>, Error> {
    let mut stmt = tx.prepare_cached("SELECT id,record FROM runtime_rooms")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    let mut retired = BTreeSet::new();
    for (id, raw) in rows {
        let mut room: Value = serde_json::from_str(&raw).map_err(StoreError::from)?;
        let bound = room
            .get("projectId")
            .and_then(Value::as_str)
            .is_some_and(|id| projects.contains(id))
            || room
                .get("projectPath")
                .and_then(Value::as_str)
                .is_some_and(|p| projects.contains(p) || paths.contains(p));
        let broadcast = room.get("kind").and_then(Value::as_str) == Some("broadcast")
            && room
                .get("rootId")
                .and_then(Value::as_str)
                .is_some_and(|root| root != "all" && agents.contains(root));
        if bound || broadcast {
            room[ProjectsRepository::ROOM_RETIRED_FIELD] = json!(now);
            tx.execute(
                "UPDATE runtime_rooms SET record=?2 WHERE id=?1",
                rusqlite::params![id, room.to_string()],
            )?;
            SyncDocumentsRepository::put_entity(tx, "room", &id, json!({}), true)?;
            retired.insert(id);
        }
    }
    Ok(retired)
}
fn prune_sidebar(
    tx: &Transaction<'_>,
    project: &str,
    paths: &BTreeSet<String>,
    agents: &BTreeSet<String>,
) -> Result<(), Error> {
    let mut order = ProjectsRepository::sidebar_order(tx)?;
    if let Some(groups) = order.get_mut("groups").and_then(Value::as_object_mut) {
        let before = groups.clone();
        groups.retain(|key, _| key != project && !paths.contains(key));
        for items in groups.values_mut().filter_map(Value::as_array_mut) {
            items.retain(|item| !item.as_str().is_some_and(|id| agents.contains(id)));
        }
        if *groups != before {
            let revision = order
                .get("revision")
                .and_then(Value::as_u64)
                .ok_or(Error::Membership)?;
            order["revision"] = json!(revision.saturating_add(1));
            ProjectsRepository::put_sidebar_order(tx, &order)?;
        }
    }
    Ok(())
}

/// Capture the project entity version before asynchronous creation preparation.
/// Rechecking it in the writer fences a delete/re-register ABA lifetime change.
pub(crate) fn admission_version(
    db: &Connection,
    id: &str,
) -> Result<Option<(i64, bool)>, StoreError> {
    db.query_row(
        "SELECT seq,deleted FROM sync_entities WHERE collection='project' AND id=?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(StoreError::from)
}
