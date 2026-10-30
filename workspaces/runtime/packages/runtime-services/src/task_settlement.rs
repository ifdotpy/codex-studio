//! Terminal task state after a turn settles.
//!
//! A provider reports the end of each tool or command item. When a turn ends
//! (completed, failed, interrupted, superseded, or cut off by a disconnect or a
//! backend restart), an item without that report stays `running` and the chat
//! shows a command that waits forever. Python closes such tasks when the turn
//! ends (`codex_runtime.py`, `turn/completed`): every running task of the turn
//! becomes `interrupted`, except a command with a provider process id. Codex
//! keeps that command as a background terminal after the turn, so it stays
//! running until the provider no longer reports it; it then becomes `lost`
//! (`codex_task_recovery.py`).
//!
//! [`settle_agent_tasks`] applies the first rule to every settled turn of one
//! agent, and [`recover_on_startup`] applies it to every agent once per
//! backend start. [`absent_command_checks`] and [`close_absent_commands`] apply
//! the second rule with a background terminal list that the account worker
//! reads from the live provider.
use rusqlite::{OptionalExtension, Transaction};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, CachedSql, RuntimeStore, StoreError, StoreResult,
    TaskId, TaskRecord, TaskRepository, truthy,
};

/// The status of a task whose turn ended before the provider reported its end.
/// Python `codex_runtime.py` names it at `turn/completed`.
pub const SETTLED_TASK_STATUS: &str = "interrupted";

/// The error of a background command that the provider no longer reports. Its
/// owner is the task recovery port.
pub use crate::recovery_absence::LOST_COMMAND;
/// The status of a task whose outcome the provider can no longer report: its
/// native command session is absent (Python `codex_task_recovery.py`).
pub const LOST_TASK_STATUS: &str = "lost";

/// A command that can outlive its turn as a provider background terminal: the
/// provider gave it a process id. Python keeps it running at turn end.
pub fn background_command(task: &Value) -> bool {
    task["kind"] == "command" && truthy(task.get("processId"))
}

fn background_task(task: &TaskRecord) -> bool {
    task.kind() == studio_runtime_schema::task_record::TaskKind::Command
        && task.process_id().is_some_and(|id| id.truthy())
}

/// Whether `turn` of `agent` can still produce item reports. A completed turn
/// is settled. Otherwise the turn is live while the agent or one of its open
/// recovery receipts names it: the provider or a recovery can still continue
/// it, and its items can still arrive.
pub fn turn_live(agent: &Value, turn: &str, completed: bool) -> bool {
    if completed {
        return false;
    }
    let names = |value: &Value| value.as_str() == Some(turn);
    let attempt = &agent["startAttempt"];
    let restart = &agent["restartRecovery"];
    let interrupted = agent["status"] == "interrupted";
    // A held restart marker waits for a manual check of an interrupted agent.
    // An agent that went on to other work no longer waits for that turn
    // (agent 77f7e96a kept a held marker of 15 October while it completed
    // later turns).
    let restart_open =
        restart["stage"] != "finished" && (restart["stage"] != "held" || interrupted);
    names(&agent["turnId"])
        || names(&attempt["turnId"])
        || names(&attempt["observedTurnId"])
        || (names(&restart["turnId"]) && restart_open)
        || (names(&agent["disconnectRecovery"]["turnId"]) && interrupted)
}

/// An owner whose tasks another operation is moving or removing keeps them,
/// as for the task recovery owner (`recovery_policy.rs`).
fn owner_settles(agent: &Value) -> bool {
    agent["id"].is_string()
        && !["deletedAt", "accountTransferId", "workspaceOperation"]
            .iter()
            .any(|field| truthy(agent.get(*field)))
}

fn completed_turn(tx: &rusqlite::Connection, agent: &str, turn: &str) -> StoreResult<bool> {
    Ok(tx
        .query_row_cached(
            "SELECT 1 FROM runtime_completed_turns WHERE id=?1",
            [format!("{agent}:{turn}")],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn stored_agent(tx: &rusqlite::Connection, id: &str) -> StoreResult<Option<Value>> {
    AgentRepository::get(tx, &AgentId::from(id)).map(|agent| agent.map(AgentRecord::into_value))
}

/// Schema-checked running tasks of one agent, with their stored row identities.
fn running_tasks(tx: &rusqlite::Connection, agent: &str) -> StoreResult<Vec<TaskRecord>> {
    TaskRepository::running_for_agent(tx, agent)
}

fn write_task(tx: &Transaction<'_>, task: &TaskRecord) -> StoreResult<()> {
    crate::notification_dispatch::put_checked_task_record(tx, task).map_err(|error| match error {
        crate::notification_dispatch::NotificationDispatchError::Store(error) => error,
        crate::notification_dispatch::NotificationDispatchError::RequestResult(error) => {
            StoreError::RequestResult(error)
        }
        crate::notification_dispatch::NotificationDispatchError::InvalidNotification { .. }
        | crate::notification_dispatch::NotificationDispatchError::InvalidAccountState { .. }
        | crate::notification_dispatch::NotificationDispatchError::StreamCapacity
        | crate::notification_dispatch::NotificationDispatchError::InvalidStoredObject { .. }
        | crate::notification_dispatch::NotificationDispatchError::ConflictingDelivery
        | crate::notification_dispatch::NotificationDispatchError::ConflictingInput
        | crate::notification_dispatch::NotificationDispatchError::UnauthorizedSubmission
        | crate::notification_dispatch::NotificationDispatchError::Dependency(_)
        | crate::notification_dispatch::NotificationDispatchError::Move(_)
        | crate::notification_dispatch::NotificationDispatchError::Json(_) => {
            StoreError::InvalidStoredObject {
                table: "runtime_tasks",
                id: task.id().as_ref().into(),
            }
        }
    })
}

/// Close every running task of a settled turn of `agent_id` that is not a
/// provider background command. A removed legacy executor also loses its
/// background commands because their sessions cannot report an outcome.
/// Returns the number of closed tasks. The
/// caller's transaction also holds the turn's settlement, so a later item
/// report cannot reopen the task: a finished task keeps its state.
pub fn settle_agent_tasks(tx: &Transaction<'_>, agent_id: &str, at: f64) -> StoreResult<usize> {
    let Some(agent) = stored_agent(tx, agent_id)? else {
        return Ok(0);
    };
    if !owner_settles(&agent) {
        return Ok(0);
    }
    let legacy = agent["legacyWorkspaceFinished"] == true
        && crate::legacy_image_workspace::is_legacy(&agent);
    let mut closed = 0;
    for task in running_tasks(tx, agent_id)? {
        let turn = task.turn_id();
        if !legacy && (turn.is_empty() || background_task(&task)) {
            continue;
        }
        if !turn.is_empty() && turn_live(&agent, turn, completed_turn(tx, agent_id, turn)?) {
            continue;
        }
        let mut settled = task;
        let lost = legacy && background_task(&settled);
        settled.set_status(if lost {
            studio_runtime_schema::task_record::TaskStatus::Lost
        } else {
            studio_runtime_schema::task_record::TaskStatus::Interrupted
        });
        if lost {
            settled.set_error(LOST_COMMAND.into());
        }
        settled.set_finished(at);
        write_task(tx, &settled)?;
        closed += 1;
    }
    Ok(closed)
}

/// Close the tasks that earlier turns left running, for every agent with a
/// running task. A backend start runs it before account workers attach, so a
/// turn that was running at the restart still names its agent and stays live
/// until restart recovery settles it.
pub fn settle_stale_tasks(tx: &Transaction<'_>, at: f64) -> StoreResult<usize> {
    let mut statement = tx.prepare(
        "SELECT DISTINCT json_extract(record,'$.agent') FROM runtime_tasks \
         WHERE json_extract(record,'$.status')='running' \
         AND json_type(record,'$.agent')='text' ORDER BY 1",
    )?;
    let agents = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut closed = 0;
    for agent in agents {
        closed += settle_agent_tasks(tx, &agent, at)?;
    }
    Ok(closed)
}

/// The startup entry: one transaction over the running tasks of all agents.
pub async fn recover_on_startup(store: &RuntimeStore, at: f64) -> StoreResult<usize> {
    if !at.is_finite() || at < 0.0 {
        return Err(StoreError::SchemaMismatch(
            "invalid task settlement clock".into(),
        ));
    }
    store
        .transaction_with::<_, StoreError>(move |tx| settle_stale_tasks(tx, at))
        .await
}

/// The background commands of one agent thread that only the provider can
/// prove ended. The worker reads the thread state and its background
/// terminals, then calls [`close_absent_commands`] with this check.
#[derive(Clone, Debug)]
pub struct CommandCheck {
    pub agent: Value,
    pub thread: String,
    pub tasks: Vec<TaskRecord>,
}

/// One check per agent of `account` with a running background command of a
/// settled turn. A command of an earlier thread or account is not checked: the
/// current thread's terminal list cannot prove its end.
pub fn absent_command_checks(
    connection: &rusqlite::Connection,
    account: &str,
) -> StoreResult<Vec<CommandCheck>> {
    let mut statement = connection.prepare_cached(
        "SELECT DISTINCT json_extract(t.record,'$.agent') FROM runtime_tasks t \
         JOIN runtime_agents a ON a.id=json_extract(t.record,'$.agent') \
         WHERE json_extract(t.record,'$.status')='running' \
         AND json_extract(t.record,'$.kind')='command' \
         AND a.account_key=?1 ORDER BY 1",
    )?;
    let agents = statement
        .query_map([account], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut checks = Vec::new();
    for agent_id in agents {
        let Some(agent) = stored_agent(connection, &agent_id)? else {
            continue;
        };
        let Some(thread) = command_thread(&agent, account) else {
            continue;
        };
        let mut tasks = Vec::new();
        for task in running_tasks(connection, &agent_id)? {
            if checkable_command(connection, &agent, account, &thread, &task)? {
                tasks.push(task);
            }
        }
        if !tasks.is_empty() {
            checks.push(CommandCheck {
                agent,
                thread,
                tasks,
            });
        }
    }
    Ok(checks)
}

/// The thread whose terminal list proves the end of `agent`'s commands.
fn command_thread(agent: &Value, account: &str) -> Option<String> {
    if !owner_settles(agent)
        || agent["executionMode"] == "vm"
        || agent["accountKey"].as_str().unwrap_or("default") != account
    {
        return None;
    }
    agent["threadId"]
        .as_str()
        .filter(|thread| !thread.is_empty())
        .map(str::to_owned)
}

fn checkable_command(
    connection: &rusqlite::Connection,
    agent: &Value,
    account: &str,
    thread: &str,
    task: &TaskRecord,
) -> StoreResult<bool> {
    let agent_id = agent["id"].as_str().unwrap_or_default();
    let turn = task.turn_id();
    if turn.is_empty() {
        return Ok(false);
    }
    let created = task.created();
    if !background_task(task)
        || Some(task.agent()) != agent["id"].as_str()
        || task.id().as_ref() != format!("{agent_id}:{}", task.item_id())
        || task.source_thread_id().is_some_and(|value| value != thread)
        || task
            .source_account_key()
            .is_some_and(|value| value != account)
    {
        return Ok(false);
    }
    // A move after the command started changed the thread or the account.
    let moved = match agent.get("accountHistory").filter(|v| !v.is_null()) {
        None => false,
        Some(Value::Array(moves)) => moves.iter().any(|m| {
            m["at"].as_f64().is_none_or(|at| {
                at >= created
                    && (m["accountKey"].as_str().unwrap_or("default") != account
                        || m["threadId"] != thread)
            })
        }),
        Some(_) => true,
    };
    Ok(!moved && !turn_live(agent, turn, completed_turn(connection, agent_id, turn)?))
}

/// What the provider reported for one thread, read after a journal barrier.
#[derive(Clone, Debug, PartialEq)]
pub enum TerminalReport {
    /// The provider has not loaded the thread, so it runs none of its commands.
    NotLoaded,
    /// The complete background terminal list of the loaded thread.
    Listed(Vec<Value>),
}

/// Mark each checked command that `report` does not list as `lost`, in one
/// transaction. A task or owner that changed since the check keeps its state:
/// a report that arrived meanwhile is the better evidence.
pub fn close_absent_commands(
    tx: &Transaction<'_>,
    check: &CommandCheck,
    account: &str,
    report: &TerminalReport,
    at: f64,
) -> StoreResult<usize> {
    let agent_id = check.agent["id"].as_str().unwrap_or_default();
    let Some(agent) = stored_agent(tx, agent_id)? else {
        return Ok(0);
    };
    if agent["epoch"] != check.agent["epoch"]
        || command_thread(&agent, account).as_deref() != Some(check.thread.as_str())
    {
        return Ok(0);
    }
    let mut closed = 0;
    for expected in &check.tasks {
        let id = expected.id().as_ref();
        let Some(mut current) = TaskRepository::get(tx, &TaskId::from(id))? else {
            continue;
        };
        if &current != expected
            || !checkable_command(tx, &agent, account, &check.thread, &current)?
            || listed(report, &current)
        {
            continue;
        }
        current.set_status(studio_runtime_schema::task_record::TaskStatus::Lost);
        current.set_finished(at);
        current.set_error(LOST_COMMAND.into());
        current.remove_recovery_hold();
        let turn = current.turn_id().to_owned();
        write_task(tx, &current)?;
        tx.execute(
            "UPDATE runtime_items SET record=json_set(record,'$.toolStatus','lost') \
             WHERE id=?1 AND agent=?2 AND json_extract(record,'$.turnId')=?3",
            rusqlite::params![id, agent_id, turn],
        )?;
        closed += 1;
    }
    Ok(closed)
}

fn process_text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| (value.is_i64() || value.is_u64()).then(|| value.to_string()))
}

fn listed(report: &TerminalReport, task: &TaskRecord) -> bool {
    match report {
        TerminalReport::NotLoaded => false,
        TerminalReport::Listed(terminals) => terminals.iter().any(|terminal| {
            terminal["itemId"].as_str() == Some(task.item_id())
                || process_text(&terminal["processId"]).is_some_and(|process| {
                    task.process_id()
                        .is_some_and(|id| id.normalized() == process)
                })
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: f64 = 2_000.0;

    #[test]
    fn settlement_readers_reject_invalid_rows_instead_of_skipping_them() {
        let connection = rusqlite::Connection::open_in_memory().expect("connection");
        connection.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT); CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY, record TEXT);").expect("tables");
        connection
            .execute("INSERT INTO runtime_agents VALUES('agent','not json')", [])
            .expect("corrupt agent");
        assert!(stored_agent(&connection, "agent").is_err());
    }

    #[test]
    fn settlement_task_reader_checks_schema_and_row_identity() {
        let connection = rusqlite::Connection::open_in_memory().expect("connection");
        connection
            .execute_batch("CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY, record TEXT);")
            .expect("table");
        for invalid in [
            json!({"id":"agent:item","agent":"agent","status":"running","created":"invalid"}),
            task("agent", "other", "turn", "tool", None),
        ] {
            connection
                .execute(
                    "INSERT OR REPLACE INTO runtime_tasks VALUES('agent:item',?1)",
                    [invalid.to_string()],
                )
                .expect("corrupt task");
            assert!(running_tasks(&connection, "agent").is_err(), "{invalid}");
        }
    }

    fn task(agent: &str, item: &str, turn: &str, kind: &str, process: Option<&str>) -> Value {
        let command = kind == "command";
        let mut task = json!({
            "id": format!("{agent}:{item}"), "agent": agent, "itemId": item, "turnId": turn,
            "created": 1_000.0, "kind": kind, "status": "running",
            "type": if command { "commandExecution" } else { "mcpToolCall" },
            "name": if command { "commandExecution" } else { "tool" },
        });
        if let Some(process) = process {
            task["processId"] = json!(process);
        }
        task
    }

    fn agent(id: &str, fields: Value) -> Value {
        let mut agent = json!({"id": id, "rootId": id, "epoch": 1, "status": "completed",
            "accountKey": "default", "provider": "codex", "threadId": format!("{id}-thread"),
            "inFlight": false, "turnId": null, "autoWake": true});
        for (key, value) in fields.as_object().expect("fields") {
            agent[key] = value.clone();
        }
        agent
    }

    async fn seeded(
        root: &tempfile::TempDir,
        agents: Vec<Value>,
        tasks: Vec<Value>,
        completed: &[&str],
    ) -> RuntimeStore {
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("open store");
        let completed = completed
            .iter()
            .map(|id| (*id).to_owned())
            .collect::<Vec<_>>();
        store
            .transaction_with(move |tx| {
                for agent in &agents {
                    tx.execute(
                        "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                        rusqlite::params![agent["id"].as_str(), agent.to_string()],
                    )?;
                }
                for task in &tasks {
                    tx.execute(
                        "INSERT INTO runtime_tasks(id,record) VALUES(?1,?2)",
                        rusqlite::params![task["id"].as_str(), task.to_string()],
                    )?;
                }
                for id in &completed {
                    tx.execute("INSERT INTO runtime_completed_turns(id) VALUES(?1)", [id])?;
                }
                Ok::<_, StoreError>(())
            })
            .await
            .expect("seed");
        store
    }

    #[tokio::test]
    async fn corrupt_task_rolls_back_other_settlement_writes() {
        let root = tempfile::tempdir().expect("state root");
        let valid = task("a-done", "item", "turn", "tool", None);
        let mut invalid = task("z-done", "item", "turn", "tool", None);
        invalid["created"] = json!("invalid");
        let store = seeded(
            &root,
            vec![agent("a-done", json!({})), agent("z-done", json!({}))],
            vec![valid.clone(), invalid],
            &[],
        )
        .await;
        assert!(recover_on_startup(&store, AT).await.is_err());
        assert_eq!(stored(&store, "a-done:item"), valid);
    }

    fn stored(store: &RuntimeStore, id: &str) -> Value {
        let raw: String = store
            .test_reader()
            .expect("read")
            .query_row(
                "SELECT record FROM runtime_tasks WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .expect("task row");
        serde_json::from_str(&raw).expect("task json")
    }

    fn checks(store: &RuntimeStore) -> Vec<CommandCheck> {
        absent_command_checks(&store.test_reader().expect("read"), "default").expect("checks")
    }

    /// Live 2026-10-20: completed chats kept a running command of a turn that
    /// had ended, so they showed "Waiting for 1 command". A backend start
    /// closes it and leaves a running turn, an open disconnect or restart
    /// recovery, a deleted owner and a provider background command alone.
    #[tokio::test]
    async fn startup_closes_the_tasks_of_settled_turns_only() {
        let root = tempfile::tempdir().expect("state root");
        let store = seeded(
            &root,
            vec![
                agent("done", json!({})),
                agent(
                    "live",
                    json!({"status":"running","inFlight":true,"turnId":"t2"}),
                ),
                agent(
                    "cut",
                    json!({"status":"interrupted","disconnectRecovery":{"turnId":"t3"}}),
                ),
                agent("gone", json!({"deletedAt": 5.0})),
                agent(
                    "moved-on",
                    json!({"restartRecovery":{"turnId":"t4","stage":"held"}}),
                ),
                agent(
                    "held",
                    json!({"status":"interrupted","restartRecovery":{"turnId":"t4","stage":"held"}}),
                ),
            ],
            vec![
                task("done", "cmd", "t1", "command", None),
                task("done", "tool", "t0", "tool", None),
                task("done", "bg", "t1", "command", Some("42")),
                task("live", "now", "t2", "command", None),
                task("live", "old", "t1", "tool", None),
                task("cut", "cmd", "t3", "command", None),
                task("gone", "cmd", "t1", "command", None),
                task("moved-on", "cmd", "t4", "command", None),
                task("held", "cmd", "t4", "command", None),
            ],
            &["done:t1", "live:t1"],
        )
        .await;
        assert_eq!(recover_on_startup(&store, AT).await.expect("repair"), 4);
        for id in ["done:cmd", "done:tool", "live:old", "moved-on:cmd"] {
            let task = stored(&store, id);
            assert_eq!(task["status"], SETTLED_TASK_STATUS, "{task}");
            assert_eq!(task["finished"], AT, "{task}");
        }
        for id in ["done:bg", "live:now", "cut:cmd", "gone:cmd", "held:cmd"] {
            let task = stored(&store, id);
            assert_eq!(task["status"], "running", "{task}");
            assert!(task.get("finished").is_none(), "{task}");
        }
        // A second start finds nothing more to close.
        assert_eq!(
            recover_on_startup(&store, AT + 1.0).await.expect("repeat"),
            0
        );
    }

    /// Only the provider's terminal list proves that a background command
    /// ended. A listed command stays running; an unlisted one becomes lost.
    #[tokio::test]
    async fn absent_background_command_is_lost_and_a_listed_one_stays_running() {
        let root = tempfile::tempdir().expect("state root");
        let store = seeded(
            &root,
            vec![
                agent("done", json!({})),
                agent(
                    "live",
                    json!({"status":"running","inFlight":true,"turnId":"t2"}),
                ),
            ],
            vec![
                task("done", "gone", "t1", "command", Some("42")),
                task("done", "kept", "t1", "command", Some("43")),
                task("done", "plain", "t1", "command", None),
                task("live", "now", "t2", "command", Some("44")),
            ],
            &["done:t1"],
        )
        .await;
        let found = checks(&store);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].thread, "done-thread");
        let ids = found[0]
            .tasks
            .iter()
            .map(|task| json!(task.id()))
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![json!("done:gone"), json!("done:kept")]);
        let check = found[0].clone();
        let listed = TerminalReport::Listed(vec![json!({"itemId":"kept","processId":"43"})]);
        let closed = store
            .transaction_with(move |tx| close_absent_commands(tx, &check, "default", &listed, AT))
            .await
            .expect("close");
        assert_eq!(closed, 1);
        let lost = stored(&store, "done:gone");
        assert_eq!(lost["status"], LOST_TASK_STATUS, "{lost}");
        assert_eq!(lost["error"], LOST_COMMAND, "{lost}");
        assert_eq!(lost["finished"], AT, "{lost}");
        assert_eq!(stored(&store, "done:kept")["status"], "running");
        assert_eq!(stored(&store, "live:now")["status"], "running");

        // A thread that the provider has not loaded runs none of its commands.
        let check = checks(&store).remove(0);
        let closed = store
            .transaction_with(move |tx| {
                close_absent_commands(tx, &check, "default", &TerminalReport::NotLoaded, AT)
            })
            .await
            .expect("close");
        assert_eq!(closed, 1);
        assert_eq!(stored(&store, "done:kept")["status"], LOST_TASK_STATUS);
        assert!(checks(&store).is_empty());
    }

    /// A report that changed the task after the check is the better evidence.
    #[tokio::test]
    async fn changed_task_keeps_its_state() {
        let root = tempfile::tempdir().expect("state root");
        let store = seeded(
            &root,
            vec![agent("done", json!({}))],
            vec![task("done", "gone", "t1", "command", Some("42"))],
            &["done:t1"],
        )
        .await;
        let check = checks(&store).remove(0);
        store
            .transaction_with(|tx| {
                let mut changed = task("done", "gone", "t1", "command", Some("42"));
                changed["tail"] = json!("more output");
                tx.execute(
                    "UPDATE runtime_tasks SET record=?1 WHERE id='done:gone'",
                    [changed.to_string()],
                )?;
                Ok::<_, StoreError>(())
            })
            .await
            .expect("change");
        let recheck = check.clone();
        let closed = store
            .transaction_with(move |tx| {
                close_absent_commands(tx, &check, "default", &TerminalReport::NotLoaded, AT)
            })
            .await
            .expect("close");
        assert_eq!(closed, 0);
        assert_eq!(stored(&store, "done:gone")["status"], "running");
        store
            .transaction_with(|tx| {
                tx.execute(
                    "UPDATE runtime_tasks SET record='{}' WHERE id='done:gone'",
                    [],
                )?;
                Ok::<_, StoreError>(())
            })
            .await
            .expect("corrupt checked task");
        assert!(
            store
                .transaction_with(move |tx| {
                    close_absent_commands(tx, &recheck, "default", &TerminalReport::NotLoaded, AT)
                })
                .await
                .is_err()
        );
    }
}
