//! P15 uses Python-written records and the real Python oracle via the canonical fixture.
#[path = "wave7_p15_support/fixtures.rs"]
mod fixtures;
#[allow(clippy::duplicate_mod)]
#[path = "support/prepared_python.rs"]
mod prepared_python_support;

use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_services::{
    monitor_policy::Input,
    monitor_receipts::{self, ResultReceipt},
    monitor_runtime_wiring::{Delivery, MonitorEvent, MonitorRuntime},
};
use studio_runtime_store::{MonitorsRepository, RuntimeStore};
use tokio::sync::Notify;

#[path = "wave7_p15_support/mod.rs"]
mod support;
use support::Port;
fn record(store: &RuntimeStore, id: &str) -> Value {
    MonitorsRepository::get(&store.read().expect("read"), id)
        .expect("get")
        .expect("monitor")
        .value()
        .clone()
}
fn running(cases: &[Value]) -> Value {
    cases
        .iter()
        .find(|case| case["name"] == "finish-0-None-False")
        .expect("running")
        .clone()
}
fn runtime(store: &Arc<RuntimeStore>, port: &Arc<Port>) -> MonitorRuntime {
    MonitorRuntime::new(store.clone(), port.clone(), Arc::new(Notify::new()))
}
fn delivery(case: &Value, cursor: &str, event: MonitorEvent) -> Delivery {
    Delivery {
        id: case["before"]["id"].as_str().expect("id").into(),
        operation: serde_json::from_value(case["before"]["operation"].clone()).expect("operation"),
        cursor: cursor.into(),
        event,
    }
}

#[test]
fn runtime_ticks_match_complete_python_rows_and_events() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let store = Arc::new(fixtures::store(root.path(), "ticks"));
    let port = Arc::new(Port::default());
    let runtime = runtime(&store, &port);
    let mut count = 0;
    for case in &cases {
        if case["action"] != "tick" {
            continue;
        }
        if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_none()
            && ![
                "tick-10-0--1-",
                "tick-0-0--1-",
                "tick-owner-epoch",
                "tick-probe-expiry-1910.0",
            ]
            .contains(&case["name"].as_str().expect("name"))
        {
            continue;
        }
        fixtures::seed(&store, case);
        runtime.tick(2000.0).expect("tick");
        assert_eq!(
            record(&store, case["before"]["id"].as_str().expect("id")),
            case["after"],
            "{}",
            case["name"]
        );
        assert_eq!(fixtures::events(&store), case["events"], "{}", case["name"]);
        let probes = std::mem::take(&mut *port.probes.lock().expect("probes"));
        let expected: Vec<_> = case["probes"]
            .as_array()
            .expect("probes")
            .iter()
            .map(|probe| {
                (
                    probe[0].as_str().expect("id").to_owned(),
                    probe[1].as_i64().expect("generation"),
                    probe[3].as_str().expect("command").to_owned(),
                )
            })
            .collect();
        assert_eq!(probes, expected, "{}", case["name"]);
        count += 1;
    }
    assert!(count >= 4);
}

#[test]
fn durable_results_win_over_stalls_across_every_write_failure_and_restart() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let mut case = running(&cases);
    case["before"]["activityAt"] = json!(1);
    let full = std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some();
    let tables = if full {
        vec![
            "runtime_monitors",
            "runtime_events",
            "runtime_event_meta",
            "runtime_agents",
            "sync_entities",
        ]
    } else {
        vec!["runtime_monitors", "runtime_events"]
    };
    for table in tables {
        for boundary in ["BEFORE", "AFTER"] {
            let store = Arc::new(fixtures::store(root.path(), &format!("{table}-{boundary}")));
            fixtures::seed(&store, &case);
            let id = case["before"]["id"].as_str().expect("id");
            let receipt = ResultReceipt {
                version: 1,
                key: id.into(),
                code: Some(0),
                error: None,
                operation: Some(
                    serde_json::from_value(case["before"]["operation"].clone()).expect("operation"),
                ),
                finished: 2000.0,
            };
            monitor_receipts::persist(store.root(), &receipt).expect("native receipt before DB");
            store.transaction(|tx| { for operation in ["INSERT", "UPDATE"] { tx.execute_batch(&format!("CREATE TRIGGER injected_{operation} {boundary} {operation} ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END"))?; } Ok(()) }).expect("failure");
            let port = Arc::new(Port::default());
            assert!(
                runtime(&store, &port).tick(2000.0).is_err(),
                "{table}/{boundary}"
            );
            assert_eq!(record(&store, id), case["before"]);
            assert_eq!(fixtures::events(&store), json!([]));
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER injected_INSERT; DROP TRIGGER injected_UPDATE")?;
                    Ok(())
                })
                .expect("release");
            let path = store.root().to_path_buf();
            drop(store);
            let store = Arc::new(RuntimeStore::open(Some(&path)).expect("restart"));
            let runtime = runtime(&store, &port);
            runtime.tick(2000.0).expect("recover before stall");
            runtime.tick(2000.0).expect("repeat");
            assert_eq!(record(&store, id)["status"], "completed");
            assert_eq!(fixtures::events(&store), case["events"]);
            assert!(port.submitted.lock().expect("submissions").is_empty());
        }
    }
}

#[test]
fn output_cursor_and_exit_replay_after_lost_ack_are_exactly_once() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let mut case = running(&cases);
    let store = Arc::new(fixtures::store(root.path(), "output"));
    let id = case["before"]["id"].as_str().expect("id").to_owned();
    case["before"]["log"] = json!(store.root().join("monitor-logs").join(format!("{id}.log")));
    fixtures::seed(&store, &case);
    let port = Arc::new(Port::default());
    let output = delivery(
        &case,
        "out:0",
        MonitorEvent::Output {
            offset: 0,
            bytes: b"marker".to_vec(),
        },
    );
    port.deliveries
        .lock()
        .expect("queue")
        .push_back(output.clone());
    *port.fail_ack.lock().expect("ack") = true;
    let runtime = runtime(&store, &port);
    assert!(runtime.tick(1999.0).is_err());
    assert_eq!(record(&store, &id)["bytes"], 6);
    let mut changed = output;
    changed.event = MonitorEvent::Output {
        offset: 0,
        bytes: b"tamper".to_vec(),
    };
    assert!(runtime.deliver(&changed, 1999.0).is_err());
    runtime.tick(1999.0).expect("output replay");
    port.deliveries.lock().expect("queue").push_back(delivery(
        &case,
        "exit",
        MonitorEvent::Exit {
            code: Some(0),
            error: None,
            finished: 2000.0,
        },
    ));
    *port.fail_ack.lock().expect("ack") = true;
    assert!(runtime.tick(2000.0).is_err());
    assert_eq!(record(&store, &id)["status"], "completed");
    runtime.tick(2000.0).expect("exit replay");
    assert_eq!(record(&store, &id)["bytes"], 6);
    assert_eq!(record(&store, &id)["tail"], "marker");
    assert_eq!(record(&store, &id)["status"], "completed");
    assert_eq!(
        fixtures::events(&store).as_array().expect("events").len(),
        1
    );
    assert_eq!(
        std::fs::read(store.root().join("monitor-logs").join(format!("{id}.log"))).expect("log"),
        b"marker"
    );
}

#[test]
fn reattach_cancel_and_unknown_input_never_resend_a_command() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let mut case = running(&cases);
    case["before"]["status"] = json!("lost");
    case["before"]["interactive"] = json!(true);
    case["before"]["reattachRecovery"] = json!({"status":"running","epoch":2});
    let store = Arc::new(fixtures::store(root.path(), "attach"));
    fixtures::seed(&store, &case);
    let port = Arc::new(Port::default());
    let runtime = runtime(&store, &port);
    runtime.tick(2000.0).expect("attach");
    let id = case["before"]["id"].as_str().expect("id");
    assert_eq!(record(&store, id)["reattachedConnectionId"], "restarted");
    assert!(port.submitted.lock().expect("submitted").is_empty());
    let input = Input {
        text: "hello".into(),
        rows: None,
        cols: None,
        close_stdin: false,
    };
    runtime
        .input(id, "acknowledged", &input, 2001.0)
        .expect("input reply");
    assert!(runtime.input(id, "acknowledged", &input, 2001.0).is_err());
    *port.unknown.lock().expect("unknown") = true;
    assert!(runtime.input(id, "unknown", &input, 2002.0).is_err());
    assert!(runtime.input(id, "retry", &input, 2003.0).is_err());
    runtime.cancel(id, None, 2004.0).expect("cancel");
    runtime.cancel(id, None, 2005.0).expect("duplicate cancel");
    assert_eq!(port.submitted.lock().expect("submitted").len(), 3);
    assert_eq!(record(&store, id)["inputAttempt"]["status"], "uncertain");
}

#[tokio::test(start_paused = true)]
async fn stopped_runtime_exits_without_ticking_or_submitting() {
    let root = fixtures::tempdir();
    fixtures::corpus(root.path());
    let store = Arc::new(fixtures::store(root.path(), "shutdown"));
    let port = Arc::new(Port::default());
    let (_send, receive) = tokio::sync::watch::channel(true);
    studio_runtime_services::monitor_runtime_wiring::start(
        Arc::new(runtime(&store, &port)),
        receive,
    )
    .await
    .expect("join")
    .expect("shutdown");
    assert!(port.submitted.lock().expect("submitted").is_empty());
}

#[test]
fn monitor_completion_commits_the_python_rule_result_and_wake_atomically() {
    use studio_runtime_store::{RuleRecord, RulesRepository};
    let root = fixtures::tempdir();
    fixtures::corpus(root.path());
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = std::process::Command::new(prepared_python_support::prepared_python(
        &manifest.join("../../apps/server/src"),
    ))
    .arg(manifest.join("tests/wave7_p15_oracle.py"))
    .arg(root.path())
    .output()
    .expect("rule oracle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cases: Vec<Value> = serde_json::from_slice(&output.stdout).expect("rule corpus");
    assert_eq!(cases.len(), 9);
    for (number, case) in cases.iter().enumerate() {
        let store = Arc::new(fixtures::store(root.path(), &format!("rule-{number}")));
        fixtures::seed(&store, case);
        if !case["rule"].is_null() {
            store
                .transaction(|tx| {
                    RulesRepository::put(tx, &RuleRecord::from_value(case["rule"].clone())?)
                })
                .expect("rule");
        }
        let runtime = runtime(&store, &Arc::new(Port::default()));
        let result = delivery(
            case,
            "exit",
            MonitorEvent::Exit {
                code: case["code"].as_i64(),
                error: case["error"].as_str().map(str::to_owned),
                finished: case["now"].as_f64().expect("now"),
            },
        );
        store.transaction(|tx| { for operation in ["INSERT", "UPDATE"] { tx.execute_batch(&format!("CREATE TRIGGER injected_{operation} AFTER {operation} ON runtime_monitors BEGIN SELECT RAISE(ABORT,'injected'); END"))?; } Ok(()) }).expect("failure");
        assert!(
            runtime
                .deliver(&result, case["now"].as_f64().expect("now"))
                .is_err()
        );
        let original_rule = RulesRepository::get(&store.read().expect("read"), "rule")
            .expect("get")
            .map(|row| row.value().clone())
            .unwrap_or(Value::Null);
        assert_eq!(original_rule, case["rule"]);
        assert_eq!(fixtures::events(&store), json!([]));
        store
            .transaction(|tx| {
                tx.execute_batch("DROP TRIGGER injected_INSERT; DROP TRIGGER injected_UPDATE")?;
                Ok(())
            })
            .expect("release");
        for _ in 0..2 {
            runtime
                .deliver(&result, case["now"].as_f64().expect("now"))
                .expect("completion replay");
        }
        assert_eq!(
            record(&store, &result.id),
            case["after"],
            "{}",
            case["name"]
        );
        let saved_rule = RulesRepository::get(&store.read().expect("read"), "rule")
            .expect("get")
            .map(|row| row.value().clone());
        assert_eq!(
            saved_rule.unwrap_or(Value::Null),
            case["afterRule"],
            "{}",
            case["name"]
        );
        let events = fixtures::events(&store);
        let wakes: Vec<_> = events
            .as_array()
            .expect("events")
            .iter()
            .map(|e| json!({"id":e["id"],"kind":e["kind"],"text":e["text"]}))
            .collect();
        let expected: Vec<_> = case["wakes"].as_array().expect("wakes").iter().map(|e| json!({"id":e["id"],"kind":e["kind"],"text":serde_json::from_str::<Value>(e["text"].as_str().expect("text")).expect("generated wake JSON")})).collect();
        assert_eq!(wakes, expected);
    }
}

#[test]
fn foreign_exit_cannot_poison_the_first_result_journal() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    let store = Arc::new(fixtures::store(root.path(), "foreign"));
    fixtures::seed(&store, &case);
    let runtime = runtime(&store, &Arc::new(Port::default()));
    let exit = delivery(
        &case,
        "exit",
        MonitorEvent::Exit {
            code: Some(0),
            error: None,
            finished: 2000.0,
        },
    );
    let mut foreign = exit.clone();
    foreign.operation.connection_id = "foreign".into();
    assert!(runtime.deliver(&foreign, 2000.0).is_err());
    assert!(!store.root().join("monitor-results").exists());
    runtime.deliver(&exit, 2000.0).expect("exact result");
    assert_eq!(fixtures::events(&store), case["events"]);
}

#[test]
fn cancelled_intent_is_committed_before_transport_and_rolls_back_on_each_write_failure() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    for table in ["runtime_monitors", "sync_entities"] {
        for timing in ["BEFORE", "AFTER"] {
            let store = Arc::new(fixtures::store(
                root.path(),
                &format!("cancel-{table}-{timing}"),
            ));
            fixtures::seed(&store, &case);
            let port = Arc::new(Port::default());
            let runtime = runtime(&store, &port);
            let id = case["before"]["id"].as_str().expect("id");
            store.transaction(|tx| { for operation in ["INSERT", "UPDATE"] { tx.execute_batch(&format!("CREATE TRIGGER injected_{operation} {timing} {operation} ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END"))?; } Ok(()) }).expect("inject");
            assert!(runtime.cancel(id, None, 2000.0).is_err());
            assert!(port.submitted.lock().expect("submitted").is_empty());
            assert_eq!(record(&store, id), case["before"]);
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER injected_INSERT; DROP TRIGGER injected_UPDATE")?;
                    Ok(())
                })
                .expect("release");
            runtime.cancel(id, None, 2000.0).expect("cancel");
            runtime.cancel(id, None, 2000.0).expect("replay");
            assert_eq!(port.submitted.lock().expect("submitted").len(), 1);
            assert_eq!(record(&store, id)["cancelRequested"], true);
        }
    }
}

#[test]
fn cancel_intent_replays_after_restart_until_written_and_holds_unknown() {
    use studio_runtime_services::monitor_driver;

    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    let id = case["before"]["id"].as_str().expect("id");
    let store = fixtures::store(root.path(), "cancel-restart");
    fixtures::seed(&store, &case);

    // Crash witness: the reservation commits, then the process stops before the
    // native port is called. The next runtime must recover this exact intent.
    let (_, operation) = store
        .transaction_with(|tx| monitor_driver::reserve_cancel(tx, id, None, 2000.0))
        .expect("cancel reservation");
    assert!(operation.is_some());
    assert_eq!(record(&store, id)["cancelDispatch"]["status"], "pending");
    let state = store.root().to_owned();
    drop(store);

    let store = Arc::new(RuntimeStore::open(Some(&state)).expect("restart"));
    let port = Arc::new(Port::default());
    *port.not_sent.lock().expect("not sent") = true;
    let restarted_runtime = runtime(&store, &port);
    restarted_runtime.tick(2001.0).expect("proven not sent");
    assert_eq!(record(&store, id)["cancelDispatch"]["status"], "pending");
    assert_eq!(port.submitted.lock().expect("submitted").len(), 1);

    *port.not_sent.lock().expect("not sent") = false;
    restarted_runtime.tick(2002.0).expect("written receipt");
    assert_eq!(record(&store, id)["cancelDispatch"]["status"], "written");
    let submitted = port.submitted.lock().expect("submitted");
    assert_eq!(submitted.len(), 2);
    assert_eq!(submitted[0], submitted[1]);
    drop(submitted);
    restarted_runtime
        .cancel(id, None, 2003.0)
        .expect("cancel replay");
    assert_eq!(port.submitted.lock().expect("submitted").len(), 2);

    let unknown_case = running(&cases);
    let unknown_store = Arc::new(fixtures::store(root.path(), "cancel-unknown"));
    fixtures::seed(&unknown_store, &unknown_case);
    let unknown_id = unknown_case["before"]["id"].as_str().expect("id");
    let unknown_port = Arc::new(Port::default());
    *unknown_port.unknown.lock().expect("unknown") = true;
    let unknown_runtime = runtime(&unknown_store, &unknown_port);
    unknown_runtime
        .cancel(unknown_id, None, 2010.0)
        .expect("unknown cancel receipt");
    assert_eq!(
        record(&unknown_store, unknown_id)["cancelDispatch"]["status"],
        "unknown"
    );
    *unknown_port.unknown.lock().expect("unknown") = false;
    unknown_runtime.tick(2011.0).expect("hold unknown");
    assert_eq!(unknown_port.submitted.lock().expect("submitted").len(), 1);
}

#[tokio::test(start_paused = true)]
async fn timer_commits_a_stall_then_shutdown_joins_the_consumer() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let mut case = running(&cases);
    case["before"]["activityAt"] = json!(1);
    let store = Arc::new(fixtures::store(root.path(), "timer"));
    fixtures::seed(&store, &case);
    let port = Arc::new(Port::default());
    let wake = Arc::new(Notify::new());
    let runtime = Arc::new(MonitorRuntime::new(store.clone(), port, wake.clone()));
    let (stop, receiver) = tokio::sync::watch::channel(false);
    let task = studio_runtime_services::monitor_runtime_wiring::start(runtime, receiver);
    wake.notified().await;
    stop.send(true).expect("shutdown request");
    task.await.expect("task join").expect("clean stop");
    let events = fixtures::events(&store);
    let events = events.as_array().expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["kind"], "monitor_stall");
}
