//! Fail-point matrix of busy-turn input. Each row is one test below; the
//! account-worker witnesses in `runtime-services` drive the same rows through
//! the worker, and `claude-bridge/steer.test.mjs` proves the bridge's side.
//!
//! | Step / failure                                   | Durable result                         |
//! | ------------------------------------------------ | -------------------------------------- |
//! | claim; restart before submission                 | input `pending`, receipt removed       |
//! | claim; turn changes before submission            | `pending`, nothing written             |
//! | claim; turn ends before submission               | `pending`, nothing written             |
//! | submitted; transport proves not sent             | `pending`                              |
//! | submitted; reply lost                            | `dispatching`, never resent            |
//! | submitted; ack (before or after any reply)       | `delivered` to the turn, once          |
//! | submitted; definite rejection                    | `pending` + `steerRejectedTurnId`      |
//! | submitted; turn ends, bridge reports it unread   | `pending`, once                        |
//! | submitted; turn ends without ack                 | `failed` unconfirmed, never resent     |
//! | unconfirmed; late ack / definite rejection       | `delivered` / `pending`                |
//! | submitted; stop, then turn ends unread           | `cancelled`                            |
//! | submitted; transport generation closes           | `failed` unconfirmed                   |
use super::*;
use crate::records::{AgentId, AgentRepository};
use rusqlite::Connection;

const AGENT: &str = "agent";
const TURN: &str = "turn-1";

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
        .expect("steer tables create");
    let transaction = connection.transaction().expect("index transaction begins");
    SchedulerRepository::prepare_account_wake_index(&transaction).expect("wake index prepares");
    transaction.commit().expect("index commits");
    put_agent(&connection, running());
    for (id, created, meta) in [
        ("u1", 1.0, None),
        ("u2", 2.0, Some(json!({"assets":[],"delivery":"steer"}))),
        (
            "later",
            3.0,
            Some(json!({"assets":[],"delivery":"after_turn"})),
        ),
        ("u4", 4.0, None),
    ] {
        connection
            .execute(
                "INSERT INTO runtime_events VALUES(?1,'agent','user',?1,'pending',?2,4,NULL,NULL)",
                params![id, created],
            )
            .expect("event writes");
        if let Some(meta) = meta {
            connection
                .execute(
                    "INSERT INTO runtime_event_meta VALUES(?1,?2)",
                    params![id, meta.to_string()],
                )
                .expect("event metadata writes");
        }
    }
    connection
}

fn running() -> AgentRecord {
    let mut extra = BTreeMap::new();
    for (key, value) in [
        ("provider", json!("claude")),
        ("accountKey", json!("acct")),
        ("threadId", json!("thread")),
        ("turnId", json!(TURN)),
        ("turnEpoch", json!(4)),
        ("autoWake", json!(true)),
        ("inFlight", json!(true)),
        (
            "startAttempt",
            json!({"id":"start","epoch":4,"submitted":true,"turnId":TURN,"threadId":"thread",
                   "accountKey":"acct","events":["initial"],"connectionGeneration":7}),
        ),
    ] {
        extra.insert(key.to_owned(), value);
    }
    AgentRecord {
        id: AgentId::from(AGENT),
        root_id: AgentId::from(AGENT),
        epoch: 4,
        status: AgentStatus::Running,
        extra,
    }
}

fn put_agent(connection: &Connection, agent: AgentRecord) {
    AgentRepository::put(connection, &agent).expect("agent writes");
}

fn agent(connection: &Connection) -> AgentRecord {
    AgentRepository::get(connection, &AgentId::from(AGENT))
        .expect("agent reads")
        .expect("agent exists")
}

fn status(connection: &Connection, id: &str) -> (String, Option<String>, Option<String>) {
    connection
        .query_row(
            "SELECT status,turn_id,error FROM runtime_events WHERE id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("event reads")
}

fn statuses(connection: &Connection) -> Vec<String> {
    ["u1", "u2", "later", "u4"]
        .iter()
        .map(|id| status(connection, id).0)
        .collect()
}

fn open_receipts(connection: &Connection) -> i64 {
    connection
        .query_row(
            "SELECT COUNT(*) FROM runtime_operation_receipts WHERE signature=?1",
            [STEER_RECEIPT_SIGNATURE],
            |row| row.get(0),
        )
        .expect("receipts count")
}

fn with<T>(connection: &mut Connection, change: impl FnOnce(&Transaction<'_>) -> T) -> T {
    let tx = connection.transaction().expect("transaction begins");
    let result = change(&tx);
    tx.commit().expect("transaction commits");
    result
}

fn claim(connection: &mut Connection, steer: &str) -> Option<ClaimedSteer> {
    with(connection, |tx| {
        SchedulerRepository::claim_steer(tx, AGENT, 4, steer, 10.0).expect("claim executes")
    })
}

fn submitted(connection: &mut Connection, steer: &str, generation: u64) -> bool {
    with(connection, |tx| {
        SchedulerRepository::steer_submitted(tx, AGENT, steer, "ticket", generation)
            .expect("submission executes")
    })
}

fn submit(connection: &mut Connection, steer: &str) {
    claim(connection, steer).expect("input is claimable");
    assert!(submitted(connection, steer, 7));
}

fn acknowledge(connection: &mut Connection) -> Option<AcknowledgedSteer> {
    with(connection, |tx| {
        SchedulerRepository::acknowledge_steer(tx, AGENT, "u1", "thread", TURN)
            .expect("acknowledgement executes")
    })
}

fn settle(connection: &mut Connection, change: impl FnOnce(&Transaction<'_>) -> StoreResult<()>) {
    with(connection, change).expect("settlement executes");
}

fn terminal(connection: &mut Connection, unapplied: &[&str]) {
    settle(connection, |tx| {
        SchedulerRepository::resolve_steers_at_terminal(tx, AGENT, TURN, unapplied)
    });
}

fn retarget(connection: &Connection, turn: &str) {
    let mut record = agent(connection);
    record.extra.insert("turnId".into(), json!(turn));
    record.extra.get_mut("startAttempt").expect("attempt")["turnId"] = json!(turn);
    put_agent(connection, record);
}

const QUEUED: [&str; 4] = ["pending"; 4];
const SENT: [&str; 4] = ["dispatching", "dispatching", "pending", "pending"];
const UNCONFIRMED: [&str; 4] = ["failed", "failed", "pending", "pending"];

#[test]
fn claim_takes_the_ordered_steerable_prefix_once() {
    let mut connection = database();
    let claimed = claim(&mut connection, "s1").expect("running Claude turn claims");
    assert_eq!(claimed.turn_id, TURN);
    assert_eq!(claimed.thread_id, "thread");
    let ids = claimed
        .events
        .iter()
        .map(|e| e.id.0.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        ["u1", "u2"],
        "after-turn input stops the batch in order"
    );
    assert_eq!(
        statuses(&connection),
        ["reserved", "reserved", "pending", "pending"]
    );
    assert!(
        claim(&mut connection, "s2").is_none(),
        "one open steer at a time"
    );
    let start = &agent(&connection).extra["startAttempt"];
    assert_eq!(start["id"], "start", "the running turn keeps its attempt");
}

#[test]
fn claim_refuses_every_ineligible_agent_state() {
    type Change = fn(&mut AgentRecord);
    let cases: [(&str, Change); 7] = [
        ("codex", |a| {
            a.extra.insert("provider".into(), json!("codex"));
        }),
        ("queued", |a| a.status = AgentStatus::Queued),
        ("rejected-this-turn", |a| {
            a.extra.insert("steerRejectedTurnId".into(), json!(TURN));
        }),
        ("vm", |a| {
            a.extra.insert("executionMode".into(), json!("vm"));
        }),
        ("unbound-turn", |a| {
            a.extra.get_mut("startAttempt").expect("attempt")["turnId"] = Value::Null;
        }),
        ("held", |a| {
            a.extra.insert("nativeFailureHold".into(), json!(true));
        }),
        ("stopped", |a| {
            a.extra.insert("autoWake".into(), json!(false));
        }),
    ];
    for (name, change) in cases {
        let mut connection = database();
        let mut record = running();
        change(&mut record);
        put_agent(&connection, record);
        assert!(
            claim(&mut connection, "s1").is_none(),
            "{name} is not steered"
        );
        assert_eq!(statuses(&connection), QUEUED, "{name} input stays queued");
    }
    let mut connection = database();
    connection
        .execute(
            "UPDATE runtime_events SET status='uncertain' WHERE id='u4'",
            [],
        )
        .expect("unresolved input writes");
    assert!(
        claim(&mut connection, "s1").is_none(),
        "unresolved input blocks steering"
    );
}

#[test]
fn restart_before_submission_returns_the_reservation() {
    let mut connection = database();
    claim(&mut connection, "s1").expect("claims");
    settle(&mut connection, |tx| {
        SchedulerRepository::abandon_steers(tx, "acct", 8)
    });
    assert_eq!(statuses(&connection), QUEUED);
    assert_eq!(open_receipts(&connection), 0);
    assert!(
        !submitted(&mut connection, "s1", 8),
        "a released reservation can never be written"
    );
}

#[test]
fn turn_change_before_submission_releases_without_writing() {
    let mut connection = database();
    claim(&mut connection, "s1").expect("claims");
    retarget(&connection, "turn-2");
    assert!(!submitted(&mut connection, "s1", 7));
    assert_eq!(statuses(&connection), QUEUED);
    assert_eq!(open_receipts(&connection), 0);
}

#[test]
fn terminal_between_claim_and_submission_releases_the_reservation() {
    let mut connection = database();
    claim(&mut connection, "s1").expect("claims");
    terminal(&mut connection, &[]);
    assert_eq!(statuses(&connection), QUEUED);
    assert!(!submitted(&mut connection, "s1", 7));
}

#[test]
fn transport_not_sent_returns_input_once() {
    let mut connection = database();
    submit(&mut connection, "s1");
    assert_eq!(statuses(&connection), SENT);
    for _ in 0..2 {
        settle(&mut connection, |tx| {
            SchedulerRepository::steer_not_sent(tx, AGENT, "s1")
        });
        assert_eq!(statuses(&connection), QUEUED);
    }
    assert_eq!(open_receipts(&connection), 0);
}

#[test]
fn lost_response_keeps_input_dispatching_until_the_acknowledgement() {
    let mut connection = database();
    submit(&mut connection, "s1");
    settle(&mut connection, |tx| {
        SchedulerRepository::steer_response(tx, AGENT, "s1", "unknown")
    });
    assert_eq!(statuses(&connection), SENT);
    assert!(
        claim(&mut connection, "s2").is_none(),
        "unknown input is never resent"
    );
    let delivered = acknowledge(&mut connection).expect("ack matches");
    assert_eq!(delivered.events.len(), 2);
    assert_eq!(
        status(&connection, "u1"),
        ("delivered".into(), Some(TURN.into()), None)
    );
    assert_eq!(status(&connection, "u2").0, "delivered");
    assert!(
        acknowledge(&mut connection).is_none(),
        "a repeated ack is ignored"
    );
    assert_eq!(open_receipts(&connection), 0);
    assert!(
        claim(&mut connection, "s2").is_none(),
        "after-turn input still waits for the next turn"
    );
}

#[test]
fn acknowledgement_before_the_response_is_final() {
    let mut connection = database();
    submit(&mut connection, "s1");
    acknowledge(&mut connection).expect("ack matches");
    settle(&mut connection, |tx| {
        SchedulerRepository::steer_response(tx, AGENT, "s1", "accepted")?;
        SchedulerRepository::steer_rejected(tx, AGENT, "s1")?;
        SchedulerRepository::abandon_steers(tx, "acct", 9)?;
        SchedulerRepository::resolve_steers_at_terminal(tx, AGENT, TURN, &["u1"])
    });
    assert_eq!(
        statuses(&connection),
        ["delivered", "delivered", "pending", "pending"]
    );
    assert!(!agent(&connection).extra.contains_key("steerRejectedTurnId"));
}

#[test]
fn acknowledgement_must_match_thread_turn_and_first_input() {
    let mut connection = database();
    submit(&mut connection, "s1");
    for (first, thread, turn) in [
        ("u2", "thread", TURN),
        ("u1", "other", TURN),
        ("u1", "thread", "turn-2"),
    ] {
        let found = with(&mut connection, |tx| {
            SchedulerRepository::acknowledge_steer(tx, AGENT, first, thread, turn)
                .expect("ack executes")
        });
        assert!(found.is_none(), "{first} {thread} {turn} is not this steer");
    }
    assert_eq!(statuses(&connection), SENT);
}

#[test]
fn rejection_requeues_once_and_blocks_this_turn() {
    let mut connection = database();
    submit(&mut connection, "s1");
    for _ in 0..2 {
        settle(&mut connection, |tx| {
            SchedulerRepository::steer_rejected(tx, AGENT, "s1")
        });
        assert_eq!(statuses(&connection), QUEUED);
    }
    assert_eq!(agent(&connection).extra["steerRejectedTurnId"], TURN);
    assert!(
        claim(&mut connection, "s2").is_none(),
        "the same turn is not steered again"
    );
    retarget(&connection, "turn-2");
    assert!(
        claim(&mut connection, "s3").is_some(),
        "a later turn may receive it"
    );
}

#[test]
fn turn_end_with_unread_input_requeues_it_once() {
    let mut connection = database();
    submit(&mut connection, "s1");
    for _ in 0..2 {
        terminal(&mut connection, &["u1"]);
        assert_eq!(statuses(&connection), QUEUED);
    }
    assert_eq!(open_receipts(&connection), 0);
}

#[test]
fn turn_end_without_ack_is_visibly_unconfirmed_and_never_resent() {
    let mut connection = database();
    submit(&mut connection, "s1");
    terminal(&mut connection, &[]);
    let (state, _, error) = status(&connection, "u1");
    assert_eq!(state, "failed");
    assert_eq!(error.as_deref(), Some(STEER_UNCONFIRMED_ERROR));
    assert_eq!(statuses(&connection), UNCONFIRMED);
    terminal(&mut connection, &["u1"]);
    assert_eq!(
        statuses(&connection),
        UNCONFIRMED,
        "a later report cannot requeue it"
    );
    retarget(&connection, "turn-2");
    assert!(
        claim(&mut connection, "s2").is_none(),
        "unconfirmed input is not offered again"
    );
    // A late echo proves delivery and corrects the record.
    acknowledge(&mut connection).expect("late ack matches");
    assert_eq!(
        status(&connection, "u1"),
        ("delivered".into(), Some(TURN.into()), None)
    );
    assert_eq!(open_receipts(&connection), 0);
}

#[test]
fn late_acknowledgement_keeps_a_user_change_to_unconfirmed_input() {
    let mut connection = database();
    submit(&mut connection, "s1");
    terminal(&mut connection, &[]);
    connection
        .execute(
            "UPDATE runtime_events SET status='cancelled' WHERE id='u2'",
            [],
        )
        .expect("user change writes");
    let delivered = acknowledge(&mut connection).expect("late ack matches");
    assert_eq!(delivered.events.len(), 1);
    assert_eq!(status(&connection, "u1").0, "delivered");
    assert_eq!(status(&connection, "u2").0, "cancelled");
}

#[test]
fn late_acknowledgement_of_withdrawn_input_changes_nothing() {
    let mut connection = database();
    submit(&mut connection, "s1");
    terminal(&mut connection, &[]);
    connection
        .execute(
            "UPDATE runtime_events SET status='cancelled' WHERE id IN ('u1','u2')",
            [],
        )
        .expect("user change writes");
    assert!(acknowledge(&mut connection).is_none());
    assert_eq!(open_receipts(&connection), 0);
    assert_eq!(status(&connection, "u1").0, "cancelled");
}

#[test]
fn definite_rejection_after_unconfirmed_requeues_once() {
    let mut connection = database();
    submit(&mut connection, "s1");
    terminal(&mut connection, &[]);
    settle(&mut connection, |tx| {
        SchedulerRepository::steer_rejected(tx, AGENT, "s1")
    });
    assert_eq!(statuses(&connection), QUEUED);
    assert!(acknowledge(&mut connection).is_none());
}

#[test]
fn stop_before_turn_end_cancels_unread_input() {
    let mut connection = database();
    submit(&mut connection, "s1");
    let mut stopped = agent(&connection);
    stopped.epoch = 5;
    stopped.status = AgentStatus::Paused;
    stopped.extra.insert("autoWake".into(), json!(false));
    put_agent(&connection, stopped);
    terminal(&mut connection, &["u1"]);
    assert_eq!(status(&connection, "u1").0, "cancelled");
    assert_eq!(status(&connection, "u2").0, "cancelled");
}

#[test]
fn closed_generation_unconfirms_only_older_submissions() {
    let mut connection = database();
    submit(&mut connection, "s1");
    settle(&mut connection, |tx| {
        SchedulerRepository::abandon_steers(tx, "acct", 7)
    });
    assert_eq!(statuses(&connection), SENT, "the same generation keeps it");
    settle(&mut connection, |tx| {
        SchedulerRepository::abandon_steers(tx, "other", 9)
    });
    assert_eq!(statuses(&connection), SENT, "other accounts are untouched");
    settle(&mut connection, |tx| {
        SchedulerRepository::abandon_steers(tx, "acct", 8)
    });
    assert_eq!(statuses(&connection), UNCONFIRMED);
}

#[test]
fn wake_page_selects_running_claude_turns_with_queued_user_input() {
    let page = |connection: &Connection| {
        SchedulerRepository::account_wake_page(connection, "", &BTreeMap::new(), 4, 4)
            .expect("wake page reads")
    };
    let connection = database();
    assert_eq!(page(&connection)["acct"][0].0, AGENT);
    let mut rejected = running();
    rejected
        .extra
        .insert("steerRejectedTurnId".into(), json!(TURN));
    put_agent(&connection, rejected);
    assert!(page(&connection).is_empty());
    let mut codex = running();
    codex.extra.insert("provider".into(), json!("codex"));
    // A submitted Codex attempt with a generation is a recovery candidate.
    codex.extra.get_mut("startAttempt").expect("attempt")["connectionGeneration"] = Value::Null;
    put_agent(&connection, codex);
    assert!(page(&connection).is_empty());
    put_agent(&connection, running());
    connection
        .execute("UPDATE runtime_events SET kind='followup'", [])
        .expect("events change kind");
    assert!(
        page(&connection).is_empty(),
        "only user input wakes a busy turn"
    );
}
