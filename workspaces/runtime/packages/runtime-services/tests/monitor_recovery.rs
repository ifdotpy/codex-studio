#[path = "monitors/fixtures.rs"]
mod fixtures;
use serde_json::{Value, json};
use studio_runtime_services::{
    monitor_output::MonitorOutput,
    monitor_policy::{self, Input, InputOutcome, MonitorSnapshot, Operation},
    monitor_receipts::{self, ResultReceipt},
    monitor_runtime,
};
use studio_runtime_store::path_identity::CanonicalPath;
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
    MonitorsRepository::get(&store.test_reader().expect("monitor fixture"), id)
        .expect("monitor fixture")
        .expect("monitor fixture")
        .value()
        .clone()
}

#[tokio::test]
async fn local_cancel_persists_before_fake_provider_termination() {
    use std::sync::{Arc, Mutex};
    use studio_runtime_services::monitor_driver::{self, MonitorNative, Submission};
    struct Port {
        store: Arc<RuntimeStore>,
        id: String,
        submitted: Mutex<Vec<String>>,
    }
    impl MonitorNative for Port {
        fn submit<'a>(
            &'a self,
            _: &'a Operation,
            effect: &'a monitor_policy::Effect,
        ) -> futures_util::future::BoxFuture<'a, Submission> {
            Box::pin(async move {
                assert_eq!(
                    record(&self.store, &self.id)["cancelRequested"],
                    json!(true)
                );
                let monitor_policy::Effect::Native { method, params, .. } = effect else {
                    panic!("expected provider termination")
                };
                assert_eq!(*method, "command/exec/terminate");
                assert_eq!(params["processId"], self.id);
                self.submitted
                    .lock()
                    .expect("submission lock")
                    .push((*method).into());
                Submission::Written
            })
        }
    }
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let mut case = running(&cases);
    let id = "local-cancel-provider-test";
    case["before"]["id"] = json!(id);
    let store = Arc::new(fixtures::store(root.path(), "local-cancel").await);
    fixtures::seed(&store, &case).await;
    let port = Port {
        store: Arc::clone(&store),
        id: id.into(),
        submitted: Mutex::new(Vec::new()),
    };

    let result = monitor_driver::cancel(&store, &port, id, None, 2001.0)
        .await
        .expect("local cancel dispatch succeeds");
    assert_eq!(result["status"], "running");
    assert_eq!(result["cancelRequested"], true);
    assert_eq!(record(&store, id)["cancelRequested"], true);
    assert_eq!(
        *port.submitted.lock().expect("submission lock"),
        ["command/exec/terminate"]
    );
}

#[tokio::test]
async fn receipt_survives_store_failure_restart_and_replays_once() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    let id = case["before"]["id"].as_str().expect("monitor fixture");
    let store = fixtures::store(root.path(), "receipt").await;
    fixtures::seed(&store, &case).await;
    let receipt = ResultReceipt {
        version: 1,
        key: id.into(),
        code: Some(0),
        error: None,
        operation: Some(operation(&case)),
        finished: 2000.0,
    };
    store.transaction(move |tx| { tx.execute_batch("CREATE TRIGGER fail_monitor BEFORE INSERT ON runtime_monitors BEGIN SELECT RAISE(ABORT,'injected'); END")?; Ok(()) }).await.expect("monitor fixture");
    assert!(monitor_receipts::finish(&store, &receipt).await.is_err());
    assert_eq!(record(&store, id)["status"], "running");
    assert_eq!(fixtures::events(&store), json!([]));
    assert_eq!(
        std::fs::read_dir(store.root().join("monitor-results"))
            .expect("monitor fixture")
            .count(),
        1
    );
    store
        .transaction(move |tx| {
            tx.execute_batch("DROP TRIGGER fail_monitor")?;
            Ok(())
        })
        .await
        .expect("monitor fixture");
    let state = store.root().to_owned();
    drop(store);
    let store = RuntimeStore::open(Some(&state))
        .await
        .expect("monitor fixture");
    let restored = monitor_receipts::recover(&store, &[id.into()]).await;
    assert!(restored[0].1.is_ok());
    assert_eq!(record(&store, id)["status"], "completed");
    assert_eq!(fixtures::events(&store), case["events"]);
    monitor_receipts::finish(&store, &receipt)
        .await
        .expect("monitor fixture");
    assert_eq!(fixtures::events(&store), case["events"]);
    assert_eq!(
        std::fs::read_dir(store.root().join("monitor-results"))
            .expect("monitor fixture")
            .count(),
        0
    );
}

/// Rehearsal 9 (rule monitor of agent 4c360fa0): the monitor finished while two
/// output chunks were still queued. The first late chunk was skipped without
/// advancing the byte count, so the second failed as "cursor has a gap" and
/// replaced the monitor's own result with that delivery error.
#[tokio::test]
async fn output_queued_before_a_finish_is_ignored_without_a_cursor_gap() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let mut case = running(&cases);
    let id = case["before"]["id"]
        .as_str()
        .expect("monitor fixture")
        .to_owned();
    let store = fixtures::store(root.path(), "late-output").await;
    case["before"]["bytes"] = json!(0);
    case["before"]["log"] = json!(
        studio_runtime_services::monitor_output::log_root(store.root()).join(format!("{id}.log"))
    );
    fixtures::seed(&store, &case).await;
    let operation = operation(&case);
    monitor_receipts::finish(
        &store,
        &ResultReceipt {
            version: 1,
            key: id.clone(),
            code: Some(0),
            error: None,
            operation: Some(operation.clone()),
            finished: 2000.0,
        },
    )
    .await
    .expect("monitor finishes");
    let finished = record(&store, &id);
    let output = MonitorOutput::default();
    for (offset, bytes) in [(0, b"one".as_slice()), (3, b"two".as_slice())] {
        output
            .append(&store, &id, &operation, offset, bytes, 2001.0)
            .await
            .expect("late output is ignored");
    }
    assert_eq!(record(&store, &id), finished);
}

#[tokio::test]
async fn malformed_pending_result_receipt_is_retained_and_skipped() {
    let root = fixtures::tempdir();
    let store = fixtures::store(root.path(), "corrupt-receipt").await;
    let directory = store.root().join("monitor-results");
    std::fs::create_dir(&directory).expect("monitor fixture");
    std::fs::write(directory.join("corrupt.json"), b"{").expect("monitor fixture");

    assert_eq!(
        monitor_receipts::recover_pending(&store, 64)
            .await
            .expect("skip corrupt receipt"),
        0
    );
    assert!(directory.join("corrupt.json").exists());
}

#[tokio::test]
async fn unknown_input_stays_blocked_and_foreign_receipt_cannot_release_it() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let mut case = running(&cases);
    case["before"]["interactive"] = json!(true);
    let store = fixtures::store(root.path(), "input").await;
    fixtures::seed(&store, &case).await;
    let id = case["before"]["id"].as_str().expect("monitor fixture");
    let operation = operation(&case);
    let input = Input {
        text: "payload".into(),
        rows: None,
        cols: None,
        close_stdin: true,
    };
    monitor_runtime::reserve_input(&store, id, &operation, "ticket-1", &input, 2000.0)
        .await
        .expect("monitor fixture");
    monitor_runtime::settle_input(
        &store,
        id,
        &operation,
        "ticket-1",
        InputOutcome::Unknown("transport disconnected".into()),
        2001.0,
    )
    .await
    .expect("monitor fixture");
    let unknown = record(&store, id);
    assert_eq!(unknown["inputAttempt"]["status"], "uncertain");
    assert!(
        monitor_runtime::reserve_input(&store, id, &operation, "ticket-2", &input, 2002.0)
            .await
            .is_err()
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
        .await
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
    .await
    .expect("monitor fixture");
    assert_eq!(record(&store, id)["stdinClosed"], true);
    assert!(record(&store, id).get("inputAttempt").is_none());
}

#[tokio::test]
async fn monitor_cancel_persists_before_dispatch_and_malformed_state_is_refused() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    let store = fixtures::store(root.path(), "refusal").await;
    fixtures::seed(&store, &case).await;
    let id = case["before"]["id"].as_str().expect("monitor fixture");
    let before = record(&store, id);
    let result = monitor_runtime::cancel(&store, id, None, 2000.0)
        .await
        .expect("cancel intent persists without a synchronous provider port");
    assert_eq!(result["cancelRequested"], true);
    assert_eq!(record(&store, id)["cancelRequested"], true);
    assert_ne!(record(&store, id), before);
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
    let store = fixtures::store(root.path(), "child").await;
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
    fixtures::seed(&store, &case).await;
    let params = json!({"processId":id,"cwd":case["before"]["cwd"],"timeoutMs":case["before"]["timeout_ms"],"sandboxPolicy":{},"command":["/bin/sh","-c","printf marker"]});
    let prepared = monitor_runtime::PreparedStart {
        monitor: case["before"].clone(),
        actor: case["actor"].clone(),
        operation: operation.clone(),
        params,
    };
    let effect = monitor_runtime::reserve_start(&store, &id, &prepared, 2000.0)
        .await
        .expect("start");
    assert!(matches!(
        effect,
        monitor_policy::Effect::Native {
            method: "command/exec",
            ..
        }
    ));
    assert!(
        monitor_runtime::reserve_start(&store, &id, &prepared, 2000.0)
            .await
            .is_err()
    );
    let mut child = StreamChild::spawn(ChildSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "printf marker".into()],
        cwd: root.path().into(),
        environment: Default::default(),
        inherited_lease: None,
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
                    .await
                    .expect("monitor fixture");
                output
                    .append(&store, &id, &operation, cursor, &bytes, 2001.0)
                    .await
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
            .await
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

#[tokio::test]
async fn result_write_boundaries_roll_back_then_recover_the_first_receipt() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    let id = case["before"]["id"].as_str().expect("id").to_owned();
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
            let store = fixtures::store(root.path(), &format!("{table}-{boundary}")).await;
            fixtures::seed(&store, &case).await;
            let receipt = ResultReceipt {
                version: 1,
                key: id.clone(),
                code: Some(0),
                error: None,
                operation: Some(operation(&case)),
                finished: 2000.0,
            };
            store.transaction(move |tx| {
                // An upsert fires AFTER UPDATE when its row already exists.
                for operation in ["INSERT", "UPDATE"] {
                    tx.execute_batch(&format!("CREATE TRIGGER injected_{operation} {boundary} {operation} ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END"))?;
                }
                Ok(())
            }).await.expect("install boundary");
            assert!(
                monitor_receipts::finish(&store, &receipt).await.is_err(),
                "{table}-{boundary}"
            );
            assert_eq!(record(&store, &id), case["before"]);
            assert_eq!(fixtures::events(&store), json!([]));
            store
                .transaction(move |tx| {
                    tx.execute_batch("DROP TRIGGER injected_INSERT; DROP TRIGGER injected_UPDATE")?;
                    Ok(())
                })
                .await
                .expect("release boundary");
            let state = store.root().to_owned();
            drop(store);
            let store = RuntimeStore::open(Some(&state)).await.expect("restart");
            assert!(
                monitor_receipts::recover(&store, std::slice::from_ref(&id)).await[0]
                    .1
                    .is_ok()
            );
            assert_eq!(record(&store, &id), case["after"]);
            assert_eq!(fixtures::events(&store), case["events"]);
        }
    }
}

#[tokio::test]
async fn native_input_is_called_after_commit_and_unknown_is_never_resent() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use studio_runtime_services::monitor_driver::{self, InputRequest, MonitorNative, Submission};
    struct Port<'a> {
        store: &'a RuntimeStore,
        id: &'a str,
        sends: AtomicUsize,
    }
    impl MonitorNative for Port<'_> {
        fn submit<'a>(
            &'a self,
            _: &'a Operation,
            _: &'a monitor_policy::Effect,
        ) -> futures_util::future::BoxFuture<'a, Submission> {
            Box::pin(async move {
                // Taking the writer here would deadlock if submit ran inside admission.
                let id = self.id.to_owned();
                self.store
                    .transaction(move |tx| {
                        let saved = MonitorsRepository::get(tx, &id)?.expect("admitted row");
                        assert_eq!(
                            saved
                                .get("inputAttempt")
                                .and_then(|v| v.get("id"))
                                .and_then(Value::as_str),
                            Some("ticket-1")
                        );
                        Ok(())
                    })
                    .await
                    .expect("outside admission transaction");
                self.sends.fetch_add(1, Ordering::SeqCst);
                Submission::Unknown
            })
        }
    }
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let mut case = running(&cases);
    case["before"]["interactive"] = json!(true);
    let store = fixtures::store(root.path(), "port").await;
    fixtures::seed(&store, &case).await;
    let id = case["before"]["id"].as_str().expect("id").to_owned();
    let operation = operation(&case);
    let input = Input {
        text: "hello".into(),
        rows: None,
        cols: None,
        close_stdin: false,
    };
    let port = Port {
        store: &store,
        id: &id,
        sends: AtomicUsize::new(0),
    };
    assert!(matches!(
        monitor_driver::input(
            &store,
            &port,
            InputRequest {
                id: &id,
                operation: &operation,
                ticket: "ticket-1",
                input: &input
            },
            2000.0
        )
        .await,
        Ok(Submission::Unknown)
    ));
    assert!(
        monitor_driver::input(
            &store,
            &port,
            InputRequest {
                id: &id,
                operation: &operation,
                ticket: "ticket-2",
                input: &input
            },
            2001.0
        )
        .await
        .is_err()
    );
    assert_eq!(port.sends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancel_of_a_running_command_is_admitted_after_its_submission() {
    use std::sync::{Arc, Mutex};
    use studio_runtime_services::monitor_driver::{self, MonitorNative, Submission};
    use tokio::sync::Notify;
    struct Port {
        entered: Notify,
        release: Notify,
        sent: Mutex<Vec<String>>,
    }
    impl MonitorNative for Port {
        fn submit<'a>(
            &'a self,
            _: &'a Operation,
            effect: &'a monitor_policy::Effect,
        ) -> futures_util::future::BoxFuture<'a, Submission> {
            Box::pin(async move {
                let monitor_policy::Effect::Native { method, .. } = effect else {
                    panic!("native effect")
                };
                // The first poll hands the write to the transport.
                self.sent.lock().expect("sent").push((*method).into());
                if *method == "command/exec" {
                    // command/exec answers only when the process exits.
                    self.entered.notify_one();
                    self.release.notified().await;
                }
                Submission::Written
            })
        }
    }
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let mut case = running(&cases);
    case["before"]["id"] = json!("monitor-admission-barrier");
    let op = operation(&case);
    case["before"]["status"] = json!("starting");
    case["before"]
        .as_object_mut()
        .expect("record")
        .remove("operation");
    let store = Arc::new(fixtures::store(root.path(), "sequence").await);
    fixtures::seed(&store, &case).await;
    let id = case["before"]["id"].as_str().expect("id").to_owned();
    let prepared = monitor_runtime::PreparedStart {
        monitor: case["before"].clone(),
        actor: case["actor"].clone(),
        operation: op,
        params: json!({"processId":id,"cwd":case["before"]["cwd"],"timeoutMs":case["before"]["timeout_ms"],"sandboxPolicy":{}}),
    };
    let port = Arc::new(Port {
        entered: Notify::new(),
        release: Notify::new(),
        sent: Mutex::new(vec![]),
    });
    let prepared = Arc::new(prepared);
    let start_store = Arc::clone(&store);
    let start_port = Arc::clone(&port);
    let start_prepared = Arc::clone(&prepared);
    let start_id = id.clone();
    let start = tokio::spawn(async move {
        monitor_driver::start(
            &start_store,
            start_port.as_ref(),
            &start_id,
            start_prepared.as_ref(),
            2000.0,
        )
        .await
    });
    port.entered.notified().await; // The running intent committed, command/exec is unresolved.
    // 2026-10-20: a cancel of a running command was refused until the command
    // exited, because start held the native sequence for the whole reply.
    monitor_driver::cancel(&store, port.as_ref(), &id, None, 2001.0)
        .await
        .expect("cancel of a running command is admitted");
    assert_eq!(record(&store, &id)["cancelRequested"], json!(true));
    assert_eq!(
        *port.sent.lock().expect("sent"),
        vec!["command/exec", "command/exec/terminate"]
    );
    port.release.notify_one();
    start.await.expect("start task").expect("start");
}

#[tokio::test]
async fn reattach_requires_exact_proof_and_cancels_a_stopped_owner() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let case = running(&cases);
    let op = operation(&case);
    let mut lost = case["before"].clone();
    lost["status"] = json!("running");
    lost["reattachRecovery"] =
        json!({"status":"running","epoch":2,"accountKey":"default","startedAt":2000.0});
    let proof = monitor_policy::ReattachProof {
        operation: op.clone(),
        operation_id: format!("monitor:{}", lost["id"].as_str().expect("id")),
        native_id: 42,
        current_connection: "new-connection".into(),
    };
    let snapshot = MonitorSnapshot::checked(lost.clone(), case["actor"].clone()).expect("snapshot");
    let restored = monitor_policy::reattach(&snapshot, &proof).expect("reattach");
    assert_eq!(restored.record["status"], "running");
    assert_eq!(restored.record["reattachedConnectionId"], "new-connection");
    assert!(restored.effects.is_empty());
    let mut foreign = op.clone();
    foreign.connection_id = "foreign".into();
    assert!(monitor_policy::output(&snapshot, &foreign, b"foreign", 2000.0).is_err());
    let mut stopped = case["actor"].clone();
    stopped["autoWake"] = json!(false);
    let stopped = MonitorSnapshot::checked(lost.clone(), stopped).expect("stopped");
    let cancelled =
        monitor_policy::reattach(&stopped, &proof).expect("proved survivor must be cancelled");
    assert_eq!(cancelled.record["cancelRequested"], true);
    assert_eq!(cancelled.effects, vec![monitor_policy::Effect::Terminate]);
    let mut actor = case["actor"].clone();
    actor["epoch"] = json!(3);
    let stale = MonitorSnapshot::checked(lost, actor).expect("stale");
    assert!(monitor_policy::reattach(&stale, &proof).is_err());
    let mut wrong = proof;
    wrong.operation_id = "monitor:foreign".into();
    assert!(monitor_policy::reattach(&snapshot, &wrong).is_err());
}

/// 2026-10-20: every command that redirected its output (for example
/// `gh run watch ... > log 2>&1`) stayed "running" after it exited, because
/// its exit receipt needed a queue that only an output delta creates.
#[tokio::test]
async fn exit_of_a_command_without_output_is_queued_and_delivered() {
    use studio_runtime_services::monitor_event_queue;
    use studio_runtime_services::monitor_runtime_wiring::MonitorEvent;
    let root = fixtures::tempdir();
    let case = running(&fixtures::corpus(root.path()));
    assert!(case["before"].get("streamQueue").is_none());
    let store = fixtures::store(root.path(), "silent-exit").await;
    fixtures::seed(&store, &case).await;
    let id = case["before"]["id"].as_str().expect("id");
    let cursor = format!("provider-result:monitor:{id}");
    monitor_event_queue::enqueue_exit(
        &store,
        id,
        &operation(&case),
        &cursor,
        Some(0),
        None,
        2001.0,
    )
    .await
    .expect("exit receipt of a silent command is queued");
    let delivery = monitor_event_queue::next(&store)
        .await
        .expect("queue read")
        .expect("queued exit");
    assert_eq!(delivery.id, id);
    assert_eq!(delivery.cursor, cursor);
    assert!(matches!(
        delivery.event,
        MonitorEvent::Exit {
            code: Some(0),
            error: None,
            ..
        }
    ));
}

/// 2026-10-20: a cancelled command whose exit was late sent a stall notice
/// that woke its finished owner.
#[test]
fn a_cancelled_command_sends_no_stall_notice() {
    let root = fixtures::tempdir();
    let case = running(&fixtures::corpus(root.path()));
    let mut quiet = case["before"].clone();
    quiet["stallTimeoutSeconds"] = json!(60);
    quiet["activityGeneration"] = json!(1);
    quiet["stallWakeGeneration"] = json!(0);
    let stalled = |record: Value| {
        let snapshot =
            MonitorSnapshot::checked(record, case["actor"].clone()).expect("monitor snapshot");
        monitor_policy::tick(&snapshot, 5000.0)
            .expect("tick")
            .effects
            .iter()
            .any(|effect| matches!(effect, monitor_policy::Effect::StallEvent { .. }))
    };
    assert!(
        stalled(quiet.clone()),
        "a quiet command sends its stall notice"
    );
    quiet["cancelRequested"] = json!(true);
    assert!(
        !stalled(quiet),
        "a cancelled command waits only for its exit"
    );
}

/// A monitor record can name its log through a link to the state folder, as
/// a state folder under the macOS temporary folder does (`/var` is a link to
/// `/private/var`). Output delivery accepts that log as the same file; a log in
/// another folder stays refused.
#[cfg(unix)]
#[tokio::test]
async fn output_accepts_a_log_named_through_a_link_to_the_state_folder() {
    let root = fixtures::tempdir();
    let canonical = CanonicalPath::existing(root.path()).expect("canonical fixture root");
    let target = canonical.join("state");
    std::fs::create_dir(&target).expect("state folder");
    let link = canonical.join("linked-state");
    std::os::unix::fs::symlink(&target, &link).expect("link to the state folder");
    let store = RuntimeStore::open(Some(&link))
        .await
        .expect("store opens through the link");
    let cases = fixtures::corpus(root.path());
    let mut case = running(&cases);
    let id = case["before"]["id"]
        .as_str()
        .expect("monitor fixture")
        .to_owned();
    let operation = operation(&case);
    let output = MonitorOutput::default();

    case["before"]["log"] = json!(canonical.join("other").join(format!("{id}.log")));
    fixtures::seed(&store, &case).await;
    assert!(
        output
            .append(&store, &id, &operation, 0, b"marker", 2001.0)
            .await
            .is_err(),
        "a log in another folder is refused"
    );

    case["before"]["log"] =
        json!(studio_runtime_services::monitor_output::log_root(&link).join(format!("{id}.log")));
    fixtures::seed(&store, &case).await;
    output
        .append(&store, &id, &operation, 0, b"marker", 2001.0)
        .await
        .expect("the linked log names the state log");
    assert_eq!(
        std::fs::read(
            studio_runtime_services::monitor_output::log_root(&target).join(format!("{id}.log"))
        )
        .expect("monitor log"),
        b"marker"
    );
}
