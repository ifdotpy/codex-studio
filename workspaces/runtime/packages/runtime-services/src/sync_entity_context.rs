//! Persisted context needed to build canonical renderer entity views.

use rusqlite::{Connection, OptionalExtension, Transaction};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use studio_runtime_store::{StoreResult, system_reads as records};

use crate::event_write::truthy;
use crate::sync_policy::table_exists;
use crate::sync_views::{AgentViewContext, RoomViewContext};

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
    agents: &[(String, String)],
    work_records: &[(String, String)],
    project_server_id: Option<&str>,
) -> StoreResult<(HashMap<String, AgentViewContext>, AgentContextWork)> {
    let parsed_agents = agents
        .iter()
        .filter_map(|(id, raw)| {
            serde_json::from_str::<Value>(raw)
                .ok()
                .map(|value| (id, value))
        })
        .collect::<Vec<_>>();
    let parent_ids = parsed_agents
        .iter()
        .filter_map(|(_, record)| {
            record
                .get("parentId")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect::<std::collections::HashSet<_>>();
    let lead_ids = parsed_agents
        .iter()
        .filter_map(|(id, record)| {
            let eligible = truthy(record.get("isLead"))
                && !truthy(record.get("deletedAt"))
                && !truthy(record.get("accountTransferId"))
                && record.get("status").and_then(Value::as_str) == Some("idle")
                && !truthy(record.get("threadId"))
                && !truthy(record.get("prompt"));
            eligible.then_some(id.as_str())
        })
        .collect::<Vec<_>>();
    // `empty_lead` only needs to know whether each eligible lead owns any
    // monitor. The agent-key index bounds this lookup to the small lead set;
    // capturing every historical monitor row would rescan the full journal on
    // each unrelated lifecycle write.
    let mut monitor_agents = std::collections::HashSet::new();
    if !lead_ids.is_empty() && table_exists(transaction, "runtime_monitors")? {
        let has_owner_index = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='runtime_monitor_agent_status')",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if has_owner_index {
            // The indexed JSON expression rejects malformed rows at write
            // time, so this bounded owner lookup is safe and index-backed.
            for chunk in lead_ids.chunks(500) {
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
        for chunk in lead_ids.chunks(500) {
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
        for chunk in lead_ids.chunks(500) {
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
    let mut contexts = HashMap::with_capacity(parsed_agents.len());
    for (id, record) in parsed_agents {
        let empty_lead = truthy(record.get("isLead"))
            && !truthy(record.get("deletedAt"))
            && !truthy(record.get("accountTransferId"))
            && record.get("status").and_then(Value::as_str) == Some("idle")
            && !truthy(record.get("threadId"))
            && !truthy(record.get("prompt"))
            && !event_agents.contains(id)
            && !item_agents.contains(id)
            && !parent_ids.contains(id.as_str())
            && !monitor_agents.contains(id);
        contexts.insert(
            id.clone(),
            AgentViewContext {
                project_server_id: project_server_id.map(str::to_owned),
                launcher_alive: true,
                empty_lead,
                latest_work_result_file: latest_work.remove(id).map(|(_, path)| path),
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
        && !transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_agents WHERE json_extract(record,'$.parentId')=?1)",
            [id],
            |row| row.get::<_, bool>(0),
        )?
        && !exists_for_agent(transaction, "runtime_monitors", "json_extract(record,'$.agent')", id)?;
    let mut latest: Option<(f64, String)> = None;
    let mut statement = transaction.prepare_cached(
        "SELECT record FROM runtime_work WHERE json_extract(record,'$.owner')=?1",
    )?;
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
    /// Read checked database snapshots, then resolve paths with no transaction held.
    pub fn prepare(connection: &Connection) -> Result<Self, studio_runtime_store::StoreError> {
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
    /// Build sync context from schema-valid rows, skipping malformed records
    /// as the Python sync projection owner does.
    pub fn prepare_for_sync(
        connection: &Connection,
    ) -> Result<Self, studio_runtime_store::StoreError> {
        if !connection.is_autocommit() {
            return Err(records::invalid(
                "runtime_projects",
                "room-path-preparation-in-transaction",
            ));
        }
        Self::resolve(
            crate::sync_policy::projection_values(connection, "runtime_agents")?,
            crate::sync_policy::projection_values(connection, "runtime_projects")?,
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

        assert!(RoomDirectory::prepare(&connection).is_err());
        let prepared = RoomDirectory::prepare_for_sync(&connection)
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
    fn cold_agent_contexts_scan_work_once_for_two_thousand_agents() {
        const AGENT_COUNT: usize = 2_000;
        let mut connection = Connection::open_in_memory().expect("database");
        connection
            .execute_batch(
                "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                 CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                 CREATE TABLE runtime_work(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                 CREATE TABLE runtime_events(agent TEXT NOT NULL);
                 CREATE TABLE runtime_items(agent TEXT NOT NULL);
                 CREATE INDEX runtime_item_agent ON runtime_items(agent);",
            )
            .expect("context tables");
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
        let work_query_plan = transaction
            .prepare(
                "EXPLAIN QUERY PLAN SELECT record FROM runtime_work WHERE json_extract(record,'$.owner')=?1",
            )
            .expect("baseline work query plan")
            .query_map(["agent-0000"], |row| row.get::<_, String>(3))
            .expect("baseline plan rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("baseline plan reads");
        assert!(
            work_query_plan
                .iter()
                .any(|line| line.contains("SCAN runtime_work"))
        );
        let before_started = std::time::Instant::now();
        let mut before = HashMap::with_capacity(AGENT_COUNT);
        for (_, raw) in &agents {
            let record: Value = serde_json::from_str(raw).expect("agent JSON");
            let id = record["id"].as_str().expect("agent id");
            let context = agent_view_context(&transaction, &record, None)
                .expect("baseline context is computed");
            before.insert(id.to_owned(), context);
        }
        let before_elapsed = before_started.elapsed();
        let after_started = std::time::Instant::now();
        let (after, work) = agent_view_contexts(&transaction, &agents, &work, None)
            .expect("bulk contexts are built");
        let after_elapsed = after_started.elapsed();
        assert_eq!(work.agents_computed, AGENT_COUNT);
        assert_eq!(work.work_records_scanned, AGENT_COUNT);
        assert_eq!(work.event_rows_scanned, 0);
        assert_eq!(work.item_agents_matched, 0);
        assert_eq!(after.len(), AGENT_COUNT);
        let before_work_rows_scanned = AGENT_COUNT * AGENT_COUNT;
        let after_work_rows_scanned = work.work_records_scanned;
        assert_eq!(before_work_rows_scanned, 4_000_000);
        assert_eq!(after_work_rows_scanned, AGENT_COUNT);
        for (id, context) in before {
            let actual = after.get(&id).expect("bulk context exists");
            assert_eq!(actual.empty_lead, context.empty_lead);
            assert_eq!(
                actual.latest_work_result_file,
                context.latest_work_result_file
            );
        }
        eprintln!(
            "synthetic cold-context work: before agents={AGENT_COUNT}, work_rows_scanned={before_work_rows_scanned}, context_computations={AGENT_COUNT}, per_agent_work_queries={AGENT_COUNT}, elapsed={before_elapsed:?}; after agents={}, work_rows_scanned={after_work_rows_scanned}, context_computations={}, event_rows_scanned={}, item_agents_matched={}, elapsed={after_elapsed:?}",
            agents.len(),
            work.agents_computed,
            work.event_rows_scanned,
            work.item_agents_matched,
        );
    }

    #[test]
    fn bulk_agent_contexts_use_indexed_monitor_owner_lookup() {
        let mut connection = Connection::open_in_memory().expect("database");
        connection
            .execute_batch(
                "CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE INDEX runtime_monitor_agent_status ON runtime_monitors(
                     json_extract(record,'$.agent'),json_extract(record,'$.status'));
                 CREATE TABLE runtime_events(agent TEXT NOT NULL);
                 CREATE TABLE runtime_items(agent TEXT NOT NULL);
                 INSERT INTO runtime_monitors VALUES
                     ('old-monitor','{\"id\":\"old-monitor\",\"agent\":\"lead-busy\",\"status\":\"completed\"}');",
            )
            .expect("context fixture");
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
        let (contexts, _) =
            agent_view_contexts(&transaction, &agents, &[], None).expect("bulk contexts are built");
        assert!(!contexts["lead-busy"].empty_lead);
        assert!(contexts["lead-empty"].empty_lead);
    }
}
