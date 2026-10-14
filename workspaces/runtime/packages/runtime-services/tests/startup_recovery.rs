#[path = "monitors/fixtures.rs"]
mod fixtures;
#[allow(clippy::duplicate_mod)]
#[path = "support/prepared_python.rs"]
mod prepared_python_support;

use serde_json::{Value, json};
use studio_runtime_services::{
    monitor_policy::Operation,
    monitor_receipts::{self, ResultReceipt},
    monitor_runtime,
};
use studio_runtime_store::{
    AgentRecord, AgentRepository, MonitorsRepository, QueueEventId, QueueEventRepository,
    RequestRecord, RuntimeStore, Table,
};

fn running(cases: &[Value]) -> Value {
    cases
        .iter()
        .find(|case| case["name"] == "finish-0-None-False")
        .expect("running monitor case")
        .clone()
}

fn operation(case: &Value) -> Operation {
    serde_json::from_value(case["before"]["operation"].clone()).expect("monitor operation")
}

fn add_request(tx: &rusqlite::Transaction<'_>, request: Value) {
    let record = RequestRecord::from_value(request).expect("valid request row");
    Table::<RequestRecord>::new(tx)
        .put(&record)
        .expect("store request");
}

fn add_event(tx: &rusqlite::Transaction<'_>, id: &str, agent: &str, status: &str, epoch: i64) {
    tx.execute(
        "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) \
         VALUES(?1,?2,'message','{}',?3,1000,?4,NULL,NULL)",
        (id, agent, status, epoch),
    )
    .expect("store event");
}

fn complete_rows(store: &RuntimeStore, id: &str) -> Value {
    let connection = store.read().expect("read startup state");
    let monitor = MonitorsRepository::get(&connection, id)
        .expect("read monitor")
        .expect("monitor exists")
        .value()
        .clone();
    let requests = Table::<RequestRecord>::new(&connection)
        .list()
        .expect("read requests")
        .into_iter()
        .map(|request| request.value().clone())
        .collect::<Vec<_>>();
    let events = fixtures::events(store);
    json!({"monitor": monitor, "requests": requests, "events": events})
}

#[test]
fn startup_recovery_is_idempotent_and_preserves_only_retained_questions() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    let store = fixtures::store(root.path(), "startup");
    fixtures::seed(&store, &case);
    let actor = case["actor"].clone();
    store
        .transaction(|tx| {
            add_request(
                tx,
                json!({"id":"approval","agent":"agent","epoch":2,"method":"monitor/approve","status":"pending","createdAt":1000,"params":{"monitorId":case["before"]["id"]}}),
            );
            add_request(
                tx,
                json!({"id":"answering","agent":"agent","epoch":2,"method":"item/commandExecution/requestApproval","status":"answering","createdAt":1000,"params":{}}),
            );
            add_request(
                tx,
                json!({"id":"retained","agent":"agent","epoch":2,"method":"agent/asyncQuestion","status":"pending","createdAt":1000,"params":{"questions":[]}}),
            );
            add_request(
                tx,
                json!({"id":"orphan","method":"mcpServer/elicitation/request","status":"pending","createdAt":1000,"params":{"mode":"form","requestedSchema":{"properties":{}}}}),
            );
            add_event(tx, "dispatching-without-attempt", "agent", "dispatching", 2);
            add_event(tx, "reserved-without-attempt", "orphan-agent", "reserved", 0);
            AgentRepository::put(tx, &AgentRecord::from_value(actor)?)?;
            Ok(())
        })
        .expect("seed startup records");

    let first = monitor_runtime::recover_on_startup(&store, 2000.0).expect("startup recovery");
    assert_eq!(first.requests, 3);
    assert_eq!(first.events, 2);
    assert_eq!(first.monitors, 1);
    let monitor_id = case["before"]["id"].as_str().expect("monitor id");
    let connection = store.read().expect("read recovered state");
    let monitor = MonitorsRepository::get(&connection, monitor_id)
        .expect("read lost monitor")
        .expect("monitor exists");
    assert_eq!(monitor.value()["status"], "lost");
    assert_eq!(
        monitor.value()["error"],
        "Server restarted. Command outcome unknown; not rerun."
    );
    assert_eq!(monitor.value()["reattachRecovery"]["status"], "running");
    let requests = Table::<RequestRecord>::new(&connection)
        .list()
        .expect("list recovered requests")
        .into_iter()
        .map(|request| (request.id().to_string(), request.value().clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(requests["approval"]["status"], "expired");
    assert_eq!(requests["answering"]["status"], "uncertain");
    assert_eq!(
        requests["answering"]["answerError"],
        "Server restarted before answer delivery completed"
    );
    assert_eq!(requests["retained"]["status"], "pending");
    assert_eq!(requests["orphan"]["status"], "expired");
    assert_eq!(
        QueueEventRepository::get(
            &connection,
            &QueueEventId::from(format!("monitor:{monitor_id}")),
        )
        .expect("read monitor exit")
        .expect("exit event")
        .status,
        "pending"
    );
    assert_eq!(
        QueueEventRepository::get(
            &connection,
            &QueueEventId::from("dispatching-without-attempt")
        )
        .expect("read event")
        .expect("event")
        .status,
        "uncertain"
    );
    assert_eq!(
        QueueEventRepository::get(&connection, &QueueEventId::from("reserved-without-attempt"))
            .expect("read event")
            .expect("event")
            .status,
        "uncertain"
    );
    drop(connection);

    let after_first = complete_rows(&store, monitor_id);
    let second = monitor_runtime::recover_on_startup(&store, 3000.0).expect("repeat startup");
    assert_eq!(second.requests, 0);
    assert_eq!(second.events, 0);
    assert_eq!(second.monitors, 0);
    assert_eq!(complete_rows(&store, monitor_id), after_first);
}

#[test]
fn startup_recovers_a_committed_monitor_result_before_marking_active_rows_lost() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    let store = fixtures::store(root.path(), "result-first");
    fixtures::seed(&store, &case);
    let id = case["before"]["id"].as_str().expect("monitor id");
    monitor_receipts::persist(
        store.root(),
        &ResultReceipt {
            version: 1,
            key: id.into(),
            code: Some(0),
            error: None,
            operation: Some(operation(&case)),
            finished: 1500.0,
        },
    )
    .expect("persist native result");

    let report = monitor_runtime::recover_on_startup(&store, 2000.0).expect("recover startup");
    assert_eq!(report.receipts, 1);
    assert_eq!(report.monitors, 0);
    let connection = store.read().expect("read recovered result");
    assert_eq!(
        MonitorsRepository::get(&connection, id)
            .expect("read monitor")
            .expect("monitor")
            .value()["status"],
        "completed"
    );
    assert_eq!(
        std::fs::read_dir(store.root().join("monitor-results"))
            .expect("result directory")
            .count(),
        0
    );
}

#[test]
fn unreadable_receipt_is_skipped_and_other_receipts_are_recovered() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    let store = fixtures::store(root.path(), "unreadable-receipt");
    fixtures::seed(&store, &case);
    let lost_id = "unreadable-result-monitor";
    let completed_id = case["before"]["id"]
        .as_str()
        .expect("monitor id")
        .to_owned();
    let mut lost_monitor = case["before"].clone();
    lost_monitor["id"] = json!(lost_id);
    store
        .transaction(|tx| {
            Table::new(tx).put(&studio_runtime_store::MonitorRecord::from_value(
                lost_monitor,
            )?)
        })
        .expect("seed monitor whose result file is unreadable");
    monitor_receipts::persist(
        store.root(),
        &ResultReceipt {
            version: 1,
            key: completed_id.clone(),
            code: Some(0),
            error: None,
            operation: Some(operation(&case)),
            finished: 1500.0,
        },
    )
    .expect("persist valid result");
    let corrupt_path = store.root().join("monitor-results").join(format!(
        "{}.json",
        studio_runtime_store::sha256_hex(lost_id.as_bytes())
    ));
    std::fs::write(&corrupt_path, b"{").expect("write corrupt receipt witness");

    let report = monitor_runtime::recover_on_startup(&store, 2000.0)
        .expect("skip corrupt receipt and continue startup");

    assert_eq!(report.receipts, 1);
    assert_eq!(report.monitors, 1);
    let connection = store.read().expect("read receipt recovery results");
    assert_eq!(
        MonitorsRepository::get(&connection, &completed_id)
            .expect("read valid result monitor")
            .expect("valid result monitor exists")
            .value()["status"],
        "completed"
    );
    let lost = MonitorsRepository::get(&connection, lost_id)
        .expect("read unreadable-result monitor")
        .expect("unreadable-result monitor exists");
    assert_eq!(lost.value()["status"], "lost");
    assert_eq!(
        lost.value()["error"],
        "Server restarted. Command outcome unknown; not rerun."
    );
    assert!(
        corrupt_path.exists(),
        "corrupt receipt is retained for diagnosis"
    );
}

#[test]
fn startup_write_failure_rolls_back_all_rows_and_next_start_converges() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    for table in ["runtime_requests", "runtime_events", "runtime_monitors"] {
        let store = fixtures::store(root.path(), table);
        fixtures::seed(&store, &case);
        let id = case["before"]["id"].as_str().expect("monitor id");
        store
            .transaction(|tx| {
                add_request(
                    tx,
                    json!({"id":"approval","agent":"agent","epoch":2,"method":"monitor/approve","status":"pending","createdAt":1000,"params":{"monitorId":id}}),
                );
                add_event(tx, "dispatching-without-attempt", "agent", "dispatching", 2);
                tx.execute_batch(&format!(
                    "CREATE TRIGGER injected BEFORE UPDATE ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END"
                ))?;
                Ok(())
            })
            .expect("install recovery failpoint");
        assert!(
            monitor_runtime::recover_on_startup(&store, 2000.0).is_err(),
            "{table}"
        );
        assert_eq!(
            MonitorsRepository::get(&store.read().expect("read rollback"), id)
                .expect("read monitor")
                .expect("monitor")
                .value()["status"],
            "running",
            "{table}"
        );
        store
            .transaction(|tx| {
                tx.execute_batch("DROP TRIGGER injected")?;
                Ok(())
            })
            .expect("remove recovery failpoint");
        monitor_runtime::recover_on_startup(&store, 2000.0).expect("restart recovery");
        monitor_runtime::recover_on_startup(&store, 3000.0).expect("idempotent restart");
        assert_eq!(complete_rows(&store, id)["monitor"]["status"], "lost");
    }
}

#[test]
fn reserved_batch_events_are_recovered_by_the_scheduler_scan() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    let store = fixtures::store(root.path(), "scheduler-owned");
    fixtures::seed(&store, &case);
    store
        .transaction(|tx| {
            let agent = AgentRecord::from_value(json!({
                "id":"starting-agent","rootId":"starting-agent","epoch":0,
                "status":"starting","autoWake":true,"inFlight":true,
                "startAttempt":{"id":"attempt","epoch":0,"submitted":false,"events":["reserved-batch"]}
            }))?;
            AgentRepository::put(tx, &agent)?;
            add_event(tx, "reserved-batch", "starting-agent", "reserved", 0);
            Ok(())
        })
        .expect("seed scheduler-owned batch");
    let report = monitor_runtime::recover_on_startup(&store, 2000.0).expect("startup recovery");
    assert_eq!(report.events, 0);
    assert_eq!(
        QueueEventRepository::get(
            &store.read().expect("read event"),
            &QueueEventId::from("reserved-batch"),
        )
        .expect("event")
        .expect("reserved event")
        .status,
        "pending"
    );
}

#[test]
fn startup_rule_linked_monitor_is_lost_without_a_monitor_exit_event() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let mut case = running(&cases);
    case["before"]["ruleId"] = json!("rule-1");
    let id = case["before"]["id"]
        .as_str()
        .expect("monitor id")
        .to_owned();
    let store = fixtures::store(root.path(), "rule-monitor-startup");
    fixtures::seed(&store, &case);

    monitor_runtime::recover_on_startup(&store, 2000.0).expect("recover rule monitor");

    let connection = store.read().expect("read recovered rule monitor");
    assert_eq!(
        MonitorsRepository::get(&connection, &id)
            .expect("read rule monitor")
            .expect("rule monitor exists")
            .value()["status"],
        "lost"
    );
    assert!(
        QueueEventRepository::get(&connection, &QueueEventId::from(format!("monitor:{id}")))
            .expect("read rule monitor exit")
            .is_none()
    );
}

#[test]
fn startup_remote_owned_monitor_is_recovered_without_aborting() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let mut case = running(&cases);
    case["actor"]["remoteWorker"] = json!(true);
    let id = case["before"]["id"]
        .as_str()
        .expect("monitor id")
        .to_owned();
    let store = fixtures::store(root.path(), "remote-monitor-startup");
    fixtures::seed(&store, &case);

    monitor_runtime::recover_on_startup(&store, 2000.0).expect("recover remote monitor");

    let connection = store.read().expect("read recovered remote monitor");
    assert_eq!(
        MonitorsRepository::get(&connection, &id)
            .expect("read remote monitor")
            .expect("remote monitor exists")
            .value()["status"],
        "lost"
    );
    assert!(
        QueueEventRepository::get(&connection, &QueueEventId::from(format!("monitor:{id}")))
            .expect("read remote monitor exit")
            .is_some_and(|event| event.status == "stored_only")
    );
}

#[test]
fn malformed_monitor_row_is_skipped_while_other_rows_are_recovered() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    let id = case["before"]["id"]
        .as_str()
        .expect("monitor id")
        .to_owned();
    let store = fixtures::store(root.path(), "malformed-monitor-startup");
    fixtures::seed(&store, &case);
    store
        .transaction(|tx| {
            // The production expression index rejects malformed JSON on insert;
            // remove it in this isolated fixture to witness row-level isolation.
            tx.execute_batch(
                "DROP INDEX runtime_monitor_agent_status;
                 DROP INDEX runtime_monitor_stall_due;
                 DROP INDEX runtime_monitor_status",
            )?;
            tx.execute(
                "INSERT INTO runtime_monitors(id,record) VALUES('unexpected-monitor','{')",
                [],
            )?;
            Ok(())
        })
        .expect("seed malformed monitor row");

    let report = monitor_runtime::recover_on_startup(&store, 2000.0)
        .expect("skip one invalid row and continue startup");

    assert_eq!(report.monitors, 1);
    let connection = store.read().expect("read recovered monitors");
    assert_eq!(
        MonitorsRepository::get(&connection, &id)
            .expect("read valid monitor")
            .expect("valid monitor exists")
            .value()["status"],
        "lost"
    );
    let unexpected: String = connection
        .query_row(
            "SELECT record FROM runtime_monitors WHERE id='unexpected-monitor'",
            [],
            |row| row.get(0),
        )
        .expect("malformed row remains unchanged");
    assert_eq!(unexpected, "{");
}

#[test]
fn startup_event_ownership_uses_exact_scheduler_batch_and_submitted_attempts_are_uncertain() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    for (name, events, submitted) in [
        ("partial", json!(["partial-event", "missing-event"]), false),
        ("submitted", json!(["submitted-event"]), true),
    ] {
        let store = fixtures::store(root.path(), name);
        fixtures::seed(&store, &case);
        store.transaction(|tx| {
            AgentRepository::put(tx, &AgentRecord::from_value(json!({
                "id":"claimed-agent","rootId":"claimed-agent","epoch":0,
                "status":"starting","autoWake":true,"inFlight":true,
                "startAttempt":{"id":"attempt","epoch":0,"submitted":submitted,"events":events}
            }))?)?;
            add_event(tx, if submitted {"submitted-event"} else {"partial-event"}, "claimed-agent", "reserved", 0);
            Ok(())
        }).expect("seed reservation");
        monitor_runtime::recover_on_startup(&store, 2000.0).expect("recover startup events");
        let connection = store.read().expect("read uncertain event");
        let event_id = if submitted {
            "submitted-event"
        } else {
            "partial-event"
        };
        assert_eq!(
            QueueEventRepository::get(&connection, &QueueEventId::from(event_id))
                .expect("read recovered event")
                .expect("recovered event exists")
                .status,
            "uncertain",
            "{name}"
        );
    }
}
