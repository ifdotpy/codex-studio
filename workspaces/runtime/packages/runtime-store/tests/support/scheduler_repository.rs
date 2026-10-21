use super::{SchedulerRepository, select_turn_batch};
use crate::records::{AgentId, AgentRecord, AgentRepository, AgentStatus};
use crate::{QueueEvent, QueueEventId};
use rusqlite::Connection;
use serde_json::json;
use std::collections::BTreeMap;

fn database() -> Connection {
    let connection = Connection::open_in_memory().expect("memory database opens");
    connection
            .execute_batch(
                &crate::agent_columns::fixture_sql("CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,agent TEXT,account TEXT,epoch INTEGER,thread TEXT,turn TEXT,created REAL,record TEXT);
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
                 CREATE TABLE sync_versions(seq INTEGER);"),
            )
            .expect("scheduler tables create");
    connection
        .execute_batch(crate::agent_volatile::agent_volatile_table_sql())
        .expect("volatile agent table creates");
    let mut extra = BTreeMap::new();
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
fn terminal_pending_ownership_includes_unresolved_non_initial_inputs() {
    let connection = database();
    connection
        .execute("DELETE FROM runtime_events", [])
        .expect("clear fixture events");
    for (id, status, epoch) in [
        ("agent:initial", "uncertain", 4),
        ("dispatching", "dispatching", 4),
        ("uncertain", "uncertain", 4),
        ("other-epoch", "uncertain", 3),
        ("delivered", "delivered", 4),
    ] {
        connection
            .execute(
                "INSERT INTO runtime_events VALUES(?1,'agent','user',?1,?2,1.0,?3,NULL,NULL)",
                rusqlite::params![id, status, epoch],
            )
            .expect("event fixture writes");
    }
    assert!(
        SchedulerRepository::has_pending_continuation_event(&connection, "agent", 4)
            .expect("pending continuation query")
    );
    connection
        .execute(
            "DELETE FROM runtime_events WHERE id IN ('dispatching','uncertain')",
            [],
        )
        .expect("remove unresolved child inputs");
    assert!(
        !SchedulerRepository::has_pending_continuation_event(&connection, "agent", 4)
            .expect("initial receipt is excluded")
    );
}

#[test]
fn named_policy_refusal_leaves_every_table_unchanged() {
    let mut connection = database();
    connection
        .execute(
            "UPDATE runtime_events SET kind='agent_message' WHERE id='first'",
            [],
        )
        .expect("protected event fixture writes");
    let before = snapshot(&connection);
    let transaction = connection
        .transaction()
        .expect("refusal transaction begins");
    let refusal = SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 32)
        .expect("policy refusal is returned")
        .expect("protected event returns refusal");
    assert!(
        refusal
            .refusal_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("team-isolation-policy"))
    );
    transaction.commit().expect("read-only refusal commits");
    assert_eq!(snapshot(&connection), before);
}

#[test]
fn radio_turn_precedes_the_ordinary_32_event_prefix_without_writes() {
    let mut connection = database();
    for index in 0..29 {
        connection.execute(
                "INSERT INTO runtime_events VALUES(?1,'agent','user','ordinary','pending',?2,4,NULL,NULL)",
                rusqlite::params![format!("ordinary-{index}"), 3.0 + f64::from(index)],
            ).expect("ordinary prefix fixture writes");
    }
    connection.execute(
            "INSERT INTO runtime_events VALUES('radio','agent','radio_turn','{}','pending',99.0,4,NULL,NULL)",
            [],
        ).expect("radio turn fixture writes");
    let before = snapshot(&connection);
    let transaction = connection.transaction().expect("radio claim begins");
    let refusal = SchedulerRepository::claim(&transaction, "agent", 4, "radio-attempt", 100.0, 32)
        .expect("radio policy refusal is returned")
        .expect("radio turn takes priority");
    assert!(
        refusal
            .refusal_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("radio-policy"))
    );
    transaction.commit().expect("radio refusal commits");
    assert_eq!(snapshot(&connection), before);
}

#[test]
fn malformed_metadata_after_the_32_event_prefix_does_not_block_admission() {
    let mut connection = database();
    for index in 0..29 {
        connection
            .execute(
                "INSERT INTO runtime_events VALUES(?1,'agent','user','ordinary','pending',?2,4,NULL,NULL)",
                rusqlite::params![format!("ordinary-{index}"), 3.0 + f64::from(index)],
            )
            .expect("ordinary prefix fixture writes");
    }
    connection
        .execute(
            "INSERT INTO runtime_events VALUES('future','agent','user','future','pending',99.0,4,NULL,NULL)",
            [],
        )
        .expect("future event writes");
    connection
        .execute(
            "INSERT INTO runtime_event_meta VALUES('future','{\"assets\":\"malformed\"}')",
            [],
        )
        .expect("far-future metadata writes");
    let transaction = connection.transaction().expect("claim begins");
    let claim = SchedulerRepository::claim(&transaction, "agent", 4, "prefix", 100.0, 32)
        .expect("only prefix metadata is decoded")
        .expect("first 32 events are claimable");
    assert_eq!(claim.events.len(), 32);
    assert!(claim.events.iter().all(|event| event.id.0 != "future"));
    transaction.commit().expect("prefix claim commits");
}

#[test]
fn null_radio_answer_metadata_is_ordinary_input() {
    let mut connection = database();
    connection
        .execute(
            r#"INSERT INTO runtime_event_meta(id,record) VALUES('first','{"radioAnswerTurnId":null}')"#,
            [],
        )
        .expect("null radio receipt writes");
    let transaction = connection.transaction().expect("claim transaction begins");
    let claim = SchedulerRepository::claim(&transaction, "agent", 4, "ordinary", 10.0, 1)
        .expect("null radio receipt is valid")
        .expect("ordinary event is claimed");
    assert_eq!(claim.refusal_reason, None);
    assert_eq!(claim.events[0].id.0, "first");
    transaction.commit().expect("ordinary claim commits");
}

#[test]
fn malformed_radio_metadata_names_its_stored_object() {
    let mut connection = database();
    connection
        .execute(
            r#"INSERT INTO runtime_event_meta(id,record) VALUES('first','{"radioAnswerTurnId":7}')"#,
            [],
        )
        .expect("malformed radio receipt writes");
    let transaction = connection.transaction().expect("claim transaction begins");
    assert!(matches!(
        SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 1),
        Err(crate::StoreError::InvalidSyncObject { scope, id }) if scope == "runtime_event_meta" && id == "first"
    ));
    transaction
        .rollback()
        .expect("invalid stored record rolls back");
}

#[test]
fn malformed_complaint_and_work_policy_fields_name_the_table_and_id() {
    let mut complaint = database();
    complaint.execute_batch("CREATE TABLE runtime_complaints(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            UPDATE runtime_events SET kind='complaint',text='{\"complaints\":[{\"complaint_id\":\"bad\"}]}' WHERE id='first';
            INSERT INTO runtime_complaints VALUES('bad','{\"leadId\":\"agent\",\"status\":\"open\",\"recipient\":\"lead\",\"responses\":null}');")
            .expect("malformed complaint fixture writes");
    let transaction = complaint.transaction().expect("claim begins");
    assert!(
        matches!(SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 1),
            Err(crate::StoreError::InvalidSyncObject { scope, id }) if scope == "runtime_complaints" && id == "bad")
    );
    transaction
        .rollback()
        .expect("malformed complaint is not claimed");

    let mut work = database();
    work.execute_batch("CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            UPDATE runtime_events SET kind='work_review',text='{\"task\":\"task\",\"result\":{\"id\":\"result\"}}' WHERE id='first';
            INSERT INTO runtime_work VALUES('task','{\"rootId\":\"agent\",\"results\":\"invalid\",\"decisions\":[]}');")
            .expect("malformed work fixture writes");
    let transaction = work.transaction().expect("claim begins");
    assert!(
        matches!(SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 1),
            Err(crate::StoreError::InvalidSyncObject { scope, id }) if scope == "runtime_work" && id == "task")
    );
    transaction
        .rollback()
        .expect("malformed work is not claimed");
}

#[test]
fn resolved_complaint_and_decided_work_review_retire_before_claiming_the_remaining_input() {
    let mut connection = database();
    connection
        .execute_batch(
            "CREATE TABLE runtime_complaints(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             UPDATE runtime_events SET kind='complaint',text='{\"complaints\":[{\"complaint_id\":\"closed\"}]}' WHERE id='first';
             UPDATE runtime_events SET kind='work_review',text='{\"task\":\"task\",\"result\":{\"id\":\"result\"}}' WHERE id='tie';
             INSERT INTO runtime_complaints VALUES('closed','{\"leadId\":\"agent\",\"status\":\"resolved\",\"recipient\":\"lead\",\"responses\":[]}');
             INSERT INTO runtime_work VALUES('task','{\"rootId\":\"agent\",\"results\":[{\"id\":\"result\"}],\"decisions\":[{\"resultId\":\"result\"}]}');",
        )
        .expect("resolved policy fixtures write");
    let transaction = connection.transaction().expect("claim begins");
    let claim = SchedulerRepository::claim(&transaction, "agent", 4, "policy", 10.0, 3)
        .expect("resolved notices reconcile")
        .expect("remaining input is claimed");
    assert_eq!(
        claim
            .events
            .iter()
            .map(|event| event.id.0.as_str())
            .collect::<Vec<_>>(),
        ["later"]
    );
    transaction.commit().expect("policy preparation commits");
    let retired = connection
        .prepare(
            "SELECT id,status,error FROM runtime_events WHERE id IN ('first','tie') ORDER BY id",
        )
        .expect("retired rows prepare")
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .expect("retired rows query")
        .collect::<Result<Vec<_>, _>>()
        .expect("retired rows read");
    assert_eq!(
        retired,
        [
            (
                "first".to_owned(),
                "stored_only".to_owned(),
                "Every referenced complaint already has a response".to_owned()
            ),
            (
                "tie".to_owned(),
                "stored_only".to_owned(),
                "The exact result already has a decision".to_owned()
            ),
        ]
    );
}

#[test]
fn deleted_agent_is_skipped_at_claim_and_refused_at_submission_recheck() {
    let mut connection = database();
    connection
        .execute(
            "UPDATE runtime_agents SET record=json_set(record,'$.deletedAt',1)",
            [],
        )
        .expect("agent deletion writes");
    let transaction = connection.transaction().expect("claim begins");
    assert!(
        SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 1)
            .expect("deleted agents are ordinary skipped candidates")
            .is_none()
    );
    transaction
        .rollback()
        .expect("deleted agent claim rolls back");
    let mut agent = AgentRepository::get(&connection, &AgentId::from("agent"))
        .expect("deleted agent reads")
        .expect("agent exists");
    agent.extra.insert(
        "startAttempt".to_owned(),
        json!({"id":"attempt","epoch":4,"submitted":false,"events":["first"]}),
    );
    AgentRepository::put(&connection, &agent).expect("submission marker writes");
    let transaction = connection.transaction().expect("submission recheck begins");
    assert!(matches!(
        SchedulerRepository::reconcile_obsolete_before_submit(&transaction, "agent", 4, "attempt"),
        Err(crate::StoreError::InvalidSyncObject { scope, id }) if scope == "runtime_agents" && id == "agent"
    ));
    transaction
        .rollback()
        .expect("deleted submission is refused");
}

#[test]
fn submission_recheck_retires_decided_work_review_and_keeps_reconciliation_fields() {
    let mut connection = database();
    connection.execute_batch("CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            UPDATE runtime_events SET kind='work_review',text='{\"task\":\"task\",\"result\":{\"id\":\"result\"}}',status='reserved' WHERE id='first';
            INSERT INTO runtime_work VALUES('task','{\"rootId\":\"agent\",\"results\":[{\"id\":\"result\"}],\"decisions\":[{\"resultId\":\"result\"}]}');")
            .expect("work review becomes decided");
    let mut agent = AgentRepository::get(&connection, &AgentId::from("agent"))
        .expect("agent reads")
        .expect("agent exists");
    agent.status = AgentStatus::Starting;
    agent.extra.insert("inFlight".into(), json!(true));
    agent.extra.insert("startAttempt".into(), json!({"id":"attempt","epoch":4,"submitted":false,"activeAtReservation":false,"events":["first"]}));
    AgentRepository::put(&connection, &agent).expect("attempt fixture writes");
    let transaction = connection
        .transaction()
        .expect("projection transaction begins");
    crate::EventWriteRepository::set_agent_sync_lifecycle(
        &transaction,
        "agent",
        "starting",
        true,
        None,
    )
    .expect("sync projection follows reserved attempt");
    transaction.commit().expect("projection commits");
    let transaction = connection
        .transaction()
        .expect("submission transaction begins");
    assert!(
        !SchedulerRepository::reconcile_obsolete_before_submit(&transaction, "agent", 4, "attempt")
            .expect("obsolete review is reconciled before submission")
    );
    transaction.commit().expect("reconciliation commits");
    let status: String = connection
        .query_row(
            "SELECT status FROM runtime_events WHERE id='first'",
            [],
            |row| row.get(0),
        )
        .expect("review status reads");
    assert_eq!(status, "stored_only");
    let agent = AgentRepository::get(&connection, &AgentId::from("agent"))
        .expect("agent reads")
        .expect("agent exists");
    assert_eq!(
        agent.extra["startAttempt"]["notSubmittedReason"],
        "Notifications resolved before native submission"
    );
    assert_eq!(
        agent.extra["startAttempt"]["retiredEvents"],
        json!(["first"])
    );
    assert!(!agent.extra["inFlight"].as_bool().unwrap_or(true));
}

#[test]
fn child_results_share_a_turn_only_on_the_preparation_path() {
    let events = vec![
        selected_event("w1", "child_result", "{}"),
        selected_event("user", "user", "status?"),
        selected_event("w2", "child_result", "{}"),
        selected_event("monitor", "monitor", "{}"),
    ];
    let ids = |preparation| {
        select_turn_batch(&events, &BTreeMap::new(), events.len(), preparation)
            .iter()
            .map(|event| event.id.0.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(true), ["w1", "user", "w2", "monitor"]);
    assert!(ids(false).is_empty());
}

/// Messages that queued during a long turn reach the next turn together, in
/// queue order. One turn each made a worker act on an old decision before it
/// read the newer one (Lumina trackwork-parity, 20 October 2026: after a
/// 32-minute turn, twelve queued messages took twelve turns over 6 minutes).
#[test]
fn queued_orchestration_messages_share_one_turn_on_the_preparation_path() {
    let events = vec![
        selected_event("complaint-response", "complaint_response", "{}"),
        selected_event("rule", "rule", "{}"),
        selected_event("old-decision", "followup", "push with 600 s"),
        selected_event("message", "agent_message", "{}"),
        selected_event("new-decision", "followup", "push with 360 s"),
        selected_event("radio", "radio_turn", "{}"),
        selected_event("after-radio", "followup", "later"),
    ];
    let ids = |preparation| {
        select_turn_batch(&events, &BTreeMap::new(), events.len(), preparation)
            .iter()
            .map(|event| event.id.0.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(true),
        [
            "complaint-response",
            "rule",
            "old-decision",
            "message",
            "new-decision"
        ]
    );
    assert!(ids(false).is_empty());
}

#[test]
fn idle_batch_selection_combines_eligible_composer_inputs() {
    let events = vec![
        selected_event("ordinary", "user", "ordinary"),
        selected_event("after-turn", "user", "first composer input"),
        selected_event("send-now", "user", "second composer input"),
    ];
    let metadata = BTreeMap::from([
        ("after-turn".to_owned(), json!({"delivery":"after_turn"})),
        ("send-now".to_owned(), json!({"sendNow":true})),
    ]);
    assert_eq!(
        select_turn_batch(&events, &metadata, 32, true)
            .iter()
            .map(|event| event.id.0.as_str())
            .collect::<Vec<_>>(),
        ["ordinary", "after-turn", "send-now"]
    );
}

fn snapshot(connection: &Connection) -> serde_json::Value {
    let agent: String = connection
        .query_row(
            "SELECT record FROM runtime_agents WHERE id='agent'",
            [],
            |row| row.get(0),
        )
        .expect("agent snapshot reads");
    let events = connection
        .prepare("SELECT id,kind,status,error FROM runtime_events ORDER BY id")
        .expect("event snapshot prepares")
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .expect("event snapshot queries")
        .collect::<Result<Vec<_>, _>>()
        .expect("event snapshot reads");
    let metadata = connection
        .prepare("SELECT id,record FROM runtime_event_meta ORDER BY id")
        .expect("metadata snapshot prepares")
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("metadata snapshot queries")
        .collect::<Result<Vec<_>, _>>()
        .expect("metadata snapshot reads");
    let sync = connection.prepare("SELECT collection,id,seq,hash,payload,deleted FROM sync_entities ORDER BY collection,id").expect("sync snapshot prepares")
            .query_map([], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,i64>(2)?,row.get::<_,String>(3)?,row.get::<_,String>(4)?,row.get::<_,i64>(5)?)))
            .expect("sync snapshot queries").collect::<Result<Vec<_>,_>>().expect("sync snapshot reads");
    let completed = connection
        .prepare("SELECT id FROM runtime_completed_turns ORDER BY id")
        .expect("completed-turn snapshot prepares")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("completed-turn snapshot queries")
        .collect::<Result<Vec<_>, _>>()
        .expect("completed-turn snapshot reads");
    let sync_documents = connection
        .prepare("SELECT seq FROM sync_documents ORDER BY seq")
        .expect("sync-document snapshot prepares")
        .query_map([], |row| row.get::<_, i64>(0))
        .expect("sync-document snapshot queries")
        .collect::<Result<Vec<_>, _>>()
        .expect("sync-document snapshot reads");
    let sync_versions = connection
        .prepare("SELECT seq FROM sync_versions ORDER BY seq")
        .expect("sync-version snapshot prepares")
        .query_map([], |row| row.get::<_, i64>(0))
        .expect("sync-version snapshot queries")
        .collect::<Result<Vec<_>, _>>()
        .expect("sync-version snapshot reads");
    json!({"agent":agent,"events":events,"metadata":metadata,"sync":sync,
        "completed":completed,"syncDocuments":sync_documents,"syncVersions":sync_versions})
}

fn selected_event(id: &str, kind: &str, text: &str) -> QueueEvent {
    QueueEvent {
        id: QueueEventId::from(id),
        agent: AgentId::from("agent"),
        kind: kind.to_owned(),
        text: text.to_owned(),
        status: "pending".to_owned(),
        created: 1.0,
        epoch: 4,
        turn_id: None,
        error: None,
    }
}

#[test]
fn preparation_claims_one_thousand_small_mixed_events_and_replays_exact_batch() {
    let mut connection = database();
    connection
        .execute("DELETE FROM runtime_events", [])
        .expect("clear queue");
    for i in 0..1000 {
        connection
            .execute(
                "INSERT INTO runtime_events VALUES(?1,'agent',?2,'x','pending',?3,4,NULL,NULL)",
                rusqlite::params![
                    format!("e-{i:04}"),
                    if i % 2 == 0 { "user" } else { "work_decision" },
                    i as f64
                ],
            )
            .expect("small input");
    }
    let tx = connection.transaction().expect("claim begins");
    let claimed =
        SchedulerRepository::claim_for_preparation(&tx, "agent", 4, "mixed", 1001.0, usize::MAX)
            .expect("claim executes")
            .expect("claim exists");
    assert_eq!(claimed.events.len(), 1000);
    let replay =
        SchedulerRepository::claim_for_preparation(&tx, "agent", 4, "mixed", 1002.0, usize::MAX)
            .expect("replay executes")
            .expect("replay exists");
    assert_eq!(claimed.events, replay.events);
    assert_eq!(claimed.metadata, replay.metadata);
    tx.commit().expect("claim commits");
}

#[test]
fn mixed_batch_splits_only_at_the_owned_attachment_admission_budget() {
    let events = vec![
        selected_event("text", "user", "plain"),
        selected_event("first", "user", "first attachments"),
        selected_event("second", "user", "second attachments"),
    ];
    let assets = (0..(studio_runtime_schema::shared_values::MAX_TURN_ATTACHMENTS / 2 + 1))
        .map(|i| format!("asset-{i}"))
        .collect::<Vec<_>>();
    let metadata = BTreeMap::from([
        ("first".into(), json!({"assets":assets})),
        ("second".into(), json!({"assets":assets})),
    ]);
    let selected = select_turn_batch(&events, &metadata, usize::MAX, true);
    assert_eq!(
        selected.iter().map(|e| e.id.0.as_str()).collect::<Vec<_>>(),
        ["text", "first"]
    );
    let oversized = BTreeMap::from([(
        "first".into(),
        json!({"assets":vec!["asset";studio_runtime_schema::shared_values::MAX_TURN_ATTACHMENTS+1]}),
    )]);
    assert_eq!(
        select_turn_batch(&events[1..], &oversized, usize::MAX, true).len(),
        1,
        "invalid single input reaches explicit compiler refusal rather than disappearing"
    );
}
