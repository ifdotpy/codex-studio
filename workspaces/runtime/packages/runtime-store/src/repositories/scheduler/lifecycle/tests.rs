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
        .execute_batch(
            "CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,agent TEXT,account TEXT,epoch INTEGER,thread TEXT,turn TEXT,created REAL,record TEXT);
             CREATE TABLE runtime_execution_attempts(id TEXT PRIMARY KEY,run TEXT,record TEXT);
             CREATE TABLE runtime_execution_inputs(run TEXT,event TEXT,PRIMARY KEY(run,event));
             CREATE TABLE runtime_completed_turns(id TEXT PRIMARY KEY);
             CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,
               created REAL,epoch INTEGER,turn_id TEXT,error TEXT);
             CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL);
             CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,
               PRIMARY KEY(collection,id));
             CREATE TABLE sync_documents(seq INTEGER);
             CREATE TABLE sync_versions(seq INTEGER);",
        )
        .expect("scheduler tables create");
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
fn a_running_python_turn_waits_for_history_reconciliation_without_a_hold() {
    for attempt in [
        // Agent a5fa7930: a Python attempt with its turn.
        Some(python_attempt("live", Some("turn-live"), true)),
        // Agent be102fa0: a supervisor-restored turn without an attempt.
        None,
    ] {
        let mut connection = database();
        let mut record = json!({
            "status": "running", "autoWake": true, "inFlight": true, "turnId": "turn-live",
            "turnEpoch": 0, "error": null
        });
        if let Some(attempt) = attempt {
            record["startAttempt"] = attempt;
        }
        put(&connection, record);
        restart_and_reconnect(&mut connection, 1);
        let agent = stored(&connection);
        assert_eq!(agent["status"], "interrupted");
        assert_eq!(agent["error"], BACKEND_RESTARTED_TURN_ERROR);
        assert_eq!(agent["restartRecovery"]["stage"], "pending");
        assert_eq!(agent["restartRecovery"]["turnId"], "turn-live");
        assert_eq!(agent["disconnectRecovery"]["source"], "restart");
        assert!(agent.get("nativeFailureHold").is_none(), "{agent}");
        assert!(agent.get("startOutcomeHold").is_none(), "{agent}");
    }
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
    assert_eq!(held["nativeFailureHold"], true);
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
        (None, None, LEGACY_RECOVERY),
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
