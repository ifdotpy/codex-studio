//! P15 checks frozen monitor effects and durable HTTP input replay.
use crate::monitor_runtime_fixtures as fixtures;

use base64::Engine;
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_schema::monitor_record::{
    MonitorCancelStatus, MonitorInputStatus as AttemptStatus, MonitorQueueEvent,
};
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
    AgentRecord, AgentRepository, MonitorRecord, MonitorsRepository, RuleRecord, RulesRepository,
    RuntimeStore, Table,
};
use tokio::sync::Notify;

use crate::monitor_runtime_support as support;
use support::Port;
fn input_request_id(label: &str) -> String {
    static ISSUED: std::sync::OnceLock<u128> = std::sync::OnceLock::new();
    let issued = *ISSUED.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis()
    });
    let digest = studio_runtime_store::sha256_hex(label.as_bytes());
    let suffix = u64::from_str_radix(&digest[..15], 16).expect("digest");
    uuid::Uuid::from_u128((issued << 80) | (7 << 76) | (2 << 62) | u128::from(suffix)).to_string()
}
async fn input_schema(store: &RuntimeStore) {
    store
        .transaction(|tx| {
            studio_runtime_store::receipt_schema::create(tx)?;
            Ok(())
        })
        .await
        .expect("target schema");
}

#[tokio::test]
async fn migrated_monitor_inputs_remain_visible_through_process_receipt() {
    use serde::Serialize;
    use studio_runtime_schema::persistence::MonitorInputStatus;
    #[derive(Serialize)]
    struct LegacyReceipt {
        monitor: String,
        operation: studio_runtime_store::MonitorOperation,
        status: MonitorInputStatus,
    }
    let root = fixtures::tempdir();
    let case = running(&fixtures::corpus(root.path()));
    let store = fixtures::store(root.path(), "migration").await;
    fixtures::seed(&store, &case).await;
    let path = store.root().to_owned();
    drop(store);
    let db = rusqlite::Connection::open(path.join("canvas.sqlite3")).expect("old store");
    db.execute_batch("DELETE FROM runtime_payload_migrations WHERE name LIKE 'receipts:%'; CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL);").expect("pre-cutover source");
    for (id, status) in [
        ("pending-input", MonitorInputStatus::Pending),
        ("unknown-input", MonitorInputStatus::Unknown),
    ] {
        let receipt = LegacyReceipt {
            monitor: "f8fc5c50-e4e4-50bc-8ff5-885105058c9d".into(),
            operation: studio_runtime_store::MonitorOperation {
                agent: "agent".to_owned().try_into().unwrap(),
                epoch: 2.try_into().unwrap(),
                account_key: "default".to_owned().try_into().unwrap(),
                connection_id: "connection".to_owned().try_into().unwrap(),
                evidence: Default::default(),
            },
            status,
        };
        db.execute(
            "INSERT INTO runtime_operation_receipts VALUES(?1,'http-input-v1:hash',?2)",
            rusqlite::params![
                format!("monitor-http-input:{id}"),
                serde_json::to_string(&receipt).expect("source receipt")
            ],
        )
        .expect("source");
    }
    drop(db);
    let store = RuntimeStore::open(Some(&path))
        .await
        .expect("migrate input");
    for (id, expected) in [("pending-input", "pending"), ("unknown-input", "unknown")] {
        let query = json!({"requestId": id, "agent": "agent", "accountKey": "default", "epoch": 2});
        let receipt = studio_runtime_services::process_controls::receipt(
            &store.test_reader().expect("read"),
            &query,
        )
        .expect("caller");
        let studio_runtime_schema::process_controls::ProcessReceipt::Found(receipt) = receipt
        else {
            panic!("migrated receipt must be found");
        };
        let Some(studio_runtime_schema::process_controls::ProcessResponse::MonitorInput(response)) =
            receipt.result.response
        else {
            panic!("monitor input response");
        };
        use studio_runtime_schema::process_controls::MonitorInputStatus;
        assert_eq!(
            response.status,
            match expected {
                "pending" => MonitorInputStatus::Pending,
                "unknown" => MonitorInputStatus::Unknown,
                _ => panic!("unexpected fixture status"),
            }
        );
    }
}

fn record(store: &RuntimeStore, id: &str) -> MonitorRecord {
    MonitorsRepository::get(&store.test_reader().expect("read"), id)
        .expect("get")
        .expect("monitor")
}
fn rule_record(store: &RuntimeStore, id: &str) -> RuleRecord {
    RulesRepository::get(&store.test_reader().expect("read"), id)
        .expect("get rule")
        .expect("rule")
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

#[derive(serde::Deserialize)]
struct RuleCase {
    name: String,
    owner: AgentRecord,
    rule: RuleRecord,
    now: f64,
    event: Option<RuleEventFixture>,
}
#[derive(serde::Deserialize)]
struct RuleEventFixture {
    kind: String,
    text: String,
    key: String,
}
fn rule_case(name: &str) -> RuleCase {
    serde_json::from_str::<Vec<RuleCase>>(
        &include_str!("rules_engine/expected.json")
            .replace("\"owner\": {", "\"owner\": {\"provider\":\"codex\","),
    )
    .expect("frozen rule cases")
    .into_iter()
    .find(|case| case.name == name)
    .unwrap_or_else(|| panic!("rule case {name}"))
}
fn due_rule_case() -> RuleCase {
    rule_case("interval-boundary--1")
}

#[tokio::test]
async fn oversized_provider_delta_is_persisted_in_order_and_replayed_exactly() {
    let root = fixtures::tempdir();
    let mut case = running(&fixtures::corpus(root.path()));
    let store = Arc::new(fixtures::store(root.path(), "large-output").await);
    let id = case["before"]["id"].as_str().expect("id").to_owned();
    case["before"]["log"] = json!(store.root().join("monitor-logs").join(format!("{id}.log")));
    let account = case["before"]["operation"]["accountKey"]
        .as_str()
        .expect("account")
        .to_owned();
    fixtures::seed(&store, &case).await;
    let mut bytes = vec![b'a'; studio_runtime_services::monitor_policy::CHUNK_BYTES];
    bytes.push(b'z');
    let params = monitor_event_queue::OutputDelta {
        stream: Some(studio_runtime_schema::monitor_record::MonitorStream::Stderr),
        delta_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
    };
    let enqueue_id = id.clone();
    store
        .transaction_with(move |tx| {
            assert!(monitor_event_queue::enqueue_output_in_transaction(
                tx,
                &enqueue_id,
                &account,
                7,
                11,
                &params
            )?);
            Ok::<_, studio_runtime_services::notification_dispatch::NotificationDispatchError>(())
        })
        .await
        .expect("accept large provider delta");
    let output = monitor_event_queue::next(&store)
        .await
        .expect("queue")
        .expect("output");
    let runtime = runtime(&store, &Arc::new(Port::default()));
    let mut partial = output.clone();
    partial.event = MonitorEvent::OutputStream {
        offset: 0,
        stream: "stderr".into(),
        bytes: bytes[..studio_runtime_services::monitor_policy::CHUNK_BYTES].to_vec(),
    };
    runtime
        .deliver(&partial, 1999.0)
        .await
        .expect("persist first chunk before interruption");
    runtime
        .deliver(&output, 1999.0)
        .await
        .expect("persist output");
    runtime
        .deliver(&output, 1999.0)
        .await
        .expect("replay after lost acknowledgement");
    monitor_event_queue::acknowledge(&store, &output)
        .await
        .expect("acknowledge");
    assert_eq!(
        std::fs::read(store.root().join("monitor-logs").join(format!("{id}.log"))).expect("log"),
        bytes
    );
    assert_eq!(record(&store, &id).bytes.get(), bytes.len() as i64);
    assert!(
        monitor_event_queue::next(&store)
            .await
            .expect("queue")
            .is_none()
    );
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
    let first = monitor_event_queue::OutputDelta {
        delta_base64: base64::engine::general_purpose::STANDARD.encode(b"one"),
        stream: None,
    };
    let second = monitor_event_queue::OutputDelta {
        delta_base64: base64::engine::general_purpose::STANDARD.encode(b"two"),
        stream: None,
    };
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
    let probe_result = studio_runtime_schema::monitor_record::MonitorProbeResult::Completed {
        exit_code: 1,
        output: "still responsive".into(),
    };
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
    let rule = rule_case.rule.clone();
    let rule_id = rule.id.clone();
    let now = rule_case.now;
    let owner = rule_case.owner;
    store
        .transaction(move |tx| {
            AgentRepository::put(tx, &owner)?;
            RulesRepository::put(tx, &rule)
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
    assert_eq!(rule_record(&store, &rule_id).in_flight, true);
    let reserved_json: String = store
        .test_reader()
        .expect("read")
        .query_row(
            "SELECT json_extract(record, '$.launchIntent') FROM runtime_rules WHERE id=?1",
            [&rule_id],
            |row| row.get(0),
        )
        .expect("saved launch intent");
    #[derive(serde::Deserialize)]
    struct Reserved {
        #[serde(flatten)]
        check: studio_runtime_services::rules_engine::RuleCheck,
        #[serde(flatten)]
        extra: std::collections::BTreeMap<String, Value>,
    }
    let reserved: Reserved = serde_json::from_str(&reserved_json).expect("typed launch intent");
    assert!(reserved.extra.is_empty());
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
    assert_eq!(launches[0], reserved.check);
}

#[tokio::test]
async fn event_submission_failure_recovers_the_committed_launch_intent() {
    use studio_runtime_services::rules_tick::{self, RuleEvent};

    let root = fixtures::tempdir();
    let monitor_cases = fixtures::corpus(root.path());
    let store = Arc::new(fixtures::store(root.path(), "event-rule-retry").await);
    fixtures::seed(&store, &running(&monitor_cases)).await;
    let event_case = rule_case("event-work_review");
    let event = event_case.event.expect("event");
    let event_kind = event.kind;
    let event_text = event.text;
    let event_key = event.key;
    let event_now = event_case.now;
    let owner = event_case.owner;
    let mut rule = event_case.rule;
    rule.event = event_kind.clone();
    rule.command = "printf marker".to_owned().try_into().expect("command");
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
    let reserved = rule_record(&store, &check.rule_id).launch_intent;

    let port = Arc::new(Port::default());
    let runtime = runtime(&store, &port);
    assert!(
        runtime
            .submit_event_checks(std::slice::from_ref(&check), event_now)
            .await
            .is_err()
    );
    assert_eq!(rule_record(&store, &check.rule_id).in_flight, true);
    assert!(port.rule_checks.lock().expect("rule checks").is_empty());

    *port.rule_available.lock().expect("rule availability") = true;
    *port
        .reconcile_not_submitted
        .lock()
        .expect("rule reconciliation") = true;
    runtime
        .tick(event_now + 1.0)
        .await
        .expect("tick recovers committed event launch");
    assert_eq!(
        rule_record(&store, &check.rule_id).launch_intent,
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
    case["before"]["activityAt"] = json!(1.0);
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
            let original = record(&store, id);
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
            assert_eq!(record(&store, id), original);
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
            assert_eq!(record(&store, id).status.as_str(), "completed");
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
    assert_eq!(record(&store, &id).cancel_requested, Some(true));

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
    assert_eq!(record(&store, &id).bytes.get(), 6);
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
    assert_eq!(record(&store, &id).status.as_str(), "completed");
    runtime.tick(2000.0).await.expect("exit replay");
    assert_eq!(record(&store, &id).bytes.get(), 6);
    assert_eq!(record(&store, &id).tail.as_str(), "marker");
    assert_eq!(record(&store, &id).status.as_str(), "completed");
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
    assert_eq!(
        record(&store, &id).reattached_connection_id.as_deref(),
        Some("restarted")
    );
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
    assert_eq!(
        record(&store, &id)
            .input_attempt
            .expect("input attempt")
            .status,
        AttemptStatus::Uncertain
    );
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
async fn monitor_completion_commits_the_python_rule_result_and_wake_atomically() {
    use studio_runtime_store::{RuleRecord, RulesRepository};
    let root = fixtures::tempdir();
    fixtures::corpus(root.path());
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct CompletionCase {
        name: String,
        actor: AgentRecord,
        before: MonitorRecord,
        after: MonitorRecord,
        rule: Option<RuleRecord>,
        after_rule: Option<RuleRecord>,
        code: Option<i64>,
        error: Option<String>,
        now: f64,
        wakes: Vec<ExpectedWake>,
    }
    #[derive(serde::Deserialize)]
    struct ExpectedWake {
        id: String,
        kind: String,
        text: String,
    }
    #[derive(Debug, PartialEq, serde::Deserialize)]
    struct WakeText {
        rule: String,
        name: String,
        instruction: String,
        output: String,
    }
    #[derive(Debug, PartialEq)]
    struct Wake {
        id: String,
        kind: String,
        text: WakeText,
    }
    let cases: Vec<CompletionCase> = serde_json::from_str(
        &include_str!("wave7_p15_support/completions.json")
            .replace("@FIXTURE_ROOT@", &root.path().to_string_lossy())
            .replace("\"actor\": {", "\"actor\": {\"provider\":\"codex\","),
    )
    .expect("frozen rule completions");
    assert_eq!(cases.len(), 9);
    for (number, case) in cases.iter().enumerate() {
        let store = Arc::new(fixtures::store(root.path(), &format!("rule-{number}")).await);
        let actor = case.actor.clone();
        let before = case.before.clone();
        let rule = case.rule.clone();
        store
            .transaction(move |tx| {
                AgentRepository::put(tx, &actor)?;
                Table::new(tx).put(&before)?;
                if let Some(rule) = rule {
                    RulesRepository::put(tx, &rule)?;
                }
                Ok(())
            })
            .await
            .expect("seed completion");
        let runtime = runtime(&store, &Arc::new(Port::default()));
        let result = Delivery {
            id: case.before.id.clone(),
            operation: case.before.operation.clone().expect("operation"),
            cursor: "exit".into(),
            event: MonitorEvent::Exit {
                code: case.code,
                error: case.error.clone(),
                finished: case.now,
            },
        };
        store.transaction(move |tx| { for operation in ["INSERT", "UPDATE"] { tx.execute_batch(&format!("CREATE TRIGGER injected_{operation} AFTER {operation} ON runtime_monitors BEGIN SELECT RAISE(ABORT,'injected'); END"))?; } Ok(()) }).await.expect("failure");
        assert!(runtime.deliver(&result, case.now).await.is_err());
        let original_rule =
            RulesRepository::get(&store.test_reader().expect("read"), "rule").expect("get");
        assert_eq!(original_rule, case.rule);
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
                .deliver(&result, case.now)
                .await
                .expect("completion replay");
        }
        assert_eq!(record(&store, &result.id), case.after, "{}", case.name);
        let saved_rule =
            RulesRepository::get(&store.test_reader().expect("read"), "rule").expect("get");
        assert_eq!(saved_rule, case.after_rule, "{}", case.name);
        let db = store.test_reader().expect("read");
        let mut query = db
            .prepare("SELECT id, kind, text FROM runtime_events ORDER BY id")
            .expect("events");
        let wakes: Vec<Wake> = query
            .query_map([], |row| {
                let text: String = row.get(2)?;
                Ok(Wake {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    text: serde_json::from_str(&text).expect("wake text"),
                })
            })
            .expect("events")
            .collect::<Result<_, _>>()
            .expect("events");
        let expected: Vec<Wake> = case
            .wakes
            .iter()
            .map(|wake| Wake {
                id: wake.id.clone(),
                kind: wake.kind.clone(),
                text: serde_json::from_str(&wake.text).expect("wake text"),
            })
            .collect();
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
    foreign.operation.connection_id = "foreign".to_owned().try_into().unwrap();
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
    foreign_input.operation.connection_id = "foreign".to_owned().try_into().unwrap();
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
            let original = record(&store, id);
            store.transaction(move |tx| { for operation in ["INSERT", "UPDATE"] { tx.execute_batch(&format!("CREATE TRIGGER injected_{operation} {timing} {operation} ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END"))?; } Ok(()) }).await.expect("inject");
            assert!(runtime.cancel(id, None, 2000.0).await.is_err());
            assert!(port.submitted.lock().expect("submitted").is_empty());
            assert_eq!(record(&store, id), original);
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
            assert_eq!(record(&store, id).cancel_requested, Some(true));
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
    assert_eq!(
        record(&store, &id)
            .cancel_dispatch
            .expect("cancel dispatch")
            .status,
        MonitorCancelStatus::Pending
    );
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
    assert_eq!(
        record(&store, &id)
            .cancel_dispatch
            .expect("cancel dispatch")
            .status,
        MonitorCancelStatus::Pending
    );
    assert_eq!(
        record(&store, &id)
            .cancel_dispatch
            .expect("cancel dispatch")
            .next_attempt_at,
        Some(2002.0)
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
    assert_eq!(
        record(&store, &id)
            .cancel_dispatch
            .expect("cancel dispatch")
            .attempts,
        Some(2)
    );
    assert_eq!(
        record(&store, &id)
            .cancel_dispatch
            .expect("cancel dispatch")
            .next_attempt_at,
        Some(2004.0)
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
    assert_eq!(
        record(&store, &id)
            .cancel_dispatch
            .expect("cancel dispatch")
            .status,
        MonitorCancelStatus::Written
    );
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
        record(&unknown_store, unknown_id)
            .cancel_dispatch
            .expect("cancel dispatch")
            .status,
        MonitorCancelStatus::Unknown
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
            MonitorCancelStatus::Done,
        ),
        (
            "cancel-session-missing",
            NotSentReason::SessionMissing,
            "running",
            MonitorCancelStatus::Unknown,
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

        assert_eq!(record(&store, id).status.as_str(), expected_status);
        assert_eq!(
            record(&store, id)
                .cancel_dispatch
                .expect("cancel dispatch")
                .status,
            expected_dispatch
        );
        assert_eq!(
            record(&store, id)
                .cancel_dispatch
                .expect("cancel dispatch")
                .error
                .as_deref(),
            Some("injected not sent")
        );
        if reason == NotSentReason::SessionMissing {
            let record = record(&store, id);
            assert_eq!(record.status.as_str(), "running");
            assert_eq!(
                record.error.as_ref().map(|error| error.as_str()),
                Some("injected not sent")
            );
            let view = studio_runtime_services::sync_views::project_persisted_record(
                "monitor",
                &json!(record),
            )
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

    assert_eq!(record(&store, &id).status.as_str(), "cancelled");
    assert_eq!(
        record(&store, &id)
            .cancel_dispatch
            .expect("cancel dispatch")
            .status,
        MonitorCancelStatus::Done
    );
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
    let bad_id = bad_case["before"]["id"]
        .as_str()
        .expect("monitor id")
        .to_owned();
    let mut healthy = MonitorsRepository::get(&store.test_reader().expect("read"), &bad_id)
        .expect("get")
        .expect("monitor");
    healthy.id = "healthy-monitor".to_owned();
    healthy.log = "unused-until-output".to_owned();
    store
        .transaction(move |tx| Table::new(tx).put(&healthy))
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
    foreign.operation.connection_id = "foreign-connection".to_owned().try_into().unwrap();
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
        .to_owned()
        .try_into()
        .unwrap();
    healthy_case["before"]["log"] = json!(
        store
            .root()
            .join("monitor-logs")
            .join("healthy-monitor.log")
    );
    let mut healthy =
        MonitorsRepository::get(&store.test_reader().expect("read"), "healthy-monitor")
            .expect("get")
            .expect("monitor");
    healthy.log = healthy_case["before"]["log"]
        .as_str()
        .expect("log")
        .to_owned();
    store
        .transaction(move |tx| Table::new(tx).put(&healthy))
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
    assert_eq!(record(&store, bad_id).status.as_str(), "failed");
    assert_eq!(
        record(&store, bad_id)
            .delivery_failure
            .expect("delivery failure")
            .cursor,
        "foreign-exit"
    );
    assert!(
        record(&store, bad_id)
            .delivery_failure
            .expect("delivery failure")
            .error
            .contains("result operation changed")
    );
    assert_eq!(record(&store, "healthy-monitor").bytes.get(), 7);
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
    input_schema(&store).await;
    fixtures::seed(&store, &case).await;
    let id = case["before"]["id"].as_str().expect("monitor id");
    let mut monitor = MonitorsRepository::get(&store.test_reader().expect("read"), id)
        .expect("get")
        .expect("monitor");
    monitor.interactive = true;
    store
        .transaction(move |tx| Table::new(tx).put(&monitor))
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
        .input_http(id, &input_request_id("lost-http-success"), &input, 2000.0)
        .await
        .expect("first input acknowledged");
    let first = record(&store, id)
        .last_input_attempt
        .expect("first ticket")
        .id;
    runtime
        .input_http(id, &input_request_id("lost-http-success"), &input, 2001.0)
        .await
        .expect("same logical input replays its acknowledgement");
    let second = record(&store, id)
        .last_input_attempt
        .expect("second ticket")
        .id;

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
async fn http_input_busy_sequence_records_a_refusal_and_preserves_prior_receipts() {
    let root = fixtures::tempdir();
    let mut case = running(&fixtures::corpus(root.path()));
    case["before"]["interactive"] = json!(true);
    let store = Arc::new(fixtures::store(root.path(), "http-input-busy").await);
    input_schema(&store).await;
    fixtures::seed(&store, &case).await;
    let id = case["before"]["id"]
        .as_str()
        .expect("monitor id")
        .to_owned();
    let port = Arc::new(Port::default());
    let runtime = Arc::new(runtime(&store, &port));
    let input = Input {
        text: "one line".into(),
        rows: None,
        cols: None,
        close_stdin: false,
    };
    runtime
        .input_http(&id, &input_request_id("previous-ack"), &input, 2000.0)
        .await
        .expect("previous input acknowledged");
    let entered = Arc::new(Notify::new());
    let (release, gate) = tokio::sync::oneshot::channel();
    *port.input_submit_gate.lock().expect("install input gate") = Some((entered.clone(), gate));
    let (active_runtime, active_id, active_input) = (runtime.clone(), id.clone(), input.clone());
    let active = tokio::spawn(async move {
        active_runtime
            .input_http(
                &active_id,
                &input_request_id("active-request"),
                &active_input,
                2001.0,
            )
            .await
    });
    entered.notified().await;
    let busy = runtime
        .input_http(&id, &input_request_id("busy-fresh-request"), &input, 2002.0)
        .await;
    let pending = runtime
        .input_http(&id, &input_request_id("active-request"), &input, 2002.0)
        .await;
    let acknowledged = runtime
        .input_http(&id, &input_request_id("previous-ack"), &input, 2002.0)
        .await;
    release
        .send(())
        .expect("release exact held native submission");
    active
        .await
        .expect("join active input")
        .expect("active input acknowledged");
    assert!(
        matches!(
            busy,
            Err(studio_runtime_services::monitor_policy::MonitorError::Refused(_))
        ),
        "a new request was never admitted: {busy:?}"
    );
    assert!(
        matches!(
            pending,
            Err(studio_runtime_services::monitor_policy::MonitorError::InputOutcomeUnknown)
        ),
        "prior pending identity remains uncertain"
    );
    assert!(
        acknowledged.is_ok(),
        "prior acknowledgement replays while another request holds the sequence"
    );
    let query = json!({"requestId":input_request_id("busy-fresh-request"),"agent":case["actor"]["id"],"accountKey":case["actor"]["accountKey"],"epoch":case["actor"]["epoch"]});
    let receipt = studio_runtime_services::process_controls::receipt(
        &store.test_reader().expect("read"),
        &query,
    )
    .expect("read durable refusal");
    let studio_runtime_schema::process_controls::ProcessReceipt::Found(receipt) = receipt else {
        panic!("refusal must commit before HTTP failure");
    };
    let Some(studio_runtime_schema::process_controls::ProcessResponse::MonitorInput(response)) =
        &receipt.result.response
    else {
        panic!("monitor input response");
    };
    assert_eq!(
        response.status,
        studio_runtime_schema::process_controls::MonitorInputStatus::Rejected
    );
    assert_eq!(
        record(&store, &id)
            .last_input_attempt
            .expect("input attempt")
            .id,
        format!("http:{}", input_request_id("active-request")),
        "busy request never reserves an inputAttempt"
    );
    assert!(
        runtime
            .input_http(&id, &input_request_id("busy-fresh-request"), &input, 2003.0)
            .await
            .is_err()
    );
    assert_eq!(
        port.submitted.lock().expect("no busy native effect").len(),
        2
    );
    runtime
        .input_http(
            &id,
            &input_request_id("fresh-intent-after-refusal"),
            &input,
            2004.0,
        )
        .await
        .expect("confirmed refusal permits a new intentional identity");
    assert_eq!(
        port.submitted.lock().expect("new intentional input").len(),
        3
    );
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
        input_schema(&store).await;
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
                tx.execute_batch("CREATE TRIGGER refuse_input_receipt BEFORE INSERT ON request_results BEGIN SELECT RAISE(ABORT,'fixture receipt failure'); END")?;
                Ok(())
            }).await.expect("inject exact settlement failure");
        }
        let input = Input {
            text: "private input\n".into(),
            rows: None,
            cols: None,
            close_stdin: false,
        };
        let request_id = uuid::Uuid::now_v7().to_string();
        let result = runtime(&store, &port)
            .input_http(id, &request_id, &input, 2000.0)
            .await;
        assert_eq!(
            result.is_ok(),
            boundary == "acknowledged",
            "{boundary}: {result:?}"
        );
        assert_eq!(
            port.submitted.lock().expect("native writes").len(),
            1,
            "{boundary}: {result:?}"
        );
        let saved = record(&store, id);
        if boundary == "settlement-rollback" {
            assert_eq!(
                saved.input_attempt.expect("input attempt").status,
                AttemptStatus::Submitted,
                "monitor settlement shares receipt transaction"
            );
            assert!(saved.last_input_attempt.is_none());
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
        let replay = restarted.input_http(id, &request_id, &input, 2001.0).await;
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
                .input_http(id, &request_id, &changed, 2002.0)
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
                .input_http(id, &uuid::Uuid::now_v7().to_string(), &input, 2003.0)
                .await
                .expect("fresh identity permits deliberate identical input");
            assert_eq!(fresh_port.submitted.lock().expect("fresh write").len(), 1);
        } else {
            assert!(
                restarted
                    .input_http(id, &uuid::Uuid::now_v7().to_string(), &input, 2003.0)
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
            let operation =
                serde_json::from_value(case["before"]["operation"].clone()).expect("operation");
            monitor_runtime::settle_input(
                &store,
                id,
                &operation,
                &format!("http:{request_id}"),
                studio_runtime_services::monitor_policy::InputOutcome::Acknowledged,
                2004.0,
            )
            .await
            .expect("late exact native callback resolves the uncertainty");
            restarted
                .input_http(id, &request_id, &input, 2005.0)
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
    let mut monitor = MonitorsRepository::get(&store.test_reader().expect("read"), id)
        .expect("get")
        .expect("monitor");
    monitor.status = studio_runtime_schema::studio_errors::MonitorStatus::Starting;
    monitor.operation = None;
    store
        .transaction(move |tx| Table::new(tx).put(&monitor))
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
    let chunk = base64::engine::general_purpose::STANDARD.encode(b"x");
    let enqueue_id = id.to_owned();
    let enqueue_account = account.to_owned();
    let enqueue_chunk = monitor_event_queue::OutputDelta {
        delta_base64: chunk.clone(),
        stream: None,
    };
    store
        .transaction_with(move |tx| {
            for cursor in 0..126 {
                monitor_event_queue::enqueue_output_in_transaction(
                    tx,
                    &enqueue_id,
                    &enqueue_account,
                    7,
                    cursor,
                    &enqueue_chunk,
                )?;
            }
            Ok::<_, studio_runtime_services::notification_dispatch::NotificationDispatchError>(())
        })
        .await
        .expect("fill output slots");
    let before = record(&store, id);
    let rejected_id = id.to_owned();
    let rejected_account = account.to_owned();
    let rejected_chunk = monitor_event_queue::OutputDelta {
        delta_base64: chunk,
        stream: None,
    };
    let rejected_identity =
        studio_runtime_services::notification_dispatch::ProviderDeliveryIdentity {
            provider_handle: format!("account:{account}"),
            generation: 7,
            cursor: 126,
        };
    let stored_identity = rejected_identity.clone();
    let rejected = store
        .transaction_with::<_, studio_runtime_services::notification_dispatch::NotificationDispatchError>(move |tx| {
            studio_runtime_services::account_worker::put_notification(tx,&rejected_identity,studio_runtime_services::notification_dispatch::NotificationSettlement {signature:"same-notification".into(),outcome:studio_runtime_services::notification_dispatch::DispatchOutcome::Written})?;
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
            studio_runtime_services::account_worker::get_notification(tx, &stored_identity)
        })
        .await
        .expect("read notification receipt");
    assert!(
        receipt.is_none(),
        "failed queue append rolls back its journal settlement"
    );

    let operation = serde_json::from_value(case["before"]["operation"].clone()).expect("operation");
    monitor_event_queue::enqueue_probe(
        &store,
        id,
        &operation,
        8,
        &studio_runtime_schema::monitor_record::MonitorProbeResult::Completed {
            exit_code: 0,
            output: String::new(),
        },
    )
    .await
    .expect("probe fits reserved event slot");
    monitor_event_queue::enqueue_exit(&store, id, &operation, "exit:count", Some(0), None, 9.0)
        .await
        .expect("exit fits reserved event slot");
    assert_eq!(
        record(&store, id).stream_queue.expect("stream queue").len(),
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
    let chunk = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let enqueue_id = id.to_owned();
    let enqueue_account = account.to_owned();
    let enqueue_chunk = monitor_event_queue::OutputDelta {
        delta_base64: chunk.clone(),
        stream: None,
    };
    store
        .transaction_with(move |tx| {
            for cursor in 0..30 {
                monitor_event_queue::enqueue_output_in_transaction(
                    tx,
                    &enqueue_id,
                    &enqueue_account,
                    7,
                    cursor,
                    &enqueue_chunk,
                )?;
            }
            Ok::<_, studio_runtime_services::notification_dispatch::NotificationDispatchError>(())
        })
        .await
        .expect("fill output byte admission limit");
    assert_eq!(
        record(&store, id).stream_queue.expect("stream queue")[29].byte_length(),
        32 * 1024
    );
    let rejected_id = id.to_owned();
    let rejected_account = account.to_owned();
    let rejected_chunk = monitor_event_queue::OutputDelta {
        delta_base64: chunk,
        stream: None,
    };
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
        monitor_event_queue::enqueue_probe(
            &store,
            id,
            &operation,
            8,
            &studio_runtime_schema::monitor_record::MonitorProbeResult::Completed {
                exit_code: 0,
                output: String::new()
            }
        )
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
    let operation_json: String = store
        .test_reader()
        .expect("read")
        .query_row(
            "SELECT json_extract(record, '$.operation') FROM runtime_monitors WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .expect("saved operation");
    let operation = serde_json::from_str(&operation_json).expect("monitor operation");
    let id_for_queue_reset = id.to_owned();
    store
        .transaction_with(move |tx| {
            let mut record = MonitorsRepository::get(tx, &id_for_queue_reset)?.expect("monitor");
            record.stream_queue = Some(vec![]);
            Table::<MonitorRecord>::new(tx).put(&record)
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
        let result = studio_runtime_schema::monitor_record::MonitorProbeResult::Failed {
            error: "x".repeat(result_bytes - 12),
        };
        monitor_event_queue::enqueue_probe(&store, id, &operation, generation as i64, &result)
            .await
            .expect("probe fills bounded admission budget");
    }
    let probe_queue = record(&store, id);
    let probe_queue = probe_queue.stream_queue.expect("probe queue");
    assert_eq!(probe_queue.len(), probe_count);
    assert_eq!(
        probe_queue
            .iter()
            .map(MonitorQueueEvent::byte_length)
            .sum::<u64>(),
        probe_limit as u64,
        "probes exactly fill their admission budget"
    );
    assert!(
        monitor_event_queue::enqueue_probe(
            &store,
            id,
            &operation,
            probe_count as i64,
            &studio_runtime_schema::monitor_record::MonitorProbeResult::Failed {
                error: "x".into()
            }
        )
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
    let queued = saved.stream_queue.expect("queued stream");
    assert_eq!(queued.len(), probe_count + 1);
    assert!(matches!(
        queued.last().expect("exit receipt"),
        MonitorQueueEvent::Exit { .. }
    ));
    assert_eq!(
        queued.last().expect("exit receipt").byte_length(),
        monitor_event_queue::EXIT_RECEIPT_MAX_ERROR_BYTES as u64
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
    assert_eq!(
        record(&store, &id)
            .cancel_dispatch
            .expect("cancel dispatch")
            .status,
        MonitorCancelStatus::Written
    );
    assert_eq!(port.submitted.lock().expect("submitted").len(), 1);
}
