//! Durable pending-event selection and claim primitives for the scheduler.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

use crate::records::{AgentId, AgentRecord, AgentRepository, AgentStatus, get_json_record};

use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{
    EventWriteRepository, ExecutionHistoryRepository, QueueEvent, StoreError, StoreResult,
};

const MAX_EVENTS_PER_TURN: usize = 32;
const TABLES: &[TableRequirement] = &[
    TableRequirement {
        name: "runtime_agents",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_completed_turns",
        columns: &["id"],
    },
    TableRequirement {
        name: "runtime_events",
        columns: &[
            "id", "agent", "kind", "text", "status", "created", "epoch", "turn_id", "error",
        ],
    },
];
const EVENT_META: &[TableRequirement] = &[TableRequirement {
    name: "runtime_event_meta",
    columns: &["id", "record"],
}];
const COMPLAINTS: &[TableRequirement] = &[TableRequirement {
    name: "runtime_complaints",
    columns: &["id", "record"],
}];
const WORK: &[TableRequirement] = &[TableRequirement {
    name: "runtime_work",
    columns: &["id", "record"],
}];

pub type SubmittedStartRequest = (String, i64, String, String, String);

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[TABLES[0], TABLES[1]],
    conditional_tables: &[
        ConditionalTableRequirement {
            table_exists: "runtime_complaints",
            tables: COMPLAINTS,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_event_meta",
            tables: EVENT_META,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_work",
            tables: WORK,
        },
    ],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[TABLES[2]],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Debug, PartialEq)]
pub struct ClaimedEvents {
    pub agent: AgentRecord,
    pub events: Vec<QueueEvent>,
    pub metadata: BTreeMap<String, Value>,
    pub refusal_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnAttemptIdentity {
    pub agent_id: String,
    pub epoch: i64,
    pub attempt_id: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SchedulerRepository;

mod lifecycle;
mod selection;
use lifecycle::*;
use selection::*;

#[cfg(test)]
mod tests {
    use super::SchedulerRepository;
    use super::{interrupt_uncertain, same_recovery_scope};
    use crate::records::{AgentId, AgentRecord, AgentRepository, AgentStatus};
    use rusqlite::Connection;
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn recovery_and_interrupt_policies_keep_exact_scope() {
        let a = json!({"epoch":1,"accountKey":"a","threadId":"t","turnId":"u","startAttempt":{"id":"one","connectionGeneration":3}});
        assert!(same_recovery_scope(&a, &a));
        let mut unknown = a.clone();
        unknown["turnId"] = serde_json::Value::Null;
        assert!(same_recovery_scope(&unknown, &a));
        for key in ["epoch", "accountKey", "threadId", "turnId"] {
            let mut changed = a.clone();
            changed[key] = json!("different");
            assert!(!same_recovery_scope(&a, &changed));
        }
        let mut changed = a.clone();
        changed["startAttempt"]["id"] = json!("two");
        assert!(!same_recovery_scope(&a, &changed));
        for (outcome, blocked) in [
            ("unknown", true),
            ("accepted", true),
            ("rejected", false),
            ("unsent", false),
        ] {
            let attempt = json!({"interrupt":{"threadId":"t","turnId":"u","outcome":outcome}});
            assert_eq!(interrupt_uncertain(&attempt, "t", "u"), blocked);
            assert!(!interrupt_uncertain(&attempt, "t", "other"));
        }
    }
    fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,agent TEXT,account TEXT,epoch INTEGER,thread TEXT,turn TEXT,created REAL,record TEXT);
                 CREATE TABLE runtime_execution_attempts(id TEXT PRIMARY KEY,run TEXT,record TEXT);
                 CREATE TABLE runtime_execution_inputs(run TEXT,event TEXT,PRIMARY KEY(run,event));
                 CREATE TABLE runtime_completed_turns(id TEXT PRIMARY KEY);
                 CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_events(
                   id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,
                   created REAL,epoch INTEGER,turn_id TEXT,error TEXT);
                 CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,
                   PRIMARY KEY(collection,id));
                 CREATE TABLE sync_documents(seq INTEGER);
                 CREATE TABLE sync_versions(seq INTEGER);",
            )
            .expect("scheduler tables create");
        let mut extra = BTreeMap::new();
        extra.insert("threadId".into(), json!("thread"));
        extra.insert("autoWake".to_owned(), json!(true));
        extra.insert("inFlight".to_owned(), json!(false));
        AgentRepository::put(
            &connection,
            &AgentRecord {
                id: AgentId::from("agent"),
                root_id: AgentId::from("agent"),
                epoch: 4,
                status: AgentStatus::Queued,
                extra,
            },
        )
        .expect("agent fixture writes");
        connection
            .execute(
                "INSERT INTO sync_entities VALUES('agent','agent',1,'hash',?1,0)",
                [json!({
                    "collection":"agent", "id":"agent",
                    "value":{"id":"agent", "status":"queued", "overview":{
                        "result":"", "resultTruncated":false, "resultTurnId":null
                    }}
                })
                .to_string()],
            )
            .expect("agent sync projection fixture writes");
        for (id, created) in [("later", 2.0), ("first", 1.0), ("tie", 1.0)] {
            connection
                .execute(
                    "INSERT INTO runtime_events VALUES(?1,'agent','user',?1,'pending',?2,4,NULL,NULL)",
                    rusqlite::params![id, created],
                )
                .expect("event fixture writes");
        }
        connection
    }

    #[test]
    fn claims_ordered_pending_events_and_updates_agent_in_one_transaction() {
        let mut connection = database();
        let transaction = connection.transaction().expect("claim transaction begins");
        let claimed = SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 2)
            .expect("claim executes")
            .expect("pending events exist");
        assert_eq!(
            claimed
                .events
                .iter()
                .map(|event| event.id.0.as_str())
                .collect::<Vec<_>>(),
            ["first", "tie"]
        );
        assert_eq!(claimed.agent.extra["startAttempt"]["submitted"], false);
        transaction.commit().expect("claim commits");
        let agent = AgentRepository::get(&connection, &AgentId::from("agent"))
            .expect("agent reads")
            .expect("agent remains");
        assert_eq!(agent.status, AgentStatus::Starting);
        assert_eq!(
            agent.extra["startAttempt"]["events"],
            json!(["first", "tie"])
        );
        let pending: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_events WHERE status='pending'",
                [],
                |row| row.get(0),
            )
            .expect("pending count reads");
        assert_eq!(pending, 1);
    }

    #[test]
    fn restart_requeues_only_an_exact_unsubmitted_reservation() {
        let mut connection = database();
        let transaction = connection.transaction().expect("claim transaction begins");
        SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 2)
            .expect("claim executes");
        transaction.commit().expect("claim commits");
        let transaction = connection
            .transaction()
            .expect("recovery transaction begins");
        assert_eq!(
            SchedulerRepository::recover_unsubmitted(&transaction).expect("recovery executes"),
            2
        );
        transaction.commit().expect("recovery commits");
        let agent = AgentRepository::get(&connection, &AgentId::from("agent"))
            .expect("agent reads")
            .expect("agent remains");
        assert_eq!(agent.status, AgentStatus::Queued);
        assert!(!agent.extra.contains_key("startAttempt"));
        let reserved: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_events WHERE status='reserved'",
                [],
                |row| row.get(0),
            )
            .expect("reserved count reads");
        assert_eq!(reserved, 0);
    }

    #[test]
    fn stop_revocation_cancels_only_the_exact_unsubmitted_claim() {
        let mut connection = database();
        let transaction = connection.transaction().expect("claim transaction begins");
        let _claimed = SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 1)
            .expect("claim executes")
            .expect("pending event exists");
        transaction.commit().expect("claim commits");

        let transaction = connection.transaction().expect("stop transaction begins");
        transaction
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.epoch',5,'$.status','paused','$.autoWake',json('false'),'$.inFlight',json('false'))",
                [],
            )
            .expect("stop revokes agent epoch");
        assert!(
            !SchedulerRepository::mark_submitted_identity(
                &transaction,
                "agent",
                4,
                "attempt",
                "ticket-attempt",
                "default",
                1,
                "thread"
            )
            .expect("revoked claim cannot submit")
        );
        SchedulerRepository::settle_start_outcome(
            &transaction,
            &super::TurnAttemptIdentity {
                agent_id: "agent".into(),
                epoch: 4,
                attempt_id: "attempt".into(),
            },
            "pending",
            None,
        )
        .expect("unsent claim settles");
        transaction.commit().expect("stop cancellation commits");

        let status: String = connection
            .query_row(
                "SELECT status FROM runtime_events WHERE id='first'",
                [],
                |row| row.get(0),
            )
            .expect("old event status reads");
        assert_eq!(status, "cancelled");
        let agent = AgentRepository::get(&connection, &AgentId::from("agent"))
            .expect("stopped agent reads")
            .expect("stopped agent remains");
        assert_eq!(agent.epoch, 5);
        assert_eq!(agent.status, AgentStatus::Paused);

        let mut resumed = agent;
        resumed.status = AgentStatus::Queued;
        resumed.extra.insert("autoWake".to_owned(), json!(true));
        resumed.extra.insert("inFlight".to_owned(), json!(false));
        AgentRepository::put(&connection, &resumed).expect("resumed agent writes");
        connection
            .execute(
                "INSERT INTO runtime_events VALUES('resumed','agent','user','resumed','pending',3.0,5,NULL,NULL)",
                [],
            )
            .expect("resumed event writes");
        let transaction = connection.transaction().expect("resumed claim begins");
        let next = SchedulerRepository::claim(&transaction, "agent", 5, "next", 11.0, 1)
            .expect("resumed claim executes")
            .expect("resumed input is not blocked by old reservation");
        assert_eq!(next.events[0].id.0, "resumed");
        transaction.commit().expect("resumed claim commits");
    }

    #[test]
    fn submitted_attempt_is_not_recovered() {
        let mut connection = database();
        let transaction = connection.transaction().expect("claim transaction begins");
        SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 2)
            .expect("claim executes");
        transaction
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.startAttempt.submitted',json('true'))",
                [],
            )
            .expect("submitted marker writes");
        transaction.commit().expect("claim commits");
        let transaction = connection
            .transaction()
            .expect("recovery transaction begins");
        assert_eq!(
            SchedulerRepository::recover_unsubmitted(&transaction).expect("recovery executes"),
            0
        );
        transaction.commit().expect("recovery commits");
        let event_status: String = connection
            .query_row(
                "SELECT status FROM runtime_events WHERE id='first'",
                [],
                |row| row.get(0),
            )
            .expect("reserved status reads");
        assert_eq!(event_status, "reserved");
    }

    #[test]
    fn submitted_claim_is_not_recovered_or_admitted_again() {
        let mut connection = database();
        let transaction = connection.transaction().expect("claim transaction begins");
        SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 2)
            .expect("claim executes");
        assert!(
            SchedulerRepository::mark_submitted_identity(
                &transaction,
                "agent",
                4,
                "attempt",
                "ticket-attempt",
                "default",
                1,
                "thread"
            )
            .expect("submitted marker persists")
        );
        transaction.commit().expect("claim commits");

        let transaction = connection
            .transaction()
            .expect("recovery transaction begins");
        assert_eq!(
            SchedulerRepository::recover_unsubmitted(&transaction).expect("recovery executes"),
            0
        );
        transaction.commit().expect("recovery commits");

        connection
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.status','queued','$.inFlight',0)",
                [],
            )
            .expect("agent is queued with unresolved attempt");
        connection
            .execute(
                "INSERT INTO runtime_events VALUES('latest','agent','user','latest','pending',3,4,NULL,NULL)",
                [],
            )
            .expect("later pending event writes");
        let transaction = connection
            .transaction()
            .expect("second claim transaction begins");
        assert!(
            SchedulerRepository::claim(&transaction, "agent", 4, "attempt-2", 11.0, 32)
                .expect("unresolved attempt blocks admission")
                .is_none()
        );
        transaction.commit().expect("second claim commits");
        let later_status: String = connection
            .query_row(
                "SELECT status FROM runtime_events WHERE id='latest'",
                [],
                |row| row.get(0),
            )
            .expect("later event status reads");
        assert_eq!(later_status, "pending");
    }

    #[test]
    fn recovery_cancels_exact_stale_claim_and_recovers_other_agents() {
        let mut connection = database();
        let transaction = connection.transaction().expect("claim transaction begins");
        SchedulerRepository::claim(&transaction, "agent", 4, "old-attempt", 10.0, 2)
            .expect("claim executes");
        transaction
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.epoch',5,'$.status','queued','$.inFlight',0)",
                [],
            )
            .expect("agent epoch advances");
        let mut other_extra = BTreeMap::new();
        other_extra.insert("autoWake".to_owned(), json!(true));
        other_extra.insert("inFlight".to_owned(), json!(false));
        AgentRepository::put(
            &transaction,
            &AgentRecord {
                id: AgentId::from("other"),
                root_id: AgentId::from("other"),
                epoch: 1,
                status: AgentStatus::Queued,
                extra: other_extra,
            },
        )
        .expect("independent agent writes");
        transaction
            .execute(
                "INSERT INTO sync_entities VALUES('agent','other',2,'hash',?1,0)",
                [json!({
                    "collection":"agent", "id":"other",
                    "value":{"id":"other", "status":"queued", "overview":{
                        "result":"", "resultTruncated":false, "resultTurnId":null
                    }}
                })
                .to_string()],
            )
            .expect("independent sync projection writes");
        transaction
            .execute(
                "INSERT INTO runtime_events VALUES('other-first','other','user','input','pending',1,1,NULL,NULL)",
                [],
            )
            .expect("independent event writes");
        SchedulerRepository::claim(&transaction, "other", 1, "other-attempt", 11.0, 1)
            .expect("independent agent claim succeeds");
        transaction.commit().expect("old claim commits");

        let transaction = connection
            .transaction()
            .expect("recovery transaction begins");
        assert_eq!(
            SchedulerRepository::recover_unsubmitted(&transaction).expect("stale recovery skips"),
            1
        );
        transaction.commit().expect("recovery commits");
        let status: String = connection
            .query_row(
                "SELECT status FROM runtime_events WHERE id='first'",
                [],
                |row| row.get(0),
            )
            .expect("stale reservation status reads");
        assert_eq!(status, "cancelled");
        let other_status: String = connection
            .query_row(
                "SELECT status FROM runtime_events WHERE id='other-first'",
                [],
                |row| row.get(0),
            )
            .expect("independent reservation recovers");
        assert_eq!(other_status, "pending");
    }

    #[test]
    fn native_failure_hold_blocks_claim_without_writes() {
        let mut connection = database();
        connection
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.nativeFailureHold',json('true'))",
                [],
            )
            .expect("native failure hold writes");
        let transaction = connection.transaction().expect("claim transaction begins");
        assert!(
            SchedulerRepository::claim(&transaction, "agent", 4, "held", 10.0, 2)
                .expect("held agent claim is refused")
                .is_none()
        );
        transaction
            .commit()
            .expect("refused claim commits without changes");
        let pending: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_events WHERE status='pending'",
                [],
                |row| row.get(0),
            )
            .expect("pending events remain");
        assert_eq!(pending, 3);
        let agent = AgentRepository::get(&connection, &AgentId::from("agent"))
            .expect("held agent reads")
            .expect("held agent remains");
        assert_eq!(agent.status, AgentStatus::Queued);
        assert!(!agent.extra.contains_key("startAttempt"));
    }

    #[test]
    fn native_failure_hold_added_after_claim_blocks_submission() {
        let mut connection = database();
        let transaction = connection.transaction().expect("claim transaction begins");
        let _claimed = SchedulerRepository::claim(&transaction, "agent", 4, "held", 10.0, 1)
            .expect("initial claim executes")
            .expect("pending event exists");
        transaction.commit().expect("initial claim commits");

        let transaction = connection.transaction().expect("hold transaction begins");
        let mut agent = AgentRepository::get(&transaction, &AgentId::from("agent"))
            .expect("agent reads")
            .expect("agent remains");
        agent
            .extra
            .insert("nativeFailureHold".to_owned(), json!(true));
        AgentRepository::put(&transaction, &agent).expect("hold persists");
        assert!(
            !SchedulerRepository::mark_submitted_identity(
                &transaction,
                "agent",
                4,
                "held",
                "ticket-held",
                "default",
                1,
                "thread"
            )
            .expect("hold refuses submission")
        );
        SchedulerRepository::settle_start_outcome(
            &transaction,
            &super::TurnAttemptIdentity {
                agent_id: "agent".into(),
                epoch: 4,
                attempt_id: "held".into(),
            },
            "pending",
            None,
        )
        .expect("unsent claim settles");
        transaction.commit().expect("hold cancellation commits");
        let status: String = connection
            .query_row(
                "SELECT status FROM runtime_events WHERE id='first'",
                [],
                |row| row.get(0),
            )
            .expect("held event status reads");
        assert_eq!(status, "pending");
    }

    #[test]
    fn recovery_does_not_override_a_same_epoch_stop_state() {
        let mut connection = database();
        let transaction = connection.transaction().expect("claim transaction begins");
        SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 2)
            .expect("claim executes");
        transaction
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.status','paused','$.autoWake',json('false'))",
                [],
            )
            .expect("stop state writes");
        transaction.commit().expect("claim commits");

        let transaction = connection
            .transaction()
            .expect("recovery transaction begins");
        assert_eq!(
            SchedulerRepository::recover_unsubmitted(&transaction)
                .expect("stopped attempt is retained"),
            0
        );
        transaction.commit().expect("recovery commits");
        let agent = AgentRepository::get(&connection, &AgentId::from("agent"))
            .expect("agent reads")
            .expect("agent remains");
        assert_eq!(agent.status, AgentStatus::Paused);
        assert_eq!(agent.extra["autoWake"], false);
        let event_status: String = connection
            .query_row(
                "SELECT status FROM runtime_events WHERE id='first'",
                [],
                |row| row.get(0),
            )
            .expect("event status reads");
        assert_eq!(event_status, "reserved");
    }

    #[test]
    fn malformed_attempt_event_ids_are_refused_without_requeueing() {
        let mut connection = database();
        let transaction = connection.transaction().expect("claim transaction begins");
        SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 1)
            .expect("claim executes");
        transaction
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.startAttempt.events',json('[\"first\",null]'))",
                [],
            )
            .expect("malformed attempt writes");
        transaction.commit().expect("claim commits");

        let transaction = connection
            .transaction()
            .expect("recovery transaction begins");
        assert!(matches!(
            SchedulerRepository::recover_unsubmitted(&transaction),
            Err(crate::StoreError::InvalidSyncObject { .. })
        ));
        transaction.rollback().expect("refused recovery rolls back");
        let status: String = connection
            .query_row(
                "SELECT status FROM runtime_events WHERE id='first'",
                [],
                |row| row.get(0),
            )
            .expect("reserved event remains");
        assert_eq!(status, "reserved");
    }

    #[test]
    fn refusal_to_claim_does_not_change_agent_or_events() {
        let mut connection = database();
        connection
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.autoWake',0)",
                [],
            )
            .expect("disabled autowake writes");
        let before_agent: String = connection
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='agent'",
                [],
                |row| row.get(0),
            )
            .expect("agent snapshot reads");
        let before_events: Vec<(String, String)> = connection
            .prepare("SELECT id,status FROM runtime_events ORDER BY id")
            .expect("event snapshot prepares")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("event snapshot queries")
            .collect::<Result<_, _>>()
            .expect("event snapshot reads");
        let transaction = connection
            .transaction()
            .expect("refusal transaction begins");
        assert!(
            SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 32)
                .expect("claim refusal is not an error")
                .is_none()
        );
        transaction.commit().expect("refusal transaction commits");
        let after_agent: String = connection
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='agent'",
                [],
                |row| row.get(0),
            )
            .expect("agent snapshot reads");
        let after_events: Vec<(String, String)> = connection
            .prepare("SELECT id,status FROM runtime_events ORDER BY id")
            .expect("event snapshot prepares")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("event snapshot queries")
            .collect::<Result<_, _>>()
            .expect("event snapshot reads");
        assert_eq!(after_agent, before_agent);
        assert_eq!(after_events, before_events);
    }

    #[test]
    fn claim_replays_saved_result_and_refuses_changed_input_after_recovery() {
        let mut connection = database();
        let tx = connection.transaction().expect("transaction");
        let first = SchedulerRepository::claim(&tx, "agent", 4, "stable", 10.0, 1)
            .expect("claim")
            .expect("input");
        let retry = SchedulerRepository::claim(&tx, "agent", 4, "stable", 10.0, 1)
            .expect("retry")
            .expect("saved claim");
        assert_eq!(retry, first);
        SchedulerRepository::recover_unsubmitted(&tx).expect("restore unsent");
        let same = SchedulerRepository::claim(&tx, "agent", 4, "stable", 10.0, 1)
            .expect("recover retry")
            .expect("same input");
        assert_eq!(same, first);
        SchedulerRepository::recover_unsubmitted(&tx).expect("restore again");
        tx.execute(
            "UPDATE runtime_events SET text='changed input' WHERE id='first'",
            [],
        )
        .expect("changed fixture");
        let before: String = tx
            .query_row(
                "SELECT group_concat(record) FROM runtime_execution_attempts",
                [],
                |r| r.get(0),
            )
            .expect("before");
        assert!(matches!(
            SchedulerRepository::claim(&tx, "agent", 4, "stable", 10.0, 1),
            Err(crate::StoreError::InvalidSyncObject { .. })
        ));
        let after: String = tx
            .query_row(
                "SELECT group_concat(record) FROM runtime_execution_attempts",
                [],
                |r| r.get(0),
            )
            .expect("after");
        assert_eq!(before, after);
        let reserved: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM runtime_events WHERE status='reserved'",
                [],
                |r| r.get(0),
            )
            .expect("no reservation");
        assert_eq!(reserved, 0);
    }
}

#[cfg(test)]
#[path = "../../tests/support/scheduler_repository.rs"]
mod policy_tests;
inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "scheduler",
        requirements: &REQUIREMENTS,
    }
}
