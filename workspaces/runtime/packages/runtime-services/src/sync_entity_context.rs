//! Persisted context needed to build canonical renderer entity views.

use rusqlite::{Connection, OptionalExtension, Transaction};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use studio_runtime_store::CachedSql;
use studio_runtime_store::{StoreResult, system_reads as records};

use crate::event_write::truthy;
use crate::sync_policy::table_exists;
use crate::sync_views::{AgentViewContext, RoomViewContext};

/// Lead ids bound into one `IN (...)` lookup while building agent contexts,
/// kept below SQLite's host-parameter limit. Unit: rows (ids) per statement.
const LEAD_LOOKUP_CHUNK_ROWS: usize = 500;

/// Work performed by one bulk projection-context pass, exposed for deterministic tests.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AgentContextWork {
    pub agents_computed: usize,
    pub work_records_scanned: usize,
    pub event_rows_scanned: usize,
    pub item_agents_matched: usize,
}

/// Build all agent contexts from the already captured source rows in one pass.
pub(crate) fn agent_view_contexts(
    transaction: &Transaction<'_>,
    agents: &[(&str, &Value)],
    work_records: &[(String, String)],
    project_server_id: Option<&str>,
) -> StoreResult<(HashMap<String, AgentViewContext>, AgentContextWork)> {
    let lead_ids = agents
        .iter()
        .filter_map(|(id, record)| {
            let eligible = truthy(record.get("isLead"))
                && !truthy(record.get("deletedAt"))
                && !truthy(record.get("accountTransferId"))
                && record.get("status").and_then(Value::as_str) == Some("idle")
                && !truthy(record.get("threadId"))
                && !truthy(record.get("prompt"));
            eligible.then_some(*id)
        })
        .collect::<Vec<_>>();
    // Children decide `empty_lead` even when the caller passes only changed
    // agents, so the parent lookup reads the indexed table, not the slice.
    let mut parent_ids = std::collections::HashSet::new();
    for chunk in lead_ids.chunks(LEAD_LOOKUP_CHUNK_ROWS) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let mut statement = transaction.prepare(&format!(
            "SELECT DISTINCT parent_id FROM runtime_agents \
             WHERE parent_id IN ({placeholders})"
        ))?;
        parent_ids.extend(
            statement
                .query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    // `empty_lead` only needs to know whether each eligible lead owns any
    // monitor. The agent-key index bounds this lookup to the small lead set;
    // capturing every historical monitor row would rescan the full journal on
    // each unrelated lifecycle write.
    let mut monitor_agents = std::collections::HashSet::new();
    if !lead_ids.is_empty() && table_exists(transaction, "runtime_monitors")? {
        if crate::sync_policy::monitor_owner_index_exists(transaction)? {
            // The indexed JSON expression rejects malformed rows at write
            // time, so this bounded owner lookup is safe and index-backed.
            for chunk in lead_ids.chunks(LEAD_LOOKUP_CHUNK_ROWS) {
                let placeholders = vec!["?"; chunk.len()].join(",");
                let mut statement = transaction.prepare(&format!(
                    "SELECT DISTINCT json_extract(record,'$.agent') FROM runtime_monitors \
                     WHERE json_extract(record,'$.agent') IN ({placeholders})"
                ))?;
                monitor_agents.extend(
                    statement
                        .query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                            row.get::<_, String>(0)
                        })?
                        .collect::<Result<Vec<_>, _>>()?,
                );
            }
        } else {
            // Legacy/test schemas without the expression index may contain
            // invalid JSON. Preserve the former decode-and-skip behavior.
            let lead_set = lead_ids
                .iter()
                .copied()
                .collect::<std::collections::HashSet<_>>();
            let mut statement = transaction.prepare("SELECT record FROM runtime_monitors")?;
            for raw in statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
            {
                if let Ok(record) = serde_json::from_str::<Value>(&raw)
                    && let Some(owner) = record.get("agent").and_then(Value::as_str)
                    && lead_set.contains(owner)
                {
                    monitor_agents.insert(owner.to_owned());
                }
            }
        }
    }
    let mut event_agents = std::collections::HashSet::new();
    let mut event_rows_scanned = 0;
    if !lead_ids.is_empty() && table_exists(transaction, "runtime_events")? {
        for chunk in lead_ids.chunks(LEAD_LOOKUP_CHUNK_ROWS) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let mut statement = transaction.prepare(&format!(
                "SELECT agent FROM runtime_events WHERE agent IN ({placeholders})"
            ))?;
            let rows = statement
                .query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            event_rows_scanned += rows.len();
            event_agents.extend(rows);
        }
    }
    let mut item_agents = std::collections::HashSet::new();
    if !lead_ids.is_empty() && table_exists(transaction, "runtime_items")? {
        for chunk in lead_ids.chunks(LEAD_LOOKUP_CHUNK_ROWS) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let mut statement = transaction.prepare(&format!(
                "SELECT DISTINCT agent FROM runtime_items WHERE agent IN ({placeholders})"
            ))?;
            item_agents.extend(
                statement
                    .query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                        row.get::<_, String>(0)
                    })?
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }
    }
    let mut latest_work = HashMap::<String, (f64, String)>::new();
    for (_, raw) in work_records {
        let Ok(record) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        let Some(owner) = record.get("owner").and_then(Value::as_str) else {
            continue;
        };
        for result in record
            .get("results")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if result.get("agent").and_then(Value::as_str) != Some(owner) {
                continue;
            }
            let Some(path) = result
                .get("resultFile")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let created = result
                .get("created")
                .and_then(Value::as_f64)
                .unwrap_or_default();
            if latest_work
                .get(owner)
                .is_none_or(|(prior, _)| created > *prior)
            {
                latest_work.insert(owner.to_owned(), (created, path.to_owned()));
            }
        }
    }
    let mut assigned = HashMap::<String, Vec<crate::agent_progress::Assignment>>::new();
    for (_, raw) in work_records {
        let task: crate::agent_progress::Assignment = serde_json::from_str(raw)?;
        if let Some(owner) = &task.owner {
            assigned.entry(owner.clone()).or_default().push(task);
        }
    }
    let mut contexts = HashMap::with_capacity(agents.len());
    for &(id, record) in agents {
        let progress = crate::agent_progress::derive(
            transaction,
            record,
            assigned.get(id).map(Vec::as_slice).unwrap_or_default(),
        )?;
        let empty_lead = truthy(record.get("isLead"))
            && !truthy(record.get("deletedAt"))
            && !truthy(record.get("accountTransferId"))
            && record.get("status").and_then(Value::as_str) == Some("idle")
            && !truthy(record.get("threadId"))
            && !truthy(record.get("prompt"))
            && !event_agents.contains(id)
            && !item_agents.contains(id)
            && !parent_ids.contains(id)
            && !monitor_agents.contains(id);
        contexts.insert(
            id.to_owned(),
            AgentViewContext {
                project_server_id: project_server_id.map(str::to_owned),
                launcher_alive: true,
                empty_lead,
                latest_work_result_file: latest_work.remove(id).map(|(_, path)| path),
                command_progress: progress.commands,
                stall: progress.stall,
            },
        );
    }
    let work = AgentContextWork {
        agents_computed: contexts.len(),
        work_records_scanned: work_records.len(),
        event_rows_scanned,
        item_agents_matched: item_agents.len(),
    };
    Ok((contexts, work))
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
        && !transaction.query_row_cached(
            "SELECT EXISTS(SELECT 1 FROM runtime_agents WHERE parent_id=?1)",
            [id],
            |row| row.get::<_, bool>(0),
        )?
        && !exists_for_agent(
            transaction,
            "runtime_monitors",
            "json_extract(record,'$.agent')",
            id,
        )?;
    let mut latest: Option<(f64, String)> = None;
    let mut statement = transaction.prepare_cached(
        "SELECT record FROM runtime_work WHERE json_extract(record,'$.owner')=?1",
    )?;
    let rows = statement
        .query_map([id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut assigned = Vec::with_capacity(rows.len());
    for raw in rows {
        assigned.push(serde_json::from_str::<crate::agent_progress::Assignment>(
            &raw,
        )?);
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
    let progress = crate::agent_progress::derive(transaction, record, &assigned)?;
    Ok(AgentViewContext {
        command_progress: progress.commands,
        stall: progress.stall,
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

/// Schema-valid agent and project rows a sync [`RoomDirectory`] is resolved from.
pub struct RoomSources {
    agents: Vec<Value>,
    projects: Vec<Value>,
}

impl RoomSources {
    /// Read the checked rows in a short snapshot; resolve them after the read ends.
    pub fn read(connection: &Connection) -> StoreResult<Self> {
        Ok(Self {
            agents: records::all(connection, "runtime_agents")?,
            projects: records::all(connection, "runtime_projects")?,
        })
    }

    /// Read the rows like [`Self::read`], skipping malformed records as the
    /// Python sync projection owner does.
    pub fn read_for_sync(connection: &Connection) -> StoreResult<Self> {
        Ok(Self {
            agents: crate::sync_policy::projection_values(connection, "runtime_agents")?,
            projects: crate::sync_policy::projection_values(connection, "runtime_projects")?,
        })
    }
}

/// Schema-validated snapshot; only the database loader can construct its contents.
pub struct RoomDirectory {
    agents: Arc<Vec<Value>>,
    projects: Vec<Value>,
    peers: Arc<Vec<Value>>,
    active_agent_indices: Arc<HashMap<String, usize>>,
}
impl RoomDirectory {
    pub(crate) fn load(tx: &Transaction<'_>) -> Result<Self, studio_runtime_store::StoreError> {
        Self::resolve(
            records::all(tx, "runtime_agents")?,
            records::all(tx, "runtime_projects")?,
        )
    }
    /// Prepare room membership from stored, already resolved path identities.
    /// This is pure; callers retaining the whole directory check [`Self::matches`].
    pub fn prepare(sources: RoomSources) -> StoreResult<Self> {
        Self::resolve(sources.agents, sources.projects)
    }
    /// Read [`RoomSources`] from `store`, then prepare the room directory.
    pub async fn read_and_prepare(store: &studio_runtime_store::RuntimeStore) -> StoreResult<Self> {
        let sources = store
            .read_async("runtime-services/sync_entity_context", |tx| {
                RoomSources::read(tx)
            })
            .await?;
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/sync_entity_context",
            move || Self::prepare(sources),
        )
        .await
        .map_err(|_| studio_runtime_store::StoreError::ReadTaskFailed)?
    }
    fn resolve(agents: Vec<Value>, projects: Vec<Value>) -> StoreResult<Self> {
        let mut peers = Vec::new();
        for project in &projects {
            let Some(path) = project["path"].as_str().filter(|s| !s.is_empty()) else {
                continue;
            };
            // Project/agent writers persist resolved host paths. Room membership
            // compares those stored identities; it must not inspect today's filesystem.
            let Some(path) =
                studio_runtime_store::path_identity::lexical(std::path::Path::new(path))
            else {
                // A project handle (for example project:<id>) is not a host
                // directory and cannot bind local peer membership.
                continue;
            };
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
                                .and_then(|path| {
                                    studio_runtime_store::path_identity::lexical(
                                        std::path::Path::new(path),
                                    )
                                })
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
        let active_agent_indices = agents
            .iter()
            .enumerate()
            .filter(|(_, agent)| !truthy(agent.get("deletedAt")))
            .filter_map(|(index, agent)| {
                agent
                    .get("id")
                    .and_then(Value::as_str)
                    .map(|id| (id.to_owned(), index))
            })
            .collect();
        Ok(Self {
            agents: Arc::new(agents),
            projects,
            peers: Arc::new(peers),
            active_agent_indices: Arc::new(active_agent_indices),
        })
    }
    /// Directory of live agents only, which is all a room view reads: peers,
    /// broadcast members and member names all exclude deleted agents.
    pub(crate) fn load_live(tx: &Connection) -> StoreResult<Self> {
        Self::resolve(
            records::live_agents(tx)?,
            records::all(tx, "runtime_projects")?,
        )
    }
    /// Build the live-agent sync context inside a writer transaction, skipping
    /// malformed rows like `RoomSources::read_for_sync`. Only agent liveness changes take
    /// this path, so path resolution under the writer stays rare.
    pub(crate) fn load_live_for_sync(tx: &Connection) -> StoreResult<Self> {
        Self::resolve(
            crate::sync_policy::live_agent_records(tx)?
                .into_iter()
                .map(|(_, raw)| serde_json::from_str(&raw))
                .collect::<Result<Vec<Value>, _>>()?,
            crate::sync_policy::projection_values(tx, "runtime_projects")?,
        )
    }
    /// Recheck persisted inputs in the consumer's snapshot without resolving paths again.
    pub fn matches(&self, connection: &Connection) -> StoreResult<bool> {
        Ok(
            self.agents.as_ref() == &records::all(connection, "runtime_agents")?
                && self.projects == records::all(connection, "runtime_projects")?,
        )
    }
    pub fn matches_for_sync(&self, connection: &Connection) -> StoreResult<bool> {
        Ok(self.agents.as_ref()
            == &crate::sync_policy::projection_values(connection, "runtime_agents")?
            && self.projects
                == crate::sync_policy::projection_values(connection, "runtime_projects")?)
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
            let row=tx.query_row_cached("SELECT id,seq,text,created,sender,deliveries FROM runtime_chat_messages WHERE room=?1 ORDER BY seq DESC LIMIT 1",[id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,f64>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?))).optional()?;
            row.map(|(key,seq,text,created,sender,deliveries)|{
                let deliveries:Value=serde_json::from_str(&deliveries).map_err(|_|records::invalid("runtime_chat_messages",&key))?;
                records::validate("runtime_chat_messages",&key,json!({"id":key,"seq":seq,"room":id,"text":text,"created":created,"sender":sender,"deliveries":deliveries}))?;
                Ok::<Value,studio_runtime_store::StoreError>(json!({"seq":seq,"text":text.chars().take(180).collect::<String>(),"created":created,"sender":sender}))
            }).transpose()?
        } else {
            None
        };
        Ok(RoomViewContext {
            agents: Arc::clone(&self.agents),
            peer_teams: Arc::clone(&self.peers),
            active_agent_indices: Arc::clone(&self.active_agent_indices),
            last_message,
            federation: if room["kind"] == "federated" {
                records::get(tx, "runtime_federation_rooms", id)?
                    .map(|row| {
                        // The authoritative table schema was checked by get;
                        // this narrower schema belongs only to the sync view.
                        let valid =
                            studio_runtime_schema::validate("PersistedFederationRoomRecord", &row)
                                .map_err(studio_runtime_store::StoreError::SchemaValidation)?;
                        if !valid {
                            return Err(records::invalid("runtime_federation_rooms", id));
                        }
                        Ok(row)
                    })
                    .transpose()?
            } else {
                None
            },
        })
    }
}

#[cfg(test)]
mod room_preparation_tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn room_membership_uses_symlink_paths_resolved_by_the_creation_owner() {
        let state = tempfile::tempdir().expect("state");
        let project = tempfile::tempdir().expect("project");
        let link = state.path().join("project-link");
        std::os::unix::fs::symlink(project.path(), &link).expect("project link");
        let first =
            studio_runtime_store::path_identity::CanonicalPath::existing_utf8_directory(&link)
                .expect("resolved linked cwd");
        let second = studio_runtime_store::path_identity::CanonicalPath::existing_utf8_directory(
            project.path(),
        )
        .expect("resolved real cwd");
        let directory = RoomDirectory::resolve(vec![
            json!({"id":"first","rootId":"first","isLead":true,"cwd":first}),
            json!({"id":"second","rootId":"second","isLead":true,"cwd":second}),
        ], vec![json!({"id":"project","path":first,"peerTeams":[{"id":"peers","members":["first","second"]}]})])
            .expect("room membership resolves canonical paths");
        let members = &directory.peers;
        assert_eq!(members[0]["members"], json!(["first", "second"]));
    }

    #[test]
    fn paths_are_prepared_after_the_read_and_snapshot_is_rechecked() {
        let root = tempfile::tempdir().expect("isolated state");
        let mut connection = Connection::open_in_memory().expect("database");
        connection.execute_batch(&studio_runtime_store::agent_columns::fixture_sql("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT); CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT);")).expect("tables");
        connection
            .execute_batch(studio_runtime_store::agent_volatile::agent_volatile_table_sql())
            .expect("volatile agent table creates");
        connection
            .execute(
                "INSERT INTO runtime_projects(id,record) VALUES ('project',?1)",
                [json!({"id":"project","path":root.path(),"name":"before"}).to_string()],
            )
            .expect("project");
        let prepared =
            RoomDirectory::prepare(RoomSources::read(&connection).expect("read room sources"))
                .expect("prepare after the read");
        connection.execute("UPDATE runtime_projects SET record=json_set(record,'$.name','after') WHERE id='project'", []).expect("project edit between prepare and transaction");
        let tx = connection.transaction().expect("read transaction");
        assert!(!prepared.matches(&tx).expect("check database preconditions"));
    }

    #[test]
    fn sync_context_skips_malformed_persisted_agents_and_projects() {
        let mut connection = Connection::open_in_memory().expect("database");
        connection
            .execute_batch(
                "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT); \
                 CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT); \
                 INSERT INTO runtime_agents(id,record) VALUES('broken-agent','{'); \
                 INSERT INTO runtime_projects(id,record) VALUES('broken-project','{');",
            )
            .expect("malformed source fixtures");
        connection
            .execute_batch(studio_runtime_store::agent_volatile::agent_volatile_table_sql())
            .expect("volatile agent table creates");

        assert!(RoomSources::read(&connection).is_err());
        let prepared = RoomDirectory::prepare(
            RoomSources::read_for_sync(&connection).expect("sync sources read"),
        )
        .expect("sync context skips malformed source rows");
        assert!(prepared.agents.is_empty());
        assert!(prepared.projects.is_empty());
        let tx = connection.transaction().expect("sync transaction");
        assert!(
            prepared
                .matches_for_sync(&tx)
                .expect("skip malformed rows on recheck")
        );
    }

    #[test]
    fn room_contexts_share_agents_peers_and_the_active_agent_index() {
        let directory = RoomDirectory::resolve(
            vec![
                json!({"id":"agent-a"}),
                json!({"id":"agent-b","deletedAt":"now"}),
            ],
            Vec::new(),
        )
        .expect("directory resolves");
        let connection = Connection::open_in_memory().expect("database");
        let room = json!({"id":"room"});
        let first = directory
            .for_room(&connection, &room, false)
            .expect("first room context");
        let second = directory
            .for_room(&connection, &room, false)
            .expect("second room context");

        assert!(Arc::ptr_eq(&first.agents, &second.agents));
        assert!(Arc::ptr_eq(&first.peer_teams, &second.peer_teams));
        assert!(Arc::ptr_eq(
            &first.active_agent_indices,
            &second.active_agent_indices
        ));
        assert_eq!(first.active_agent_indices.len(), 1);
    }

    #[test]
    fn cold_agent_contexts_scan_work_once_for_many_agents() {
        const AGENT_COUNT: usize = 32;
        let mut connection = Connection::open_in_memory().expect("database");
        connection
            .execute_batch(&studio_runtime_store::agent_columns::fixture_sql(
                "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                 CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                 CREATE TABLE runtime_work(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                 CREATE TABLE runtime_events(agent TEXT NOT NULL);
                 CREATE TABLE runtime_items(agent TEXT NOT NULL);
                 CREATE INDEX runtime_item_agent ON runtime_items(agent);",
            ))
            .expect("context tables");
        connection
            .execute_batch(studio_runtime_store::agent_volatile::agent_volatile_table_sql())
            .expect("volatile agent table creates");
        let mut agents = Vec::with_capacity(AGENT_COUNT);
        let mut work = Vec::with_capacity(AGENT_COUNT);
        let transaction = connection.transaction().expect("seed transaction");
        for index in 0..AGENT_COUNT {
            let id = format!("agent-{index:04}");
            let agent = json!({"id":id,"status":"idle"}).to_string();
            let work_id = format!("work-{index:04}");
            let task = json!({"id":work_id,"owner":id,"results":[]}).to_string();
            transaction
                .execute(
                    "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                    rusqlite::params![id, agent],
                )
                .expect("agent row");
            transaction
                .execute(
                    "INSERT INTO runtime_work(id,record) VALUES(?1,?2)",
                    rusqlite::params![work_id, task],
                )
                .expect("work row");
            agents.push((id, agent));
            work.push((work_id, task));
        }
        transaction.commit().expect("seed commit");
        let transaction = connection.transaction().expect("read transaction");
        let decoded = agents
            .iter()
            .map(|(id, raw)| {
                (
                    id.as_str(),
                    serde_json::from_str::<Value>(raw).expect("agent JSON"),
                )
            })
            .collect::<Vec<_>>();
        let agents = decoded
            .iter()
            .map(|(id, value)| (*id, value))
            .collect::<Vec<_>>();
        let (after, work) = agent_view_contexts(&transaction, &agents, &work, None)
            .expect("bulk contexts are built");
        assert_eq!(work.agents_computed, AGENT_COUNT);
        assert_eq!(work.work_records_scanned, AGENT_COUNT);
        assert_eq!(work.event_rows_scanned, 0);
        assert_eq!(work.item_agents_matched, 0);
        assert_eq!(after.len(), AGENT_COUNT);
        for (id, record) in agents {
            let context =
                agent_view_context(&transaction, record, None).expect("single-agent context");
            let actual = after.get(id).expect("bulk context exists");
            assert_eq!(actual.empty_lead, context.empty_lead);
            assert_eq!(
                actual.latest_work_result_file,
                context.latest_work_result_file
            );
        }
    }

    #[test]
    fn bulk_agent_contexts_use_indexed_monitor_owner_lookup() {
        let mut connection = Connection::open_in_memory().expect("database");
        connection
            .execute_batch(
                &studio_runtime_store::agent_columns::fixture_sql("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE INDEX runtime_monitor_agent_status ON runtime_monitors(
                     json_extract(record,'$.agent'),json_extract(record,'$.status'));
                 CREATE TABLE runtime_events(agent TEXT NOT NULL);
                 CREATE TABLE runtime_items(agent TEXT NOT NULL);
                 INSERT INTO runtime_monitors VALUES
                     ('old-monitor','{\"id\":\"old-monitor\",\"agent\":\"lead-busy\",\"status\":\"completed\"}');"),
            )
            .expect("context fixture");
        connection
            .execute_batch(studio_runtime_store::agent_volatile::agent_volatile_table_sql())
            .expect("volatile agent table creates");
        let agents = [
            (
                "lead-busy".to_owned(),
                json!({"id":"lead-busy","status":"idle","isLead":true}).to_string(),
            ),
            (
                "lead-empty".to_owned(),
                json!({"id":"lead-empty","status":"idle","isLead":true}).to_string(),
            ),
        ];
        let transaction = connection.transaction().expect("context transaction");
        let plan = transaction
            .prepare(
                "EXPLAIN QUERY PLAN SELECT DISTINCT json_extract(record,'$.agent') \
                 FROM runtime_monitors WHERE json_extract(record,'$.agent') IN (?1,?2)",
            )
            .expect("owner lookup explain plan")
            .query_map(["lead-busy", "lead-empty"], |row| row.get::<_, String>(3))
            .expect("owner plan rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("owner plan read");
        assert!(
            plan.iter()
                .any(|line| line.contains("runtime_monitor_agent_status")),
            "monitor owner lookup must use the owner index: {plan:?}"
        );
        let decoded = agents
            .iter()
            .map(|(id, raw)| {
                (
                    id.as_str(),
                    serde_json::from_str::<Value>(raw).expect("agent JSON"),
                )
            })
            .collect::<Vec<_>>();
        let agents = decoded
            .iter()
            .map(|(id, value)| (*id, value))
            .collect::<Vec<_>>();
        let (contexts, _) =
            agent_view_contexts(&transaction, &agents, &[], None).expect("bulk contexts are built");
        assert!(!contexts["lead-busy"].empty_lead);
        assert!(contexts["lead-empty"].empty_lead);
    }
}
