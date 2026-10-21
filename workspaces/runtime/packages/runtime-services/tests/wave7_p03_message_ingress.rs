//! Message ingress at its transaction boundary, using Python-written databases.
#[path = "support/prepared_python.rs"]
mod python_test_support;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use studio_runtime_services::coordination::{self, PostManagedMessage};
use studio_runtime_services::message_admission::PreparedAdmission;
use studio_runtime_store::RuntimeStore;

struct Oracle {
    child: Child,
    input: ChildStdin,
    output: Receiver<Result<String, String>>,
    reader: Option<std::thread::JoinHandle<()>>,
    root: tempfile::TempDir,
    next_root: usize,
    case_name: String,
}
impl Oracle {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("isolated root");
        let base = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut child = Command::new(python_test_support::prepared_python(
            &base.join("../../apps/server/src"),
        ))
        .arg("-B")
        .arg(base.join("tests/wave7_p03_message_ingress/oracle.py"))
        .arg(root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("Python oracle");
        let stdout = child.stdout.take().expect("stdout");
        let (sender, output) = mpsc::channel();
        let reader = Some(std::thread::spawn(move || {
            read_oracle_lines(stdout, sender)
        }));
        let case_name = std::thread::current()
            .name()
            .unwrap_or("Python message-ingress test")
            .to_owned();
        Self {
            input: child.stdin.take().expect("stdin"),
            output,
            reader,
            child,
            root,
            next_root: 0,
            case_name,
        }
    }
    fn call(&mut self, command: Value) -> Value {
        if let Some(name) = command.pointer("/case/name").and_then(Value::as_str) {
            self.case_name = name.to_owned();
        }
        writeln!(self.input, "{command}")
            .and_then(|()| self.input.flush())
            .unwrap_or_else(|error| {
                panic!("{}: send Python oracle command: {error}", self.case_name)
            });
        let line = match self
            .output
            .recv_timeout(python_test_support::PYTHON_ORACLE_TIMEOUT)
        {
            Ok(Ok(line)) => line,
            Ok(Err(error)) => panic!("{}: read Python oracle reply: {error}", self.case_name),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.terminate("reply timed out")
                    .unwrap_or_else(|error| panic!("{}: {error}", self.case_name));
                panic!(
                    "{}: Python oracle exceeded {} second reply timeout",
                    self.case_name,
                    python_test_support::PYTHON_ORACLE_TIMEOUT.as_secs()
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("{}: Python oracle closed its reply stream", self.case_name)
            }
        };
        let result: Value = serde_json::from_str(&line).unwrap_or_else(|error| {
            panic!(
                "{}: invalid Python oracle JSON reply: {error}",
                self.case_name
            )
        });
        assert!(
            result.get("oracleError").is_none(),
            "{}: {result}",
            self.case_name
        );
        result
    }
    fn seed(&mut self, name: &str, case: &Value) -> (PathBuf, Value) {
        let root = self
            .root
            .path()
            .join(format!("{:04}-{name}", self.next_root));
        self.next_root += 1;
        let expected = self.call(json!({"rust":root,"case":case}));
        (root, expected)
    }

    fn terminate(&mut self, reason: &str) -> Result<(), String> {
        match self.child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(error) => return Err(format!("check Python oracle before termination: {error}")),
        }
        if let Err(error) = self.child.kill()
            && error.kind() != std::io::ErrorKind::InvalidInput
        {
            return Err(format!("terminate Python oracle after {reason}: {error}"));
        }
        self.child
            .wait()
            .map(|_| ())
            .map_err(|error| format!("reap Python oracle after {reason}: {error}"))
    }
}
impl Drop for Oracle {
    fn drop(&mut self) {
        let mut close_error = None;
        if let Err(error) = writeln!(self.input, "{{\"close\":true}}") {
            close_error = Some(format!("send close: {error}"));
        }
        let status =
            wait_child_with_timeout(&mut self.child, python_test_support::PYTHON_ORACLE_TIMEOUT);
        let mut failure = close_error;
        match status {
            Ok(status) if status.success() => {}
            Ok(status) => {
                failure.get_or_insert_with(|| format!("Python oracle exited with {status}"));
            }
            Err(()) => {
                if let Err(error) = self.terminate("shutdown timed out") {
                    failure.get_or_insert(error);
                }
                failure.get_or_insert_with(|| {
                    format!(
                        "Python oracle exceeded {} second shutdown timeout",
                        python_test_support::PYTHON_ORACLE_TIMEOUT.as_secs()
                    )
                });
            }
        }
        if let Some(reader) = self.reader.take()
            && reader.join().is_err()
        {
            failure.get_or_insert_with(|| "Python oracle output reader panicked".into());
        }
        if !std::thread::panicking()
            && let Some(failure) = failure
        {
            panic!("{}: {failure}", self.case_name);
        }
    }
}

fn read_oracle_lines(stdout: ChildStdout, sender: mpsc::Sender<Result<String, String>>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => {
                if sender.send(Ok(line)).is_err() {
                    return;
                }
            }
            Err(error) => {
                if sender.send(Err(error.to_string())).is_err() {
                    return;
                }
                return;
            }
        }
    }
}

fn wait_child_with_timeout(child: &mut Child, timeout: Duration) -> Result<ExitStatus, ()> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => return Err(()),
        }
    }
}
async fn send(
    store: &RuntimeStore,
    step: &Value,
) -> Result<coordination::PostMessageResult, coordination::CoordinationError> {
    let room = step.get("room").and_then(Value::as_str).unwrap_or("agent");
    let prepared = PreparedAdmission::load(&store.read().expect("read"), room)?;
    if let Some(operation) = step
        .get("startOperation")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        // A separate writer wins after filesystem preparation, before admission.
        let room_for_write = room.to_owned();
        store
            .transaction(move |tx| {
                let mut agent =
                    studio_runtime_store::AgentRepository::get(tx, &room_for_write.into())?
                        .expect("agent");
                agent
                    .extra
                    .insert("workspaceOperation".into(), json!(operation));
                studio_runtime_store::AgentRepository::put(tx, &agent)?;
                studio_runtime_services::agents_create::publish_agent(
                    tx,
                    &serde_json::to_value(agent)?,
                )?;
                Ok(())
            })
            .await?;
    }
    let step = step.clone();
    store
        .transaction_with_error(move |tx| post(tx, &step, &prepared))
        .await
}

fn post(
    tx: &studio_runtime_store::Transaction<'_>,
    step: &Value,
    prepared: &PreparedAdmission,
) -> Result<coordination::PostMessageResult, coordination::CoordinationError> {
    let assets = step
        .get("assets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    coordination::post_message_with_admission(
        tx,
        PostManagedMessage {
            room: step.get("room").and_then(Value::as_str).unwrap_or("agent"),
            id: "message",
            text: step
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or(" hello "),
            delivery: step
                .get("delivery")
                .and_then(Value::as_str)
                .unwrap_or("queue"),
            assets: &assets,
        },
        prepared,
    )
}

async fn python_send_and_receipt_corpus(oracle: &mut Oracle) {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("wave7_p03_message_ingress/cases.json")).expect("cases");
    for case in cases {
        let name = case["name"].as_str().expect("case name");
        let (root, expected) = oracle.seed(name, &case);
        let store = RuntimeStore::open(Some(&root))
            .await
            .unwrap_or_else(|error| panic!("{name}: Python state opens: {error}"));
        oracle.call(json!({"configure":root,"case":case}));
        // Python wrote old-format agents into the opened state: move their
        // volatile fields as the open does (format 2026-10-20).
        store
            .transaction(studio_runtime_store::agent_volatile::migrate_2026_10_20)
            .await
            .expect("volatile agent fields move");
        let mut results = Vec::new();
        for step in case["steps"].as_array().expect("steps") {
            if let Some(status) = step.get("status") {
                let status = status.as_str().map(str::to_owned);
                let error = step.get("error").and_then(Value::as_str).map(str::to_owned);
                store
                    .transaction(move |tx| {
                        tx.execute(
                            "UPDATE runtime_events SET status=?1,error=?2 WHERE id='message'",
                            rusqlite::params![status, error],
                        )?;
                        Ok(())
                    })
                    .await
                    .expect("advance event");
            } else {
                results.push(match send(&store, step).await {
                    Ok(posted) => posted.receipt,
                    Err(error) => json!({"error":error.to_string()}),
                });
            }
        }
        assert_eq!(
            json!(results),
            case.get("rustResults")
                .unwrap_or(&expected["results"])
                .clone(),
            "{name}: complete send results (existing account-name redaction is explicit)"
        );
        let receipts = store
            .transaction_with_error(move |tx| {
                studio_runtime_services::coordination_reads::delivery_receipts(
                    tx,
                    "agent",
                    &["message".into(), "missing".into(), "message".into()],
                )
            })
            .await
            .unwrap_or_else(|error| json!({"error":error.to_string()}));
        assert_eq!(receipts, expected["receipts"], "{name}: complete receipts");
        let actual = oracle.call(json!({"snapshot":root}));
        for (table, rows) in actual.as_object().expect("snapshot").iter() {
            if table == "runtime_operation_receipts" {
                assert_eq!(
                    expected["state"][table],
                    json!([]),
                    "Python has no auxiliary admission receipt"
                );
                assert_eq!(
                    rows, &expected["operationReceipts"],
                    "{name}: complete auxiliary receipts"
                );
            } else {
                assert_json(rows, &expected["state"][table], &format!("{name}/{table}"));
            }
        }
    }
}

fn assert_json(actual: &Value, expected: &Value, path: &str) {
    match (actual, expected) {
        (Value::Object(a), Value::Object(b)) => {
            assert_eq!(
                a.keys().collect::<Vec<_>>(),
                b.keys().collect::<Vec<_>>(),
                "{path}: keys"
            );
            for (key, value) in a {
                assert_json(value, &b[key], &format!("{path}/{key}"));
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len(), "{path}: rows");
            for (i, (a, b)) in a.iter().zip(b).enumerate() {
                assert_json(a, b, &format!("{path}/{i}"));
            }
        }
        _ => assert_eq!(actual, expected, "{path}"),
    }
}

fn snapshot(store: &RuntimeStore) -> Vec<(String, Vec<String>)> {
    let connection = store.read().expect("snapshot connection");
    let mut tables = connection
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .expect("tables");
    tables
        .query_map([], |row| row.get::<_, String>(0))
        .expect("names")
        .map(|name| {
            let name = name.expect("name");
            let mut query = connection
                .prepare(
                    &studio_runtime_store::agent_columns::select_declared_sql(&connection, &name)
                        .expect("columns"),
                )
                .expect("table");
            let count = query.column_count();
            let mut rows: Vec<String> = query
                .query_map([], |row| {
                    (0..count)
                        .map(|i| row.get::<_, rusqlite::types::Value>(i))
                        .collect::<Result<Vec<_>, _>>()
                })
                .expect("rows")
                .map(|row| format!("{:?}", row.expect("row")))
                .collect();
            rows.sort();
            (name, rows)
        })
        .collect()
}

const WRITE_BOUNDARIES: &[(&str, &str)] = &[
    ("runtime_agents", "UPDATE"),
    ("runtime_event_meta", "INSERT"),
    ("runtime_events", "INSERT"),
    ("runtime_event_meta", "UPDATE"),
    ("runtime_operation_receipts", "INSERT"),
    ("messages", "INSERT"),
    ("sync_entities", "INSERT"),
    ("sync_entities", "UPDATE"),
    ("runtime_capacity_retries", "UPDATE"),
];

async fn arm_statement(
    store: &RuntimeStore,
    table: &str,
    operation: &str,
    point: &str,
    target: usize,
) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let count = Arc::new(AtomicUsize::new(0));
    let counter = count.clone();
    let table = table.to_owned();
    let operation = operation.to_owned();
    let point = point.to_owned();
    store.transaction(move |tx| {
        tx.create_scalar_function("p03_statement", 0, rusqlite::functions::FunctionFlags::SQLITE_UTF8, move |_| {
            let occurrence = counter.fetch_add(1, Ordering::SeqCst) + 1;
            if occurrence == target { std::process::exit(73); }
            Ok(0_i64)
        })?;
        tx.execute_batch(&format!("CREATE TEMP TRIGGER p03_statement {point} {operation} ON {table} BEGIN SELECT p03_statement(); END"))?;
        Ok(())
    }).await.expect("arm occurrence-specific boundary");
    count
}

fn crash_child(root: &Path, table: &str, operation: &str, point: &str, occurrence: usize) {
    let status = Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "ingress_crash_child", "--nocapture"])
        .env("P03_CRASH_ROOT", root)
        .env("P03_CRASH_TABLE", table)
        .env("P03_CRASH_OPERATION", operation)
        .env("P03_CRASH_POINT", point)
        .env("P03_CRASH_OCCURRENCE", occurrence.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .expect("crash child");
    assert_eq!(
        status.code(),
        Some(73),
        "{table}/{operation}/{point}/{occurrence}: boundary reached"
    );
}

#[tokio::test]
async fn ingress_crash_child() {
    let Some(root) = std::env::var_os("P03_CRASH_ROOT") else {
        return;
    };
    let store = RuntimeStore::open(Some(Path::new(&root)))
        .await
        .expect("child store");
    let table = std::env::var("P03_CRASH_TABLE").expect("table");
    let point = std::env::var("P03_CRASH_POINT").expect("point");
    if table != "handoff" {
        arm_statement(
            &store,
            &table,
            &std::env::var("P03_CRASH_OPERATION").expect("operation"),
            &point,
            std::env::var("P03_CRASH_OCCURRENCE")
                .expect("occurrence")
                .parse()
                .expect("number"),
        )
        .await;
        send(&store, &json!({})).await.expect("send before crash");
        panic!("statement boundary was not reached");
    }
    let prepared = PreparedAdmission::load(&store.read().expect("read"), "agent").expect("prepare");
    let before_commit = point == "before_commit";
    let posted = store
        .transaction_with_error(move |tx| {
            let posted = post(tx, &json!({}), &prepared)?;
            if before_commit {
                std::process::exit(73);
            }
            Ok::<_, coordination::CoordinationError>(posted)
        })
        .await
        .expect("commit");
    if point == "after_commit" || point == "before_wake" {
        std::process::exit(73);
    }
    // The host wake carries no provider input. The durable event is the only
    // source for a restarted consumer, even if this in-memory signal is lost.
    let wakes = std::cell::Cell::new(0);
    assert!(posted.event_inserted);
    posted.notify_committed(|| wakes.set(wakes.get() + 1));
    let replay = send(&store, &json!({}))
        .await
        .expect("committed replay before handoff");
    replay.notify_committed(|| wakes.set(wakes.get() + 1));
    assert_eq!(
        wakes.get(),
        1,
        "each wake callback must be counted, without coalescing"
    );
    std::process::exit(73);
}

async fn each_write_occurrence_and_commit_wake_crash_recovers_exactly_once(oracle: &mut Oracle) {
    use std::sync::atomic::Ordering;
    let capacity = json!({"capacity":{"id":"retry","status":"scheduled","dueAt":100,"updatedAt":1},"steps":[]});
    let mut reached = 0;
    for &(table, operation) in WRITE_BOUNDARIES {
        if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_none()
            && !["runtime_events", "runtime_operation_receipts"].contains(&table)
        {
            continue;
        }
        for point in ["BEFORE", "AFTER"] {
            let (root, _) = oracle.seed(&format!("count-{table}-{operation}-{point}"), &capacity);
            let store = RuntimeStore::open(Some(&root)).await.expect("count store");
            oracle.call(json!({"configure":root,"case":capacity}));
            let count = arm_statement(&store, table, operation, point, usize::MAX).await;
            let prepared =
                PreparedAdmission::load(&store.read().expect("read"), "agent").expect("prepare");
            let before = snapshot(&store);
            let prepared = prepared.clone();
            let result = store
                .transaction_with_error(move |tx| {
                    post(tx, &json!({}), &prepared)?;
                    Err::<(), _>(coordination::CoordinationError::InvalidRequest(
                        "census rollback",
                    ))
                })
                .await;
            assert!(matches!(
                result,
                Err(coordination::CoordinationError::InvalidRequest(
                    "census rollback"
                ))
            ));
            assert_eq!(snapshot(&store), before);
            let occurrences = count.load(Ordering::SeqCst);
            assert!(occurrences > 0, "{table}/{operation}: real write");
            drop(store);
            for occurrence in 1..=occurrences {
                crash_child(&root, table, operation, point, occurrence);
                let recovered = RuntimeStore::open(Some(&root))
                    .await
                    .expect("recover abrupt exit");
                assert_eq!(
                    snapshot(&recovered),
                    before,
                    "{table}/{operation}/{point}/{occurrence}: atomic crash recovery"
                );
                drop(recovered);
                reached += 1;
            }
            let store = RuntimeStore::open(Some(&root)).await.expect("retry store");
            assert!(
                send(&store, &json!({}))
                    .await
                    .expect("retry")
                    .event_inserted
            );
            let committed = snapshot(&store);
            drop(store);
            let store = RuntimeStore::open(Some(&root))
                .await
                .expect("lost-response restart");
            assert!(
                !send(&store, &json!({}))
                    .await
                    .expect("replay")
                    .event_inserted
            );
            assert_eq!(snapshot(&store), committed);
        }
    }
    for point in ["before_commit", "after_commit", "before_wake", "after_wake"] {
        let (root, _) = oracle.seed(point, &json!({"steps":[]}));
        let store = RuntimeStore::open(Some(&root))
            .await
            .expect("before handoff");
        let before = snapshot(&store);
        drop(store);
        crash_child(&root, "handoff", "", point, 0);
        let store = RuntimeStore::open(Some(&root))
            .await
            .expect("handoff restart");
        if point == "before_commit" {
            assert_eq!(snapshot(&store), before);
        }
        let response = send(&store, &json!({})).await.expect("handoff retry");
        assert_eq!(response.event_inserted, point == "before_commit");
        let committed = snapshot(&store);
        assert!(
            !send(&store, &json!({}))
                .await
                .expect("replay")
                .event_inserted
        );
        assert_eq!(snapshot(&store), committed);
        let count: i64 = store
            .read()
            .expect("read")
            .query_row(
                "SELECT count(*) FROM runtime_events WHERE id='message'",
                [],
                |row| row.get(0),
            )
            .expect("event count");
        assert_eq!(count, 1);
    }
    eprintln!(
        "P03 covered {reached} occurrence-specific abrupt statement exits and 4 commit/wake exits"
    );
}

#[tokio::test]
async fn event_projection_failure_returns_error_and_rolls_back_message_admission() {
    let mut oracle = Oracle::new();
    let (root, _) = oracle.seed("projection-failure", &json!({"steps":[]}));
    let store = RuntimeStore::open(Some(&root)).await.expect("store");
    store
        .transaction(move |tx| {
            tx.execute_batch(
                "CREATE TRIGGER fail_event_projection BEFORE INSERT ON sync_entities
                 WHEN NEW.collection='event'
                 BEGIN SELECT RAISE(ABORT,'injected event projection failure'); END;",
            )?;
            Ok(())
        })
        .await
        .expect("arm projection failure");
    let before = snapshot(&store);
    let prepared =
        PreparedAdmission::load(&store.read().expect("read"), "agent").expect("prepare admission");
    let prepared = prepared.clone();
    let result = store
        .transaction_with_error(move |tx| {
            post(
                tx,
                &json!({"id":"projection-failure-message","room":"agent","text":"rollback"}),
                &prepared,
            )
        })
        .await;
    assert!(matches!(
        result,
        Err(coordination::CoordinationError::Store(_))
    ));
    assert_eq!(snapshot(&store), before);
}

async fn injected_policy_denials_and_changed_workspace_snapshot_write_nothing(oracle: &mut Oracle) {
    use studio_runtime_services::coordination::CoordinationError;
    use studio_runtime_services::message_admission::MessageAdmission;
    use studio_runtime_store::{AgentRecord, Transaction};
    struct Policy {
        workspace_denied: bool,
        budget_denied: bool,
    }
    impl MessageAdmission for Policy {
        fn workspace(
            &self,
            _tx: &Transaction<'_>,
            _agent: &AgentRecord,
        ) -> Result<Vec<Value>, CoordinationError> {
            if self.workspace_denied {
                return Err(CoordinationError::InvalidRequest("Workspace busy"));
            }
            Ok(Vec::new())
        }
        fn budget(
            &self,
            _tx: &Transaction<'_>,
            _agent: &AgentRecord,
            _root: &AgentRecord,
        ) -> Result<(), CoordinationError> {
            if self.budget_denied {
                return Err(CoordinationError::InvalidRequest(
                    "Team token budget reached. Increase the budget before resuming",
                ));
            }
            Ok(())
        }
    }
    let (root, _) = oracle.seed("gates", &json!({"steps":[]}));
    let store = RuntimeStore::open(Some(&root)).await.expect("store");
    for policy in [
        Policy {
            workspace_denied: true,
            budget_denied: false,
        },
        Policy {
            workspace_denied: false,
            budget_denied: true,
        },
    ] {
        let before = snapshot(&store);
        let result = store
            .transaction_with_error(move |tx| {
                coordination::post_message_with_admission(
                    tx,
                    PostManagedMessage {
                        room: "agent",
                        id: "message",
                        text: "input",
                        delivery: "queue",
                        assets: &[],
                    },
                    &policy,
                )
            })
            .await;
        assert!(matches!(result, Err(CoordinationError::InvalidRequest(_))));
        assert_eq!(snapshot(&store), before);
    }
    let prepared = PreparedAdmission::load(&store.read().expect("read"), "agent").expect("prepare");
    store.transaction(|tx| {
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.cwd','/unprepared/path','$.workspaceOperation','capture') WHERE id='agent'",[])?;
        Ok(())
    }).await.expect("change workspace");
    let before = snapshot(&store);
    let prepared = prepared.clone();
    let result = store
        .transaction_with_error(move |tx| {
            coordination::post_message_with_admission(
                tx,
                PostManagedMessage {
                    room: "agent",
                    id: "message",
                    text: "input",
                    delivery: "queue",
                    assets: &[],
                },
                &prepared,
            )
        })
        .await;
    assert!(
        matches!(result, Err(CoordinationError::AdmissionDenied(_))),
        "a changed cwd missing from the prepared snapshot must fail closed: {result:?}"
    );
    assert_eq!(snapshot(&store), before);
}

async fn voice_delivery_prepares_workspace_admission(oracle: &mut Oracle) {
    use studio_runtime_services::voice;
    for operation in ["unrelated", "checkpoint", "capture"] {
        let (root, _) = oracle.seed(operation, &json!({"steps":[]}));
        let store = std::sync::Arc::new(RuntimeStore::open(Some(&root)).await.expect("store"));
        let record = studio_runtime_store::VoiceRecord {
            sequence: 0,
            id: "spoken".into(),
            agent: "agent".into(),
            session: "session".into(),
            kind: "user".into(),
            text: "spoken input".into(),
            item_id: String::new(),
            previous_item_id: String::new(),
            payload: "{}".into(),
            created: 1.0,
        };
        let record_id = record.id.clone();
        store
            .transaction(move |tx| {
                studio_runtime_store::VoiceRepository::insert_record(tx, &record)
            })
            .await
            .expect("transcript");
        store
            .transaction(move |tx| {
                let mut other = studio_runtime_store::AgentRepository::get(tx, &"agent".into())?
                    .expect("agent");
                other.id = "other".into();
                other.extra.insert(
                    "workspaceOperation".into(),
                    json!(if operation == "unrelated" {
                        "restore"
                    } else {
                        operation
                    }),
                );
                if operation == "unrelated" {
                    other.extra.insert("cwd".into(), json!(root));
                }
                studio_runtime_store::AgentRepository::put(tx, &other)
            })
            .await
            .expect("active operation");
        let receipt = voice::submit(
            std::sync::Arc::clone(&store),
            "agent",
            "voice-message",
            &[record_id],
            None,
        )
        .await
        .expect("accepted voice delivery");
        assert_eq!(receipt["status"], "queued", "{operation}");
        assert_eq!(
            receipt.get("waitingFor").is_some(),
            operation != "unrelated"
        );
    }
}

async fn corrupt_replay_and_workspace_operation_errors_name_the_row(oracle: &mut Oracle) {
    let (root, _) = oracle.seed("malformed", &json!({"steps":[]}));
    let store = RuntimeStore::open(Some(&root)).await.expect("store");
    send(&store, &json!({})).await.expect("accepted");
    store
        .transaction(move |tx| {
            tx.execute(
                "UPDATE runtime_events SET status='corrupt' WHERE id='message'",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("corrupt event");
    let before = snapshot(&store);
    assert!(
        send(&store, &json!({}))
            .await
            .expect_err("invalid status")
            .to_string()
            .contains("runtime_events/message")
    );
    assert_eq!(snapshot(&store), before);
    store.transaction(|tx| {
        tx.execute("DELETE FROM runtime_events WHERE id='message'", [])?;
        tx.execute("DELETE FROM runtime_operation_receipts WHERE id='message'", [])?;
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.workspaceOperation','capture','$.workspaceReservationId','operation') WHERE id='agent'", [])?;
        tx.execute("DROP INDEX IF EXISTS runtime_workspace_operation_phase", [])?;
        tx.execute("DROP INDEX IF EXISTS runtime_workspace_operation_agent_phase", [])?;
        tx.execute("INSERT INTO runtime_workspace_operations(id,record) VALUES('operation','{broken')", [])?;
        Ok(())
    }).await.expect("corrupt operation");
    let before = snapshot(&store);
    assert!(
        send(&store, &json!({}))
            .await
            .expect_err("invalid operation JSON")
            .to_string()
            .contains("runtime_workspace_operations/operation")
    );
    assert_eq!(snapshot(&store), before);
}

async fn late_operation_paths_use_the_prepared_canonical_relation(oracle: &mut Oracle) {
    use studio_runtime_store::AgentRepository;
    for scenario in ["known_unrelated", "known_alias", "unknown_alias"] {
        let (root, _) = oracle.seed(scenario, &json!({"steps":[]}));
        let store = RuntimeStore::open(Some(&root)).await.expect("store");
        let mut other = AgentRepository::get(&store.read().expect("read"), &"agent".into())
            .expect("get")
            .expect("agent");
        other.id = "other".into();
        let path = if scenario == "known_unrelated" {
            root.clone()
        } else {
            let alias = root.join("workspace-alias");
            std::os::unix::fs::symlink(other.extra["cwd"].as_str().expect("cwd"), &alias)
                .expect("workspace alias");
            alias
        };
        other.extra.insert("cwd".into(), json!(path));
        if scenario != "unknown_alias" {
            store
                .transaction({
                    let other = other.clone();
                    move |tx| AgentRepository::put(tx, &other)
                })
                .await
                .expect("idle other");
        }
        let prepared =
            PreparedAdmission::load(&store.read().expect("read"), "agent").expect("prepare");
        other
            .extra
            .insert("workspaceOperation".into(), json!("capture"));
        store
            .transaction({
                let other = other.clone();
                move |tx| AgentRepository::put(tx, &other)
            })
            .await
            .expect("late operation");
        let before = snapshot(&store);
        let prepared = prepared.clone();
        let result = store
            .transaction_with_error(move |tx| post(tx, &json!({}), &prepared))
            .await;
        if scenario == "unknown_alias" {
            // A new cwd may alias this workspace: raw string inequality cannot
            // establish independence. Python resolves it under its runtime lock;
            // this adapter requires a fresh host snapshot outside the writer.
            assert!(
                matches!(
                    result,
                    Err(coordination::CoordinationError::AdmissionDenied(_))
                ),
                "a late, previously unseen workspace alias must fail closed: {result:?}"
            );
            assert_eq!(snapshot(&store), before);
        } else {
            let receipt = result.expect("known relation permits admission").receipt;
            assert_eq!(receipt["status"], "queued");
            if scenario == "known_alias" {
                assert_eq!(
                    receipt["waitingFor"],
                    json!([{"agentId":"other","operation":"capture"}])
                );
            } else {
                assert!(receipt.get("waitingFor").is_none());
            }
        }
    }
}

#[tokio::test]
async fn message_ingress_python_oracle_corpus() {
    let mut oracle = Oracle::new();
    corrupt_replay_and_workspace_operation_errors_name_the_row(&mut oracle).await;
    injected_policy_denials_and_changed_workspace_snapshot_write_nothing(&mut oracle).await;
    late_operation_paths_use_the_prepared_canonical_relation(&mut oracle).await;
    python_send_and_receipt_corpus(&mut oracle).await;
    voice_delivery_prepares_workspace_admission(&mut oracle).await;
    each_write_occurrence_and_commit_wake_crash_recovers_exactly_once(&mut oracle).await;
}
