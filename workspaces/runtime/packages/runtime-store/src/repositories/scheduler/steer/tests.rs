//! Fail-point matrix of busy-turn input. Each row is one test below; the
//! account-worker witnesses in `runtime-services` drive the same rows through
//! the worker, and `claude-bridge/steer.test.ts` proves the bridge's side.
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
//! | submitted or unconfirmed; written after the provider's reported turn end | `pending`, once |
//! | unconfirmed; late ack / definite rejection       | `delivered` / `pending`                |
//! | turn ended; rejection or not-sent returns input  | `pending`, idle agent `queued` to wake |
//! | submitted; stop, then turn ends unread           | `cancelled`                            |
//! | submitted; transport generation closes           | `failed` unconfirmed                   |
//! | submitted; restart: `recover()` reconciles turn  | `failed` unconfirmed, before `uncertain` |
//! | submitted; worker closes during submission       | `pending`, nothing written (worker)    |
//! | submitted; restart recovery settles the turn     | unconfirmed / `pending` (`settle_terminal`) |
//! | unconfirmed; a later turn of the agent ends      | receipt dropped, input stays `failed`  |
//! | any; wake while the queue is blocked or empty    | read only, no write transaction        |
use super::*;
use crate::records::{AgentId, AgentRepository};
use rusqlite::Connection;

const AGENT: &str = "agent";
const TURN: &str = "turn-1";

fn database() -> Connection {
    let mut connection = Connection::open_in_memory().expect("memory database opens");
    connection
        .execute_batch(
            &crate::agent_columns::fixture_sql("CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,agent TEXT,account TEXT,epoch INTEGER,thread TEXT,turn TEXT,created REAL,record TEXT);
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
             CREATE TABLE sync_versions(seq INTEGER);"),
        )
        .expect("steer tables create");
    connection
        .execute_batch(crate::agent_volatile::agent_volatile_table_sql())
        .expect("volatile agent table creates");
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
const SENT: [&str; 4] = ["dispatching", "dispatching", "pending", "dispatching"];
const UNCONFIRMED: [&str; 4] = ["failed", "failed", "pending", "failed"];

#[test]
fn claim_takes_eligible_current_turn_input_in_order_once() {
    let mut connection = database();
    let claimed = claim(&mut connection, "s1").expect("running turn claims");
    assert_eq!(claimed.turn_id, TURN);
    assert_eq!(claimed.thread_id, "thread");
    let ids = claimed
        .events
        .iter()
        .map(|e| e.id.0.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        ["u1", "u2", "u4"],
        "deferred input stays queued while later current-turn input is eligible"
    );
    assert_eq!(
        statuses(&connection),
        ["reserved", "reserved", "pending", "reserved"]
    );
    assert!(
        claim(&mut connection, "s2").is_none(),
        "one open steer at a time"
    );
    let start = &agent(&connection).extra["startAttempt"];
    assert_eq!(start["id"], "start", "the running turn keeps its attempt");
}

#[test]
fn codex_and_unlabelled_turns_are_steered_like_claude() {
    for provider in [Some("codex"), None] {
        let mut connection = database();
        let mut record = running();
        match provider {
            Some(provider) => record.extra.insert("provider".into(), json!(provider)),
            None => record.extra.remove("provider"),
        };
        put_agent(&connection, record);
        let claimed = claim(&mut connection, "s1").expect("running turn claims");
        assert_eq!(claimed.turn_id, TURN, "{provider:?}");
    }
}

#[test]
fn claim_refuses_every_ineligible_agent_state() {
    type Change = fn(&mut AgentRecord);
    let cases: [(&str, Change); 6] = [
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
        let requeued = with(&mut connection, |tx| {
            SchedulerRepository::steer_not_sent(tx, AGENT, "s1").expect("release executes")
        });
        assert!(!requeued, "the running turn keeps the agent busy");
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
    assert_eq!(delivered.events.len(), 3);
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
        ["delivered", "delivered", "pending", "delivered"]
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
        let requeued = with(&mut connection, reject);
        assert!(!requeued, "the running turn keeps the agent busy");
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
    assert_eq!(delivered.events.len(), 2);
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
            "UPDATE runtime_events SET status='cancelled' WHERE id IN ('u1','u2','u4')",
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
    // The turn ended first: `settle_terminal` left the agent idle.
    let mut idle = agent(&connection);
    idle.status = AgentStatus::Completed;
    idle.extra.insert("inFlight".into(), json!(false));
    idle.extra.insert("turnId".into(), Value::Null);
    put_agent(&connection, idle);
    assert!(
        with(&mut connection, reject),
        "the idle agent is queued to wake"
    );
    assert_eq!(statuses(&connection), QUEUED);
    assert_eq!(agent(&connection).status, AgentStatus::Queued);
    assert!(
        !with(&mut connection, reject),
        "a repeated reply changes nothing"
    );
    assert!(acknowledge(&mut connection).is_none());
}

fn release_after_end(connection: &mut Connection, ended_at: f64) {
    settle(connection, |tx| {
        SchedulerRepository::release_steers_written_after_turn_end(tx, AGENT, TURN, ended_at)
    });
}

#[test]
fn steer_written_after_the_reported_turn_end_returns_once() {
    let mut connection = database();
    submit(&mut connection, "s1");
    let submitted_at = connection
        .query_row(
            "SELECT json_extract(result,'$.submittedAt') FROM runtime_operation_receipts",
            [],
            |row| row.get::<_, f64>(0),
        )
        .expect("receipt reads");
    // Written before the end, or too close to it to order the two clock reads.
    for ended_at in [submitted_at + 60.0, submitted_at - 1.0, f64::NAN] {
        release_after_end(&mut connection, ended_at);
        assert_eq!(statuses(&connection), SENT, "ended at {ended_at}");
    }
    release_after_end(&mut connection, submitted_at - 60.0);
    assert_eq!(statuses(&connection), QUEUED);
    assert_eq!(open_receipts(&connection), 0);
    terminal(&mut connection, &[]);
    assert_eq!(statuses(&connection), QUEUED, "the turn end finds no steer");
    assert!(acknowledge(&mut connection).is_none());
}

#[test]
fn steer_unconfirmed_by_a_closed_transport_returns_when_written_after_the_end() {
    let mut connection = database();
    submit(&mut connection, "s1");
    settle(&mut connection, |tx| {
        SchedulerRepository::abandon_steers(tx, "acct", 8)
    });
    assert_eq!(statuses(&connection), UNCONFIRMED);
    let submitted_at: f64 = connection
        .query_row(
            "SELECT json_extract(result,'$.submittedAt') FROM runtime_operation_receipts",
            [],
            |row| row.get(0),
        )
        .expect("persisted submission time");
    release_after_end(&mut connection, submitted_at - 60.0);
    assert_eq!(statuses(&connection), QUEUED);
    assert_eq!(open_receipts(&connection), 0);
    // An input the user changed while it was unconfirmed keeps that change.
    let mut connection = database();
    submit(&mut connection, "s1");
    settle(&mut connection, |tx| {
        SchedulerRepository::abandon_steers(tx, "acct", 8)
    });
    connection
        .execute(
            "UPDATE runtime_events SET status='cancelled' WHERE id='u2'",
            [],
        )
        .expect("user change writes");
    let submitted_at: f64 = connection
        .query_row(
            "SELECT json_extract(result,'$.submittedAt') FROM runtime_operation_receipts",
            [],
            |row| row.get(0),
        )
        .expect("persisted submission time");
    release_after_end(&mut connection, submitted_at - 60.0);
    assert_eq!(
        statuses(&connection),
        ["pending", "cancelled", "pending", "pending"]
    );
}

fn reject(tx: &Transaction<'_>) -> bool {
    SchedulerRepository::steer_rejected(tx, AGENT, "s1").expect("rejection executes")
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
fn wake_page_selects_running_turns_of_every_provider_with_queued_user_input() {
    let page = |connection: &Connection| {
        SchedulerRepository::account_wake_page(connection, "", &BTreeMap::new(), 4, 4, 0.0)
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
    assert_eq!(
        page(&connection)["acct"][0].0,
        AGENT,
        "Codex turns are steered too"
    );
    put_agent(&connection, running());
    connection
        .execute("UPDATE runtime_events SET kind='followup'", [])
        .expect("events change kind");
    assert_eq!(
        page(&connection)["acct"][0].0,
        AGENT,
        "followups wake a busy turn too"
    );
}

#[test]
fn generation_recovery_unconfirms_steers_before_disconnecting_the_turn() {
    // `reconcile_generation` (account-worker recovery) and `disconnect_account`
    // (worker finish) turn `dispatching` rows `uncertain`; steers are decided first.
    for disconnect in [true, false] {
        let mut connection = database();
        submit(&mut connection, "s1");
        settle(&mut connection, |tx| {
            if disconnect {
                SchedulerRepository::disconnect_account(tx, "acct", 7, 20.0, false).map(drop)
            } else {
                SchedulerRepository::reconcile_generation(tx, "acct", 8, 20.0, true).map(drop)
            }
        });
        let (state, _, error) = status(&connection, "u1");
        assert_eq!(state, "failed", "disconnect={disconnect}");
        assert_eq!(error.as_deref(), Some(STEER_UNCONFIRMED_ERROR));
        assert_eq!(statuses(&connection), UNCONFIRMED);
        acknowledge(&mut connection).expect("a late ack still repairs it");
        assert_eq!(status(&connection, "u1").0, "delivered");
    }
}

#[test]
fn every_terminal_settlement_resolves_open_steers() {
    // Restart recovery settles a turn through `settle_terminal` without the
    // notification path; even a stale attempt identity resolves the turn's steers.
    for (unapplied, expected) in [(&[][..], UNCONFIRMED), (&["u1"][..], QUEUED)] {
        let mut connection = database();
        submit(&mut connection, "s1");
        let settled = with(&mut connection, |tx| {
            SchedulerRepository::settle_terminal(
                tx,
                AGENT,
                4,
                "unknown-attempt",
                TURN,
                AgentStatus::Completed,
                None,
                "completed",
                None,
                unapplied,
            )
            .expect("settlement executes")
        });
        assert!(!settled, "the stale attempt itself is not settled");
        assert_eq!(statuses(&connection), expected);
        retarget(&connection, "turn-2");
        assert_eq!(
            claim(&mut connection, "s2").is_some(),
            !unapplied.is_empty(),
            "only requeued input is offered to the next turn"
        );
    }
}

#[test]
fn a_later_turn_end_drops_earlier_unconfirmed_receipts() {
    let mut connection = database();
    submit(&mut connection, "s1");
    terminal(&mut connection, &[]);
    assert_eq!(open_receipts(&connection), 1);
    retarget(&connection, "turn-2");
    settle(&mut connection, |tx| {
        SchedulerRepository::resolve_steers_at_terminal(tx, AGENT, "turn-2", &[])
    });
    assert_eq!(open_receipts(&connection), 0);
    assert_eq!(
        statuses(&connection),
        UNCONFIRMED,
        "the input stays visibly failed"
    );
}

#[test]
fn claimable_check_matches_the_claim_without_writing() {
    let mut connection = database();
    let ready = |connection: &Connection, epoch| {
        let before = connection.total_changes();
        let ready = SchedulerRepository::steer_claimable(connection, &agent(connection), epoch)
            .expect("check reads");
        assert_eq!(
            connection.total_changes(),
            before,
            "eligibility writes no rows"
        );
        ready
    };
    assert!(ready(&connection, 4));
    assert!(!ready(&connection, 3), "another epoch");
    submit(&mut connection, "s1");
    assert!(
        !ready(&connection, 4),
        "an unresolved steer blocks the queue"
    );
    terminal(&mut connection, &["u1"]);
    assert!(ready(&connection, 4));
    connection
        .execute("UPDATE runtime_events SET status='cancelled'", [])
        .expect("queue empties");
    assert!(!ready(&connection, 4), "nothing pending");
}

#[test]
fn current_user_behind_mixed_notifications_and_next_turn_keeps_exact_batch() {
    let mut connection = database();
    connection
        .execute(
            "UPDATE runtime_events SET kind='work_available' WHERE id='u1'",
            [],
        )
        .expect("work event");
    connection
        .execute(
            "UPDATE runtime_events SET kind='work_decision' WHERE id='u2'",
            [],
        )
        .expect("decision event");
    connection
        .execute(
            "UPDATE runtime_event_meta SET record='{\"delivery\":\"next_turn\"}' WHERE id='later'",
            [],
        )
        .expect("deferred event");
    let claimed = claim(&mut connection, "mixed").expect("mixed current turn claims");
    assert_eq!(
        claimed
            .events
            .iter()
            .map(|e| (e.id.0.as_str(), e.kind.as_str()))
            .collect::<Vec<_>>(),
        [
            ("u1", "work_available"),
            ("u2", "work_decision"),
            ("u4", "user")
        ]
    );
    assert_eq!(
        claimed.metadata["u2"]["delivery"], "steer",
        "stored provenance is carried"
    );
    assert!(submitted(&mut connection, "mixed", 7));
    settle(&mut connection, |tx| {
        SchedulerRepository::steer_response(tx, AGENT, "mixed", "unknown")
    });
    assert!(claim(&mut connection, "duplicate").is_none());
    let ack = acknowledge(&mut connection).expect("exact acknowledgement");
    assert_eq!(ack.steer_id, "mixed");
    assert_eq!(ack.events.len(), 3);
    assert_eq!(status(&connection, "later").0, "pending");
    assert!(acknowledge(&mut connection).is_none());
}

#[test]
fn one_thousand_small_mixed_events_are_one_busy_batch() {
    let mut connection = database();
    connection
        .execute("DELETE FROM runtime_events", [])
        .expect("clear queue");
    connection
        .execute("DELETE FROM runtime_event_meta", [])
        .expect("clear metadata");
    for i in 0..1000 {
        connection
            .execute(
                "INSERT INTO runtime_events VALUES(?1,'agent',?2,'x','pending',?3,4,NULL,NULL)",
                params![
                    format!("e-{i:04}"),
                    if i % 2 == 0 { "user" } else { "work_decision" },
                    i as f64
                ],
            )
            .expect("small event");
    }
    let claimed = claim(&mut connection, "thousand").expect("one batch");
    assert_eq!(claimed.events.len(), 1000);
    assert_eq!(claimed.events.first().expect("first").id.0, "e-0000");
    assert_eq!(claimed.events.last().expect("last").id.0, "e-0999");
    assert!(submitted(&mut connection, "thousand", 7));
    let delivered = with(&mut connection, |tx| {
        SchedulerRepository::acknowledge_steer(tx, AGENT, "e-0000", "thread", TURN)
    })
    .expect("ack executes")
    .expect("ack matches");
    assert_eq!(delivered.events.len(), 1000);
}

#[test]
fn native_commands_assets_and_radio_answers_wait_without_blocking_current_input() {
    let mut connection = database();
    connection
        .execute(
            "UPDATE runtime_events SET text='/compact' WHERE id='u1'",
            [],
        )
        .expect("native command");
    connection
        .execute(
            "UPDATE runtime_event_meta SET record='{\"assets\":[\"file\"]}' WHERE id='u2'",
            [],
        )
        .expect("asset");
    connection.execute("UPDATE runtime_event_meta SET record='{\"radioAnswerTurnId\":\"radio\"}' WHERE id='later'",[]).expect("radio answer");
    assert!(
        SchedulerRepository::steer_claimable(&connection, &agent(&connection), 4)
            .expect("read eligibility")
    );
    let claimed = claim(&mut connection, "safe").expect("current input behind special handling");
    assert_eq!(
        claimed
            .events
            .iter()
            .map(|e| e.id.0.as_str())
            .collect::<Vec<_>>(),
        ["u4"]
    );
}

#[test]
fn invalid_delivery_metadata_fails_closed_and_next_turn_only_is_read_only() {
    let connection = database();
    connection
        .execute(
            "UPDATE runtime_event_meta SET record='{\"delivery\":\"next_turn\"}'",
            [],
        )
        .expect("deferred metadata");
    connection
        .execute(
            "UPDATE runtime_events SET status='cancelled' WHERE id IN ('u1','u4')",
            [],
        )
        .expect("only deferred remain");
    let before = connection.total_changes();
    assert!(
        !SchedulerRepository::steer_claimable(&connection, &agent(&connection), 4)
            .expect("eligibility reads")
    );
    assert_eq!(before, connection.total_changes());
    connection
        .execute(
            "UPDATE runtime_event_meta SET record='{\"delivery\":\"invalid\"}' WHERE id='u2'",
            [],
        )
        .expect("invalid delivery");
    assert!(
        matches!(SchedulerRepository::steer_claimable(&connection,&agent(&connection),4),Err(StoreError::InvalidSyncObject{scope,id}) if scope=="runtime_event_meta" && id=="u2")
    );
}

#[test]
fn frozen_projection_survives_response_loss_and_rejects_payload_changes() {
    let mut connection = database();
    submit(&mut connection, "frozen");
    let projection = json!({"operation":"frozen","compiled":{"text":"bounded evidence"}});
    for _ in 0..2 {
        settle(&mut connection, |tx| {
            SchedulerRepository::freeze_steer_submission(tx, AGENT, "frozen", &projection)
        });
    }
    let changed = with(&mut connection, |tx| {
        SchedulerRepository::freeze_steer_submission(
            tx,
            AGENT,
            "frozen",
            &json!({"compiled":"changed"}),
        )
    });
    assert!(changed.is_err(), "a retry cannot rewrite frozen content");
    settle(&mut connection, |tx| {
        SchedulerRepository::steer_response(tx, AGENT, "frozen", "unknown")
    });
    terminal(&mut connection, &[]);
    let ack = acknowledge(&mut connection).expect("late acknowledgement");
    assert_eq!(ack.prepared_submission, Some(projection));
    assert_eq!(ack.event_ids, ["u1", "u2", "u4"]);
    assert_eq!(open_receipts(&connection), 0);
}

#[test]
fn business_notification_resolved_during_busy_preparation_releases_the_exact_batch() {
    let mut connection = database();
    connection
        .execute_batch("CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
        .expect("work table");
    connection
        .execute(
            "UPDATE runtime_events SET kind='work_review',text=?1 WHERE id='u1'",
            [json!({"task":"work","result":{"id":"result","text":"ready"}}).to_string()],
        )
        .expect("review event");
    connection
        .execute(
            "INSERT INTO runtime_work VALUES('work',?1)",
            [
                json!({"id":"work","rootId":AGENT,"results":[{"id":"result"}],"decisions":[]})
                    .to_string(),
            ],
        )
        .expect("undecided work");
    for id in ["progress-one", "progress-two"] {
        connection.execute("INSERT INTO runtime_events VALUES(?1,'agent','agent_message',?2,'pending',2.5,4,NULL,NULL)",params![id,json!({"sender":"child","room":"room","importance":"progress","progress_key":"work","progress_version":1,"text":id}).to_string()]).expect("progress fixture writes");
        connection.execute("INSERT INTO runtime_event_meta VALUES(?1,?2)",params![id,json!({"delivery":"current_turn","requestedDelivery":"current_turn","acceptedAt":17,"source":"retained"}).to_string()]).expect("progress fixture writes");
    }
    claim(&mut connection, "obsolete").expect("claims review and user input");
    let progress_metadata = |connection: &Connection| {
        let mut statement = connection
            .prepare("SELECT record FROM runtime_event_meta WHERE id LIKE 'progress-%' ORDER BY id")
            .expect("progress metadata query");
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("progress metadata reads")
            .collect::<Result<Vec<_>, _>>()
            .expect("progress metadata decodes")
    };
    let before = progress_metadata(&connection);
    connection.execute("UPDATE runtime_work SET record=json_set(record,'$.decisions',json('[{\"resultId\":\"result\"}]')) WHERE id='work'",[]).expect("review gets decided");
    let current = with(&mut connection, |tx| {
        SchedulerRepository::reconcile_obsolete_steer_before_submit(tx, AGENT, "obsolete")
    })
    .expect("reconcile");
    assert!(!current);
    let after = progress_metadata(&connection);
    assert_eq!(
        after, before,
        "submission reconciliation preserves progress flags and original metadata byte-for-byte"
    );
    assert_eq!(status(&connection, "u1").0, "stored_only");
    assert_eq!(status(&connection, "u2").0, "pending");
    assert_eq!(status(&connection, "u4").0, "pending");
    assert_eq!(open_receipts(&connection), 0);
    assert!(!submitted(&mut connection, "obsolete", 7));
}

#[test]
fn stop_receipt_refines_unconfirmed_input_and_rejects_wrong_scope() {
    let mut connection = database();
    submit(&mut connection, "steer");
    terminal(&mut connection, &[]);
    let response = json!({"cancelled":["u1"],"still_queued":[]});
    for (account, generation, thread) in [
        ("other", 7, "thread"),
        ("acct", 8, "thread"),
        ("acct", 7, "other"),
    ] {
        settle(&mut connection, |tx| {
            SchedulerRepository::settle_interrupt_inputs(
                tx, AGENT, account, generation, thread, &response,
            )
        });
        assert_eq!(status(&connection, "u1").0, "failed");
    }
    for _ in 0..2 {
        settle(&mut connection, |tx| {
            SchedulerRepository::settle_interrupt_inputs(tx, AGENT, "acct", 7, "thread", &response)
        });
        assert_eq!(status(&connection, "u1").0, "cancelled");
    }
    assert_eq!(
        statuses(&connection),
        ["cancelled", "cancelled", "pending", "cancelled"]
    );
    assert!(acknowledge(&mut connection).is_none());
}

#[test]
fn stop_receipt_survivor_stays_uncertain_until_cancellation_or_acknowledgement() {
    for cancel in [false, true] {
        let mut connection = database();
        submit(&mut connection, "steer");
        settle(&mut connection, |tx| {
            SchedulerRepository::settle_interrupt_inputs(
                tx,
                AGENT,
                "acct",
                7,
                "thread",
                &json!({"cancelled":["u1"],"still_queued":["u1"]}),
            )
        });
        terminal(&mut connection, &[]);
        terminal(&mut connection, &[]);
        assert_eq!(status(&connection, "u1").0, "uncertain");
        if cancel {
            settle(&mut connection, |tx| {
                SchedulerRepository::settle_interrupt_inputs(
                    tx,
                    AGENT,
                    "acct",
                    7,
                    "thread",
                    &json!({"cancelled":["u1"],"still_queued":[]}),
                )
            });
            assert_eq!(status(&connection, "u1").0, "cancelled");
        } else {
            assert!(acknowledge(&mut connection).is_some());
            assert_eq!(status(&connection, "u1").0, "delivered");
        }
    }
}

#[test]
fn stop_receipt_cancels_the_initial_prompt_in_pending_dispatch() {
    let mut connection = database();
    connection.execute("INSERT INTO runtime_events VALUES('initial','agent','user','first','delivered',0,4,'turn-1',NULL)", []).expect("initial prompt inserts");
    settle(&mut connection, |tx| {
        SchedulerRepository::settle_interrupt_inputs(
            tx,
            AGENT,
            "acct",
            7,
            "thread",
            &json!({"cancelled":["initial"],"still_queued":[]}),
        )
    });
    assert_eq!(status(&connection, "initial").0, "cancelled");
    assert_eq!(statuses(&connection), QUEUED);
}

/// SQLite virtual machine steps of one steer acknowledgement of the fixture
/// input `u1`, after `history` delivered events of another agent. The state
/// has the production `runtime_event_queue` index from the schema owner.
fn acknowledgement_work(history: usize) -> u64 {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    let snapshot: Value = serde_json::from_str(include_str!("../../../runtime-state-schema.json"))
        .expect("schema snapshot parses");
    let queue_index = snapshot["canvas.sqlite3"]["sqlite_master"]
        .as_array()
        .expect("schema objects")
        .iter()
        .find(|object| object["name"] == "runtime_event_queue")
        .and_then(|object| object["sql"].as_str())
        .expect("queue index is in the schema");
    let mut connection = database();
    connection
        .execute_batch(queue_index)
        .expect("queue index creates");
    with(&mut connection, |tx| {
        let mut insert = tx
            .prepare("INSERT INTO runtime_events VALUES(?1,'other','user','x','delivered',?2,1,'old-turn',NULL)")
            .expect("history insert");
        for index in 0..history {
            insert
                .execute(params![format!("history-{index:06}"), index as f64])
                .expect("history event");
        }
    });
    submit(&mut connection, "steer");
    let steps = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&steps);
    connection
        .progress_handler(
            1,
            Some(move || {
                counter.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )
        .expect("progress counter");
    let ack = acknowledge(&mut connection).expect("acknowledgement matches");
    connection
        .progress_handler(1, None::<fn() -> bool>)
        .expect("progress counter");
    assert_eq!(
        ack.events
            .iter()
            .map(|e| e.id.0.as_str())
            .collect::<Vec<_>>(),
        ["u1", "u2", "u4"]
    );
    steps.load(Ordering::Relaxed)
}

/// The account worker acknowledges each delivered steer inside the single
/// Canvas writer. On 23 October 2026 its read of the delivered rows searched
/// `runtime_event_queue` by `status='delivered'` and visited all 162,187
/// delivered events of the state (0.38 s per acknowledgement). The work must
/// stay bounded by the acknowledged input, not by delivered history.
#[test]
fn steer_acknowledgement_work_is_independent_of_delivered_history() {
    let small = acknowledgement_work(128);
    let large = acknowledgement_work(512);
    assert!(
        large <= small + small / 20,
        "acknowledgement work grew with delivered history: {small} steps, then {large} with 384 more delivered events"
    );
}
