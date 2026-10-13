#[path = "monitors/fixtures.rs"]
mod fixtures;
use serde_json::{Value, json};
use studio_runtime_services::{
    monitor_output::MonitorOutput,
    monitor_policy::{self, Input, InputOutcome, MonitorSnapshot, Operation},
    monitor_receipts::{self, ResultReceipt},
    monitor_runtime,
};
use studio_runtime_store::{MonitorsRepository, RuntimeStore};

fn running(cases: &[Value]) -> Value {
    cases
        .iter()
        .find(|c| c["name"] == "finish-0-None-False")
        .expect("monitor fixture")
        .clone()
}
fn operation(case: &Value) -> Operation {
    serde_json::from_value(case["before"]["operation"].clone()).expect("monitor fixture")
}
fn record(store: &RuntimeStore, id: &str) -> Value {
    MonitorsRepository::get(&store.read().expect("monitor fixture"), id)
        .expect("monitor fixture")
        .expect("monitor fixture")
        .value()
        .clone()
}

#[test]
fn receipt_survives_store_failure_restart_and_replays_once() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    let id = case["before"]["id"].as_str().expect("monitor fixture");
    let store = fixtures::store(root.path(), "receipt");
    fixtures::seed(&store, &case);
    let receipt = ResultReceipt {
        version: 1,
        key: id.into(),
        code: Some(0),
        error: None,
        operation: Some(operation(&case)),
        finished: 2000.0,
    };
    store.transaction(|tx| { tx.execute_batch("CREATE TRIGGER fail_monitor BEFORE INSERT ON runtime_monitors BEGIN SELECT RAISE(ABORT,'injected'); END")?; Ok(()) }).expect("monitor fixture");
    assert!(monitor_receipts::finish(&store, &receipt).is_err());
    assert_eq!(record(&store, id)["status"], "running");
    assert_eq!(fixtures::events(&store), json!([]));
    assert_eq!(
        std::fs::read_dir(store.root().join("monitor-results"))
            .expect("monitor fixture")
            .count(),
        1
    );
    store
        .transaction(|tx| {
            tx.execute_batch("DROP TRIGGER fail_monitor")?;
            Ok(())
        })
        .expect("monitor fixture");
    let state = store.root().to_owned();
    drop(store);
    let store = RuntimeStore::open(Some(&state)).expect("monitor fixture");
    let restored = monitor_receipts::recover(&store, &[id.into()]);
    assert!(restored[0].1.is_ok());
    assert_eq!(record(&store, id)["status"], "completed");
    assert_eq!(fixtures::events(&store), case["events"]);
    monitor_receipts::finish(&store, &receipt).expect("monitor fixture");
    assert_eq!(fixtures::events(&store), case["events"]);
    assert_eq!(
        std::fs::read_dir(store.root().join("monitor-results"))
            .expect("monitor fixture")
            .count(),
        0
    );
}

#[test]
fn unknown_input_stays_blocked_and_foreign_receipt_cannot_release_it() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let mut case = running(&cases);
    case["before"]["interactive"] = json!(true);
    let store = fixtures::store(root.path(), "input");
    fixtures::seed(&store, &case);
    let id = case["before"]["id"].as_str().expect("monitor fixture");
    let operation = operation(&case);
    let input = Input {
        text: "payload".into(),
        rows: None,
        cols: None,
        close_stdin: true,
    };
    monitor_runtime::reserve_input(&store, id, &operation, "ticket-1", &input, 2000.0)
        .expect("monitor fixture");
    monitor_runtime::settle_input(
        &store,
        id,
        &operation,
        "ticket-1",
        InputOutcome::Unknown("transport disconnected".into()),
        2001.0,
    )
    .expect("monitor fixture");
    let unknown = record(&store, id);
    assert_eq!(unknown["inputAttempt"]["status"], "uncertain");
    assert!(
        monitor_runtime::reserve_input(&store, id, &operation, "ticket-2", &input, 2002.0).is_err()
    );
    assert!(
        monitor_runtime::settle_input(
            &store,
            id,
            &operation,
            "foreign-ticket",
            InputOutcome::Acknowledged,
            2003.0
        )
        .is_err()
    );
    assert_eq!(record(&store, id), unknown);
    monitor_runtime::settle_input(
        &store,
        id,
        &operation,
        "ticket-1",
        InputOutcome::Acknowledged,
        2004.0,
    )
    .expect("monitor fixture");
    assert_eq!(record(&store, id)["stdinClosed"], true);
    assert!(record(&store, id).get("inputAttempt").is_none());
}

#[test]
fn malformed_state_and_unavailable_native_effects_write_nothing() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    let store = fixtures::store(root.path(), "refusal");
    fixtures::seed(&store, &case);
    let id = case["before"]["id"].as_str().expect("monitor fixture");
    let before = record(&store, id);
    assert!(monitor_runtime::cancel(&store, id, None, 2000.0).is_err());
    assert_eq!(record(&store, id), before);
    assert_eq!(fixtures::events(&store), json!([]));
    for (field, value) in [
        ("bytes", json!(-1)),
        ("tail", json!(17)),
        ("activityAt", Value::Null),
        ("activityGeneration", json!(-1)),
        ("successExitCodes", json!([])),
        ("timeout_ms", json!(0)),
        ("operation", json!({"agent":"foreign"})),
    ] {
        let mut malformed = before.clone();
        malformed[field] = value;
        assert!(
            MonitorSnapshot::checked(malformed, case["actor"].clone()).is_err(),
            "{field}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn actual_child_start_output_receipt_and_exit_event_cross_the_consumer() {
    use studio_runtime_process::stream_child::{ChildEvent, ChildSpec, StreamChild};
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let mut case = running(&cases);
    let store = fixtures::store(root.path(), "child");
    let id = case["before"]["id"]
        .as_str()
        .expect("monitor fixture")
        .to_owned();
    let operation = operation(&case);
    case["before"]["status"] = json!("starting");
    case["before"]["log"] = json!(store.root().join("monitor-logs").join(format!("{id}.log")));
    case["before"]
        .as_object_mut()
        .expect("monitor fixture")
        .remove("operation");
    fixtures::seed(&store, &case);
    let params = json!({"processId":id,"cwd":case["before"]["cwd"],"timeoutMs":case["before"]["timeout_ms"],"sandboxPolicy":{},"command":["/bin/sh","-c","printf marker"]});
    let prepared = monitor_runtime::PreparedStart {
        monitor: case["before"].clone(),
        actor: case["actor"].clone(),
        operation: operation.clone(),
        params,
    };
    let effect = monitor_runtime::reserve_start(&store, &id, &prepared, 2000.0).expect("start");
    assert!(matches!(
        effect,
        monitor_policy::Effect::Native {
            method: "command/exec",
            ..
        }
    ));
    assert!(monitor_runtime::reserve_start(&store, &id, &prepared, 2000.0).is_err());
    let mut child = StreamChild::spawn(ChildSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "printf marker".into()],
        cwd: root.path().into(),
        environment: Default::default(),
        timeout: std::time::Duration::from_secs(60),
    })
    .await
    .expect("monitor fixture");
    let output = MonitorOutput::default();
    let mut cursor = 0;
    while let Some(event) = child.next_event().await {
        match event {
            ChildEvent::Output { bytes, .. } => {
                output
                    .append(&store, &id, &operation, cursor, &bytes, 2001.0)
                    .expect("monitor fixture");
                output
                    .append(&store, &id, &operation, cursor, &bytes, 2001.0)
                    .expect("monitor fixture");
                cursor += bytes.len() as u64;
            }
            ChildEvent::Exited { code, .. } => monitor_receipts::finish(
                &store,
                &ResultReceipt {
                    version: 1,
                    key: id.clone(),
                    code: code.map(i64::from),
                    error: None,
                    operation: Some(operation.clone()),
                    finished: 2002.0,
                },
            )
            .expect("monitor fixture"),
        }
    }
    child.close().await.expect("monitor fixture");
    assert_eq!(record(&store, &id)["tail"], "marker");
    assert_eq!(record(&store, &id)["bytes"], 6);
    assert_eq!(record(&store, &id)["status"], "completed");
    assert_eq!(
        fixtures::events(&store)
            .as_array()
            .expect("monitor fixture")
            .len(),
        1
    );
    assert_eq!(
        std::fs::read(store.root().join("monitor-logs").join(format!("{id}.log")))
            .expect("monitor fixture"),
        b"marker"
    );
}

#[test]
fn result_write_boundaries_roll_back_then_recover_the_first_receipt() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    let id = case["before"]["id"].as_str().expect("id");
    let full = std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some();
    let tables = if full {
        vec![
            "runtime_events",
            "runtime_event_meta",
            "runtime_agents",
            "runtime_monitors",
            "sync_entities",
        ]
    } else {
        vec!["runtime_events", "sync_entities"]
    };
    for table in tables {
        for boundary in ["BEFORE", "AFTER"] {
            let store = fixtures::store(root.path(), &format!("{table}-{boundary}"));
            fixtures::seed(&store, &case);
            let receipt = ResultReceipt {
                version: 1,
                key: id.into(),
                code: Some(0),
                error: None,
                operation: Some(operation(&case)),
                finished: 2000.0,
            };
            store.transaction(|tx| {
                // An upsert fires AFTER UPDATE when its row already exists.
                for operation in ["INSERT", "UPDATE"] {
                    tx.execute_batch(&format!("CREATE TRIGGER injected_{operation} {boundary} {operation} ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END"))?;
                }
                Ok(())
            }).expect("install boundary");
            assert!(
                monitor_receipts::finish(&store, &receipt).is_err(),
                "{table}-{boundary}"
            );
            assert_eq!(record(&store, id), case["before"]);
            assert_eq!(fixtures::events(&store), json!([]));
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER injected_INSERT; DROP TRIGGER injected_UPDATE")?;
                    Ok(())
                })
                .expect("release boundary");
            let state = store.root().to_owned();
            drop(store);
            let store = RuntimeStore::open(Some(&state)).expect("restart");
            assert!(monitor_receipts::recover(&store, &[id.into()])[0].1.is_ok());
            assert_eq!(record(&store, id), case["after"]);
            assert_eq!(fixtures::events(&store), case["events"]);
        }
    }
}

#[test]
fn native_input_is_called_after_commit_and_unknown_is_never_resent() {
    use std::cell::Cell;
    use studio_runtime_services::monitor_driver::{self, InputRequest, MonitorNative, Submission};
    struct Port<'a> {
        store: &'a RuntimeStore,
        id: &'a str,
        sends: Cell<usize>,
    }
    impl MonitorNative for Port<'_> {
        fn submit(&self, _: &Operation, _: &monitor_policy::Effect) -> Submission {
            // Taking the writer here would deadlock if submit ran inside admission.
            self.store
                .transaction(|tx| {
                    let saved = MonitorsRepository::get(tx, self.id)?.expect("admitted row");
                    assert_eq!(
                        saved
                            .get("inputAttempt")
                            .and_then(|v| v.get("id"))
                            .and_then(Value::as_str),
                        Some("ticket-1")
                    );
                    Ok(())
                })
                .expect("outside admission transaction");
            self.sends.set(self.sends.get() + 1);
            Submission::Unknown
        }
    }
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let mut case = running(&cases);
    case["before"]["interactive"] = json!(true);
    let store = fixtures::store(root.path(), "port");
    fixtures::seed(&store, &case);
    let id = case["before"]["id"].as_str().expect("id");
    let operation = operation(&case);
    let input = Input {
        text: "hello".into(),
        rows: None,
        cols: None,
        close_stdin: false,
    };
    let port = Port {
        store: &store,
        id,
        sends: Cell::new(0),
    };
    assert!(matches!(
        monitor_driver::input(
            &store,
            &port,
            InputRequest {
                id,
                operation: &operation,
                ticket: "ticket-1",
                input: &input
            },
            2000.0
        ),
        Ok(Submission::Unknown)
    ));
    assert!(
        monitor_driver::input(
            &store,
            &port,
            InputRequest {
                id,
                operation: &operation,
                ticket: "ticket-2",
                input: &input
            },
            2001.0
        )
        .is_err()
    );
    assert_eq!(port.sends.get(), 1);
}
