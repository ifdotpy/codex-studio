//! Derive agent-list progress from durable assignments and managed commands.
use crate::monitor_policy::MonitorStatus;
use rusqlite::{Connection, OptionalExtension, Transaction};
use serde::Deserialize;
use studio_runtime_schema::agent_progress::{AgentCommandProgress, AgentStall};
use studio_runtime_store::{AgentStatus, StoreResult, WorkStatus};

// These are projections of open persisted records: unrelated stored fields are ignored.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentSource {
    id: String,
    status: Option<AgentStatus>,
    #[serde(default)]
    is_lead: bool,
    deleted_at: Option<f64>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Assignment {
    pub id: String,
    pub owner: Option<String>,
    status: Option<WorkStatus>,
    #[serde(default)]
    updated: f64,
}
#[derive(Deserialize)]
struct MonitorSource {
    id: String,
    agent: String,
    status: MonitorStatus,
    #[serde(default)]
    command: String,
    #[serde(default)]
    created: f64,
    #[serde(default)]
    tail: String,
}
#[derive(Deserialize)]
struct ExecutionSource {
    finished: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct AgentProgress {
    pub commands: Vec<AgentCommandProgress>,
    pub stall: Option<AgentStall>,
}

pub(crate) fn derive(
    connection: &Connection,
    record: &serde_json::Value,
    assignments: &[Assignment],
) -> StoreResult<AgentProgress> {
    let agent: AgentSource = serde_json::from_value(record.clone())?;
    let mut commands = Vec::new();
    if crate::sync_policy::table_exists(connection, "runtime_monitors")? {
        // Guard before JSON extraction: one malformed sibling must not refuse
        // an owner's projection (including startup recovery). Parse bad rows to
        // report their ids while bounding valid rows to this owner.
        let mut statement = connection.prepare_cached(
            "SELECT id,record FROM runtime_monitors WHERE CASE WHEN json_valid(record) \
             THEN json_extract(record,'$.agent')=?1 ELSE 1 END ORDER BY rowid",
        )?;
        let rows = statement.query_map([&agent.id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, raw) = row?;
            let monitor: MonitorSource = match serde_json::from_str(&raw) {
                Ok(monitor) => monitor,
                Err(error) => {
                    eprintln!(
                        "Skipping invalid monitor {id} in agent progress projection: {error}"
                    );
                    continue;
                }
            };
            if monitor.agent != agent.id
                || !matches!(
                    monitor.status,
                    MonitorStatus::Starting | MonitorStatus::Running
                )
            {
                continue;
            }
            commands.push(AgentCommandProgress {
                monitor_id: monitor.id,
                stage: monitor
                    .command
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
                started_at: monitor.created,
                last_output_line: monitor
                    .tail
                    .lines()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or_default()
                    .to_owned(),
            });
        }
    }
    let assignment = assignments.iter().find(|task| {
        task.owner.as_deref() == Some(&agent.id)
            && matches!(
                task.status,
                Some(WorkStatus::Ready | WorkStatus::Running | WorkStatus::Blocked)
            )
    });
    let stall = if !agent.is_lead
        && agent.deleted_at.is_none()
        && matches!(
            agent.status,
            Some(AgentStatus::Idle | AgentStatus::Completed)
        )
        && commands.is_empty()
    {
        if let Some(task) = assignment {
            let mut since = task.updated;
            if crate::sync_policy::table_exists(connection, "runtime_execution_runs")? {
                let raw: Option<String> = connection.query_row(
                    "SELECT record FROM runtime_execution_runs WHERE agent=?1 ORDER BY created DESC LIMIT 1",
                    [&agent.id], |row| row.get(0),
                ).optional()?;
                if let Some(raw) = raw {
                    let execution: ExecutionSource = serde_json::from_str(&raw)?;
                    since = since.max(execution.finished.unwrap_or_default());
                }
            }
            Some(AgentStall {
                task_id: task.id.clone(),
                since,
            })
        } else {
            None
        }
    } else {
        None
    };
    Ok(AgentProgress { commands, stall })
}

pub(crate) fn assignments(connection: &Connection, agent_id: &str) -> StoreResult<Vec<Assignment>> {
    if !crate::sync_policy::table_exists(connection, "runtime_work")? {
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare_cached(
        "SELECT record FROM runtime_work WHERE json_extract(record,'$.owner')=?1 ORDER BY rowid",
    )?;
    let rows = statement.query_map([agent_id], |row| row.get::<_, String>(0))?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}

/// Publish the owner after a managed-command write, including output-only updates.
pub(crate) fn publish_owner(transaction: &Transaction<'_>, agent_id: &str) -> StoreResult<()> {
    use studio_runtime_store::{AgentId, AgentRepository, SyncDocumentsRepository};
    let Some(agent) = AgentRepository::get(transaction, &AgentId::from(agent_id))? else {
        return Ok(());
    };
    let record = agent.value();
    let server_id = studio_runtime_store::RuntimeStore::transaction_local_server_id(transaction)?;
    let context =
        crate::sync_entity_context::agent_view_context(transaction, &record, server_id.as_deref())?;
    if let Some(view) = crate::sync_views::agent_entity_view(&record, &context) {
        SyncDocumentsRepository::put_entity(transaction, "agent", agent_id, view, false)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn idle_assignment_stalls_but_live_commands_and_submitted_work_do_not() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE INDEX runtime_monitor_agent_status ON runtime_monitors(json_extract(record,'$.agent'),json_extract(record,'$.status'));
            CREATE TABLE runtime_execution_runs(agent TEXT, created REAL, record TEXT);").unwrap();
        connection
            .execute(
                "INSERT INTO runtime_execution_runs VALUES('worker',20,?1)",
                [json!({"finished":30.0}).to_string()],
            )
            .unwrap();
        let task: Assignment = serde_json::from_value(
            json!({"id":"task","owner":"worker","status":"running","updated":10}),
        )
        .unwrap();
        let mut agent = json!({"id":"worker","rootId":"lead","epoch":0,"status":"completed"});
        let mut tasks = vec![task];
        let progress = derive(&connection, &agent, &tasks).unwrap();
        assert_eq!(
            progress.stall,
            Some(AgentStall {
                task_id: "task".into(),
                since: 30.0
            })
        );
        let context = crate::sync_views::AgentViewContext {
            command_progress: progress.commands,
            stall: progress.stall,
            ..Default::default()
        };
        let view = crate::sync_views::agent_entity_view(&agent, &context).unwrap();
        assert_eq!(view["stall"]["taskId"], "task");
        assert!(
            studio_runtime_schema::validate_sync_entity_payload_strict(
                "agent",
                "worker",
                false,
                &json!({"collection":"agent","id":"worker","value":view})
            )
            .unwrap()
        );
        for status in [
            AgentStatus::Running,
            AgentStatus::Queued,
            AgentStatus::Waiting,
            AgentStatus::Paused,
            AgentStatus::Parked,
            AgentStatus::Failed,
        ] {
            agent["status"] = json!(status);
            assert!(
                derive(&connection, &agent, &tasks).unwrap().stall.is_none(),
                "{status:?}"
            );
        }
        agent["status"] = json!("waiting_on_monitor");
        assert!(derive(&connection, &agent, &tasks).unwrap().stall.is_none());
        agent["status"] = json!("idle");
        for status in [
            WorkStatus::Review,
            WorkStatus::Accepted,
            WorkStatus::Cancelled,
        ] {
            tasks[0].status = Some(status);
            assert!(derive(&connection, &agent, &tasks).unwrap().stall.is_none());
        }
        tasks[0].status = Some(WorkStatus::Running);
        for (id, status) in [("m1", "running"), ("m2", "starting"), ("old", "completed")] {
            connection.execute("INSERT INTO runtime_monitors VALUES(?1,?2)",rusqlite::params![id,json!({"id":id,"agent":"worker","status":status,"command":"just test-fast\nsecond line","created":5,"tail":"old output\nfinal output\n"}).to_string()]).unwrap();
        }
        let progress = derive(&connection, &agent, &tasks).unwrap();
        assert!(progress.stall.is_none());
        assert_eq!(progress.commands.len(), 2);
        assert_eq!(progress.commands[0].stage, "just test-fast");
        assert_eq!(progress.commands[0].last_output_line, "final output");
        let context = crate::sync_views::AgentViewContext {
            command_progress: progress.commands,
            ..Default::default()
        };
        let view = crate::sync_views::agent_entity_view(&agent, &context).unwrap();
        assert_eq!(view["commandProgress"][1]["monitorId"], "m2");
        assert!(
            studio_runtime_schema::validate_sync_entity_payload_strict(
                "agent",
                "worker",
                false,
                &json!({"collection":"agent","id":"worker","value":view})
            )
            .unwrap()
        );
    }

    #[test]
    fn invalid_monitor_rows_do_not_hide_live_owner_progress() {
        let connection = Connection::open_in_memory().unwrap();
        // Match the recovery corruption fixture: no expression index can be
        // maintained over the malformed JSON row.
        connection
            .execute_batch(
                "CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL)",
            )
            .unwrap();
        for (id, raw) in [
            ("malformed", "{".to_owned()),
            ("invalid-owner-row", json!({"id":"invalid-owner-row","agent":"worker","status":"running","command":42}).to_string()),
            ("other-owner", json!({"id":"other-owner","agent":"other","status":"running","command":"other gate"}).to_string()),
            ("live", json!({"id":"live","agent":"worker","status":"running","command":"owner gate","tail":"still running"}).to_string()),
        ] {
            connection.execute("INSERT INTO runtime_monitors VALUES(?1,?2)", [id,&raw]).unwrap();
        }
        let progress = derive(&connection, &json!({"id":"worker","status":"idle"}), &[]).unwrap();
        assert_eq!(progress.commands.len(), 1);
        assert_eq!(progress.commands[0].monitor_id, "live");
        assert_eq!(progress.commands[0].last_output_line, "still running");
        assert!(progress.stall.is_none());
    }

    #[tokio::test]
    async fn output_only_monitor_write_updates_the_agent_sync_entity() {
        use studio_runtime_store::{
            AgentId, AgentRecord, AgentRepository, MonitorRecord, RuntimeStore, Table,
        };
        let directory = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(directory.path())).await.unwrap();
        store.transaction_with(|tx| {
            let actor = AgentRecord::from_value(json!({"id":"worker","rootId":"worker","epoch":0,"status":"running","autoWake":true})).unwrap();
            AgentRepository::put(tx,&actor)?;
            let record = json!({"id":"m","agent":"worker","epoch":0,"status":"running","command":"just test-fast","created":5,"tail":"first"});
            Table::<MonitorRecord>::new(tx).put(&MonitorRecord::from_value(record.clone()).unwrap())?;
            let snapshot = crate::monitor_policy::MonitorSnapshot::checked(record.clone(),actor.value())?;
            let mut decision = snapshot.decision();
            decision.record["tail"] = json!("first\nlast gate passed");
            crate::monitor_runtime::apply(tx,&snapshot,&actor,&decision,20.0)?;
            let raw: String = tx.query_row("SELECT payload FROM sync_entities WHERE collection='agent' AND id='worker'",[],|r|r.get(0)).unwrap();
            let view: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(view["value"]["commandProgress"][0]["lastOutputLine"],"last gate passed");
            assert_eq!(AgentRepository::get(tx,&AgentId::from("worker"))?.unwrap().status,AgentStatus::Running);
            Ok::<(),crate::monitor_policy::MonitorError>(())
        }).await.unwrap();
    }
}
