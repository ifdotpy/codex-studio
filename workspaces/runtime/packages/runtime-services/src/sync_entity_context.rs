//! Persisted context needed to build canonical renderer entity views.

use rusqlite::{Connection, OptionalExtension, Transaction};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::HashMap;
use studio_runtime_store::CachedSql;
use studio_runtime_store::{StoreResult, system_reads as records};

use crate::event_write::truthy;
use crate::submit_gates::{AgentAuthorityRow, Deletion, ProjectAuthorityRow};
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
    work_records: &[studio_runtime_store::WorkItemRecord],
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
    let mut assigned = HashMap::<String, Vec<&crate::agent_progress::Assignment>>::new();
    for record in work_records {
        let record_value = record.value();
        let Some(owner) = record_value.get("owner").and_then(Value::as_str) else {
            continue;
        };
        for result in record_value
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
        assigned.entry(owner.to_owned()).or_default().push(record);
    }
    let mut contexts = HashMap::with_capacity(agents.len());
    for &(id, record) in agents {
        let progress = crate::agent_progress::derive(
            transaction,
            record,
            assigned.get(id).into_iter().flatten().copied(),
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
        let attention = pending_attention(transaction, id, record)?;
        contexts.insert(
            id.to_owned(),
            AgentViewContext {
                project_server_id: project_server_id.map(str::to_owned),
                launcher_alive: true,
                empty_lead,
                has_question: attention.0,
                has_approval: attention.1,
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
        "SELECT id,record FROM runtime_work WHERE json_extract(record,'$.owner')=?1",
    )?;
    let rows = statement
        .query_map([id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut assigned = Vec::with_capacity(rows.len());
    for (work_id, raw) in rows {
        let assignment = crate::agent_progress::Assignment::decode(&work_id, &raw)?;
        let work = assignment.value();
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
        assigned.push(assignment);
    }
    let progress = crate::agent_progress::derive(transaction, record, &assigned)?;
    let attention = pending_attention(transaction, id, record)?;
    Ok(AgentViewContext {
        command_progress: progress.commands,
        stall: progress.stall,
        project_server_id: project_server_id.map(str::to_owned),
        launcher_alive: true,
        empty_lead,
        has_question: attention.0,
        has_approval: attention.1,
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

/// Checked record text retained only to recheck the directory's exact snapshot.
#[derive(Default, PartialEq)]
pub struct RoomSources {
    agents: Vec<(String, String)>,
    projects: Vec<(String, String)>,
}

impl RoomSources {
    /// Read the checked rows in a short snapshot; resolve them after the read ends.
    pub fn read(connection: &Connection) -> StoreResult<Self> {
        Self::checked(Self::read_raw(connection)?)
    }

    fn read_raw(connection: &Connection) -> StoreResult<Self> {
        Ok(Self {
            agents: crate::sync_policy::stored_record_rows(connection, "runtime_agents")?,
            projects: crate::sync_policy::stored_record_rows(connection, "runtime_projects")?,
        })
    }

    fn checked(sources: Self) -> StoreResult<Self> {
        for (table, rows) in [
            ("runtime_agents", &sources.agents),
            ("runtime_projects", &sources.projects),
        ] {
            for (id, raw) in rows {
                records::decode(table, id, raw)?;
            }
        }
        Ok(sources)
    }

    /// Read the rows like [`Self::read`], skipping malformed records as the
    /// Python sync projection owner does.
    pub fn read_for_sync(connection: &Connection) -> StoreResult<Self> {
        Ok(Self {
            agents: crate::sync_policy::projection_records(connection, "runtime_agents")?,
            projects: crate::sync_policy::projection_records(connection, "runtime_projects")?,
        })
    }
}

fn room_rows<T: DeserializeOwned>(rows: &[(String, String)], table: &str) -> StoreResult<Vec<T>> {
    rows.iter()
        .map(|(id, raw)| serde_json::from_str(raw).map_err(|_| records::invalid(table, id)))
        .collect()
}

/// Schema-validated snapshot; only the database loader can construct its contents.
pub struct RoomDirectory {
    context: RoomViewContext,
    projects: Vec<ProjectAuthorityRow>,
    sources: RoomSources,
}
impl RoomDirectory {
    pub(crate) fn load(tx: &Transaction<'_>) -> Result<Self, studio_runtime_store::StoreError> {
        Self::prepare(RoomSources::read(tx)?)
    }
    /// Prepare room membership from stored, already resolved path identities.
    /// This is pure; callers retaining the whole directory check [`Self::matches`].
    pub fn prepare(sources: RoomSources) -> StoreResult<Self> {
        let mut directory = Self::resolve(
            room_rows(&sources.agents, "runtime_agents")?,
            room_rows(&sources.projects, "runtime_projects")?,
        )?;
        directory.sources = sources;
        Ok(directory)
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
    /// Use already decoded rows inside the caller's transaction. Retained
    /// directories use `prepare`, which also captures the snapshot for rechecks.
    pub(crate) fn resolve(
        agents: Vec<AgentAuthorityRow>,
        projects: Vec<ProjectAuthorityRow>,
    ) -> StoreResult<Self> {
        let mut peers = Vec::new();
        for project in &projects {
            let Some(path) = project.path.as_deref().filter(|s| !s.is_empty()) else {
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
            for team in &project.peer_teams {
                let mut members = Vec::new();
                for id in &team.members {
                    if agents.iter().any(|a| {
                        a.id.as_ref() == Some(id)
                            && a.root_id.as_ref() == Some(id)
                            && a.is_lead == Some(true)
                            && !a.deleted_at.as_ref().is_some_and(Deletion::is_deleted)
                            && a.parent_id.as_deref().is_none_or(str::is_empty)
                            && a.cwd
                                .as_deref()
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
                    team.members = members;
                    peers.push(team);
                }
            }
        }
        Ok(Self {
            context: RoomViewContext::new(agents, peers),
            projects,
            sources: RoomSources::default(),
        })
    }
    /// Directory of live agents only, which is all a room view reads: peers,
    /// broadcast members and member names all exclude deleted agents.
    pub(crate) fn load_live(tx: &Connection) -> StoreResult<Self> {
        Self::prepare(RoomSources::checked(RoomSources {
            agents: studio_runtime_store::agent_volatile::merged_rows(
                tx,
                crate::sync_policy::live_agent_record_rows(tx)?,
            )?,
            projects: crate::sync_policy::stored_record_rows(tx, "runtime_projects")?,
        })?)
    }
    /// Build the live-agent sync context inside a writer transaction, skipping
    /// malformed rows like `RoomSources::read_for_sync`. Only agent liveness changes take
    /// this path, so path resolution under the writer stays rare.
    pub(crate) fn load_live_for_sync(tx: &Connection) -> StoreResult<Self> {
        Self::prepare(RoomSources {
            agents: crate::sync_policy::live_agent_records(tx)?,
            projects: crate::sync_policy::projection_records(tx, "runtime_projects")?,
        })
    }
    /// Recheck persisted inputs in the consumer's snapshot without resolving paths again.
    pub fn matches(&self, connection: &Connection) -> StoreResult<bool> {
        Ok(self.sources == RoomSources::read_raw(connection)?)
    }
    pub fn matches_for_sync(&self, connection: &Connection) -> StoreResult<bool> {
        Ok(self.sources == RoomSources::read_for_sync(connection)?)
    }
    pub(crate) fn agents(&self) -> &[AgentAuthorityRow] {
        &self.context.agents
    }
    pub(crate) fn projects(&self) -> &[ProjectAuthorityRow] {
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
            ..self.context.clone()
        })
    }
}

/// Pending input flags are list data, independent of whether a chat is visible.
fn pending_attention(
    transaction: &Transaction<'_>,
    id: &str,
    record: &Value,
) -> StoreResult<(bool, bool)> {
    if !table_exists(transaction, "runtime_requests")? {
        return Ok((false, false));
    }
    let epoch = record
        .get("epoch")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    transaction.query_row_cached(
        "SELECT EXISTS(SELECT 1 FROM runtime_requests WHERE json_valid(record) AND json_extract(record,'$.agent')=?1 AND (json_extract(record,'$.epoch') IS NULL OR json_extract(record,'$.epoch')=?2) AND COALESCE(json_extract(record,'$.status'),'pending')='pending' AND COALESCE(json_extract(record,'$.deferred'),0)=0 AND json_extract(record,'$.method') IN ('item/tool/requestUserInput','mcpServer/elicitation/request')), \
                EXISTS(SELECT 1 FROM runtime_requests WHERE json_valid(record) AND json_extract(record,'$.agent')=?1 AND (json_extract(record,'$.epoch') IS NULL OR json_extract(record,'$.epoch')=?2) AND COALESCE(json_extract(record,'$.status'),'pending')='pending' AND COALESCE(json_extract(record,'$.deferred'),0)=0 AND COALESCE(json_extract(record,'$.method'),'') NOT IN ('item/tool/requestUserInput','mcpServer/elicitation/request'))",
        rusqlite::params![id, epoch], |row| Ok((row.get(0)?, row.get(1)?)),
    ).map_err(Into::into)
}

#[cfg(test)]
mod room_preparation_tests {
    use super::*;
    use crate::submit_gates::{PeerTeamRow, StoredField};
    use std::sync::Arc;

    fn agent(id: &str) -> AgentAuthorityRow {
        AgentAuthorityRow {
            id: Some(id.to_owned()),
            root_id: None,
            parent_id: None,
            is_lead: None,
            deleted_at: None,
            cwd: None,
            name: StoredField::Missing,
            shared_room_id: None,
        }
    }

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
        let agents = [("first", &first), ("second", &second)]
            .into_iter()
            .map(|(id, cwd)| AgentAuthorityRow {
                root_id: Some(id.to_owned()),
                is_lead: Some(true),
                cwd: Some(cwd.to_str().expect("UTF-8 path").to_owned()),
                ..agent(id)
            })
            .collect();
        let projects = vec![ProjectAuthorityRow {
            id: Some("project".to_owned()),
            path: Some(first.to_str().expect("UTF-8 path").to_owned()),
            peer_teams: vec![PeerTeamRow {
                id: "peers".to_owned(),
                name: "Peers".to_owned(),
                members: vec!["first".to_owned(), "second".to_owned()],
            }],
        }];
        let directory = RoomDirectory::resolve(agents, projects)
            .expect("room membership resolves canonical paths");
        let members = &directory.context.peer_teams;
        assert_eq!(members[0].members, ["first", "second"]);
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
        assert!(
            prepared
                .matches(&connection)
                .expect("unchanged source text")
        );
        connection.execute("UPDATE runtime_projects SET record=json_set(record,'$.name','after') WHERE id='project'", []).expect("project edit between prepare and transaction");
        let tx = connection.transaction().expect("read transaction");
        assert!(!prepared.matches(&tx).expect("check database preconditions"));
    }

    #[test]
    fn snapshot_rechecks_unprojected_and_volatile_agent_fields() {
        let connection = Connection::open_in_memory().expect("database");
        connection
            .execute_batch(&studio_runtime_store::agent_columns::fixture_sql(
                "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT);",
            ))
            .expect("agents table");
        connection
            .execute_batch(studio_runtime_store::agent_volatile::agent_volatile_table_sql())
            .expect("volatile table");
        connection.execute("INSERT INTO runtime_agents(id,record) VALUES('agent',?1)", [json!({"id":"agent","rootId":"agent","epoch":1,"status":"idle","provider":"codex"}).to_string()]).expect("agent");
        for sql in [
            "UPDATE runtime_agents SET record=json_set(record,'$.prompt','changed') WHERE id='agent'",
            "INSERT INTO runtime_agent_volatile(id,tail) VALUES('agent','\"changed\"')",
        ] {
            let directory =
                RoomDirectory::prepare(RoomSources::read(&connection).expect("sources"))
                    .expect("directory");
            assert!(directory.matches(&connection).expect("unchanged snapshot"));
            connection.execute(sql, []).expect("source edit");
            assert!(!directory.matches(&connection).expect("changed snapshot"));
        }
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
        assert!(prepared.agents().is_empty());
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
                agent("agent-a"),
                AgentAuthorityRow {
                    deleted_at: Some(Deletion::Number(1.0)),
                    ..agent("agent-b")
                },
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
            let agent = json!({"id":id,"provider":studio_runtime_schema::ProviderKind::Codex,"status":"idle"}).to_string();
            let work_id = format!("work-{index:04}");
            let task = json!({"id":work_id,"owner":id,"status":"running","results":[]}).to_string();
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
            work.push(
                studio_runtime_store::WorkItemRecord::decode(&work_id, &task).expect("work JSON"),
            );
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
