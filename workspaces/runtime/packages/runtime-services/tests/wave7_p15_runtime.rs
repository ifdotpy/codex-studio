//! P15 checks frozen monitor effects and durable HTTP input replay.
use crate::monitor_runtime_fixtures as fixtures;

use base64::Engine;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use studio_runtime_services::{
    agent_lifecycle_runtime,
    agent_management::Action,
    monitor_driver, monitor_event_queue,
    monitor_lifecycle::MonitorLifecyclePorts,
    monitor_policy::{Effect, Input},
    monitor_receipts::{self, ResultReceipt},
    monitor_runtime,
    monitor_runtime_wiring::{Delivery, MonitorEvent, MonitorRuntime},
};
use studio_runtime_store::{
    AgentRecord, AgentRepository, MonitorRecord, MonitorsRepository,
    NotificationDispatchRepository, RuleRecord, RulesRepository, RuntimeStore, Table,
};
use tokio::sync::Notify;

use crate::monitor_runtime_support as support;
use support::Port;
fn record(store: &RuntimeStore, id: &str) -> Value {
    MonitorsRepository::get(&store.test_reader().expect("read"), id)
        .expect("get")
        .expect("monitor")
        .value()
        .clone()
}
fn rule_record(store: &RuntimeStore, id: &str) -> Value {
    RulesRepository::get(&store.test_reader().expect("read"), id)
        .expect("get rule")
        .expect("rule")
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

fn rule_case(name: &str) -> Value {
    serde_json::from_str::<Vec<Value>>(include_str!("rules_engine/expected.json"))
        .expect("frozen rule cases")
        .into_iter()
        .find(|case| case["name"] == name)
        .unwrap_or_else(|| panic!("rule case {name}"))
}

fn due_rule_case() -> Value {
    rule_case("interval-boundary--1")
}

#[tokio::test]
async fn account_worker_monitor_output_is_durable_ordered_and_exactly_acknowledged() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    let store = fixtures::store(root.path(), "monitor-output-queue").await;
    fixtures::seed(&store, &case).await;
    let id = case["before"]["id"].as_str().expect("monitor id");
    let account = case["before"]["operation"]["accountKey"]
        .as_str()
        .expect("account");
    let id_for_enqueue = id.to_owned();
    let account_for_enqueue = account.to_owned();
    let first = json!({
        "processId": id,
        "deltaBase64": base64::engine::general_purpose::STANDARD.encode(b"one")
    });
    let second = json!({
        "processId": id,
        "deltaBase64": base64::engine::general_purpose::STANDARD.encode(b"two")
    });
    store
        .transaction_with(move |tx| {
            monitor_event_queue::enqueue_output_in_transaction(
                tx,
                &id_for_enqueue,
                &account_for_enqueue,
                7,
                11,
                &first,
            )?;
            monitor_event_queue::enqueue_output_in_transaction(
                tx,
                &id_for_enqueue,
                &account_for_enqueue,
                7,
                12,
                &second,
            )?;
            Ok::<_, studio_runtime_services::notification_dispatch::NotificationDispatchError>(())
        })
        .await
        .expect("enqueue output in provider receipt transaction");

    let first_delivery = monitor_event_queue::next(&store)
        .await
        .expect("read output queue")
        .expect("first output");
    assert_eq!(first_delivery.cursor, format!("{account}:7:11"));
    assert!(matches!(
        &first_delivery.event,
        MonitorEvent::OutputStream { offset: 0, stream, bytes } if stream == "stdout" && bytes == b"one"
    ));
    let replay = monitor_event_queue::next(&store)
        .await
        .expect("replay output queue")
        .expect("same first output");
    assert_eq!(replay.cursor, first_delivery.cursor);
    monitor_event_queue::acknowledge(&store, &first_delivery)
        .await
        .expect("ack first output");

    let second_delivery = monitor_event_queue::next(&store)
        .await
        .expect("read second output")
        .expect("second output");
    assert_eq!(second_delivery.cursor, format!("{account}:7:12"));
    assert!(matches!(
        &second_delivery.event,
        MonitorEvent::OutputStream { offset: 3, stream, bytes } if stream == "stdout" && bytes == b"two"
    ));
    monitor_event_queue::acknowledge(&store, &second_delivery)
        .await
        .expect("ack second output");
    monitor_event_queue::acknowledge(&store, &second_delivery)
        .await
        .expect("idempotent exact ACK");
    let operation =
        serde_json::from_value(case["before"]["operation"].clone()).expect("monitor operation");
    monitor_event_queue::enqueue_exit(&store, id, &operation, "result:monitor", Some(0), None, 9.0)
        .await
        .expect("enqueue definitive result behind output");
    let exit = monitor_event_queue::next(&store)
        .await
        .expect("read result queue")
        .expect("exit after both output deltas");
    assert_eq!(exit.cursor, "result:monitor");
    assert!(matches!(
        exit.event,
        MonitorEvent::Exit { code: Some(0), .. }
    ));
    monitor_event_queue::acknowledge(&store, &exit)
        .await
        .expect("ack exit");
    monitor_event_queue::enqueue_exit(&store, id, &operation, "result:monitor", Some(0), None, 9.0)
        .await
        .expect("replay acknowledged result without duplicating it");
    let probe_result = json!({"exitCode":1,"output":"still responsive"});
    monitor_event_queue::enqueue_probe(&store, id, &operation, 8, &probe_result)
        .await
        .expect("enqueue completed liveness probe");
    let probe = monitor_event_queue::next(&store)
        .await
        .expect("read probe queue")
        .expect("probe result");
    assert_eq!(probe.cursor, format!("liveness:{id}:8"));
    assert!(matches!(
        &probe.event,
        MonitorEvent::Probe { generation: 8, result } if result == &probe_result
    ));
    monitor_event_queue::acknowledge(&store, &probe)
        .await
        .expect("ack probe");
    monitor_event_queue::enqueue_probe(&store, id, &operation, 8, &probe_result)
        .await
        .expect("replay acknowledged probe without duplicating it");
    assert!(
        monitor_event_queue::next(&store)
            .await
            .expect("empty output queue")
            .is_none()
    );
}

#[tokio::test]
async fn transient_rule_submit_failure_reconciles_the_saved_launch_identity() {
    let root = fixtures::tempdir();
    let monitor_cases = fixtures::corpus(root.path());
    let store = Arc::new(fixtures::store(root.path(), "rule-retry").await);
    fixtures::seed(&store, &running(&monitor_cases)).await;
    let rule_case = due_rule_case();
    let rule = rule_case["rule"].clone();
    let rule_id = rule["id"].as_str().expect("rule id").to_owned();
    let now = rule_case["now"].as_f64().expect("rule time");
    let owner = AgentRecord::from_value(rule_case["owner"].clone()).expect("rule owner");
    store
        .transaction(move |tx| {
            AgentRepository::put(tx, &owner)?;
            RulesRepository::put(tx, &RuleRecord::from_value(rule)?)
        })
        .await
        .expect("seed due rule and owner");

    let port = Arc::new(Port::default());
    let runtime = runtime(&store, &port);
    runtime
        .tick(now)
        .await
        .expect("startup rule reconciliation");
    *port.rule_available.lock().expect("rule availability") = true;
    port.availability_overrides
        .lock()
        .expect("rule availability")
        .extend([true, false]);

    assert!(runtime.tick(now + 1.0).await.is_err());
    assert_eq!(rule_record(&store, &rule_id)["inFlight"], true);
    let reserved = rule_record(&store, &rule_id)["launchIntent"].clone();
    assert!(runtime.last_tick_error().is_some());

    *port
        .reconcile_not_submitted
        .lock()
        .expect("reconcile proof") = true;
    runtime
        .tick(now + 2.0)
        .await
        .expect("retry reconciles saved launch intent");
    assert!(runtime.last_tick_error().is_none());
    let launches = port.rule_checks.lock().expect("rule checks");
    assert_eq!(launches.len(), 1);
    assert_eq!(
        serde_json::to_value(&launches[0]).expect("check JSON"),
        reserved
    );
}

#[tokio::test]
async fn event_submission_failure_recovers_the_committed_launch_intent() {
    use studio_runtime_services::rules_tick::{self, RuleEvent};

    let root = fixtures::tempdir();
    let monitor_cases = fixtures::corpus(root.path());
    let store = Arc::new(fixtures::store(root.path(), "event-rule-retry").await);
    fixtures::seed(&store, &running(&monitor_cases)).await;
    let event_case = rule_case("event-work_review");
    let event = event_case["event"].clone();
    let event_kind = event["kind"].as_str().expect("event kind").to_owned();
    let event_text = event["text"].as_str().expect("event text").to_owned();
    let event_key = event["key"].as_str().expect("event key").to_owned();
    let event_now = event_case["now"].as_f64().expect("event time");
    let owner = AgentRecord::from_value(event_case["owner"].clone()).expect("event owner");
    let mut event_rule = event_case["rule"].clone();
    event_rule["event"] = event["kind"].clone();
    event_rule["command"] = json!("printf marker");
    let rule = RuleRecord::from_value(event_rule).expect("event rule");
    let check = store
        .transaction_with::<_, rules_tick::RulesTickError>(move |tx| {
            AgentRepository::put(tx, &owner)?;
            RulesRepository::put(tx, &rule)?;
            let (_, checks) = rules_tick::event_in_transaction(
                tx,
                RuleEvent {
                    agent_id: "owner",
                    kind: &event_kind,
                    text: &event_text,
                    key: &event_key,
                    now: event_now,
                },
                0,
                true,
            )?;
            Ok(checks.into_iter().next().expect("event launch check"))
        })
        .await
        .expect("event transaction commits its launch intent");
    let reserved = rule_record(&store, &check.rule_id)["launchIntent"].clone();

    let port = Arc::new(Port::default());
    let runtime = runtime(&store, &port);
    assert!(
        runtime
            .submit_event_checks(
                std::slice::from_ref(&check),
                event_case["now"].as_f64().expect("event time")
            )
            .await
            .is_err()
    );
    assert_eq!(rule_record(&store, &check.rule_id)["inFlight"], true);
    assert!(port.rule_checks.lock().expect("rule checks").is_empty());

    *port.rule_available.lock().expect("rule availability") = true;
    *port
        .reconcile_not_submitted
        .lock()
        .expect("rule reconciliation") = true;
    runtime
        .tick(event_case["now"].as_f64().expect("event time") + 1.0)
        .await
        .expect("tick recovers committed event launch");
    assert_eq!(
        rule_record(&store, &check.rule_id)["launchIntent"],
        reserved,
        "recovery retains the exact persisted request identity"
    );
    assert_eq!(
        port.rule_checks.lock().expect("rule checks").as_slice(),
        &[check]
    );
}

#[tokio::test]
async fn durable_results_win_over_stalls_across_every_write_failure_and_restart() {
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
            let store =
                Arc::new(fixtures::store(root.path(), &format!("{table}-{boundary}")).await);
            fixtures::seed(&store, &case).await;
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
            store.transaction(move |tx| { for operation in ["INSERT", "UPDATE"] { tx.execute_batch(&format!("CREATE TRIGGER injected_{operation} {boundary} {operation} ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END"))?; } Ok(()) }).await.expect("failure");
            let port = Arc::new(Port::default());
            assert!(
                runtime(&store, &port).tick(2000.0).await.is_err(),
                "{table}/{boundary}"
            );
            assert_eq!(record(&store, id), case["before"]);
            assert_eq!(fixtures::events(&store), json!([]));
            store
                .transaction(move |tx| {
                    tx.execute_batch("DROP TRIGGER injected_INSERT; DROP TRIGGER injected_UPDATE")?;
                    Ok(())
                })
                .await
                .expect("release");
            let path = store.root().to_path_buf();
            drop(store);
            let store = Arc::new(RuntimeStore::open(Some(&path)).await.expect("restart"));
            let runtime = runtime(&store, &port);
            runtime.tick(2000.0).await.expect("recover before stall");
            runtime.tick(2000.0).await.expect("repeat");
            assert_eq!(record(&store, id)["status"], "completed");
            assert_eq!(fixtures::events(&store), case["events"]);
            assert!(port.submitted.lock().expect("submissions").is_empty());
        }
    }
}

#[tokio::test]
async fn lifecycle_cancel_intent_survives_crash_before_committed_and_dispatches_after_restart() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    let id = case["before"]["id"]
        .as_str()
        .expect("monitor id")
        .to_owned();
    let store = fixtures::store(root.path(), "lifecycle-cancel").await;
    fixtures::seed(&store, &case).await;
    let wake = Arc::new(Notify::new());
    let lifecycle = MonitorLifecyclePorts::new(wake);
    store
        .transaction_with_error(move |tx| {
            agent_lifecycle_runtime::execute_in_transaction(
                tx,
                &Action::Stop {
                    target: "agent".to_owned(),
                    descendants: false,
                    reason: None,
                    sender: None,
                    sender_epoch: None,
                },
                2000.0,
                &lifecycle,
            )
        })
        .await
        .expect("stop commits monitor cancellation without invoking committed hook");
    assert_eq!(record(&store, &id)["cancelRequested"], true);

    let path = store.root().to_owned();
    drop(store);
    let store = Arc::new(
        RuntimeStore::open(Some(&path))
            .await
            .expect("restart store"),
    );
    let pending = monitor_driver::pending_cancel_ids(&store, 2000.0)
        .await
        .expect("pending IDs");
    assert_eq!(pending.as_slice(), std::slice::from_ref(&id));

    let port = Arc::new(Port::default());
    runtime(&store, &port)
        .tick(2000.0)
        .await
        .expect("restart tick dispatches committed cancellation");
    assert_eq!(
        *port.submitted.lock().expect("submissions"),
        [Effect::Native {
            identity: format!("monitor-cancel:{id}"),
            method: "command/exec/terminate",
            params: json!({"processId": id}),
        }]
    );
}

#[tokio::test]
async fn output_cursor_and_exit_replay_after_lost_ack_are_exactly_once() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let mut case = running(&cases);
    let store = Arc::new(fixtures::store(root.path(), "output").await);
    let id = case["before"]["id"].as_str().expect("id").to_owned();
    case["before"]["log"] = json!(store.root().join("monitor-logs").join(format!("{id}.log")));
    fixtures::seed(&store, &case).await;
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
    assert!(runtime.tick(1999.0).await.is_err());
    assert_eq!(record(&store, &id)["bytes"], 6);
    let mut changed = output;
    changed.event = MonitorEvent::Output {
        offset: 0,
        bytes: b"tamper".to_vec(),
    };
    let error = runtime
        .deliver(&changed, 1999.0)
        .await
        .expect_err("changed bytes at a committed output offset");
    assert!(matches!(
        error,
        studio_runtime_services::monitor_policy::MonitorError::InvalidOutputReceipt(_)
    ));
    assert!(error.fatal_for_tick());
    runtime.tick(1999.0).await.expect("output replay");
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
    assert!(runtime.tick(2000.0).await.is_err());
    assert_eq!(record(&store, &id)["status"], "completed");
    runtime.tick(2000.0).await.expect("exit replay");
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

#[tokio::test]
async fn reattach_cancel_and_unknown_input_never_resend_a_command() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let mut case = running(&cases);
    case["before"]["status"] = json!("running");
    case["before"]["interactive"] = json!(true);
    case["before"]["reattachRecovery"] =
        json!({"status":"running","epoch":2,"accountKey":"default","startedAt":2000.0});
    let store = Arc::new(fixtures::store(root.path(), "attach").await);
    fixtures::seed(&store, &case).await;
    let port = Arc::new(Port::default());
    let runtime = runtime(&store, &port);
    runtime.tick(2000.0).await.expect("attach");
    let id = case["before"]["id"].as_str().expect("id").to_owned();
    assert_eq!(record(&store, &id)["reattachedConnectionId"], "restarted");
    assert!(port.submitted.lock().expect("submitted").is_empty());
    let input = Input {
        text: "hello".into(),
        rows: None,
        cols: None,
        close_stdin: false,
    };
    runtime
        .input(&id, "acknowledged", &input, 2001.0)
        .await
        .expect("input reply");
    assert!(
        runtime
            .input(&id, "acknowledged", &input, 2001.0)
            .await
            .is_err()
    );
    *port.unknown.lock().expect("unknown") = true;
    assert!(runtime.input(&id, "unknown", &input, 2002.0).await.is_err());
    assert!(runtime.input(&id, "retry", &input, 2003.0).await.is_err());
    runtime.cancel(&id, None, 2004.0).await.expect("cancel");
    runtime
        .cancel(&id, None, 2005.0)
        .await
        .expect("duplicate cancel");
    assert_eq!(port.submitted.lock().expect("submitted").len(), 3);
    assert_eq!(record(&store, &id)["inputAttempt"]["status"], "uncertain");
}

#[tokio::test(start_paused = true)]
async fn stopped_runtime_exits_without_ticking_or_submitting() {
    let root = fixtures::tempdir();
    fixtures::corpus(root.path());
    let store = Arc::new(fixtures::store(root.path(), "shutdown").await);
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

#[tokio::test]
#[allow(clippy::disallowed_methods)]
async fn monitor_completion_commits_the_python_rule_result_and_wake_atomically() {
    use studio_runtime_store::{RuleRecord, RulesRepository};
    let root = fixtures::tempdir();
    fixtures::corpus(root.path());
    let cases: Vec<Value> = serde_json::from_str(
        &include_str!("wave7_p15_support/completions.json")
            .replace("@FIXTURE_ROOT@", &root.path().to_string_lossy()),
    )
    .expect("frozen rule completions");
    assert_eq!(cases.len(), 9);
    for (number, case) in cases.iter().enumerate() {
        let store = Arc::new(fixtures::store(root.path(), &format!("rule-{number}")).await);
        fixtures::seed(&store, case).await;
        if !case["rule"].is_null() {
            let rule = case["rule"].clone();
            store
                .transaction(move |tx| RulesRepository::put(tx, &RuleRecord::from_value(rule)?))
                .await
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
        store.transaction(move |tx| { for operation in ["INSERT", "UPDATE"] { tx.execute_batch(&format!("CREATE TRIGGER injected_{operation} AFTER {operation} ON runtime_monitors BEGIN SELECT RAISE(ABORT,'injected'); END"))?; } Ok(()) }).await.expect("failure");
        assert!(
            runtime
                .deliver(&result, case["now"].as_f64().expect("now"))
                .await
                .is_err()
        );
        let original_rule = RulesRepository::get(&store.test_reader().expect("read"), "rule")
            .expect("get")
            .map(|row| row.value().clone())
            .unwrap_or(Value::Null);
        assert_eq!(original_rule, case["rule"]);
        assert_eq!(fixtures::events(&store), json!([]));
        store
            .transaction(move |tx| {
                tx.execute_batch("DROP TRIGGER injected_INSERT; DROP TRIGGER injected_UPDATE")?;
                Ok(())
            })
            .await
            .expect("release");
        for _ in 0..2 {
            runtime
                .deliver(&result, case["now"].as_f64().expect("now"))
                .await
                .expect("completion replay");
        }
        assert_eq!(
            record(&store, &result.id),
            case["after"],
            "{}",
            case["name"]
        );
        let saved_rule = RulesRepository::get(&store.test_reader().expect("read"), "rule")
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

#[tokio::test]
async fn foreign_exit_cannot_poison_the_first_result_journal() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    let store = Arc::new(fixtures::store(root.path(), "foreign").await);
    fixtures::seed(&store, &case).await;
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
    assert!(matches!(
        runtime.deliver(&foreign, 2000.0).await,
        Err(studio_runtime_services::monitor_policy::MonitorError::InvalidDeliveryIdentity(_))
    ));
    let mut foreign_input = delivery(
        &case,
        "input:foreign",
        MonitorEvent::Input {
            ticket: "foreign-ticket".into(),
            outcome: studio_runtime_services::monitor_policy::InputOutcome::Acknowledged,
        },
    );
    foreign_input.operation.connection_id = "foreign".into();
    assert!(matches!(
        runtime.deliver(&foreign_input, 2000.0).await,
        Err(studio_runtime_services::monitor_policy::MonitorError::InvalidDeliveryIdentity(_))
    ));
    assert!(!store.root().join("monitor-results").exists());
    runtime.deliver(&exit, 2000.0).await.expect("exact result");
    assert_eq!(fixtures::events(&store), case["events"]);
}

#[tokio::test]
async fn cancelled_intent_is_committed_before_transport_and_rolls_back_on_each_write_failure() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    for table in ["runtime_monitors", "sync_entities"] {
        for timing in ["BEFORE", "AFTER"] {
            let store =
                Arc::new(fixtures::store(root.path(), &format!("cancel-{table}-{timing}")).await);
            fixtures::seed(&store, &case).await;
            let port = Arc::new(Port::default());
            let runtime = runtime(&store, &port);
            let id = case["before"]["id"].as_str().expect("id");
            store.transaction(move |tx| { for operation in ["INSERT", "UPDATE"] { tx.execute_batch(&format!("CREATE TRIGGER injected_{operation} {timing} {operation} ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END"))?; } Ok(()) }).await.expect("inject");
            assert!(runtime.cancel(id, None, 2000.0).await.is_err());
            assert!(port.submitted.lock().expect("submitted").is_empty());
            assert_eq!(record(&store, id), case["before"]);
            store
                .transaction(move |tx| {
                    tx.execute_batch("DROP TRIGGER injected_INSERT; DROP TRIGGER injected_UPDATE")?;
                    Ok(())
                })
                .await
                .expect("release");
            runtime.cancel(id, None, 2000.0).await.expect("cancel");
            runtime.cancel(id, None, 2000.0).await.expect("replay");
            assert_eq!(port.submitted.lock().expect("submitted").len(), 1);
            assert_eq!(record(&store, id)["cancelRequested"], true);
        }
    }
}

#[tokio::test]
async fn cancel_intent_replays_after_restart_until_written_and_holds_unknown() {
    use studio_runtime_services::monitor_driver;

    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    let id = case["before"]["id"].as_str().expect("id").to_owned();
    let store = fixtures::store(root.path(), "cancel-restart").await;
    fixtures::seed(&store, &case).await;

    // Crash witness: the reservation commits, then the process stops before the
    // native port is called. The next runtime must recover this exact intent.
    let id_for_reservation = id.clone();
    let (_, operation) = store
        .transaction_with(move |tx| {
            monitor_driver::reserve_cancel(tx, &id_for_reservation, None, 2000.0)
        })
        .await
        .expect("cancel reservation");
    assert!(operation.is_some());
    assert_eq!(record(&store, &id)["cancelDispatch"]["status"], "pending");
    let state = store.root().to_owned();
    drop(store);

    let store = Arc::new(RuntimeStore::open(Some(&state)).await.expect("restart"));
    let port = Arc::new(Port::default());
    *port.not_sent.lock().expect("not sent") = true;
    let restarted_runtime = runtime(&store, &port);
    restarted_runtime
        .tick(2001.0)
        .await
        .expect("proven not sent");
    assert_eq!(record(&store, &id)["cancelDispatch"]["status"], "pending");
    assert_eq!(
        record(&store, &id)["cancelDispatch"]["nextAttemptAt"],
        2002.0
    );
    assert_eq!(port.submitted.lock().expect("submitted").len(), 1);
    restarted_runtime
        .tick(2001.5)
        .await
        .expect("backoff is active");
    assert_eq!(port.submitted.lock().expect("submitted").len(), 1);

    restarted_runtime
        .tick(2002.0)
        .await
        .expect("second not-sent receipt");
    assert_eq!(record(&store, &id)["cancelDispatch"]["attempts"], 2);
    assert_eq!(
        record(&store, &id)["cancelDispatch"]["nextAttemptAt"],
        2004.0
    );
    restarted_runtime
        .tick(2003.5)
        .await
        .expect("second backoff is active");
    assert_eq!(port.submitted.lock().expect("submitted").len(), 2);

    *port.not_sent.lock().expect("not sent") = false;
    restarted_runtime
        .tick(2004.0)
        .await
        .expect("written receipt");
    assert_eq!(record(&store, &id)["cancelDispatch"]["status"], "written");
    {
        let submitted = port.submitted.lock().expect("submitted");
        assert_eq!(submitted.len(), 3);
        assert_eq!(submitted[0], submitted[1]);
        assert_eq!(submitted[1], submitted[2]);
    }
    restarted_runtime
        .cancel(&id, None, 2005.0)
        .await
        .expect("cancel replay");
    assert_eq!(port.submitted.lock().expect("submitted").len(), 3);

    let unknown_case = running(&cases);
    let unknown_store = Arc::new(fixtures::store(root.path(), "cancel-unknown").await);
    fixtures::seed(&unknown_store, &unknown_case).await;
    let unknown_id = unknown_case["before"]["id"].as_str().expect("id");
    let unknown_port = Arc::new(Port::default());
    *unknown_port.unknown.lock().expect("unknown") = true;
    let unknown_runtime = runtime(&unknown_store, &unknown_port);
    unknown_runtime
        .cancel(unknown_id, None, 2010.0)
        .await
        .expect("unknown cancel receipt");
    assert_eq!(
        record(&unknown_store, unknown_id)["cancelDispatch"]["status"],
        "unknown"
    );
    *unknown_port.unknown.lock().expect("unknown") = false;
    unknown_runtime.tick(2011.0).await.expect("hold unknown");
    assert_eq!(unknown_port.submitted.lock().expect("submitted").len(), 1);
}

#[tokio::test]
async fn permanent_cancel_refusal_settles_and_is_not_retried() {
    use studio_runtime_services::monitor_driver::NotSentReason;

    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    for (name, reason, expected_status, expected_dispatch) in [
        (
            "cancel-monitor-missing",
            NotSentReason::MonitorMissing,
            "cancelled",
            "done",
        ),
        (
            "cancel-session-missing",
            NotSentReason::SessionMissing,
            "running",
            "unknown",
        ),
    ] {
        let store = Arc::new(fixtures::store(root.path(), name).await);
        fixtures::seed(&store, &case).await;
        let port = Arc::new(Port::default());
        *port.not_sent.lock().expect("not sent") = true;
        *port.not_sent_reason.lock().expect("not sent reason") = reason;
        let runtime = runtime(&store, &port);
        let id = case["before"]["id"].as_str().expect("id");

        runtime
            .cancel(id, None, 2000.0)
            .await
            .expect("permanent refusal recorded");

        assert_eq!(record(&store, id)["status"], expected_status);
        assert_eq!(
            record(&store, id)["cancelDispatch"]["status"],
            expected_dispatch
        );
        assert_eq!(
            record(&store, id)["cancelDispatch"]["error"],
            "injected not sent"
        );
        if reason == NotSentReason::SessionMissing {
            let record = record(&store, id);
            assert_eq!(record["status"], "running");
            assert_eq!(record["error"], "injected not sent");
            let view =
                studio_runtime_services::sync_views::project_persisted_record("monitor", &record)
                    .expect("monitor DTO projection");
            assert_eq!(view["error"], "injected not sent");
        }
        runtime
            .tick(2100.0)
            .await
            .expect("permanent refusal remains settled");
        assert_eq!(port.submitted.lock().expect("submitted").len(), 1);
    }
}

#[tokio::test]
async fn cancellation_racing_exit_settles_without_native_terminate() {
    use studio_runtime_services::monitor_driver;

    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    let id = case["before"]["id"].as_str().expect("id").to_owned();
    let store = Arc::new(fixtures::store(root.path(), "cancel-exit-race").await);
    fixtures::seed(&store, &case).await;
    let id_for_reservation = id.clone();
    let (_, operation) = store
        .transaction_with(move |tx| {
            monitor_driver::reserve_cancel(tx, &id_for_reservation, None, 2000.0)
        })
        .await
        .expect("durable cancel intent");
    assert!(operation.is_some());

    let port = Arc::new(Port::default());
    port.deliveries
        .lock()
        .expect("deliveries")
        .push_back(delivery(
            &case,
            "exit-races-cancel",
            MonitorEvent::Exit {
                code: Some(0),
                error: None,
                finished: 2001.0,
            },
        ));
    runtime(&store, &port)
        .tick(2001.0)
        .await
        .expect("exit wins the race");

    assert_eq!(record(&store, &id)["status"], "cancelled");
    assert_eq!(record(&store, &id)["cancelDispatch"]["status"], "done");
    assert!(port.submitted.lock().expect("submitted").is_empty());
    assert!(port.deliveries.lock().expect("deliveries").is_empty());
}

#[tokio::test(start_paused = true)]
async fn quarantined_delivery_does_not_stop_other_monitors_or_timer() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let bad_case = running(&cases);
    let mut healthy_case = bad_case.clone();
    healthy_case["before"]["id"] = json!("healthy-monitor");
    healthy_case["before"]["log"] = json!("unused-until-output");
    let store = Arc::new(fixtures::store(root.path(), "bad-delivery").await);
    fixtures::seed(&store, &bad_case).await;
    let healthy_case_for_seed = healthy_case.clone();
    store
        .transaction(move |tx| {
            Table::new(tx).put(&MonitorRecord::from_value(
                healthy_case_for_seed["before"].clone(),
            )?)
        })
        .await
        .expect("seed second monitor");

    let port = Arc::new(Port::default());
    let mut foreign = delivery(
        &bad_case,
        "foreign-exit",
        MonitorEvent::Exit {
            code: Some(0),
            error: None,
            finished: 2001.0,
        },
    );
    foreign.operation.connection_id = "foreign-connection".into();
    port.deliveries
        .lock()
        .expect("deliveries")
        .push_back(foreign);
    let mut healthy_output = delivery(
        &healthy_case,
        "healthy-output",
        MonitorEvent::Output {
            offset: 0,
            bytes: b"healthy".to_vec(),
        },
    );
    healthy_output.operation.connection_id = healthy_case["before"]["operation"]["connectionId"]
        .as_str()
        .expect("connection id")
        .into();
    healthy_case["before"]["log"] = json!(
        store
            .root()
            .join("monitor-logs")
            .join("healthy-monitor.log")
    );
    let healthy_case_for_log = healthy_case.clone();
    store
        .transaction(move |tx| {
            Table::new(tx).put(&MonitorRecord::from_value(
                healthy_case_for_log["before"].clone(),
            )?)
        })
        .await
        .expect("set healthy log path");
    port.deliveries
        .lock()
        .expect("deliveries")
        .push_back(healthy_output);

    let runtime = Arc::new(runtime(&store, &port));
    let (stop, receiver) = tokio::sync::watch::channel(false);
    let acknowledged = Arc::new(tokio::sync::Semaphore::new(0));
    *port.acknowledged.lock().expect("acknowledged") = Some(acknowledged.clone());
    let task = studio_runtime_services::monitor_runtime_wiring::start(runtime.clone(), receiver);
    let mut task = task;
    tokio::select! {
        result = &mut task => panic!("monitor timer ended before delivery ACKs: {result:?}"),
        permits = acknowledged.acquire_many(2) => {
            permits.expect("both delivery ACKs").forget();
        }
    }

    let bad_id = bad_case["before"]["id"].as_str().expect("bad id");
    assert_eq!(record(&store, bad_id)["status"], "failed");
    assert_eq!(
        record(&store, bad_id)["deliveryFailure"]["cursor"],
        "foreign-exit"
    );
    assert!(
        record(&store, bad_id)["deliveryFailure"]["error"]
            .as_str()
            .expect("located error")
            .contains("result operation changed")
    );
    assert_eq!(record(&store, "healthy-monitor")["bytes"], 7);
    assert!(
        !task.is_finished(),
        "bad delivery does not stop the timer task"
    );

    stop.send(true).expect("shutdown signal");
    task.await
        .expect("task joins")
        .expect("store remains healthy");
}

#[tokio::test]
async fn http_input_retry_after_ack_replays_one_logical_input() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    let store = Arc::new(fixtures::store(root.path(), "http-input-replay").await);
    fixtures::seed(&store, &case).await;
    let id = case["before"]["id"].as_str().expect("monitor id");
    let mut monitor = record(&store, id);
    monitor["interactive"] = json!(true);
    store
        .transaction(move |tx| Table::new(tx).put(&MonitorRecord::from_value(monitor.clone())?))
        .await
        .expect("mark seeded watch interactive");

    let port = Arc::new(Port::default());
    let runtime = runtime(&store, &port);
    let input = Input {
        text: "same input".into(),
        rows: None,
        cols: None,
        close_stdin: false,
    };
    // A lost HTTP success must not authorize another stdin write.
    runtime
        .input_http(id, "lost-http-success", &input, 2000.0)
        .await
        .expect("first input acknowledged");
    let first = record(&store, id)["lastInputAttempt"]["id"]
        .as_str()
        .expect("first ticket")
        .to_owned();
    runtime
        .input_http(id, "lost-http-success", &input, 2001.0)
        .await
        .expect("same logical input replays its acknowledgement");
    let second = record(&store, id)["lastInputAttempt"]["id"]
        .as_str()
        .expect("second ticket")
        .to_owned();

    assert_eq!(first, second);
    let submissions = port.submitted.lock().expect("submissions");
    assert_eq!(submissions.len(), 1);
    assert!(submissions.iter().all(|effect| matches!(
        effect,
        Effect::Native { identity, method: "command/exec/write", .. }
            if identity.starts_with(&format!("monitor-input:{id}:"))
    )));
}

#[tokio::test]
async fn http_input_receipts_survive_restart_and_refuse_uncertain_or_changed_content() {
    for boundary in [
        "acknowledged",
        "unknown",
        "written-unknown",
        "rejected",
        "not-sent",
        "settlement-rollback",
    ] {
        let root = fixtures::tempdir();
        let mut case = running(&fixtures::corpus(root.path()));
        case["before"]["interactive"] = json!(true);
        let store = Arc::new(fixtures::store(root.path(), boundary).await);
        fixtures::seed(&store, &case).await;
        let id = case["before"]["id"].as_str().expect("monitor id");
        let port = Arc::new(Port::default());
        *port.unknown.lock().expect("unknown") = boundary == "unknown";
        *port.not_sent.lock().expect("not sent") = boundary == "not-sent";
        if boundary == "written-unknown" {
            *port.input_reply.lock().expect("input reply") = Some(
                studio_runtime_services::monitor_runtime_wiring::InputReply::Unknown(
                    "lost native reply".into(),
                ),
            );
        } else if boundary == "rejected" {
            *port.input_reply.lock().expect("input reply") = Some(
                studio_runtime_services::monitor_runtime_wiring::InputReply::Rejected(
                    "native refusal".into(),
                ),
            );
        }
        if boundary == "settlement-rollback" {
            store.transaction(|tx| {
                tx.execute_batch("CREATE TRIGGER refuse_input_receipt BEFORE UPDATE ON runtime_operation_receipts WHEN NEW.id LIKE 'monitor-http-input:%' BEGIN SELECT RAISE(ABORT,'fixture receipt failure'); END")?;
                Ok(())
            }).await.expect("inject exact settlement failure");
        }
        let input = Input {
            text: "private input\n".into(),
            rows: None,
            cols: None,
            close_stdin: false,
        };
        let result = runtime(&store, &port)
            .input_http(id, "same-request", &input, 2000.0)
            .await;
        assert_eq!(
            result.is_ok(),
            boundary == "acknowledged",
            "{boundary}: {result:?}"
        );
        assert_eq!(port.submitted.lock().expect("native writes").len(), 1);
        let saved = record(&store, id);
        if boundary == "settlement-rollback" {
            assert_eq!(
                saved["inputAttempt"]["status"], "submitted",
                "monitor settlement shares receipt transaction"
            );
            assert!(saved.get("lastInputAttempt").is_none());
        }
        let path = store.root().to_owned();
        drop(store);
        let store = Arc::new(
            RuntimeStore::open(Some(&path))
                .await
                .expect("restart store"),
        );
        let fresh_port = Arc::new(Port::default());
        let restarted = runtime(&store, &fresh_port);
        let replay = restarted
            .input_http(id, "same-request", &input, 2001.0)
            .await;
        assert_eq!(
            replay.is_ok(),
            boundary == "acknowledged",
            "{boundary}: {replay:?}"
        );
        if !matches!(boundary, "acknowledged" | "rejected" | "not-sent") {
            assert!(matches!(
                replay,
                Err(studio_runtime_services::monitor_policy::MonitorError::InputOutcomeUnknown)
            ));
        }
        let changed = Input {
            text: "different input".into(),
            ..input.clone()
        };
        assert!(
            restarted
                .input_http(id, "same-request", &changed, 2002.0)
                .await
                .is_err()
        );
        assert!(
            fresh_port
                .submitted
                .lock()
                .expect("no replay writes")
                .is_empty()
        );
        if matches!(boundary, "acknowledged" | "rejected" | "not-sent") {
            restarted
                .input_http(id, "new-intentional-request", &input, 2003.0)
                .await
                .expect("fresh identity permits deliberate identical input");
            assert_eq!(fresh_port.submitted.lock().expect("fresh write").len(), 1);
        } else {
            assert!(
                restarted
                    .input_http(id, "different-request", &input, 2003.0)
                    .await
                    .is_err()
            );
            assert!(
                fresh_port
                    .submitted
                    .lock()
                    .expect("unknown blocks new write")
                    .is_empty()
            );
        }
        if matches!(boundary, "unknown" | "written-unknown") {
            let operation = serde_json::from_value(case["before"]["operation"].clone())
                .expect("original operation");
            monitor_runtime::settle_input(
                &store,
                id,
                &operation,
                "http:same-request",
                studio_runtime_services::monitor_policy::InputOutcome::Acknowledged,
                2004.0,
            )
            .await
            .expect("late exact native callback resolves the uncertainty");
            restarted
                .input_http(id, "same-request", &input, 2005.0)
                .await
                .expect("replay the late durable acknowledgement without native I/O");
            assert!(
                fresh_port
                    .submitted
                    .lock()
                    .expect("late acknowledgement does not send")
                    .is_empty()
            );
        }
    }
}

#[tokio::test]
async fn committed_managed_start_is_dispatched_after_admission() {
    let root = fixtures::tempdir();
    let case = running(&fixtures::corpus(root.path()));
    let store = Arc::new(fixtures::store(root.path(), "managed-monitor-start").await);
    monitor_runtime::recover_on_startup(&store, 1999.0)
        .await
        .expect("server startup recovery");
    fixtures::seed(&store, &case).await;
    let id = case["before"]["id"].as_str().expect("monitor id");
    let mut monitor = record(&store, id);
    monitor["status"] = json!("starting");
    monitor["operation"] = Value::Null;
    store
        .transaction(move |tx| Table::new(tx).put(&MonitorRecord::from_value(monitor.clone())?))
        .await
        .expect("persist admitted monitor start");

    let port = Arc::new(Port::default());
    runtime(&store, &port)
        .tick(2000.0)
        .await
        .expect("dispatch managed start");
    assert_eq!(
        port.starts.lock().expect("monitor start calls").as_slice(),
        &[(id.to_owned(), 2000.0)]
    );
}

#[tokio::test]
async fn output_queue_enforces_capacity_and_preserves_terminal_reserve_atomically() {
    let root = fixtures::tempdir();
    let case = running(&fixtures::corpus(root.path()));
    let store = fixtures::store(root.path(), "output-count-boundary").await;
    fixtures::seed(&store, &case).await;
    let id = case["before"]["id"].as_str().expect("monitor id");
    let account = case["before"]["operation"]["accountKey"]
        .as_str()
        .expect("account");
    let chunk = json!({
        "processId":id,
        "deltaBase64":base64::engine::general_purpose::STANDARD.encode(b"x")
    });
    for cursor in 0..126 {
        let enqueue_id = id.to_owned();
        let enqueue_account = account.to_owned();
        let enqueue_chunk = chunk.clone();
        store
            .transaction_with(move |tx| {
                monitor_event_queue::enqueue_output_in_transaction(
                    tx,
                    &enqueue_id,
                    &enqueue_account,
                    7,
                    cursor,
                    &enqueue_chunk,
                )
            })
            .await
            .expect("fill output admission slots");
    }
    let before = record(&store, id);
    let rejected_id = id.to_owned();
    let rejected_account = account.to_owned();
    let rejected_chunk = chunk.clone();
    let rejected = store
        .transaction_with::<_, studio_runtime_services::notification_dispatch::NotificationDispatchError>(move |tx| {
            NotificationDispatchRepository::put_notification_receipt(
                tx,
                "provider-notification-capacity",
                "same-notification",
                "settled",
            )?;
            monitor_event_queue::enqueue_output_in_transaction(
                tx, &rejected_id, &rejected_account, 7, 126, &rejected_chunk,
            )?;
            Ok(())
        })
        .await;
    assert!(matches!(
        rejected,
        Err(studio_runtime_services::notification_dispatch::NotificationDispatchError::StreamCapacity)
    ));
    assert_eq!(record(&store, id), before);
    let receipt = store
        .transaction_with(move |tx| {
            NotificationDispatchRepository::notification_receipt(
                tx,
                "provider-notification-capacity",
            )
        })
        .await
        .expect("read notification receipt");
    assert!(
        receipt.is_none(),
        "failed queue append rolls back its receipt"
    );

    let operation = serde_json::from_value(case["before"]["operation"].clone()).expect("operation");
    monitor_event_queue::enqueue_probe(&store, id, &operation, 8, &json!({"ok":true}))
        .await
        .expect("probe fits reserved event slot");
    monitor_event_queue::enqueue_exit(&store, id, &operation, "exit:count", Some(0), None, 9.0)
        .await
        .expect("exit fits reserved event slot");
    assert_eq!(
        record(&store, id)["streamQueue"]
            .as_array()
            .expect("stream queue")
            .len(),
        128
    );
}

#[tokio::test]
async fn output_queue_enforces_byte_limit_and_keeps_room_for_terminal_receipts() {
    let root = fixtures::tempdir();
    let case = running(&fixtures::corpus(root.path()));
    let store = fixtures::store(root.path(), "output-byte-boundary").await;
    fixtures::seed(&store, &case).await;
    let id = case["before"]["id"].as_str().expect("monitor id");
    let account = case["before"]["operation"]["accountKey"]
        .as_str()
        .expect("account");
    // P15 admits 960 KiB of output and reserves 64 KiB for terminal events.
    let bytes = vec![b'x'; 32 * 1024];
    let chunk = json!({
        "processId":id,
        "deltaBase64":base64::engine::general_purpose::STANDARD.encode(&bytes)
    });
    for cursor in 0..30 {
        let enqueue_id = id.to_owned();
        let enqueue_account = account.to_owned();
        let enqueue_chunk = chunk.clone();
        store
            .transaction_with(move |tx| {
                monitor_event_queue::enqueue_output_in_transaction(
                    tx,
                    &enqueue_id,
                    &enqueue_account,
                    7,
                    cursor,
                    &enqueue_chunk,
                )
            })
            .await
            .expect("fill output byte admission limit");
    }
    assert_eq!(
        record(&store, id)["streamQueue"][29]["byteLength"],
        32 * 1024
    );
    let rejected_id = id.to_owned();
    let rejected_account = account.to_owned();
    let rejected_chunk = chunk.clone();
    let error = store
        .transaction_with(move |tx| {
            monitor_event_queue::enqueue_output_in_transaction(
                tx,
                &rejected_id,
                &rejected_account,
                7,
                30,
                &rejected_chunk,
            )
        })
        .await
        .expect_err("output beyond 960 KiB is refused");
    assert!(matches!(
        error,
        studio_runtime_services::notification_dispatch::NotificationDispatchError::StreamCapacity
    ));
    let operation = serde_json::from_value(case["before"]["operation"].clone()).expect("operation");
    assert!(
        monitor_event_queue::enqueue_probe(&store, id, &operation, 8, &json!({"ok":true}))
            .await
            .expect_err("probe cannot consume terminal reserve")
            .to_string()
            .contains("queue is full")
    );
    monitor_event_queue::enqueue_exit(&store, id, &operation, "exit:bytes", Some(0), None, 9.0)
        .await
        .expect("exit fits reserved byte capacity");
}

#[tokio::test]
async fn bounded_probes_preserve_room_for_the_largest_exit_receipt() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    let store = fixtures::store(root.path(), "probe-terminal-reserve").await;
    fixtures::seed(&store, &case).await;
    let id = case["before"]["id"].as_str().expect("id");
    let operation =
        serde_json::from_value(case["before"]["operation"].clone()).expect("monitor operation");
    let id_for_queue_reset = id.to_owned();
    store
        .transaction_with(move |tx| {
            let mut value = MonitorsRepository::get(tx, &id_for_queue_reset)?
                .expect("monitor")
                .value()
                .clone();
            value["streamQueue"] = json!([]);
            Table::<MonitorRecord>::new(tx).put(
                &MonitorRecord::from_value(value)
                    .map_err(studio_runtime_store::StoreError::from)?,
            )
        })
        .await
        .expect("initialize bounded stream queue");
    let probe_limit = monitor_event_queue::STREAM_QUEUE_MAX_BYTES
        - monitor_event_queue::TERMINAL_RECEIPT_RESERVE_BYTES;
    let full_probes = probe_limit / monitor_event_queue::PROBE_RESULT_MAX_BYTES;
    let remainder = probe_limit % monitor_event_queue::PROBE_RESULT_MAX_BYTES;
    let probe_count = full_probes + usize::from(remainder > 0);
    for generation in 0..probe_count {
        let result_bytes = if generation < full_probes {
            monitor_event_queue::PROBE_RESULT_MAX_BYTES
        } else {
            remainder
        };
        let result = Value::String("x".repeat(result_bytes - 2));
        monitor_event_queue::enqueue_probe(&store, id, &operation, generation as i64, &result)
            .await
            .expect("probe fills bounded admission budget");
    }
    let probe_queue = record(&store, id);
    let probe_queue = probe_queue["streamQueue"].as_array().expect("probe queue");
    assert_eq!(probe_queue.len(), probe_count);
    assert_eq!(
        probe_queue
            .iter()
            .map(|item| item["byteLength"].as_u64().expect("probe bytes"))
            .sum::<u64>(),
        probe_limit as u64,
        "probes exactly fill their admission budget"
    );
    assert!(
        monitor_event_queue::enqueue_probe(&store, id, &operation, probe_count as i64, &json!("x"))
            .await
            .expect_err("additional probe exceeds its budget")
            .to_string()
            .contains("queue is full")
    );
    let largest_error = "e".repeat(monitor_event_queue::EXIT_RECEIPT_MAX_ERROR_BYTES);
    monitor_event_queue::enqueue_exit(
        &store,
        id,
        &operation,
        "result:largest",
        None,
        Some(&largest_error),
        99.0,
    )
    .await
    .expect("the maximum exit receipt still fits after probes");
    let saved = record(&store, id);
    let queued = saved["streamQueue"].as_array().expect("queued stream");
    assert_eq!(queued.len(), probe_count + 1);
    assert_eq!(queued.last().expect("exit receipt")["kind"], "exit");
    assert_eq!(
        queued.last().expect("exit receipt")["byteLength"],
        monitor_event_queue::EXIT_RECEIPT_MAX_ERROR_BYTES
    );
}

#[tokio::test]
async fn cancellation_recovery_runs_after_a_full_delivery_batch() {
    use studio_runtime_services::monitor_driver;

    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let mut case = running(&cases);
    let id = case["before"]["id"].as_str().expect("id").to_owned();
    let store = Arc::new(fixtures::store(root.path(), "cancel-full-batch").await);
    case["before"]["log"] = json!(store.root().join("monitor-logs").join(format!("{id}.log")));
    fixtures::seed(&store, &case).await;
    let id_for_reservation = id.clone();
    let (_, operation) = store
        .transaction_with(move |tx| {
            monitor_driver::reserve_cancel(tx, &id_for_reservation, None, 2000.0)
        })
        .await
        .expect("cancel reservation");
    assert!(operation.is_some());

    let port = Arc::new(Port::default());
    for offset in 0..64 {
        port.deliveries
            .lock()
            .expect("deliveries")
            .push_back(delivery(
                &case,
                &format!("out:{offset}"),
                MonitorEvent::Output {
                    offset,
                    bytes: vec![b'x'],
                },
            ));
    }
    let runtime = runtime(&store, &port);
    runtime.tick(2001.0).await.expect("full output batch");
    assert_eq!(port.deliveries.lock().expect("deliveries").len(), 0);
    assert_eq!(record(&store, &id)["cancelDispatch"]["status"], "written");
    assert_eq!(port.submitted.lock().expect("submitted").len(), 1);
}

#[tokio::test]
async fn timer_retries_a_lost_ack_then_joins_the_consumer() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let mut case = running(&cases);
    case["before"]["activityAt"] = json!(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs_f64()
    );
    case["before"]["stallTimeoutSeconds"] = json!(86400);
    let store = Arc::new(fixtures::store(root.path(), "timer").await);
    let id = case["before"]["id"].as_str().expect("id").to_owned();
    case["before"]["log"] = json!(store.root().join("monitor-logs").join(format!("{id}.log")));
    fixtures::seed(&store, &case).await;
    let port = Arc::new(Port::default());
    port.deliveries
        .lock()
        .expect("deliveries")
        .push_back(delivery(
            &case,
            "out:0",
            MonitorEvent::Output {
                offset: 0,
                bytes: b"marker".to_vec(),
            },
        ));
    *port.fail_ack.lock().expect("ack") = true;
    let first_ack_attempt = Arc::new(Notify::new());
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    *port.fail_ack_gate.lock().expect("ack gate") = Some((first_ack_attempt.clone(), release_rx));
    let acknowledged = Arc::new(tokio::sync::Semaphore::new(0));
    *port.acknowledged.lock().expect("acknowledged") = Some(acknowledged.clone());
    let wake = Arc::new(Notify::new());
    let runtime = Arc::new(MonitorRuntime::new(
        store.clone(),
        port.clone(),
        wake.clone(),
    ));
    let (stop, receiver) = tokio::sync::watch::channel(false);
    let first_ack = first_ack_attempt.notified();
    tokio::pin!(first_ack);
    first_ack.as_mut().enable();
    let task = studio_runtime_services::monitor_runtime_wiring::start(runtime.clone(), receiver);
    tokio::time::timeout(Duration::from_secs(5), first_ack)
        .await
        .expect("first ACK attempt");
    assert_eq!(port.deliveries.lock().expect("deliveries").len(), 1);
    release_tx.send(()).expect("release failed ACK");
    let ack = tokio::time::timeout(Duration::from_secs(5), acknowledged.acquire())
        .await
        .expect("retry tick acknowledges the retained delivery")
        .expect("acknowledgement semaphore remains open");
    drop(ack);
    assert_eq!(port.deliveries.lock().expect("deliveries").len(), 0);
    assert_eq!(record(&store, &id)["bytes"], 6);
    stop.send(true).expect("shutdown request");
    task.await.expect("task join").expect("clean stop");
    assert_eq!(runtime.last_tick_error(), None);
    let events = fixtures::events(&store);
    assert!(events.as_array().expect("events").is_empty());
}
