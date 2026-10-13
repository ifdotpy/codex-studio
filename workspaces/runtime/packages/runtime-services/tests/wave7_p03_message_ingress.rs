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
    let prepared = PreparedAdmission::load(&store.read().expect("read"), "agent")?;
    let assets = step
        .get("assets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    store.transaction_with_error(|tx| {
        coordination::post_message_with_admission(
            tx,
            PostManagedMessage {
                room: "agent",
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
            &prepared,
        )
    })
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
        if let Some(fields) = case.get("agent").and_then(Value::as_object) {
            store
                .transaction(|tx| {
                    let mut agent =
                        studio_runtime_store::AgentRepository::get(tx, &"agent".into())?
                            .expect("agent");
                    agent.extra.extend(
                        fields
                            .iter()
                            .map(|(key, value)| (key.clone(), value.clone())),
                    );
                    studio_runtime_store::AgentRepository::put(tx, &agent)
                })
                .expect("live policy state after startup recovery");
        }
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
            .expect("receipts");
        assert_eq!(receipts, expected["receipts"], "{name}: complete receipts");
        let actual = oracle.call(json!({"snapshot":root}));
        assert_eq!(
            actual, expected["state"],
            "{name}: complete event, metadata and agent rows"
        );
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

#[test]
fn each_write_failure_rolls_back_and_restart_retry_is_exactly_once() {
    let mut oracle = Oracle::new();
    let boundaries = [
        ("runtime_agents", "UPDATE"),
        ("runtime_event_meta", "INSERT"),
        ("runtime_events", "INSERT"),
        ("runtime_event_meta", "UPDATE"),
        ("runtime_operation_receipts", "INSERT"),
        ("messages", "INSERT"),
        ("sync_entities", "INSERT"),
        ("sync_entities", "UPDATE"),
    ];
    for (table, operation) in boundaries {
        for point in ["BEFORE", "AFTER"] {
            // The full matrix is an explicitly selected corpus run. The fast
            // set covers admission-to-event and event-to-receipt boundaries.
            if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_none()
                && !["runtime_events", "runtime_operation_receipts"].contains(&table)
            {
                continue;
            }
            let name = format!("fail-{table}-{operation}-{point}");
            let (root, _) = oracle.seed(&name, &json!({"steps":[]}));
            let store = RuntimeStore::open(Some(&root)).expect("seed");
            store.transaction(|tx| {
                tx.execute_batch(&format!("CREATE TRIGGER p03_fail {point} {operation} ON {table} BEGIN SELECT RAISE(ABORT,'p03 failpoint'); END"))?;
                Ok(())
            }).expect("arm failpoint");
            let before = snapshot(&store);
            let failure = send(&store, &json!({}));
            assert!(failure.is_err(), "{name}: failure boundary reached");
            assert_eq!(snapshot(&store), before, "{name}: no partial write");
            drop(store);
            let store = RuntimeStore::open(Some(&root)).expect("restart");
            assert_eq!(
                snapshot(&store),
                before,
                "{name}: restart preserves rollback"
            );
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER p03_fail")?;
                    Ok(())
                })
                .expect("disarm");
            assert!(
                send(&store, &json!({}))
                    .expect("retry succeeds")
                    .event_inserted
            );
            let committed = snapshot(&store);
            drop(store);
            let store = RuntimeStore::open(Some(&root)).expect("restart after lost response");
            let replay = send(&store, &json!({})).expect("replay");
            assert!(!replay.event_inserted);
            assert_eq!(
                replay.receipt,
                json!({"id":"message","status":"pending","error":null})
            );
            assert_eq!(
                snapshot(&store),
                committed,
                "{name}: exactly once after commit"
            );
        }
    }
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
