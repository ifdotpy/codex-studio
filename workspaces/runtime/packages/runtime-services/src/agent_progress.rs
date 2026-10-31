//! Derive agent-list progress from durable assignments and managed commands.
use crate::monitor_policy::MonitorStatus;
use rusqlite::{Connection, OptionalExtension, Transaction};
use serde::Deserialize;
use studio_runtime_schema::agent_progress::{AgentCommandProgress, AgentStall};
use studio_runtime_store::{AgentStatus, StoreResult, WorkItemRecord, WorkStatus};

// These are projections of open persisted records: unrelated stored fields are ignored.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentSource {
    id: String,
    status: Option<AgentStatus>,
    #[serde(default)]
    is_lead: bool,
    deleted_at: Option<f64>,
    active_execution: Option<ActiveExecution>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActiveExecution {
    run_id: String,
}
pub(crate) type Assignment = WorkItemRecord;
#[derive(Deserialize)]
struct ExecutionSource {
    finished: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct AgentProgress {
    pub commands: Vec<AgentCommandProgress>,
    pub stall: Option<AgentStall>,
}

fn command_progress(
    record: &studio_runtime_store::MonitorRecord,
) -> Option<(&str, &str, f64, &str)> {
    matches!(
        record.status,
        MonitorStatus::Starting | MonitorStatus::Running
    )
    .then(|| {
        (
            record.id.as_str(),
            record.command.as_str().lines().next().unwrap_or_default(),
            record.created.seconds(),
            record
                .tail
                .as_str()
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default(),
        )
    })
}

pub(crate) fn publish_monitor_owner_change(
    transaction: &Transaction<'_>,
    before: &studio_runtime_store::MonitorRecord,
    after: &studio_runtime_store::MonitorRecord,
) -> StoreResult<()> {
    if command_progress(before) != command_progress(after)
        || !transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sync_entities WHERE collection='agent' AND id=?1 AND deleted=0)",
            [&after.agent], |row| row.get::<_, bool>(0),
        )?
    {
        publish_owner(transaction, &after.agent)?;
    }
    Ok(())
}

pub(crate) fn derive<'a>(
    connection: &Connection,
    record: &serde_json::Value,
    assignments: impl IntoIterator<Item = &'a Assignment>,
) -> StoreResult<AgentProgress> {
    let agent: AgentSource = serde_json::from_value(record.clone())?;
    let mut commands = Vec::new();
    if crate::sync_policy::table_exists(connection, "runtime_monitors")? {
        // This runs for every agent write inside the writer transaction. The
        // owner index bounds it to the agent's live monitors; a guarded scan
        // parsed every historical monitor (93,116 rows, 198 MB, for each
        // journal batch on 22 October 2026). Without the index, guard before
        // JSON extraction: one malformed sibling must not refuse an owner's
        // projection (including startup recovery). Parse bad rows to report
        // their ids while bounding valid rows to this owner.
        let mut statement = connection.prepare_cached(
            if crate::sync_policy::monitor_owner_index_exists(connection)? {
                "SELECT id,record FROM runtime_monitors WHERE json_extract(record,'$.agent')=?1 \
                 AND json_extract(record,'$.status') IN ('starting','running') ORDER BY rowid"
            } else {
                "SELECT id,record FROM runtime_monitors WHERE CASE WHEN json_valid(record) \
                 THEN json_extract(record,'$.agent')=?1 ELSE 1 END ORDER BY rowid"
            },
        )?;
        let rows = statement.query_map([&agent.id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, raw) = row?;
            let monitor: studio_runtime_store::MonitorRecord = match serde_json::from_str(&raw) {
                Ok(monitor) => monitor,
                Err(error) => {
                    eprintln!(
                        "Skipping invalid monitor {id} in agent progress projection: {error}"
                    );
                    continue;
                }
            };
            if monitor.agent != agent.id {
                continue;
            }
            if let Some((id, stage, started_at, last_output_line)) = command_progress(&monitor) {
                commands.push(AgentCommandProgress {
                    monitor_id: id.to_owned(),
                    stage: stage.to_owned(),
                    started_at,
                    last_output_line: last_output_line.to_owned(),
                });
            }
        }
    }
    let assignment = assignments.into_iter().find(|task| {
        task.owner() == Some(&agent.id)
            && matches!(
                task.status(),
                WorkStatus::Ready | WorkStatus::Running | WorkStatus::Blocked
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
            let mut since = task.updated();
            if let Some(execution) = &agent.active_execution {
                let raw: Option<String> = connection
                    .query_row(
                        "SELECT record FROM runtime_execution_runs WHERE id=?1 AND agent=?2",
                        rusqlite::params![execution.run_id, agent.id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(raw) = raw {
                    let execution: ExecutionSource = serde_json::from_str(&raw)?;
                    since = since.max(execution.finished.unwrap_or_default());
                }
            }
            Some(AgentStall {
                task_id: task.id().0.clone(),
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
        "SELECT id,record FROM runtime_work WHERE json_extract(record,'$.owner')=?1 ORDER BY rowid",
    )?;
    let rows = statement.query_map([agent_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.map(|row| {
        let (id, raw) = row?;
        WorkItemRecord::decode(&id, &raw)
    })
    .collect()
}

/// Publish the owner after a managed-command write, including output-only updates.
pub(crate) fn publish_owner(transaction: &Transaction<'_>, agent_id: &str) -> StoreResult<()> {
    use studio_runtime_store::{AgentId, AgentRepository};
    let Some(agent) = AgentRepository::get(transaction, &AgentId::from(agent_id))? else {
        return Ok(());
    };
    let record = agent.value();
    let server_id = studio_runtime_store::RuntimeStore::transaction_local_server_id(transaction)?;
    let context =
        crate::sync_entity_context::agent_view_context(transaction, &record, server_id.as_deref())?;
    if let Some(view) = crate::sync_views::agent_entity_view(&record, &context) {
        crate::sync_policy::publish_agent_projection(
            transaction,
            agent_id,
            view,
            false,
            &|_, _, _, _| true,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::trace::{TraceEvent, TraceEventCodes};
    use serde_json::json;
    use std::cell::Cell;

    thread_local! { static AGENT_PUBLICATIONS: Cell<usize> = const { Cell::new(0) }; }
    fn trace_agent_publication(event: TraceEvent<'_>) {
        if let TraceEvent::Stmt(statement, _) = event
            && statement.expanded_sql().is_some_and(|sql| {
                sql.starts_with("SELECT hash,deleted FROM sync_entities")
                    && sql.contains("collection='agent'")
            })
        {
            AGENT_PUBLICATIONS.with(|count| count.set(count.get() + 1));
        }
    }

    #[test]
    fn stored_review_boundary_progress_refuses_present_invalid_updated() {
        let connection = Connection::open_in_memory().expect("fixture database");
        connection
            .execute_batch("CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
            .expect("fixture work table");
        for updated in [json!("bad"), json!(null), json!(false), json!([])] {
            connection.execute("INSERT OR REPLACE INTO runtime_work VALUES('assigned',?1)",[json!({"id":"assigned","status":"running","owner":"worker","dependencies":[],"updated":updated}).to_string()]).expect("corrupt timestamp fixture");
            let error = assignments(&connection, "worker")
                .expect_err("malformed timestamp cannot become epoch zero");
            assert!(
                error.to_string().contains("runtime_work/assigned"),
                "{error:?}"
            );
        }
    }

    #[test]
    fn stored_work_boundary_progress_refuses_missing_status_instead_of_omitting_assignment() {
        let connection = Connection::open_in_memory().expect("fixture database");
        connection
            .execute_batch("CREATE TABLE runtime_work(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            .expect("fixture work table");
        for record in [
            json!({"id":"assigned","owner":"worker"}),
            json!({"id":"assigned","owner":"worker","status":null}),
        ] {
            connection
                .execute(
                    "INSERT OR REPLACE INTO runtime_work VALUES('assigned',?1)",
                    [record.to_string()],
                )
                .expect("corrupt fixture");
            assert!(
                matches!(assignments(&connection,"worker"), Err(studio_runtime_store::StoreError::InvalidSyncObjectField {scope,id,..}) if scope == "runtime_work" && id == "assigned")
            );
        }
        connection
            .execute(
                "UPDATE runtime_work SET record=?1 WHERE id='assigned'",
                [
                    json!({"id":"assigned","owner":"worker","status":"running","dependencies":[]})
                        .to_string(),
                ],
            )
            .expect("healthy assignment");
        assert_eq!(
            assignments(&connection, "worker")
                .expect("healthy assignment remains visible")
                .len(),
            1
        );
    }

    #[test]
    fn idle_assignment_stalls_but_live_commands_and_submitted_work_do_not() {
        let connection = Connection::open_in_memory().expect("test fixture value");
        connection.execute_batch("CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY, record TEXT NOT NULL, stream_pending INTEGER NOT NULL DEFAULT 0, monitor_status TEXT, monitor_error_candidate INTEGER NOT NULL DEFAULT 1, monitor_created REAL NOT NULL DEFAULT 0);
            CREATE INDEX runtime_monitor_agent_status ON runtime_monitors(json_extract(record,'$.agent'),json_extract(record,'$.status'));
            CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,agent TEXT, created REAL, record TEXT);").expect("test fixture value");
        connection
            .execute(
                "INSERT INTO runtime_execution_runs VALUES('current','worker',20,?1)",
                [json!({"finished":30.0}).to_string()],
            )
            .expect("test fixture value");
        let task: Assignment = serde_json::from_value(
            json!({"id":"task","owner":"worker","status":"running","updated":10}),
        )
        .expect("test fixture value");
        let mut agent = json!({"compactions":0,"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,"id":"worker","rootId":"lead","epoch":0,"status":"completed","activeExecution":{"runId":"current","attemptId":"attempt"}});
        let mut tasks = vec![task];
        let progress = derive(&connection, &agent, &tasks).expect("test fixture value");
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
        let view =
            crate::sync_views::agent_entity_view(&agent, &context).expect("test fixture value");
        assert_eq!(view["stall"]["taskId"], "task");
        assert!(
            studio_runtime_schema::validate_sync_entity_payload_strict(
                "agent",
                "worker",
                false,
                &json!({"collection":"agent","id":"worker","value":view})
            )
            .expect("test fixture value")
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
                derive(&connection, &agent, &tasks)
                    .expect("test fixture value")
                    .stall
                    .is_none(),
                "{status:?}"
            );
        }
        agent["status"] = json!("waiting_on_monitor");
        assert!(
            derive(&connection, &agent, &tasks)
                .expect("test fixture value")
                .stall
                .is_none()
        );
        agent["status"] = json!("idle");
        for status in [
            WorkStatus::Review,
            WorkStatus::Accepted,
            WorkStatus::Cancelled,
        ] {
            tasks[0].set_status(status);
            assert!(
                derive(&connection, &agent, &tasks)
                    .expect("test fixture value")
                    .stall
                    .is_none()
            );
        }
        tasks[0].set_status(WorkStatus::Running);
        for (id, status) in [("m1", "running"), ("m2", "starting"), ("old", "completed")] {
            connection.execute("INSERT INTO runtime_monitors(id,record,monitor_status) VALUES(?1,?2,COALESCE(CASE WHEN json_valid(?2) THEN json_extract(?2,'$.status') END,'lost'))",rusqlite::params![id,crate::monitor_policy::complete_test_monitor(json!({"id":id,"agent":"worker","status":status,"command":"just test-fast\nsecond line","created":5,"tail":"old output\nfinal output\n"})).to_string()]).expect("test fixture value");
        }
        let progress = derive(&connection, &agent, &tasks).expect("test fixture value");
        assert!(progress.stall.is_none());
        assert_eq!(progress.commands.len(), 2);
        assert_eq!(progress.commands[0].stage, "just test-fast");
        assert_eq!(progress.commands[0].last_output_line, "final output");
        let context = crate::sync_views::AgentViewContext {
            command_progress: progress.commands,
            ..Default::default()
        };
        let view =
            crate::sync_views::agent_entity_view(&agent, &context).expect("test fixture value");
        assert_eq!(view["commandProgress"][1]["monitorId"], "m2");
        assert!(
            studio_runtime_schema::validate_sync_entity_payload_strict(
                "agent",
                "worker",
                false,
                &json!({"collection":"agent","id":"worker","value":view})
            )
            .expect("test fixture value")
        );
    }

    /// SQLite work of one projection, in virtual machine steps. The count is
    /// deterministic for a given database state, unlike wall time.
    fn derive_work(connection: &Connection, agent: &serde_json::Value) -> (u64, AgentProgress) {
        let ticks = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counter = std::sync::Arc::clone(&ticks);
        connection
            .progress_handler(
                1,
                Some(move || {
                    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    false
                }),
            )
            .expect("progress counter");
        let progress = derive(connection, agent, &[]).expect("projection");
        connection
            .progress_handler(1, None::<fn() -> bool>)
            .expect("progress counter");
        (ticks.load(std::sync::atomic::Ordering::Relaxed), progress)
    }

    /// Every agent write projects progress inside the writer transaction. With
    /// the owner index, that work must not grow with historical monitors of
    /// other agents (93,116 rows parsed per journal batch on 22 October 2026).
    #[test]
    fn indexed_progress_work_is_independent_of_finished_monitor_history() {
        const HISTORY: usize = 2_000;
        let connection = Connection::open_in_memory().expect("test fixture value");
        connection.execute_batch("CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY, record TEXT NOT NULL, stream_pending INTEGER NOT NULL DEFAULT 0, monitor_status TEXT, monitor_error_candidate INTEGER NOT NULL DEFAULT 1, monitor_created REAL NOT NULL DEFAULT 0);
            CREATE INDEX runtime_monitor_agent_status ON runtime_monitors(json_extract(record,'$.agent'),json_extract(record,'$.status'));").expect("test fixture value");
        let insert = |from: usize| {
            for index in from..from + HISTORY {
                let id = format!("old-{index}");
                connection.execute("INSERT INTO runtime_monitors(id,record,monitor_status) VALUES(?1,?2,COALESCE(CASE WHEN json_valid(?2) THEN json_extract(?2,'$.status') END,'lost'))", rusqlite::params![id, json!({"id":id,"agent":format!("other-{index}"),"status":"completed","command":"old","created":1,"tail":"done"}).to_string()]).expect("test fixture value");
            }
        };
        connection.execute("INSERT INTO runtime_monitors(id,record,monitor_status) VALUES('live',?1,COALESCE(CASE WHEN json_valid(?1) THEN json_extract(?1,'$.status') END,'lost'))", [crate::monitor_policy::complete_test_monitor(json!({"id":"live","agent":"worker","status":"running","command":"owner gate","created":5,"tail":"still running"})).to_string()]).expect("test fixture value");
        let agent = json!({"id":"worker","status":"idle"});
        insert(0);
        let (small, progress) = derive_work(&connection, &agent);
        assert_eq!(progress.commands.len(), 1);
        assert_eq!(progress.commands[0].monitor_id, "live");
        insert(HISTORY);
        let (large, progress) = derive_work(&connection, &agent);
        assert_eq!(progress.commands.len(), 1);
        assert!(
            large <= small + small / 20,
            "progress work grew with history: {small} steps, then {large} with {} more finished monitors",
            HISTORY
        );
    }

    #[test]
    fn invalid_monitor_rows_do_not_hide_live_owner_progress() {
        let connection = Connection::open_in_memory().expect("test fixture value");
        // Match the recovery corruption fixture: no expression index can be
        // maintained over the malformed JSON row.
        connection
            .execute_batch(
                "CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL, stream_pending INTEGER NOT NULL DEFAULT 0, monitor_status TEXT, monitor_error_candidate INTEGER NOT NULL DEFAULT 1, monitor_created REAL NOT NULL DEFAULT 0)",
            )
            .expect("test fixture value");
        for (id, raw) in [
            ("malformed", "{".to_owned()),
            ("invalid-owner-row", json!({"id":"invalid-owner-row","agent":"worker","status":"running","command":42}).to_string()),
            ("other-owner", json!({"id":"other-owner","agent":"other","status":"running","command":"other gate"}).to_string()),
            ("live", crate::monitor_policy::complete_test_monitor(json!({"id":"live","agent":"worker","status":"running","command":"owner gate","tail":"still running"})).to_string()),
        ] {
            connection.execute("INSERT INTO runtime_monitors(id,record,monitor_status) VALUES(?1,?2,COALESCE(CASE WHEN json_valid(?2) THEN json_extract(?2,'$.status') END,'lost'))", [id,&raw]).expect("test fixture value");
        }
        let progress = derive(&connection, &json!({"id":"worker","status":"idle"}), &[])
            .expect("test fixture value");
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
        let directory = tempfile::tempdir().expect("test fixture value");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("test fixture value");
        store.transaction_with(|tx| {
            let actor = AgentRecord::from_value(json!({"compactions":0,"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,"id":"worker","rootId":"worker","epoch":0,"status":"running","autoWake":true})).expect("test fixture value");
            AgentRepository::put(tx,&actor)?;
            let record = json!({"id":"m","agent":"worker","epoch":0,"status":"running","command":"just test-fast","created":5,"tail":"first","requestId":"request","activityAt":5,"activityGeneration":0,"bytes":0,"cwd":std::env::temp_dir(),"interactive":false,"livenessCommand":"","log":std::env::temp_dir().join("m.log"),"stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"successExitCodes":[0],"timeout_ms":3600000,"wakeOn":"exit"});
            let raw = format!(r#"{{"id":"m","agent":"worker","epoch":0,"status":"running","command":"just test-fast","created":5,"tail":"first","requestId":"request","activityAt":5,"activityGeneration":0,"bytes":0,"cwd":{},"interactive":false,"livenessCommand":"","log":{},"stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"successExitCodes":[0],"timeout_ms":3600000,"wakeOn":"exit"}}"#, serde_json::to_string(&std::env::temp_dir()).expect("cwd"), serde_json::to_string(&std::env::temp_dir().join("m.log")).expect("log"));
            let typed: MonitorRecord = serde_json::from_str(&raw).expect("test fixture value");
            Table::<MonitorRecord>::new(tx).put(&typed)?;
            let snapshot = crate::monitor_policy::MonitorSnapshot::checked(record.clone(),actor.value())?;
            let mut decision = snapshot.decision();
            decision.record.tail = "first\nlast gate passed".to_owned().try_into().expect("tail");
            crate::monitor_runtime::apply(tx,&snapshot,&actor,&decision,20.0)?;
            let raw: String = tx.query_row("SELECT payload FROM sync_entities WHERE collection='agent' AND id='worker'",[],|r|r.get(0)).expect("test fixture value");
            let view: serde_json::Value = serde_json::from_str(&raw).expect("test fixture value");
            assert_eq!(view["value"]["commandProgress"][0]["lastOutputLine"],"last gate passed");
            assert_eq!(AgentRepository::get(tx,&AgentId::from("worker"))?.expect("test fixture value").status,AgentStatus::Running);
            let (before, actor) = crate::monitor_runtime::snapshot(tx, "m")?;
            let mut repeated = before.decision();
            repeated.record.tail = "first\nlast gate passed\nlast gate passed".to_owned().try_into().expect("tail");
            AGENT_PUBLICATIONS.with(|count| count.set(0));
            tx.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(trace_agent_publication));
            crate::monitor_runtime::apply(tx, &before, &actor, &repeated, 21.0)?;
            tx.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, None);
            assert_eq!(AGENT_PUBLICATIONS.with(Cell::get), 0, "unchanged progress should not republish the owner");
            tx.execute("DELETE FROM sync_entities WHERE collection='agent' AND id='worker'", []).map_err(studio_runtime_store::StoreError::from)?;
            let (before, actor) = crate::monitor_runtime::snapshot(tx, "m")?;
            let mut repair = before.decision();
            repair.record.tail = "last gate passed\nlast gate passed".to_owned().try_into().expect("tail");
            crate::monitor_runtime::apply(tx, &before, &actor, &repair, 22.0)?;
            let restored: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sync_entities WHERE collection='agent' AND id='worker' AND deleted=0)", [], |row| row.get(0)).map_err(studio_runtime_store::StoreError::from)?;
            assert!(restored, "missing owner projection must be repaired");
            Ok::<(),crate::monitor_policy::MonitorError>(())
        }).await.expect("test fixture value");
    }

    #[tokio::test]
    #[ignore = "focused monitor output apply measurement"]
    async fn monitor_output_apply_measurement() {
        use studio_runtime_store::{
            AgentRecord, AgentRepository, MonitorRecord, RuntimeStore, Table,
        };
        let directory = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(directory.path())).await.unwrap();
        store.transaction_with(|tx| {
            let actor = AgentRecord::from_value(json!({"compactions":0,"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,"id":"worker","rootId":"worker","epoch":0,"status":"running","autoWake":true})).unwrap();
            AgentRepository::put(tx, &actor)?;
            let record: MonitorRecord = serde_json::from_str(r#"{"id":"m","agent":"worker","epoch":0,"status":"running","command":"test","created":5,"tail":"","requestId":"request","activityAt":5,"activityGeneration":0,"bytes":0,"cwd":"/work","interactive":false,"livenessCommand":"","log":"/work/m.log","stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"successExitCodes":[0],"timeout_ms":3600000,"wakeOn":"exit","operation":{"agent":"worker","epoch":0,"accountKey":"default","connectionId":"connection"}}"#).unwrap();
            Table::new(tx).put(&record)?;
            let start = std::time::Instant::now();
            for _ in 0..10_000 {
                let (before, actor) = crate::monitor_runtime::snapshot(tx, "m")?;
                let decision = crate::monitor_policy::output(&before, before.operation().unwrap(), b"output line\n", 20.0)?;
                crate::monitor_runtime::apply(tx, &before, &actor, &decision, 20.0)?;
            }
            eprintln!("10000 monitor output applies: {:?}", start.elapsed());
            Ok::<(), crate::monitor_policy::MonitorError>(())
        }).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "focused monitor output transaction measurement"]
    async fn monitor_output_transaction_measurement() {
        use studio_runtime_store::{
            AgentRecord, AgentRepository, MonitorRecord, RuntimeStore, Table,
        };
        let directory = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(directory.path())).await.unwrap();
        store.transaction_with(|tx| {
            let actor = AgentRecord::from_value(json!({"compactions":0,"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,"id":"worker","rootId":"worker","epoch":0,"status":"running","autoWake":true})).unwrap();
            AgentRepository::put(tx, &actor)?;
            let record: MonitorRecord = serde_json::from_str(r#"{"id":"m","agent":"worker","epoch":0,"status":"running","command":"test","created":5,"tail":"","requestId":"request","activityAt":5,"activityGeneration":0,"bytes":0,"cwd":"/work","interactive":false,"livenessCommand":"","log":"/work/m.log","stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"successExitCodes":[0],"timeout_ms":3600000,"wakeOn":"exit","operation":{"agent":"worker","epoch":0,"accountKey":"default","connectionId":"connection"}}"#).unwrap();
            Table::new(tx).put(&record)?;
            Ok::<(), crate::monitor_policy::MonitorError>(())
        }).await.unwrap();
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            store
                .transaction_with(|tx| {
                    let (before, actor) = crate::monitor_runtime::snapshot(tx, "m")?;
                    let decision = crate::monitor_policy::output(
                        &before,
                        before.operation().unwrap(),
                        b"output line\n",
                        20.0,
                    )?;
                    crate::monitor_runtime::apply(tx, &before, &actor, &decision, 20.0)
                })
                .await
                .unwrap();
        }
        eprintln!("1000 monitor output transactions: {:?}", start.elapsed());
    }

    #[tokio::test]
    #[ignore = "focused changing-line output measurement"]
    async fn monitor_output_changing_line_measurement() {
        use studio_runtime_store::{
            AgentRecord, AgentRepository, MonitorRecord, RuntimeStore, Table,
        };
        let directory = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(directory.path())).await.unwrap();
        store.transaction_with(|tx| {
            let actor = AgentRecord::from_value(json!({"compactions":0,"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,"id":"worker","rootId":"worker","epoch":0,"status":"running","autoWake":true})).unwrap();
            AgentRepository::put(tx, &actor)?;
            let record: MonitorRecord = serde_json::from_str(r#"{"id":"m","agent":"worker","epoch":0,"status":"running","command":"test","created":5,"tail":"","requestId":"request","activityAt":5,"activityGeneration":0,"bytes":0,"cwd":"/tmp","interactive":false,"livenessCommand":"","log":"/tmp/m.log","stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"successExitCodes":[0],"timeout_ms":3600000,"wakeOn":"exit","operation":{"agent":"worker","epoch":0,"accountKey":"default","connectionId":"connection"}}"#).unwrap();
            Table::new(tx).put(&record)?;
            let start = std::time::Instant::now();
            for index in 0..10_000 {
                let (before, actor) = crate::monitor_runtime::snapshot(tx, "m")?;
                let chunk: &[u8] = if index % 2 == 0 { b"output one\n" } else { b"output two\n" };
                let decision = crate::monitor_policy::output(&before, before.operation().unwrap(), chunk, 20.0)?;
                crate::monitor_runtime::apply(tx, &before, &actor, &decision, 20.0)?;
            }
            eprintln!("10000 changing-line output applies: {:?}", start.elapsed());
            Ok::<(), crate::monitor_policy::MonitorError>(())
        }).await.unwrap();
    }
}
