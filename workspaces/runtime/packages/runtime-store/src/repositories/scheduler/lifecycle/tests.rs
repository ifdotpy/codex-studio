//! Restart and provider-generation recovery of records that Python wrote.
//! Each record shape below comes from the 18 October 2026 cutover state.
use super::*;
use crate::records::{AgentId, AgentRepository};
use rusqlite::Connection;

const AGENT: &str = "agent";
const ACCOUNT: &str = "acct";

fn database() -> Connection {
    let mut connection = Connection::open_in_memory().expect("memory database opens");
    connection
        .execute_batch(crate::agent_columns::agent_table_sql())
        .expect("agents table creates");
    connection
        .execute_batch(
            "CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,agent TEXT,account TEXT,epoch INTEGER,thread TEXT,turn TEXT,created REAL,record TEXT);
             CREATE TABLE runtime_execution_attempts(id TEXT PRIMARY KEY,run TEXT,record TEXT);
             CREATE TABLE runtime_execution_inputs(run TEXT,event TEXT,PRIMARY KEY(run,event));
             CREATE TABLE runtime_completed_turns(id TEXT PRIMARY KEY);
             CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,
               created REAL,epoch INTEGER,turn_id TEXT,error TEXT);
             CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL);
             CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,
               PRIMARY KEY(collection,id));
             CREATE TABLE sync_documents(seq INTEGER);
             CREATE TABLE sync_versions(seq INTEGER);",
        )
        .expect("scheduler tables create");
    connection
        .execute_batch(crate::agent_volatile::agent_volatile_table_sql())
        .expect("volatile agent table creates");
    let transaction = connection.transaction().expect("index transaction begins");
    SchedulerRepository::prepare_account_wake_index(&transaction).expect("wake index prepares");
    transaction.commit().expect("index commits");
    connection
}

/// A Python start attempt: it has a supervisor identity, but no Rust
/// `connectionGeneration` or `requestTicket`.
fn python_attempt(id: &str, turn: Option<&str>, active: bool) -> Value {
    let mut attempt = json!({
        "id": id, "epoch": 0, "activeAtReservation": active, "accountKey": ACCOUNT,
        "events": [format!("event-{id}")], "submitted": true, "created": 1.0,
        "settingsFixed": true, "connectionId": "python-connection", "threadId": "thread",
        "supervisorIdentity": {"stateDir": "/state", "handle": format!("account:{ACCOUNT}"), "generation": 2}
    });
    if let Some(turn) = turn {
        attempt["turnId"] = json!(turn);
    }
    attempt
}

fn agent_from(record: Value) -> AgentRecord {
    let mut record = record;
    for (key, value) in [
        ("id", json!(AGENT)),
        ("rootId", json!(AGENT)),
        ("epoch", json!(0)),
        ("accountKey", json!(ACCOUNT)),
        ("threadId", json!("thread")),
        ("provider", json!("codex")),
    ] {
        if record.get(key).is_none() {
            record[key] = value;
        }
    }
    AgentRecord::from_value(record).expect("agent record is valid")
}

fn put(connection: &Connection, record: Value) {
    AgentRepository::put(connection, &agent_from(record)).expect("agent writes");
}

fn stored(connection: &Connection) -> Value {
    AgentRepository::get(connection, &AgentId::from(AGENT))
        .expect("agent reads")
        .expect("agent exists")
        .value()
}

fn with<T>(connection: &mut Connection, change: impl FnOnce(&Transaction<'_>) -> T) -> T {
    let transaction = connection.transaction().expect("transaction begins");
    let result = change(&transaction);
    transaction.commit().expect("transaction commits");
    result
}

fn restart_and_reconnect(connection: &mut Connection, generation: u64) {
    with(connection, |tx| {
        SchedulerRepository::recover_submitted(tx).expect("startup recovery runs")
    });
    with(connection, |tx| {
        SchedulerRepository::reconcile_generation(tx, ACCOUNT, generation, 10.0, true)
            .expect("generation reconciliation runs")
    });
}

/// A waiting agent that finished a later turn. Its restart marker and
/// disconnect receipt describe a turn from an earlier restart (agents 50f9a796
/// and fbcb2568; agent 841ebc0e after startup cleanup removed its completed
/// attempt).
fn waiting_with_finished_marker() -> Value {
    json!({
        "status": "waiting", "autoWake": true, "inFlight": false, "turnId": null, "error": null,
        "lastCompletedTurn": "turn-later", "lastCompletedTurnStatus": "completed",
        "restartRecovery": {
            "epoch": 0, "accountKey": ACCOUNT, "threadId": "thread", "turnId": "turn-old",
            "autoWake": true, "at": 1.0, "stage": "finished", "reconciledAt": 2.0,
            "startAttempt": python_attempt("old", Some("turn-old"), true)
        },
        "disconnectRecovery": {
            "epoch": 0, "accountKey": ACCOUNT, "threadId": "thread", "turnId": "turn-old",
            "autoWake": true, "at": 1.0, "source": "restart"
        }
    })
}

#[test]
fn a_finished_python_restart_marker_does_not_interrupt_a_waiting_agent() {
    let mut connection = database();
    let before = waiting_with_finished_marker();
    put(&connection, before);
    let expected = stored(&connection);
    restart_and_reconnect(&mut connection, 1);
    with(&mut connection, |tx| {
        SchedulerRepository::disconnect_account(tx, ACCOUNT, 1, 11.0, false)
            .expect("disconnect runs")
    });
    assert_eq!(stored(&connection), expected);
}

#[test]
fn a_held_restart_is_not_applied_again_after_the_user_sends_a_message() {
    let mut connection = database();
    // A Python input without a turn identity and without an unresolved event is
    // truly unprovable: restart holds it once.
    put(
        &connection,
        json!({
            "status": "running", "autoWake": true, "inFlight": true, "turnId": null,
            "error": null, "startAttempt": python_attempt("lost", None, true)
        }),
    );
    with(&mut connection, |tx| {
        SchedulerRepository::recover_submitted(tx).expect("startup recovery runs")
    });
    let held = stored(&connection);
    assert_eq!(held["restartRecovery"]["stage"], "held");
    assert_eq!(
        held["restartRecovery"]["reason"],
        "Native input submission has no confirmed turn identity."
    );
    // The user's message clears the hold and queues the agent (coordination).
    let mut sent = held.clone();
    sent["autoWake"] = json!(true);
    sent["error"] = Value::Null;
    sent["status"] = json!("queued");
    sent.as_object_mut()
        .expect("record object")
        .remove("nativeFailureHold");
    put(&connection, sent);
    let expected = stored(&connection);
    // A Claude bridge starts on demand after that message: its first worker is
    // a new generation in this backend (agent 841ebc0e).
    with(&mut connection, |tx| {
        SchedulerRepository::reconcile_generation(tx, ACCOUNT, 3, 12.0, true)
            .expect("generation reconciliation runs")
    });
    with(&mut connection, |tx| {
        SchedulerRepository::disconnect_account(tx, ACCOUNT, 3, 13.0, false)
            .expect("disconnect runs")
    });
    assert_eq!(stored(&connection), expected);
}

#[test]
fn a_finished_python_turn_reserved_during_activity_stays_settled() {
    // Agents 50f9a796 and 83e59e82 in rehearsal 8: the attempt was reserved
    // while the agent was active, and its turn is the last completed turn.
    let mut connection = database();
    let mut record = waiting_with_finished_marker();
    record["lastCompletedTurn"] = json!("turn-busy");
    record["startAttempt"] = python_attempt("busy", Some("turn-busy"), true);
    put(&connection, record);
    let expected = stored(&connection);
    restart_and_reconnect(&mut connection, 1);
    assert_eq!(stored(&connection), expected);
}

#[test]
fn an_old_uncertain_input_does_not_block_a_new_message() {
    // Agent 841ebc0e kept one `uncertain` input from 10 days before the cutover
    // and ran many later turns under Python. Rust never claimed its next message.
    let mut connection = database();
    put(
        &connection,
        json!({"status": "queued", "autoWake": true, "inFlight": false, "turnId": null, "error": null}),
    );
    connection
        .execute_batch(
            "INSERT INTO runtime_events VALUES('old','agent','user','old','uncertain',1,0,NULL,'Server restarted before delivery acknowledgement');
             INSERT INTO runtime_events VALUES('new','agent','user','new','pending',2,0,NULL,NULL);",
        )
        .expect("events write");
    let claimed = with(&mut connection, |tx| {
        SchedulerRepository::claim(tx, AGENT, 0, "attempt", 3.0, 4).expect("claim runs")
    })
    .expect("the new message is claimed");
    assert_eq!(
        claimed
            .events
            .iter()
            .map(|event| event.id.0.as_str())
            .collect::<Vec<_>>(),
        ["new"]
    );
    let old: String = connection
        .query_row(
            "SELECT status FROM runtime_events WHERE id='old'",
            [],
            |row| row.get(0),
        )
        .expect("old input reads");
    assert_eq!(old, "uncertain", "the unknown input is never sent again");
}

#[test]
fn a_finished_turn_with_only_an_old_uncertain_input_waits_instead_of_queueing() {
    // Agent 841ebc0e in rehearsal 8: after its turn it showed `queued` with no
    // pending input, and no wake ever came. Python's next scheduler pass turns
    // an empty batch into `waiting`; new pending input still queues the agent.
    for (extra_input, expected) in [(None, "waiting"), (Some("next"), "queued")] {
        let mut connection = database();
        put(
            &connection,
            json!({
                "status": "running", "autoWake": true, "inFlight": true, "turnId": "turn-now",
                "turnEpoch": 0, "error": null,
                "startAttempt": {
                    "id": "now", "epoch": 0, "submitted": true, "turnId": "turn-now",
                    "threadId": "thread", "accountKey": ACCOUNT, "events": ["message"],
                    "connectionGeneration": 1, "requestTicket": "ticket"
                }
            }),
        );
        connection
            .execute_batch(
                "INSERT INTO runtime_events VALUES('old','agent','user','old','uncertain',1,0,NULL,'Server restarted before delivery acknowledgement');
                 INSERT INTO runtime_events VALUES('message','agent','user','message','dispatching',2,0,NULL,NULL);",
            )
            .expect("events write");
        if let Some(id) = extra_input {
            connection
                .execute(
                    "INSERT INTO runtime_events VALUES(?1,'agent','user',?1,'pending',3,0,NULL,NULL)",
                    [id],
                )
                .expect("pending input writes");
        }
        let settled = with(&mut connection, |tx| {
            SchedulerRepository::settle_terminal(
                tx,
                AGENT,
                0,
                "now",
                "turn-now",
                AgentStatus::Completed,
                None,
                "completed",
                None,
                &[],
            )
            .expect("settlement runs")
        });
        assert!(settled);
        let agent = stored(&connection);
        assert_eq!(agent["status"], expected, "{agent}");
        assert_eq!(agent["lastCompletedTurn"], "turn-now");
    }
}

#[test]
fn an_observed_turn_without_acceptance_stays_unknown_after_a_restart() {
    // An activity notice is not a receipt for the input batch: a turn that the
    // provider reported but never accepted for this attempt stays held, so a
    // restart cannot treat the input as delivered (the scripted Claude `wait`
    // turn in wave7_p01_registry, and the radio fixture's Python attempt).
    for (generation, request_ticket, reason) in [
        (
            Some(1),
            Some("ticket"),
            "Native input submission has no confirmed turn identity.",
        ),
        (
            None,
            None,
            "Native input submission has no confirmed turn identity.",
        ),
    ] {
        let mut connection = database();
        let mut attempt = json!({
            "id": "seen", "epoch": 0, "submitted": true, "observedTurnId": "turn-seen",
            "threadId": "thread", "accountKey": ACCOUNT, "events": ["message"]
        });
        if let (Some(generation), Some(ticket)) = (generation, request_ticket) {
            attempt["connectionGeneration"] = json!(generation);
            attempt["requestTicket"] = json!(ticket);
        }
        put(
            &connection,
            json!({
                "status": "running", "autoWake": true, "inFlight": true, "turnId": "turn-seen",
                "turnEpoch": 0, "error": null, "startAttempt": attempt
            }),
        );
        connection
            .execute(
                "INSERT INTO runtime_events VALUES('message','agent','user','message','delivered',1,0,'turn-seen',NULL)",
                [],
            )
            .expect("event writes");
        restart_and_reconnect(&mut connection, 2);
        let agent = stored(&connection);
        assert_eq!(agent["restartRecovery"]["stage"], "held", "{agent}");
        assert_eq!(agent["restartRecovery"]["reason"], reason, "{agent}");
        assert_eq!(agent["autoWake"], false);
    }
}

#[test]
fn a_later_finished_turn_closes_a_pending_restart_marker() {
    // Rehearsal 9, agent 3a35babb: a turn ran at the backend swap, so restart
    // recovery left a pending marker. Its history was never read; the user's
    // next turn finished and startup cleanup removed that settled attempt. The
    // next restart must not interrupt the idle agent for the old turn again.
    let mut connection = database();
    put(
        &connection,
        json!({
            "status": "running", "autoWake": true, "inFlight": true, "turnId": "turn-old",
            "turnEpoch": 0, "error": null,
            "startAttempt": python_attempt("old", Some("turn-old"), false)
        }),
    );
    restart_and_reconnect(&mut connection, 1);
    let mut next = stored(&connection);
    assert_eq!(next["restartRecovery"]["stage"], "pending");
    // The user's message runs a new turn on the same thread.
    next["status"] = json!("running");
    next["autoWake"] = json!(true);
    next["inFlight"] = json!(true);
    next["error"] = Value::Null;
    next["turnId"] = json!("turn-next");
    next["startAttempt"] = json!({
        "id": "next", "epoch": 0, "submitted": true, "turnId": "turn-next",
        "threadId": "thread", "accountKey": ACCOUNT, "events": ["message"],
        "connectionGeneration": 1, "requestTicket": "ticket"
    });
    put(&connection, next);
    connection
        .execute_batch(
            "INSERT INTO runtime_events VALUES('message','agent','user','message','dispatching',2,0,NULL,NULL);",
        )
        .expect("event writes");
    let settled = with(&mut connection, |tx| {
        SchedulerRepository::settle_terminal(
            tx,
            AGENT,
            0,
            "next",
            "turn-next",
            AgentStatus::Completed,
            None,
            "completed",
            None,
            &[],
        )
        .expect("settlement runs")
    });
    assert!(settled);
    let mut idle = stored(&connection);
    // Startup cleanup removes the settled attempt.
    idle.as_object_mut()
        .expect("record object")
        .remove("startAttempt");
    put(&connection, idle);
    let expected = stored(&connection);
    restart_and_reconnect(&mut connection, 1);
    let agent = stored(&connection);
    assert_eq!(agent, expected);
    assert_eq!(agent["status"], "completed");
    assert_eq!(agent["restartRecovery"]["stage"], "finished", "{agent}");
    assert_eq!(agent["restartRecovery"]["replacedByTurn"], "turn-next");
}

#[test]
fn a_settled_turn_of_any_terminal_status_is_not_interrupted_again() {
    // Rehearsal 11, agent 841ebc0e: its turn failed with an auth error. At the
    // next backend stop the provider disconnect interrupted it again and its
    // real error became "Codex disconnected". Python interrupts only active
    // turns, so a settled attempt of every terminal status stays as it is.
    for (status, native) in [
        (AgentStatus::Completed, "completed"),
        (AgentStatus::Failed, "failed"),
        (AgentStatus::Interrupted, "interrupted"),
    ] {
        let mut connection = database();
        put(
            &connection,
            json!({
                "status": "running", "autoWake": true, "inFlight": true, "turnId": "turn-now",
                "turnEpoch": 0, "error": null,
                "startAttempt": {
                    "id": "now", "epoch": 0, "submitted": true, "turnId": "turn-now",
                    "observedTurnId": "turn-now", "threadId": "thread", "accountKey": ACCOUNT,
                    "events": ["message"], "connectionGeneration": 1, "requestTicket": "ticket"
                }
            }),
        );
        connection
            .execute_batch(
                "INSERT INTO runtime_events VALUES('message','agent','user','message','dispatching',2,0,NULL,NULL);",
            )
            .expect("event writes");
        let error = (native == "failed").then(|| {
            json!({"codexErrorInfo": "unauthorized", "message": "Failed to authenticate: OAuth token revoked."})
        });
        let settled = with(&mut connection, |tx| {
            SchedulerRepository::settle_terminal(
                tx,
                AGENT,
                0,
                "now",
                "turn-now",
                status.clone(),
                error.clone(),
                native,
                error.as_ref(),
                &[],
            )
            .expect("settlement runs")
        });
        assert!(settled, "{native}");
        let expected = stored(&connection);
        assert_eq!(expected["lastCompletedTurn"], "turn-now");
        // A backend stop disconnects the account; the next start recovers
        // submitted attempts and reconciles the new provider generation.
        with(&mut connection, |tx| {
            SchedulerRepository::disconnect_account(tx, ACCOUNT, 1, 11.0, false)
                .expect("disconnect runs")
        });
        assert_eq!(stored(&connection), expected, "{native} after disconnect");
        restart_and_reconnect(&mut connection, 2);
        assert_eq!(stored(&connection), expected, "{native} after restart");
    }
}

fn rejected_start(error: Value) -> (Value, (String, Option<String>)) {
    let mut connection = database();
    put(
        &connection,
        json!({
            "status": "starting", "autoWake": true, "inFlight": true, "turnEpoch": 0, "error": null,
            "startAttempt": {
                "id": "start", "epoch": 0, "submitted": true, "activeAtReservation": false,
                "accountKey": ACCOUNT, "threadId": "thread", "events": ["input"],
                "connectionGeneration": 1, "requestTicket": "ticket", "created": 1.0
            }
        }),
    );
    connection
        .execute(
            "INSERT INTO runtime_events VALUES('input','agent','user','Exact input','dispatching',1,0,NULL,NULL)",
            [],
        )
        .expect("event writes");
    connection
        .execute(
            "INSERT INTO runtime_execution_attempts VALUES('start','run','{\"id\":\"start\",\"epoch\":0,\"runId\":\"run\",\"events\":[\"input\"],\"submission\":\"submitted\"}')",
            [],
        )
        .expect("attempt writes");
    let settled = with(&mut connection, |tx| {
        SchedulerRepository::settle_start_rejected_provider(
            tx, AGENT, 0, "start", "ticket", ACCOUNT, 1, "thread", error,
        )
        .expect("rejection settles")
    });
    assert!(settled);
    let event = connection
        .query_row(
            "SELECT status, error FROM runtime_events WHERE id='input'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("event reads");
    (stored(&connection), event)
}

#[test]
fn an_unapplied_disk_full_start_returns_the_exact_input_to_the_queue() {
    let message =
        "Disk is full. Claude did not receive this input. Studio sends it when free space returns.";
    let (agent, event) = rejected_start(json!({
        "code": -32000, "message": message,
        "data": {"turnStartOutcome": "not_applied", "inputSubmitted": false, "diskFull": true}
    }));
    assert_eq!(
        event,
        ("pending".to_owned(), None),
        "the input is not failed"
    );
    assert_eq!(agent["status"], "queued", "{agent}");
    assert_eq!(agent["inFlight"], false);
    assert_eq!(agent["error"], Value::Null);
    assert_eq!(agent.get("startAttempt"), None);

    // Without the provider's proof the rejection keeps the ordinary failure.
    let (agent, event) = rejected_start(json!({"code": -32000, "message": message}));
    assert_eq!(event, ("failed".to_owned(), Some(message.to_owned())));
    assert_eq!(agent["status"], "failed", "{agent}");
}

/// Python `codex_agent_management.parked_after_turn`: a worker that asked to
/// wait for an event during its turn is parked when that turn ends.
#[test]
fn a_park_requested_during_a_turn_takes_effect_when_the_turn_ends() {
    let mut connection = database();
    put(
        &connection,
        json!({
            "status": "running", "autoWake": true, "inFlight": true, "turnId": "turn-park",
            "turnEpoch": 0, "error": null, "parkedEvent": "deploy", "parkAfterTurn": true,
            "parkSequence": 1, "parkReceipt": {"event": "deploy", "epoch": 0, "sequence": 1},
            "startAttempt": {
                "id": "park", "epoch": 0, "submitted": true, "turnId": "turn-park",
                "threadId": "thread", "accountKey": ACCOUNT, "events": ["message"],
                "connectionGeneration": 1, "requestTicket": "ticket"
            }
        }),
    );
    connection
        .execute_batch(
            "INSERT INTO runtime_events VALUES('message','agent','user','message','dispatching',2,0,NULL,NULL);",
        )
        .expect("event writes");
    let settled = with(&mut connection, |tx| {
        SchedulerRepository::settle_terminal(
            tx,
            AGENT,
            0,
            "park",
            "turn-park",
            AgentStatus::Completed,
            None,
            "completed",
            None,
            &[],
        )
        .expect("settlement runs")
    });
    assert!(settled);
    let agent = stored(&connection);
    assert_eq!(agent["status"], "parked", "{agent}");
    assert_eq!(agent["autoWake"], false);
    assert!(agent.get("parkAfterTurn").is_none(), "{agent}");
    assert_eq!(agent["parkedEvent"], "deploy");
    assert_eq!(agent["parkReceipt"]["sequence"], 1);
}

fn turn_runs(connection: &Connection, turn: &str) -> Vec<(String, String)> {
    connection
        .prepare("SELECT id,json_extract(record,'$.status') FROM runtime_execution_runs WHERE turn=?1 ORDER BY created,id")
        .expect("run query prepares")
        .query_map([turn], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("runs read")
        .collect::<Result<_, _>>()
        .expect("runs decode")
}

/// A start accepted with the turn that an earlier start already owns: the
/// provider merged the second input into its active turn. Both attempts keep
/// their runs, and the turn's outcome settles both. Before this contract the
/// outcome refused the turn with `invalid stored sync object
/// runtime_execution_runs/<first run>` and ended the account worker (agents
/// d0c9c794 and 50f9a796, 21 October 2026).
#[test]
fn a_turn_outcome_settles_every_run_that_bound_the_turn() {
    let mut connection = database();
    connection
        .execute_batch(
            "INSERT INTO runtime_events VALUES('event-first','agent','user','first','delivered',1,0,'turn-shared',NULL);
             INSERT INTO runtime_events VALUES('event-second','agent','user','second','dispatching',2,0,NULL,NULL);",
        )
        .expect("events write");
    for (attempt, created) in [("first", 1.0), ("second", 2.0)] {
        let mut start = python_attempt(attempt, Some("turn-shared"), false);
        start["created"] = json!(created);
        let agent = agent_from(json!({
            "status": "running", "autoWake": true, "inFlight": true, "turnId": "turn-shared",
            "turnEpoch": 0, "error": null, "startAttempt": start
        }));
        with(&mut connection, |tx| {
            publish_agent(tx, &agent).expect("the accepted attempt records its run")
        });
    }
    assert_eq!(
        turn_runs(&connection, "turn-shared"),
        [
            ("run:first".to_owned(), "running".to_owned()),
            ("run:second".to_owned(), "running".to_owned())
        ]
    );
    let settled = with(&mut connection, |tx| {
        SchedulerRepository::settle_terminal(
            tx,
            AGENT,
            0,
            "second",
            "turn-shared",
            AgentStatus::Completed,
            None,
            "completed",
            None,
            &[],
        )
    });
    assert!(settled.expect("the shared turn settles"));
    assert_eq!(
        turn_runs(&connection, "turn-shared"),
        [
            ("run:first".to_owned(), "completed".to_owned()),
            ("run:second".to_owned(), "completed".to_owned())
        ]
    );
    // The same outcome again is a replay; a different one is a conflict.
    with(&mut connection, |tx| {
        crate::ExecutionHistoryRepository::terminal(tx, AGENT, "turn-shared", "completed", None)
    })
    .expect("a repeated outcome is accepted");
    let conflict = with(&mut connection, |tx| {
        crate::ExecutionHistoryRepository::terminal(tx, AGENT, "turn-shared", "failed", None)
    });
    assert!(
        matches!(&conflict, Err(StoreError::InvalidSyncObject { scope, id })
            if scope == "runtime_execution_runs" && id == "run:first"),
        "{conflict:?}"
    );
}

/// The stored rows of the incident load as they are: their recovery marker
/// has `connectionId` and `supervisor` set to null, and three runs name one
/// turn. Shapes are copied from canvas.sqlite3 (agent d0c9c794); times are
/// the shortest decimal form of the same `f64` values.
#[test]
fn stored_incident_runs_that_share_a_turn_settle_without_an_edit() {
    let mut connection = database();
    put(
        &connection,
        json!({"status": "interrupted", "autoWake": true, "turnId": "01a15bfb", "epoch": 2}),
    );
    for (id, created, at, connection_id) in [
        ("2a953440", 1792544583.54869, 1792546623.0489, "f879d500"),
        ("8e3f28f4", 1792546628.945204, 1792546669.374566, "c7f1ba69"),
        (
            "b1846549",
            1_792_547_053.921_893,
            1_792_547_477.119_958,
            "fcd49da7",
        ),
    ] {
        let run = json!({
            "accountKey": ACCOUNT, "agent": AGENT, "created": created, "epoch": 2,
            "disconnectRecovery": {
                "accountKey": ACCOUNT, "at": at, "autoWake": true, "connectionId": null,
                "epoch": 2, "supervisor": null, "threadId": "thread", "turnId": "01a15bfb",
                "startAttempt": {
                    "accountKey": ACCOUNT, "activeAtReservation": false,
                    "connectionGeneration": 6, "connectionId": connection_id,
                    "created": created, "epoch": 2, "events": [format!("event-{id}")],
                    "id": id, "observedTurnId": "01a15bfb", "settingsFixed": true,
                    "submitted": true, "supervisorIdentity": null, "threadId": "thread",
                    "turnId": "01a15bfb"
                }
            },
            "firstAttemptId": id, "id": format!("run:{id}"), "latestAttemptId": id,
            "rootAttemptId": id, "status": "running", "threadId": "thread", "turnId": "01a15bfb"
        });
        connection
            .execute(
                "INSERT INTO runtime_execution_runs VALUES(?1,?2,?3,2,'thread','01a15bfb',?4,?5)",
                rusqlite::params![
                    format!("run:{id}"),
                    AGENT,
                    ACCOUNT,
                    created,
                    run.to_string()
                ],
            )
            .expect("incident run writes");
    }
    with(&mut connection, |tx| {
        crate::ExecutionHistoryRepository::terminal(tx, AGENT, "01a15bfb", "interrupted", None)
    })
    .expect("the incident turn settles");
    let runs = turn_runs(&connection, "01a15bfb");
    assert_eq!(runs.len(), 3);
    assert!(
        runs.iter().all(|(_, status)| status == "interrupted"),
        "{runs:?}"
    );
}

/// An account worker whose supervised host provider outlives it ends only its
/// VM actors: their sessions belong to the worker. The host turn stays live
/// for the next worker of the same generation.
#[test]
fn a_retained_host_provider_disconnects_only_the_vm_actors() {
    let mut connection = database();
    let attempt = |id: &str| {
        json!({"id": id, "epoch": 0, "accountKey": ACCOUNT, "events": [format!("event-{id}")],
            "submitted": true, "turnId": "turn", "connectionGeneration": 4, "created": 1.0,
            "settingsFixed": true, "connectionId": "connection", "threadId": "thread"})
    };
    let running = |attempt: Value| {
        json!({"status": "running", "autoWake": true, "inFlight": true, "turnId": "turn",
            "error": null, "startAttempt": attempt})
    };
    put(&connection, running(attempt("host")));
    let mut vm = running(attempt("vm"));
    vm["id"] = json!("vm-agent");
    vm["rootId"] = json!("vm-agent");
    vm["executionMode"] = json!("vm");
    put(&connection, vm);
    let host = stored(&connection);
    let interrupted = with(&mut connection, |tx| {
        SchedulerRepository::disconnect_vm_actors(tx, ACCOUNT, 4, 20.0).expect("disconnect runs")
    });
    assert_eq!(interrupted, ["vm-agent"]);
    assert_eq!(stored(&connection), host);
    let vm = AgentRepository::get(&connection, &AgentId::from("vm-agent"))
        .expect("agent reads")
        .expect("agent exists")
        .value();
    assert_eq!(vm["status"], "interrupted");
    assert_eq!(vm["error"], PROVIDER_DISCONNECTED_TURN_ERROR);
}

#[test]
fn a_restart_of_an_attempt_without_a_supervisor_identity_records_no_supervisor() {
    // A Rust start attempt has a connection generation but no supervisor
    // identity. Its restart receipt once stored `{"generation": N}` as the
    // supervisor, which is not an identity and made `/api/diagnostics` fail
    // (live run of 25 October 2026 with `supervisor: {"generation": 96}`).
    // The receipt keeps the generation in its start attempt only.
    let mut connection = database();
    put(
        &connection,
        json!({
            "status": "running", "autoWake": true, "inFlight": true, "turnId": "turn-live",
            "turnEpoch": 0, "error": null,
            "startAttempt": {
                "id": "rust", "epoch": 0, "submitted": true, "turnId": "turn-live",
                "threadId": "thread", "accountKey": ACCOUNT, "events": ["message"],
                "created": 1.0, "connectionGeneration": 96, "requestTicket": "ticket"
            }
        }),
    );
    with(&mut connection, |tx| {
        SchedulerRepository::recover_submitted(tx).expect("startup recovery runs")
    });
    let agent = stored(&connection);
    for key in ["restartRecovery", "disconnectRecovery"] {
        assert_eq!(agent[key]["supervisor"], Value::Null, "{key}: {agent}");
        assert_eq!(
            agent[key]["startAttempt"]["connectionGeneration"], 96,
            "{key}: {agent}"
        );
    }
}
