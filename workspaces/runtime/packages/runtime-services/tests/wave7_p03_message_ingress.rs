//! Message ingress at its transaction boundary, using Python-written databases.
#[path = "support/prepared_python.rs"]
mod python_test_support;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};
use studio_runtime_services::coordination::{self, PostManagedMessage};
use studio_runtime_services::message_admission::PreparedAdmission;
use studio_runtime_store::RuntimeStore;

struct Oracle {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    root: tempfile::TempDir,
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
        Self {
            input: child.stdin.take().expect("stdin"),
            output: BufReader::new(child.stdout.take().expect("stdout")),
            child,
            root,
        }
    }
    fn call(&mut self, command: Value) -> Value {
        writeln!(self.input, "{command}").expect("command");
        self.input.flush().expect("flush");
        let mut line = String::new();
        self.output.read_line(&mut line).expect("reply");
        let result: Value = serde_json::from_str(&line).expect("JSON oracle reply");
        assert!(result.get("oracleError").is_none(), "{result}");
        result
    }
    fn seed(&mut self, name: &str, case: &Value) -> (PathBuf, Value) {
        let root = self.root.path().join(name);
        let expected = self.call(json!({"rust":root,"case":case}));
        (root, expected)
    }
}
impl Drop for Oracle {
    fn drop(&mut self) {
        if writeln!(self.input, "{{\"close\":true}}").is_err() {
            self.child.kill().expect("stop failed oracle");
        }
        let status = self.child.wait().expect("reap oracle");
        if !std::thread::panicking() {
            assert!(status.success());
        }
    }
}
fn send(
    store: &RuntimeStore,
    step: &Value,
) -> Result<coordination::PostMessageResult, coordination::CoordinationError> {
    let room = step.get("room").and_then(Value::as_str).unwrap_or("agent");
    let prepared = PreparedAdmission::load(&store.read().expect("read"), room)?;
    if let Some(operation) = step.get("startOperation").and_then(Value::as_str) {
        // A separate writer wins after filesystem preparation, before admission.
        store.transaction(|tx| {
            let mut agent =
                studio_runtime_store::AgentRepository::get(tx, &room.into())?.expect("agent");
            agent
                .extra
                .insert("workspaceOperation".into(), json!(operation));
            studio_runtime_store::AgentRepository::put(tx, &agent)?;
            studio_runtime_services::agents_create::publish_agent(
                tx,
                &serde_json::to_value(agent)?,
            )?;
            Ok(())
        })?;
    }
    store.transaction_with_error(|tx| post(tx, step, &prepared))
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

#[test]
fn python_send_and_receipt_corpus() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("wave7_p03_message_ingress/cases.json")).expect("cases");
    let mut oracle = Oracle::new();
    for case in cases {
        let name = case["name"].as_str().expect("case name");
        let (root, expected) = oracle.seed(name, &case);
        let store = RuntimeStore::open(Some(&root))
            .unwrap_or_else(|error| panic!("{name}: Python state opens: {error}"));
        oracle.call(json!({"configure":root,"case":case}));
        let mut results = Vec::new();
        for step in case["steps"].as_array().expect("steps") {
            if let Some(status) = step.get("status") {
                store
                    .transaction(|tx| {
                        tx.execute(
                            "UPDATE runtime_events SET status=?1,error=?2 WHERE id='message'",
                            rusqlite::params![
                                status.as_str(),
                                step.get("error").and_then(Value::as_str)
                            ],
                        )?;
                        Ok(())
                    })
                    .expect("advance event");
            } else {
                results.push(match send(&store, step) {
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
            .transaction_with_error(|tx| {
                studio_runtime_services::coordination_reads::delivery_receipts(
                    tx,
                    "agent",
                    &["message".into(), "missing".into(), "message".into()],
                )
            })
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
                .prepare(&format!("SELECT * FROM \"{}\"", name.replace('"', "\"\"")))
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

fn arm_statement(
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
    store.transaction(|tx| {
        tx.create_scalar_function("p03_statement", 0, rusqlite::functions::FunctionFlags::SQLITE_UTF8, move |_| {
            let occurrence = counter.fetch_add(1, Ordering::SeqCst) + 1;
            if occurrence == target { std::process::exit(73); }
            Ok(0_i64)
        })?;
        tx.execute_batch(&format!("CREATE TEMP TRIGGER p03_statement {point} {operation} ON {table} BEGIN SELECT p03_statement(); END"))?;
        Ok(())
    }).expect("arm occurrence-specific boundary");
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
        .stdout(Stdio::null())
        .status()
        .expect("crash child");
    assert_eq!(
        status.code(),
        Some(73),
        "{table}/{operation}/{point}/{occurrence}: boundary reached"
    );
}

#[test]
fn ingress_crash_child() {
    let Some(root) = std::env::var_os("P03_CRASH_ROOT") else {
        return;
    };
    let store = RuntimeStore::open(Some(Path::new(&root))).expect("child store");
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
        );
        send(&store, &json!({})).expect("send before crash");
        panic!("statement boundary was not reached");
    }
    let prepared = PreparedAdmission::load(&store.read().expect("read"), "agent").expect("prepare");
    let posted = store
        .transaction_with_error(|tx| {
            let posted = post(tx, &json!({}), &prepared)?;
            if point == "before_commit" {
                std::process::exit(73);
            }
            Ok::<_, coordination::CoordinationError>(posted)
        })
        .expect("commit");
    if point == "after_commit" || point == "before_wake" {
        std::process::exit(73);
    }
    // The host wake carries no provider input. The durable event is the only
    // source for a restarted consumer, even if this in-memory signal is lost.
    let wake = tokio::sync::Notify::new();
    assert!(posted.event_inserted);
    posted.notify_committed(&wake);
    let replay = send(&store, &json!({})).expect("committed replay before handoff");
    replay.notify_committed(&wake);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(wake.notified());
    use std::future::Future;
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
        std::pin::pin!(wake.notified())
            .poll(&mut context)
            .is_pending()
    );
    std::process::exit(73);
}

#[test]
fn each_write_occurrence_and_commit_wake_crash_recovers_exactly_once() {
    use std::sync::atomic::Ordering;
    let mut oracle = Oracle::new();
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
            let store = RuntimeStore::open(Some(&root)).expect("count store");
            oracle.call(json!({"configure":root,"case":capacity}));
            let count = arm_statement(&store, table, operation, point, usize::MAX);
            let prepared =
                PreparedAdmission::load(&store.read().expect("read"), "agent").expect("prepare");
            let before = snapshot(&store);
            let result = store.transaction_with_error(|tx| {
                post(tx, &json!({}), &prepared)?;
                Err::<(), _>(coordination::CoordinationError::InvalidRequest(
                    "census rollback",
                ))
            });
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
                let recovered = RuntimeStore::open(Some(&root)).expect("recover abrupt exit");
                assert_eq!(
                    snapshot(&recovered),
                    before,
                    "{table}/{operation}/{point}/{occurrence}: atomic crash recovery"
                );
                drop(recovered);
                reached += 1;
            }
            let store = RuntimeStore::open(Some(&root)).expect("retry store");
            assert!(send(&store, &json!({})).expect("retry").event_inserted);
            let committed = snapshot(&store);
            drop(store);
            let store = RuntimeStore::open(Some(&root)).expect("lost-response restart");
            assert!(!send(&store, &json!({})).expect("replay").event_inserted);
            assert_eq!(snapshot(&store), committed);
        }
    }
    for point in ["before_commit", "after_commit", "before_wake", "after_wake"] {
        let (root, _) = oracle.seed(point, &json!({"steps":[]}));
        let store = RuntimeStore::open(Some(&root)).expect("before handoff");
        let before = snapshot(&store);
        drop(store);
        crash_child(&root, "handoff", "", point, 0);
        let store = RuntimeStore::open(Some(&root)).expect("handoff restart");
        if point == "before_commit" {
            assert_eq!(snapshot(&store), before);
        }
        let response = send(&store, &json!({})).expect("handoff retry");
        assert_eq!(response.event_inserted, point == "before_commit");
        let committed = snapshot(&store);
        assert!(!send(&store, &json!({})).expect("replay").event_inserted);
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

#[test]
fn injected_policy_denials_and_changed_workspace_snapshot_write_nothing() {
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
    let mut oracle = Oracle::new();
    let (root, _) = oracle.seed("gates", &json!({"steps":[]}));
    let store = RuntimeStore::open(Some(&root)).expect("store");
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
        let result = store.transaction_with_error(|tx| {
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
        });
        assert!(matches!(result, Err(CoordinationError::InvalidRequest(_))));
        assert_eq!(snapshot(&store), before);
    }
    let prepared = PreparedAdmission::load(&store.read().expect("read"), "agent").expect("prepare");
    store.transaction(|tx| {
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.cwd','/unprepared/path','$.workspaceOperation','capture') WHERE id='agent'",[])?;
        Ok(())
    }).expect("change workspace");
    let before = snapshot(&store);
    let result = store.transaction_with_error(|tx| {
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
    });
    assert!(matches!(
        result,
        Err(CoordinationError::RequiresDependency(
            "workspace-policy/message-admission"
        ))
    ));
    assert_eq!(snapshot(&store), before);
}

#[test]
fn voice_delivery_prepares_workspace_admission() {
    use studio_runtime_services::voice;
    let mut oracle = Oracle::new();
    for operation in ["unrelated", "checkpoint", "capture"] {
        let (root, _) = oracle.seed(operation, &json!({"steps":[]}));
        let store = RuntimeStore::open(Some(&root)).expect("store");
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
        store
            .transaction(|tx| studio_runtime_store::VoiceRepository::insert_record(tx, &record))
            .expect("transcript");
        store
            .transaction(|tx| {
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
            .expect("active operation");
        let receipt = voice::submit(&store, "agent", "voice-message", &[record.id], None)
            .expect("accepted voice delivery");
        assert_eq!(receipt["status"], "queued", "{operation}");
        assert_eq!(
            receipt.get("waitingFor").is_some(),
            operation != "unrelated"
        );
    }
}

#[test]
fn corrupt_replay_and_workspace_operation_errors_name_the_row() {
    let mut oracle = Oracle::new();
    let (root, _) = oracle.seed("malformed", &json!({"steps":[]}));
    let store = RuntimeStore::open(Some(&root)).expect("store");
    send(&store, &json!({})).expect("accepted");
    store
        .transaction(|tx| {
            tx.execute(
                "UPDATE runtime_events SET status='corrupt' WHERE id='message'",
                [],
            )?;
            Ok(())
        })
        .expect("corrupt event");
    let before = snapshot(&store);
    assert!(
        send(&store, &json!({}))
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
    }).expect("corrupt operation");
    let before = snapshot(&store);
    assert!(
        send(&store, &json!({}))
            .expect_err("invalid operation JSON")
            .to_string()
            .contains("runtime_workspace_operations/operation")
    );
    assert_eq!(snapshot(&store), before);
}
