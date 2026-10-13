#[path = "support/prepared_python.rs"]
mod python_test_support;
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    fs,
    path::Path,
    process::Command,
    sync::OnceLock,
    time::{Duration, Instant},
};
use studio_runtime_services::{
    recovery::*, recovery_absence::*, recovery_history::*, recovery_policy::*, recovery_runtime::*,
    recovery_store::*, task_recovery::*, task_recovery_store::*,
};
use studio_runtime_store::{
    AgentId, AgentRepository, RuntimeStore, TaskRecord, TaskRepository, Transaction,
};
fn scope() -> ConnectionScope {
    ConnectionScope {
        account: "default".into(),
        connection: "connection".into(),
        generation: 1,
    }
}
fn observation() -> RecoveryObservation {
    RecoveryObservation {
        scope: scope(),
        at: 1000.,
        supervisor: None,
        quiet: true,
        journal_drained: true,
        account_reserved: false,
        tools_refreshing: false,
        preparation_pending: false,
        completed_turn: None,
    }
}
fn fixture() -> tempfile::TempDir {
    static TEMPLATE: OnceLock<Vec<(std::ffi::OsString, Vec<u8>)>> = OnceLock::new();
    let files = TEMPLATE.get_or_init(|| {
        let root = tempfile::tempdir().expect("temporary state");
        let output = Command::new(python())
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/recovery/state.py"))
            .arg(root.path())
            .output()
            .expect("seed");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::read_dir(root.path())
            .expect("fixture files")
            .map(|entry| {
                let entry = entry.expect("file");
                (entry.file_name(), fs::read(entry.path()).expect("bytes"))
            })
            .collect()
    });
    let root = tempfile::tempdir().expect("temporary state");
    for (name, bytes) in files {
        fs::write(root.path().join(name), bytes).expect("clone fixture");
    }
    root
}
fn python() -> std::path::PathBuf {
    python_test_support::prepared_python(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
    )
}
fn corpus() -> &'static Value {
    static CORPUS: OnceLock<Value> = OnceLock::new();
    CORPUS.get_or_init(|| {
        let output = Command::new(python())
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wave7_p07_fixtures/oracle.py"))
            .output()
            .expect("oracle");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("oracle output")
    })
}
fn seed(store: &RuntimeStore, agent: &Value) {
    store
        .transaction(|tx| {
            tx.execute("DELETE FROM runtime_events", [])?;
            tx.execute(
                "UPDATE runtime_agents SET record=?1 WHERE id='agent'",
                [agent.to_string()],
            )?;
            Ok(())
        })
        .expect("seed agent");
}
fn agent(store: &RuntimeStore) -> Value {
    serde_json::from_str(
        &AgentRepository::get_raw(&store.read().expect("read"), &AgentId::from("agent"))
            .expect("agent")
            .expect("row"),
    )
    .expect("json")
}
struct RootDelivery {
    fail: bool,
}
impl RecoveryDelivery for RootDelivery {
    fn start_held(
        &mut self,
        _: &Transaction<'_>,
        _: &Value,
        _: &str,
        _: &str,
    ) -> Result<(), RecoveryError> {
        if self.fail {
            Err(RecoveryError::CommitFailed)
        } else {
            Ok(())
        }
    }
    fn terminal(
        &mut self,
        _: &Transaction<'_>,
        agent: &mut Value,
        _: &Value,
        _: CompletionMode,
    ) -> Result<(), RecoveryError> {
        assert!(agent["parentId"].is_null());
        if self.fail {
            Err(RecoveryError::CommitFailed)
        } else {
            Ok(())
        }
    }
}
// The canonical message owner is injected on integration; this double asserts
// routing and returns agent changes, never constructs a parallel transcript.
fn mock_message(
    _: &Transaction<'_>,
    agent: &mut Value,
    account: &str,
    generation: u64,
    turn: &Value,
    item: &Value,
    _: f64,
) -> Result<
    Option<studio_runtime_services::notification_dispatch::NotificationStreamBuffer>,
    studio_runtime_services::notification_dispatch::NotificationDispatchError,
> {
    assert_eq!(account, "default");
    assert_eq!(generation, 1);
    assert_eq!(agent["turnId"], turn["id"]);
    assert_eq!(item["type"], "agentMessage");
    agent["events"] = json!(agent["events"].as_u64().unwrap_or(0) + 1);
    agent["lastAnswer"] = item["text"].clone();
    Ok(None)
}
fn apply(
    store: &RuntimeStore,
    before: &Value,
    action: RecoveryAction,
    fail: bool,
) -> Result<RecoveryOutcome, RecoveryError> {
    let mut delivery = RootDelivery { fail };
    let mut completion = LifecycleCompletionWriter {
        at: 1000.,
        delivery: &mut delivery,
        write_message: &mut mock_message,
        stream_update: None,
    };
    RecoveryStore {
        store,
        connection_current: &|s| *s == scope(),
        completion: &mut completion,
        at: 1000.,
        observation_current: &|_| Some(observation()),
    }
    .apply(
        &RecoverySnapshot::from_record(before.clone()).expect("valid snapshot"),
        &scope(),
        action,
    )
}
fn terminal(case: &Value) -> RecoveryAction {
    RecoveryAction::Terminal {
        turn: case["turn"].clone(),
        native_state: "idle".into(),
        mode: CompletionMode::ManualConnection,
    }
}
fn records(store: &RuntimeStore, table: &str) -> Vec<Value> {
    store
        .read()
        .expect("read")
        .prepare(&format!("SELECT record FROM {table} ORDER BY id"))
        .expect("prepare")
        .query_map([], |r| r.get::<_, String>(0))
        .expect("query")
        .map(|r| serde_json::from_str(&r.expect("row")).expect("json"))
        .collect()
}
#[test]
fn python_terminal_notification_count_decodes_without_changing_attempt_events() {
    let record = corpus()["notification_counter"].clone();
    let snapshot = RecoverySnapshot::from_record(record.clone()).expect("Python terminal record");
    assert_eq!(snapshot.record(), &record);
    assert_eq!(record["events"], 8);
    assert_eq!(record["startAttempt"]["events"], json!(["event"]));
    for value in [json!([]), json!(-1), json!(true), Value::Null] {
        let mut invalid = record.clone();
        invalid["events"] = value;
        assert!(RecoverySnapshot::from_record(invalid).is_err());
    }
    let mut invalid = record;
    invalid["startAttempt"]["events"] = json!(8);
    assert!(RecoverySnapshot::from_record(invalid).is_err());
}
#[test]
fn connection_completion_matches_python_full_records_and_results() {
    let cases = corpus()["connection"].as_array().expect("cases");
    assert_eq!(cases.len(), 27);
    for case in cases {
        let root = fixture();
        let store = RuntimeStore::open(Some(root.path())).expect("store");
        seed(&store, &case["before"]);
        let result = apply(&store, &case["before"], terminal(case), false).expect("complete");
        assert_eq!(
            result,
            RecoveryOutcome::Applied(case["result"].clone()),
            "{}",
            case["name"]
        );
        assert_eq!(agent(&store), case["after"], "{}", case["name"]);
        assert_eq!(
            json!(records(&store, "runtime_items")),
            case["items"],
            "{}",
            case["name"]
        );
        let read = store.read().expect("read");
        let full: Vec<(String, String)> = read
            .prepare("SELECT id,body FROM runtime_item_fulltext ORDER BY id")
            .expect("prepare")
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(json!(full), case["fulltext"]);
        let completed: Vec<String> = read
            .prepare("SELECT id FROM runtime_completed_turns")
            .expect("prepare")
            .query_map([], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(json!(completed), case["completed"]);
        drop(read);
        assert_eq!(
            apply(&store, &case["before"], terminal(case), false).expect("replay"),
            RecoveryOutcome::Superseded
        );
    }
}
#[test]
fn unresolved_operations_match_python_including_legacy_scope() {
    let cases = corpus()["unresolved"].as_array().expect("cases");
    assert_eq!(cases.len(), 36);
    for case in cases {
        let root = fixture();
        let store = RuntimeStore::open(Some(root.path())).expect("store");
        let before = &corpus()["connection"][0]["before"];
        seed(&store, before);
        store
            .transaction(|tx| {
                tx.execute(
                    &format!(
                        "INSERT INTO runtime_{}(id,record) VALUES('operation',?1)",
                        case["table"].as_str().expect("table")
                    ),
                    [case["record"].to_string()],
                )?;
                Ok(())
            })
            .expect("operation");
        let actual = store
            .transaction_with::<_, RecoveryError>(|tx| unresolved_turn_receipts(tx, before, "turn"))
            .expect("decision");
        assert_eq!(json!(actual), case["expected"], "{case}");
    }
}
#[test]
fn completion_failpoints_roll_back_all_rows_and_restart_is_replay_safe() {
    let case = &corpus()["connection"][0];
    for (table, op) in [
        ("runtime_completed_turns", "INSERT"),
        ("runtime_items", "INSERT"),
        ("runtime_agents", "UPDATE"),
        ("sync_entities", "INSERT"),
    ] {
        for timing in ["BEFORE", "AFTER"] {
            let root = fixture();
            let store = RuntimeStore::open(Some(root.path())).expect("store");
            seed(&store, &case["before"]);
            let before = agent(&store);
            store.transaction(|tx| {tx.execute_batch(&format!("CREATE TRIGGER p07_fail {timing} {op} ON {table} BEGIN SELECT RAISE(ABORT,'p07 injected crash'); END;"))?;Ok(())}).expect("failpoint");
            assert!(
                apply(&store, &before, terminal(case), false).is_err(),
                "{timing} {table}"
            );
            drop(store);
            let store = RuntimeStore::open(Some(root.path())).expect("restart");
            assert_eq!(agent(&store), before);
            assert!(records(&store, "runtime_items").is_empty());
            assert_eq!(
                store
                    .read()
                    .expect("read")
                    .query_row("SELECT count(*) FROM runtime_completed_turns", [], |r| r
                        .get::<_, i64>(0))
                    .expect("count"),
                0
            );
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER p07_fail")?;
                    Ok(())
                })
                .expect("remove fault");
            apply(&store, &before, terminal(case), false).expect("recover");
            assert_eq!(agent(&store), case["after"]);
        }
    }
    let root = fixture();
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    seed(&store, &case["before"]);
    assert!(apply(&store, &case["before"], terminal(case), true).is_err());
    assert_eq!(agent(&store), case["before"]);
}
#[test]
fn stop_or_notification_before_response_supersedes_completion() {
    for change in ["epoch", "turnId", "inFlight", "error"] {
        let case = &corpus()["connection"][0];
        let root = fixture();
        let store = RuntimeStore::open(Some(root.path())).expect("store");
        let mut newer = case["before"].clone();
        newer[change] = match change {
            "epoch" => json!(1),
            "inFlight" => json!(true),
            _ => json!("changed"),
        };
        seed(&store, &newer);
        assert_eq!(
            apply(&store, &case["before"], terminal(case), false).expect("stale"),
            RecoveryOutcome::Superseded
        );
        assert_eq!(agent(&store), newer);
        assert!(records(&store, "runtime_items").is_empty());
    }
}
struct Port<'a> {
    calls: Vec<(HistoryMethod, Instant)>,
    stop: Option<&'a Cell<bool>>,
    reply_scope: ConnectionScope,
    drained: Option<&'a Cell<bool>>,
}
impl RetainedHistory for Port<'_> {
    fn request(
        &mut self,
        _: &ConnectionScope,
        method: HistoryMethod,
        _: Value,
        deadline: Instant,
    ) -> Result<HistoryReceipt, HistoryError> {
        self.calls.push((method, deadline));
        if let Some(stop) = self.stop {
            stop.set(true);
        }
        Ok(HistoryReceipt {
            scope: self.reply_scope.clone(),
            body: json!({"data":[]}),
        })
    }
    fn barrier(&mut self, _: &ConnectionScope, _: Instant) -> Result<(), HistoryError> {
        if let Some(drained) = self.drained {
            drained.set(true);
        }
        Ok(())
    }
}
#[test]
fn retained_reads_enforce_deadline_thread_generation_and_stop() {
    for scenario in ["ok", "expired", "thread", "generation", "stopped"] {
        let stopped = Cell::new(false);
        let observe = |_: &ConnectionScope| (!stopped.get()).then(observation);
        let mut port = Port {
            calls: vec![],
            stop: (scenario == "stopped").then_some(&stopped),
            reply_scope: scope(),
            drained: None,
        };
        if scenario == "generation" {
            port.reply_scope.generation += 1;
        }
        let deadline = if scenario == "expired" {
            Instant::now()
        } else {
            Instant::now() + Duration::from_secs(20)
        };
        let mut reader = RetainedRecoveryReader::new(scope(), "thread".into(), &mut port, &observe)
            .expect("reader");
        let result = HistoryRead::new(&mut reader, scope(), deadline).call(
            HistoryMethod::Turns,
            json!({"threadId":if scenario=="thread"{"foreign"}else{"thread"}}),
        );
        assert_eq!(result.is_ok(), scenario == "ok", "{scenario}");
        drop(reader);
        assert_eq!(
            port.calls.len(),
            usize::from(!["expired", "thread"].contains(&scenario))
        );
        if let Some((_, seen)) = port.calls.first() {
            assert_eq!(*seen, deadline);
        }
    }
}
#[test]
fn barrier_requires_committed_journal_and_preserves_absolute_deadline() {
    let drained = Cell::new(false);
    let observe = |_: &ConnectionScope| {
        let mut o = observation();
        o.journal_drained = drained.get();
        Some(o)
    };
    let mut port = Port {
        calls: vec![],
        stop: None,
        reply_scope: scope(),
        drained: None,
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut reader =
        RetainedRecoveryReader::new(scope(), "thread".into(), &mut port, &observe).expect("reader");
    assert_eq!(
        reader.barrier(&scope(), deadline),
        Err(HistoryError::Unavailable)
    );
    drop(reader);
    port.drained = Some(&drained);
    let mut reader =
        RetainedRecoveryReader::new(scope(), "thread".into(), &mut port, &observe).expect("reader");
    reader.barrier(&scope(), deadline).expect("drained");
}
#[test]
fn scan_is_bounded_serial_and_uses_backoff_without_sleeping() {
    let job = RecoveryJob::Connection {
        snapshot: RecoverySnapshot::from_record(corpus()["connection"][0]["before"].clone())
            .expect("snapshot"),
        automatic: true,
    };
    let mut scan = RecoveryScan::default();
    let now = Instant::now();
    assert!(scan.take([job.clone()], now, 1000.).is_some());
    assert!(scan.take([job.clone()], now, 1000.).is_none());
    scan.finish(&job, now, &Ok(RecoveryOutcome::Unconfirmed))
        .expect("finish");
    assert_eq!(scan.next_at(now), Some(now + Duration::from_secs(15)));
    assert!(scan.take([job.clone()], now, 1000.).is_none());
    assert!(
        scan.take([job.clone()], now + Duration::from_secs(15), 1015.)
            .is_some()
    );
    scan.finish(
        &job,
        now + Duration::from_secs(15),
        &Err(RecoveryError::CommitFailed),
    )
    .expect("finish");
    assert_eq!(scan.next_at(now), Some(now + Duration::from_secs(45)));
}
#[test]
fn blob_preview_is_never_mistaken_for_complete_receipt() {
    assert!(matches!(
        resolve_inline_receipt(
            &json!({"_payloadBlobs":{"result":{"sha256":"hash"}},"result":{"success":true}})
        ),
        Err(RecoveryError::Dependency(
            "payload-store/tool-request-recovery"
        ))
    ));
}

#[test]
fn unknown_start_accepts_exact_client_receipt_without_resending_input() {
    let root = fixture();
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    let before = json!({"id":"agent","rootId":"agent","epoch":0,"status":"running","accountKey":"default","threadId":"thread","turnEpoch":0,"turnId":null,"inFlight":true,"autoWake":true,"error":"turn/start response timed out; outcome unknown","startAttempt":{"id":"attempt","epoch":0,"accountKey":"default","threadId":"thread","connectionId":"connection","connectionGeneration":1,"requestTicket":"request","events":["event"],"created":1.0,"submitted":true,"executionOutcome":"unknown"}});
    seed(&store, &before);
    store.transaction(|tx|{tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) VALUES('event','agent','user','one original input','uncertain',1,0,NULL,NULL)",[])?;Ok(())}).expect("unknown input");
    let turn = json!({"id":"turn","status":"inProgress","items":[{"id":"input","type":"userMessage","clientId":"event"}]});
    let result = apply(
        &store,
        &before,
        RecoveryAction::AcceptStart { turn: turn.clone() },
        false,
    )
    .expect("accept");
    assert_eq!(
        result,
        RecoveryOutcome::Applied(json!({"status":"accepted","turnId":"turn"}))
    );
    assert_eq!(agent(&store)["turnId"], "turn");
    assert_eq!(
        store
            .read()
            .expect("read")
            .query_row(
                "SELECT status FROM runtime_events WHERE id='event'",
                [],
                |r| r.get::<_, String>(0)
            )
            .expect("status"),
        "delivered"
    );
    assert_eq!(
        apply(&store, &before, RecoveryAction::AcceptStart { turn }, false).expect("replay"),
        RecoveryOutcome::Superseded
    );
    let accepted = agent(&store);
    let finished=apply(&store,&accepted,RecoveryAction::Terminal{turn:json!({"id":"turn","status":"completed","items":[{"id":"answer","type":"agentMessage","text":"recovered"}]}),native_state:"idle".into(),mode:CompletionMode::ObservedTurn},false).expect("finish");
    assert!(matches!(finished, RecoveryOutcome::Applied(_)));
    assert_eq!(agent(&store)["inFlight"], false);
    assert_eq!(agent(&store)["lastAnswer"], "recovered");
    assert_eq!(
        store
            .read()
            .expect("read")
            .query_row("SELECT count(*) FROM runtime_events", [], |r| r
                .get::<_, i64>(0))
            .expect("count"),
        1
    );
}

#[test]
fn task_writer_checks_receipts_and_scope_in_the_completion_transaction() {
    for scenario in [
        "complete",
        "stopped",
        "changed-task",
        "changed-epoch",
        "writer-failure",
    ] {
        let root = fixture();
        let store = RuntimeStore::open(Some(root.path())).expect("store");
        let before = json!({"id":"agent","rootId":"agent","epoch":0,"status":"completed","accountKey":"default","provider":"codex","threadId":"thread","turnId":null,"inFlight":false});
        seed(&store, &before);
        let task = json!({"id":"agent:item","agent":"agent","itemId":"item","turnId":"turn","type":"commandExecution","kind":"command","status":"running","created":1.0});
        store
            .transaction(|tx| {
                TaskRepository::put(tx, &TaskRecord::from_value(task.clone()).expect("task"))
            })
            .expect("save");
        let snapshot =
            TaskRecoverySnapshot::from_records(before.clone(), task.clone(), vec![], vec![])
                .expect("snapshot");
        if scenario == "changed-task" {
            store
                .transaction(|tx| {
                    tx.execute(
                        "UPDATE runtime_tasks SET record=json_set(record,'$.processId','new')",
                        [],
                    )?;
                    Ok(())
                })
                .expect("race");
        }
        if scenario == "changed-epoch" {
            // Revoked epoch is authoritative even for a terminal native item.
            let mut changed = before.clone();
            changed["epoch"] = json!(1);
            seed(&store, &changed);
        }
        let calls = Cell::new(0);
        let observe = |_: &ConnectionScope| (scenario != "stopped").then(observation);
        let write = |tx: &Transaction<'_>, _: &Value, previous: &Value, item: &Value, at: f64| {
            calls.set(calls.get() + 1);
            let mut next = previous.clone();
            next["status"] = item["status"].clone();
            next["finished"] = json!(at);
            TaskRepository::put(tx, &TaskRecord::from_value(next).expect("task"))?;
            if scenario == "writer-failure" {
                return Err(studio_runtime_services::notification_dispatch::NotificationDispatchError::InvalidNotification);
            }
            Ok(())
        };
        let mut writer = RuntimeTaskCompletionWriter {
            store: &store,
            observation_current: &observe,
            write_task: &write,
            resolve_payload: &resolve_inline_receipt,
        };
        let result = writer.complete(
            &snapshot,
            &scope(),
            json!({"id":"item","type":"commandExecution","status":"completed","exitCode":0}),
        );
        if scenario == "writer-failure" {
            assert!(result.is_err());
        } else {
            assert_eq!(result.expect("complete"), scenario == "complete");
        }
        assert_eq!(
            calls.get(),
            usize::from(["complete", "writer-failure"].contains(&scenario))
        );
        assert_eq!(
            records(&store, "runtime_tasks")[0]["status"],
            if scenario == "complete" {
                "completed"
            } else {
                "running"
            }
        );
    }
}

#[test]
fn runtime_entry_applies_history_only_after_barrier_and_rechecks_notification_race() {
    struct History<'a> {
        store: &'a RuntimeStore,
        turn: Value,
        race: bool,
        reads: RefCell<Vec<HistoryMethod>>,
    }
    impl RetainedHistory for History<'_> {
        fn request(
            &mut self,
            s: &ConnectionScope,
            m: HistoryMethod,
            _: Value,
            _: Instant,
        ) -> Result<HistoryReceipt, HistoryError> {
            self.reads.borrow_mut().push(m);
            let body = match m {
                HistoryMethod::Thread => json!({"thread":{"id":"thread","status":{"type":"idle"}}}),
                HistoryMethod::Turns => json!({"data":[self.turn]}),
                _ => return Err(HistoryError::Unsupported),
            };
            Ok(HistoryReceipt {
                scope: s.clone(),
                body,
            })
        }
        fn barrier(&mut self, _: &ConnectionScope, _: Instant) -> Result<(), HistoryError> {
            if self.race {
                self.store.transaction(|tx|{tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.epoch',1,'$.autoWake',json('false'))",[])?;Ok(())}).expect("stop before response");
            }
            Ok(())
        }
    }
    for race in [false, true] {
        let case = &corpus()["connection"][0];
        let root = fixture();
        let store = RuntimeStore::open(Some(root.path())).expect("store");
        seed(&store, &case["before"]);
        let mut history = History {
            store: &store,
            turn: case["turn"].clone(),
            race,
            reads: RefCell::new(vec![]),
        };
        let observe = |_: &ConnectionScope| Some(observation());
        let mut reader =
            RetainedRecoveryReader::new(scope(), "thread".into(), &mut history, &observe)
                .expect("reader");
        let mut delivery = RootDelivery { fail: false };
        let mut completion = LifecycleCompletionWriter {
            at: 1000.,
            delivery: &mut delivery,
            write_message: &mut mock_message,
            stream_update: None,
        };
        let task_write = |_: &Transaction<'_>, _: &Value, _: &Value, _: &Value, _: f64| {
            Err(studio_runtime_services::notification_dispatch::NotificationDispatchError::InvalidNotification)
        };
        let mut task_completion = RuntimeTaskCompletionWriter {
            store: &store,
            observation_current: &observe,
            write_task: &task_write,
            resolve_payload: &resolve_inline_receipt,
        };
        let job = RecoveryJob::Connection {
            snapshot: RecoverySnapshot::from_record(case["before"].clone()).expect("snapshot"),
            automatic: false,
        };
        let outcome = RecoveryRuntime {
            store: &store,
            observation: &observe,
            completion: &mut completion,
            task_completion: &mut task_completion,
        }
        .reconcile(&job, scope(), &mut reader)
        .expect("reconcile");
        assert_eq!(outcome == RecoveryOutcome::Superseded, race);
        assert_eq!(records(&store, "runtime_items").is_empty(), race);
    }
}

#[test]
fn stored_scan_pages_alternate_without_starving_later_agents_or_foreign_scope() {
    let root = fixture();
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    let template = &corpus()["connection"][0]["before"];
    store
        .transaction(|tx| {
            tx.execute("DELETE FROM runtime_agents", [])?;
            for n in 0..35 {
                let id = format!("agent-{n:02}");
                let mut row = template.clone();
                row["id"] = json!(id);
                row["rootId"] = json!(id);
                if n == 34 {
                    row["accountKey"] = json!("foreign");
                }
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                    rusqlite::params![id, row.to_string()],
                )?;
            }
            Ok(())
        })
        .expect("seed page");
    let mut cursor = RecoveryCursor::default();
    let mut observed = Vec::new();
    for _ in 0..34 {
        let jobs = load_recovery_jobs(&store, "default", &mut cursor).expect("agent candidate");
        assert_eq!(jobs.len(), 1);
        let RecoveryJob::Connection { snapshot, .. } = &jobs[0] else {
            panic!("connection");
        };
        observed.push(snapshot.record()["id"].as_str().expect("id").to_owned());
        assert!(
            load_recovery_jobs(&store, "default", &mut cursor)
                .expect("task page")
                .is_empty()
        );
    }
    assert_eq!(
        observed,
        (0..34).map(|n| format!("agent-{n:02}")).collect::<Vec<_>>()
    );
    assert!(
        load_recovery_jobs(&store, "default", &mut cursor)
            .expect("exhausted")
            .is_empty()
    );
    assert!(
        load_recovery_jobs(&store, "default", &mut cursor)
            .expect("task page")
            .is_empty()
    );
    assert_eq!(
        load_recovery_jobs(&store, "default", &mut cursor)
            .expect("wrap")
            .len(),
        1
    );
}

#[test]
fn diagnostic_updates_do_not_reset_connection_backoff() {
    let mut before = corpus()["connection"][0]["before"].clone();
    let job = RecoveryJob::Connection {
        snapshot: RecoverySnapshot::from_record(before.clone()).expect("snapshot"),
        automatic: true,
    };
    let now = Instant::now();
    let mut scan = RecoveryScan::default();
    assert!(scan.take([job.clone()], now, 1000.).is_some());
    scan.finish(&job, now, &Ok(RecoveryOutcome::Unconfirmed))
        .expect("finish");
    before["connectionCheck"] = json!({"at":1001.,"nativeState":"active"});
    let checked = RecoveryJob::Connection {
        snapshot: RecoverySnapshot::from_record(before).expect("snapshot"),
        automatic: true,
    };
    assert!(scan.take([checked], now, 1001.).is_none());
}

#[test]
fn accepted_start_failpoints_never_deliver_input_without_its_attempt_receipt() {
    let before = json!({"id":"agent","rootId":"agent","epoch":0,"status":"running","accountKey":"default","threadId":"thread","turnEpoch":0,"turnId":null,"inFlight":true,"autoWake":true,"error":"turn/start response timed out; outcome unknown","startAttempt":{"id":"attempt","epoch":0,"accountKey":"default","threadId":"thread","connectionId":"connection","connectionGeneration":1,"requestTicket":"request","events":["event"],"created":1.0,"submitted":true,"executionOutcome":"unknown"}});
    let turn = json!({"id":"turn","status":"inProgress","items":[{"id":"input","type":"userMessage","clientId":"event"}]});
    for (table, operation) in [
        ("runtime_events", "UPDATE"),
        ("runtime_agents", "UPDATE"),
        ("runtime_execution_attempts", "UPDATE"),
        ("sync_entities", "INSERT"),
    ] {
        for timing in ["BEFORE", "AFTER"] {
            let root = fixture();
            let store = RuntimeStore::open(Some(root.path())).expect("store");
            seed(&store, &before);
            store.transaction(|tx|{
                tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) VALUES('event','agent','user','original input','uncertain',1,0,NULL,NULL)",[])?;
                tx.execute_batch(&format!("CREATE TRIGGER p07_accept_fail {timing} {operation} ON {table} BEGIN SELECT RAISE(ABORT,'p07 crash'); END;"))?;
                Ok(())
            }).expect("fault");
            assert!(
                apply(
                    &store,
                    &before,
                    RecoveryAction::AcceptStart { turn: turn.clone() },
                    false
                )
                .is_err(),
                "{timing} {table}"
            );
            drop(store);
            let store = RuntimeStore::open(Some(root.path())).expect("restart");
            assert_eq!(agent(&store), before);
            assert_eq!(
                store
                    .read()
                    .expect("read")
                    .query_row(
                        "SELECT status FROM runtime_events WHERE id='event'",
                        [],
                        |r| r.get::<_, String>(0)
                    )
                    .expect("status"),
                "uncertain"
            );
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER p07_accept_fail")?;
                    Ok(())
                })
                .expect("remove fault");
            apply(
                &store,
                &before,
                RecoveryAction::AcceptStart { turn: turn.clone() },
                false,
            )
            .expect("recover");
            assert_eq!(agent(&store)["turnId"], "turn");
        }
    }
}

#[test]
fn task_completion_rejects_changed_alias_or_operation_receipt() {
    for change in ["alias", "receipt"] {
        let root = fixture();
        let store = RuntimeStore::open(Some(root.path())).expect("store");
        let before = json!({"id":"agent","rootId":"agent","epoch":0,"status":"completed","accountKey":"default","provider":"codex","threadId":"thread","turnId":null,"inFlight":false});
        seed(&store, &before);
        let task = json!({"id":"agent:item","agent":"agent","itemId":"item","turnId":"turn","type":"dynamicToolCall","kind":"tool","status":"running","created":1.});
        let receipt = json!({"id":"receipt","agent":"agent","accountKey":"default","threadId":"thread","turnId":"turn","callId":"item","finished":2.,"stage":"completed","outcome":"applied","result":{"success":true,"contentItems":[]}});
        store.transaction(|tx|{
            TaskRepository::put(tx,&TaskRecord::from_value(task.clone()).expect("task"))?;
            tx.execute("INSERT INTO runtime_tool_request_aliases(agent,alias,request) VALUES('agent','item','receipt')",[])?;
            tx.execute("INSERT INTO runtime_tool_requests(id,record) VALUES('receipt',?1)",[receipt.to_string()])?;
            Ok(())
        }).expect("seed");
        let snapshot = TaskRecoverySnapshot::from_records(
            before,
            task,
            vec!["receipt".into()],
            vec![("receipt".into(), receipt)],
        )
        .expect("snapshot");
        store.transaction(|tx|{
            if change=="alias" {tx.execute("UPDATE runtime_tool_request_aliases SET request='changed'",[])?;}
            else {tx.execute("UPDATE runtime_tool_requests SET record=json_set(record,'$.result.success',json('false'))",[])?;}
            Ok(())
        }).expect("race");
        let write = |_: &Transaction<'_>,
                     _: &Value,
                     _: &Value,
                     _: &Value,
                     _: f64|
         -> Result<
            (),
            studio_runtime_services::notification_dispatch::NotificationDispatchError,
        > { panic!("changed receipt must not reach writer") };
        let observe = |_: &ConnectionScope| Some(observation());
        let mut writer = RuntimeTaskCompletionWriter {
            store: &store,
            observation_current: &observe,
            write_task: &write,
            resolve_payload: &resolve_inline_receipt,
        };
        assert!(!writer.complete(&snapshot,&scope(),json!({"id":"item","type":"dynamicToolCall","status":"completed","success":true})).expect("recheck"));
        assert_eq!(records(&store, "runtime_tasks")[0]["status"], "running");
    }
}

#[test]
fn held_scan_survives_257_full_pages_and_check_eviction() {
    let root = fixture();
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    let template = &corpus()["connection"][0]["before"];
    const ROWS: usize = 257 * MAX_SCAN_JOBS;
    store
        .transaction(|tx| {
            tx.execute("DELETE FROM runtime_agents", [])?;
            for n in 0..ROWS {
                let id = format!("agent-{n:05}");
                let mut row = template.clone();
                row["id"] = json!(id);
                row["rootId"] = json!(id);
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                    rusqlite::params![id, row.to_string()],
                )?;
            }
            Ok(())
        })
        .expect("held rows");
    let mut cursor = RecoveryCursor::default();
    let mut scan = RecoveryScan::default();
    let now = Instant::now();
    for n in 0..ROWS {
        let jobs = load_recovery_jobs(&store, "default", &mut cursor).expect("scan");
        let job = scan
            .take(jobs, now, 1000.)
            .expect("each held candidate once");
        let RecoveryJob::Connection { snapshot, .. } = &job else {
            panic!("connection");
        };
        assert_eq!(snapshot.record()["id"], format!("agent-{n:05}"));
        scan.finish(&job, now, &Ok(RecoveryOutcome::Unconfirmed))
            .expect("hold");
        assert!(
            load_recovery_jobs(&store, "default", &mut cursor)
                .expect("tasks")
                .is_empty()
        );
    }
}

#[test]
fn corrupt_scan_rows_keep_the_sql_table_and_identity() {
    for bad in ["agent", "task", "receipt"] {
        let root = fixture();
        let store = RuntimeStore::open(Some(root.path())).expect("store");
        let mut owner = corpus()["connection"][0]["before"].clone();
        owner["events"] = if bad == "agent" { json!(-1) } else { json!(0) };
        seed(&store, &owner);
        let mut task = json!({"id":"agent:item","agent":"agent","itemId":"item","turnId":"turn","type":"dynamicToolCall","kind":"tool","status":"running","created":1.0});
        if bad == "task" {
            task["created"] = json!(-1);
        }
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_tasks(id,record) VALUES(?1,?2)",
                    rusqlite::params!["agent:item", task.to_string()],
                )?;
                if bad == "receipt" {
                    tx.execute(
                        "INSERT INTO runtime_tool_requests(id,record) VALUES('thread:item',?1)",
                        [json!({"id":"thread:item","agent":"agent","callId":8}).to_string()],
                    )?;
                }
                Ok(())
            })
            .expect("corrupt record");
        let mut cursor = RecoveryCursor::default();
        let result = load_recovery_jobs(&store, "default", &mut cursor);
        let error = if bad == "agent" {
            result.err().expect("agent rejected")
        } else {
            result.expect("agent valid");
            load_recovery_jobs(&store, "default", &mut cursor)
                .err()
                .expect("task/receipt rejected")
        }
        .to_string();
        let (table, id) = match bad {
            "agent" => ("runtime_agents", "agent"),
            "task" => ("runtime_tasks", "agent:item"),
            _ => ("runtime_tool_requests", "thread:item"),
        };
        assert!(
            error.contains(table) && error.contains(id),
            "{bad}: {error}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn blocked_retained_barrier_ends_at_the_absolute_deadline() {
    struct Blocked {
        runtime: tokio::runtime::Handle,
        entered: Option<tokio::sync::oneshot::Sender<Instant>>,
    }
    impl RetainedHistory for Blocked {
        fn request(
            &mut self,
            _: &ConnectionScope,
            _: HistoryMethod,
            _: Value,
            _: Instant,
        ) -> Result<HistoryReceipt, HistoryError> {
            Err(HistoryError::Unsupported)
        }
        fn barrier(&mut self, _: &ConnectionScope, deadline: Instant) -> Result<(), HistoryError> {
            self.runtime.block_on(async {
                let timer = tokio::time::sleep_until(deadline.into());
                tokio::pin!(timer);
                // Register the deadline before the test advances paused time.
                assert!(futures_util::poll!(&mut timer).is_pending());
                self.entered
                    .take()
                    .expect("one barrier")
                    .send(deadline)
                    .expect("test receiver");
                timer.await;
                Err(HistoryError::Deadline)
            })
        }
    }
    let (sender, entered) = tokio::sync::oneshot::channel();
    let runtime = tokio::runtime::Handle::current();
    let deadline = tokio::time::Instant::now().into_std() + Duration::from_secs(20);
    let waiter = tokio::task::spawn_blocking(move || {
        let mut port = Blocked {
            runtime,
            entered: Some(sender),
        };
        let observe = |_: &ConnectionScope| Some(observation());
        RetainedRecoveryReader::new(scope(), "thread".into(), &mut port, &observe)
            .expect("reader")
            .barrier(&scope(), deadline)
    });
    assert_eq!(entered.await.expect("waiting"), deadline);
    tokio::time::advance(Duration::from_secs(19)).await;
    assert!(!waiter.is_finished());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(
        waiter.await.expect("barrier task"),
        Err(HistoryError::Deadline)
    );
}

#[test]
fn accepted_start_matches_real_python_agent_event_and_transcript_rows() {
    let case = &corpus()["accepted_start"];
    let root = fixture();
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    seed(&store, &case["before"]);
    store.transaction(|tx| {
        tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) VALUES('event','agent','user','original input','uncertain',1,0,NULL,NULL)",[])?;
        Ok(())
    }).expect("input");
    apply(
        &store,
        &case["before"],
        RecoveryAction::AcceptStart {
            turn: case["turn"].clone(),
        },
        false,
    )
    .expect("accept");
    assert_eq!(agent(&store), case["after"]);
    let rows: Vec<Value> = store.read().expect("read").prepare("SELECT id,agent,kind,text,status,created,epoch,turn_id,error FROM runtime_events ORDER BY id").expect("query")
        .query_map([], |r| Ok(json!({"id":r.get::<_,String>(0)?,"agent":r.get::<_,String>(1)?,"kind":r.get::<_,String>(2)?,"text":r.get::<_,String>(3)?,"status":r.get::<_,String>(4)?,"created":r.get::<_,f64>(5)?,"epoch":r.get::<_,i64>(6)?,"turn_id":r.get::<_,Option<String>>(7)?,"error":r.get::<_,Option<String>>(8)?}))).expect("rows").collect::<Result<_,_>>().expect("events");
    assert_eq!(json!(rows), case["events"]);
    assert_eq!(json!(records(&store, "runtime_items")), case["items"]);
}

#[test]
fn accepted_terminal_start_calls_canonical_messages_and_finishes_atomically() {
    for native_status in ["completed", "failed", "interrupted"] {
        for fail in [false, true] {
            let root = fixture();
            let store = RuntimeStore::open(Some(root.path())).expect("store");
            let before = &corpus()["accepted_start"]["before"];
            seed(&store, before);
            store.transaction(|tx| {
            tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) VALUES('event','agent','user','original input','uncertain',1,0,NULL,NULL)",[])?;
            Ok(())
        }).expect("input");
            let messages = json!([
                {"id":"buffered","type":"agentMessage","text":""},
                {"id":"question","type":"agentMessage","text":"Choose?","questions":[{"title":"Which?","options":["one","two"]}]}
            ]);
            let turn = json!({"id":"turn","status":native_status,"items":[
                {"id":"input","type":"userMessage","clientId":"event"},messages[0],messages[1]
            ]});
            let mut called = Vec::new();
            let mut message_writer = |_: &Transaction<'_>,
                                      a: &mut Value,
                                      account: &str,
                                      generation: u64,
                                      t: &Value,
                                      item: &Value,
                                      at: f64| {
                assert_eq!((account, generation, at), ("default", 1, 1000.));
                assert_eq!(a["turnId"], "turn");
                assert_eq!(t, &turn);
                called.push(item.clone());
                a["events"] = json!(a["events"].as_u64().expect("counter") + 1);
                if fail {
                    return Err(studio_runtime_services::notification_dispatch::NotificationDispatchError::InvalidNotification);
                }
                Ok(Some(
                studio_runtime_services::notification_dispatch::NotificationStreamBuffer::default(),
            ))
            };
            let mut delivery = RootDelivery { fail: false };
            let mut completion = LifecycleCompletionWriter {
                at: 1000.,
                delivery: &mut delivery,
                write_message: &mut message_writer,
                stream_update: None,
            };
            let outcome = RecoveryStore {
                store: &store,
                connection_current: &|_| true,
                completion: &mut completion,
                at: 1000.,
                observation_current: &|_| Some(observation()),
            }
            .apply(
                &RecoverySnapshot::from_record(before.clone()).expect("snapshot"),
                &scope(),
                RecoveryAction::AcceptStart { turn: turn.clone() },
            );
            assert_eq!(outcome.is_err(), fail);
            if fail {
                assert_eq!(agent(&store), *before);
                assert_eq!(
                    store
                        .read()
                        .expect("read")
                        .query_row("SELECT status FROM runtime_events", [], |r| r
                            .get::<_, String>(0))
                        .expect("event"),
                    "uncertain"
                );
            } else {
                assert!(completion.stream_update.is_some());
                assert_eq!(
                    outcome.expect("settled"),
                    RecoveryOutcome::Applied(
                        json!({"status":"reconciled","turnId":"turn","outcome":native_status})
                    )
                );
                assert_eq!(agent(&store)["inFlight"], false);
                assert_eq!(agent(&store)["events"], 3);
                assert_eq!(json!(called), messages);
            }
        }
    }
}
