//! Both sides start from a committed Python-written database, never a Rust snapshot.
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use studio_runtime_store::RuntimeStore;

#[path = "support/prepared_python.rs"]
mod prepared_python;

struct Oracle {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Oracle {
    fn new(scratch: &Path) -> Self {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let python = prepared_python::prepared_python(&source);
        let mut child = Command::new(python)
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/turn_lifecycle/oracle.py"))
            .arg(scratch)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Python oracle starts");
        let input = child.stdin.take().expect("oracle stdin");
        let output = BufReader::new(child.stdout.take().expect("oracle stdout"));
        Self {
            child,
            input,
            output,
        }
    }
    fn call(&mut self, value: Value) -> Value {
        writeln!(self.input, "{value}").expect("oracle command");
        self.input.flush().expect("oracle flush");
        let mut line = String::new();
        self.output.read_line(&mut line).expect("oracle response");
        let response: Value = serde_json::from_str(&line).expect("oracle JSON response");
        assert!(response.get("error").is_none(), "{}", response["error"]);
        response["result"].clone()
    }
}
impl Drop for Oracle {
    fn drop(&mut self) {
        if std::thread::panicking() {
            drop(self.child.kill());
        } else {
            writeln!(self.input, "{{\"command\":\"close\"}}").expect("close oracle");
            self.input.flush().expect("close flush");
        }
        self.child.wait().expect("reap oracle");
    }
}
struct State(PathBuf);
impl Drop for State {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove disposable states");
    }
}
fn apply(store: &RuntimeStore, case: &Value, prepared: &Value) -> Result<Value, String> {
    use studio_runtime_providers::LineDecoder;
    use studio_runtime_services::notification_dispatch::{
        NotificationStreamBuffer, ProviderDeliveryIdentity, dispatch_notification,
    };
    if case["submission"] == true {
        use studio_runtime_services::notification_dispatch::{
            PreparedSubmission, SubmittedInput, record_user_submission,
        };
        let submission = PreparedSubmission {
            first_event_id: prepared["firstEventId"].as_str().expect("first ID").into(),
            text: prepared["text"].as_str().expect("text").into(),
            submitted_at: 1234.5,
            created_at: 1234.5,
            assets: prepared["assets"].clone(),
            inputs: prepared["inputs"]
                .as_array()
                .expect("inputs")
                .iter()
                .map(|i| SubmittedInput {
                    id: i["id"].as_str().expect("ID").into(),
                    at: i["at"].as_f64().expect("time"),
                    kind: i["kind"].as_str().expect("kind").into(),
                    text: i["text"].as_str().expect("text").into(),
                    assets: i["assets"].clone(),
                })
                .collect(),
        };
        let mut rolled = None;
        let rollback: Result<(), studio_runtime_services::notification_dispatch::NotificationDispatchError> = store.transaction_with(|tx| {
            rolled = Some(record_user_submission(tx, "agent", &submission)?);
            Err(studio_runtime_services::notification_dispatch::NotificationDispatchError::InvalidNotification)
        });
        assert!(rollback.is_err());
        let result = store
            .transaction_with(|tx| record_user_submission(tx, "agent", &submission))
            .map_err(|e| e.to_string())?;
        assert_eq!(rolled, Some(result.clone()), "submission outer rollback");
        store.transaction_with(|tx| {
            let before = tx.total_changes();
            assert_eq!(record_user_submission(tx,"agent",&submission)?,result);
            assert_eq!(before, tx.total_changes(), "submission replay wrote");
            let mut changed = submission.clone();
            changed.text.push_str("CONFLICT-SUFFIX");
            assert!(matches!(record_user_submission(tx,"agent",&changed),Err(studio_runtime_services::notification_dispatch::NotificationDispatchError::ConflictingInput)));
            changed = submission.clone();
            changed.inputs[0].text.push_str("CONFLICT-INPUT-SUFFIX");
            assert!(matches!(record_user_submission(tx,"agent",&changed),Err(studio_runtime_services::notification_dispatch::NotificationDispatchError::ConflictingInput)));
            assert_eq!(before, tx.total_changes(), "conflicting submission wrote");
            Ok::<_,studio_runtime_services::notification_dispatch::NotificationDispatchError>(())
        }).map_err(|e|e.to_string())?;
        let deleted_replay: Result<(), studio_runtime_services::notification_dispatch::NotificationDispatchError> = store.transaction_with(|tx| {
            tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.deletedAt',99.0) WHERE id='agent'", []).expect("delete fixture agent");
            let before = tx.total_changes();
            assert_eq!(record_user_submission(tx,"agent",&submission)?,result);
            assert_eq!(before, tx.total_changes(), "saved replay depends on mutable agent");
            Err(studio_runtime_services::notification_dispatch::NotificationDispatchError::InvalidNotification)
        });
        assert!(deleted_replay.is_err());
        return Ok(json!([result]));
    }
    let mut results = Vec::new();
    let mut streams = NotificationStreamBuffer::default();
    for (index, step) in case["steps"].as_array().expect("steps").iter().enumerate() {
        let agent_id = step["agent"].as_str().unwrap_or("agent");
        let mut bytes = serde_json::to_vec(&step["frame"]).expect("frame JSON");
        bytes.push(b'\n');
        let frame = LineDecoder::new().push(&bytes).expect("frame").remove(0);
        let identity = ProviderDeliveryIdentity {
            provider_handle: step["provider_handle"]
                .as_str()
                .unwrap_or("account:default")
                .into(),
            generation: step["generation"].as_u64().unwrap_or(7),
            cursor: step["cursor"].as_u64().unwrap_or(index as u64 + 1),
        };
        if case["late_failure"] == true {
            let original_streams = streams.clone();
            let refusal = store.transaction_with(|tx| {
                tx.execute_batch("CREATE TABLE corpus_caller(value TEXT); CREATE TEMP TRIGGER corpus_late_failure BEFORE INSERT ON runtime_operation_receipts BEGIN SELECT RAISE(ABORT, 'injected late failure'); END;").expect("late service failure trigger");
                let before = raw_rows(tx);
                let refusal = dispatch_notification(tx, agent_id, store.root().to_str().expect("path"), &streams, &identity, &frame, 1234.5).expect_err("late failure must reach the service caller");
                assert_eq!(before, raw_rows(tx), "service leaked rows after caller caught its error");
                assert_eq!(streams, original_streams, "service leaked stream state");
                tx.execute_batch("DROP TRIGGER corpus_late_failure; INSERT INTO corpus_caller VALUES ('caller committed');")?;
                Ok::<_, studio_runtime_store::StoreError>(refusal.to_string())
            }).map_err(|e|e.to_string())?;
            store
                .transaction(|tx| {
                    assert_eq!(
                        tx.query_row("SELECT value FROM corpus_caller", [], |r| r
                            .get::<_, String>(0))?,
                        "caller committed"
                    );
                    Ok(())
                })
                .expect("unrelated caller work committed");
            return Err(refusal);
        }
        let mut rolled = None;
        let rollback: Result<(), studio_runtime_services::notification_dispatch::NotificationDispatchError> = store.transaction_with(|tx| {
            rolled = Some(dispatch_notification(tx, agent_id, store.root().to_str().expect("state path"), &streams, &identity, &frame, 1234.5)?);
            Err(studio_runtime_services::notification_dispatch::NotificationDispatchError::InvalidNotification)
        });
        assert!(rollback.is_err());
        if step["rollback"] == true {
            continue;
        }
        let result = store
            .transaction_with(|tx| {
                dispatch_notification(
                    tx,
                    agent_id,
                    store.root().to_str().expect("state path"),
                    &streams,
                    &identity,
                    &frame,
                    1234.5,
                )
            })
            .map_err(|e| e.to_string())?;
        assert_eq!(
            rolled.as_ref(),
            Some(&result),
            "rollback must leave buffer unchanged"
        );
        let replay = store
            .transaction_with(|tx| {
                let before = tx.total_changes();
                let replay = dispatch_notification(
                    tx,
                    agent_id,
                    store.root().to_str().expect("path"),
                    &streams,
                    &identity,
                    &frame,
                    9876.0,
                )?;
                assert_eq!(before, tx.total_changes(), "replay wrote SQLite");
                assert!(replay.stream_update.is_none(), "replay changed buffer");
                Ok::<_, studio_runtime_services::notification_dispatch::NotificationDispatchError>(
                    replay,
                )
            })
            .map_err(|e| e.to_string())?;
        assert_eq!(
            replay.outcome, result.outcome,
            "replay must return saved result"
        );
        let mut conflicting = step["frame"].clone();
        conflicting["conflict"] = json!("different full payload");
        let mut bytes = serde_json::to_vec(&conflicting).expect("JSON");
        bytes.push(b'\n');
        let conflict = LineDecoder::new()
            .push(&bytes)
            .expect("conflict frame")
            .remove(0);
        store.transaction_with(|tx| {
            let before = tx.total_changes();
            assert!(matches!(dispatch_notification(tx,agent_id,store.root().to_str().expect("path"),&streams,&identity,&conflict,1234.5), Err(studio_runtime_services::notification_dispatch::NotificationDispatchError::ConflictingDelivery)));
            assert_eq!(before,tx.total_changes());
            Ok::<_,studio_runtime_store::StoreError>(())
        }).map_err(|e|e.to_string())?;
        results.push(json!(result.outcome));
        if let Some(update) = result.stream_update {
            streams = update;
        }
        if step["tick"] == true {
            flush_agents(store, case, &mut streams)?;
        }
    }
    flush_agents(store, case, &mut streams)?;
    Ok(json!(results))
}

fn raw_rows(
    tx: &studio_runtime_store::Transaction<'_>,
) -> std::collections::BTreeMap<String, Vec<Vec<String>>> {
    let tables = tx.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name != 'corpus_caller' ORDER BY name").expect("tables").query_map([], |r| r.get::<_,String>(0)).expect("names").collect::<Result<Vec<_>,_>>().expect("names");
    tables
        .into_iter()
        .map(|table| {
            let mut statement = tx
                .prepare(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\"")))
                .expect("raw table");
            let columns = statement.column_count();
            let rows = statement
                .query_map([], |r| {
                    (0..columns)
                        .map(|i| r.get_ref(i).map(|v| format!("{v:?}")))
                        .collect::<Result<Vec<_>, _>>()
                })
                .expect("raw rows")
                .collect::<Result<Vec<_>, _>>()
                .expect("raw content");
            (table, rows)
        })
        .collect()
}
fn flush_agents(
    store: &RuntimeStore,
    case: &Value,
    streams: &mut studio_runtime_services::notification_dispatch::NotificationStreamBuffer,
) -> Result<(), String> {
    if let Some(agents) = case["flush_agents"].as_array() {
        for agent in agents {
            flush(store, agent.as_str().expect("agent"), streams)?;
        }
    } else {
        flush(store, "agent", streams)?;
    }
    Ok(())
}

fn flush(
    store: &RuntimeStore,
    agent_id: &str,
    streams: &mut studio_runtime_services::notification_dispatch::NotificationStreamBuffer,
) -> Result<(), String> {
    use studio_runtime_services::notification_dispatch::{
        NotificationDispatchError, flush_notification_streams,
    };
    let mut rolled = None;
    let rollback: Result<(), NotificationDispatchError> = store.transaction_with(|tx| {
        rolled = Some(flush_notification_streams(
            tx,
            agent_id,
            store.root().to_str().expect("path"),
            streams,
            1234.5,
        )?);
        Err(NotificationDispatchError::InvalidNotification)
    });
    assert!(rollback.is_err());
    let result = store
        .transaction_with(|tx| {
            flush_notification_streams(
                tx,
                agent_id,
                store.root().to_str().expect("path"),
                streams,
                1234.5,
            )
        })
        .map_err(|e| e.to_string())?;
    assert_eq!(
        rolled,
        Some(result.clone()),
        "timer rollback must preserve pending batches"
    );
    if let Some(update) = result {
        *streams = update;
    }
    Ok(())
}

fn differences(python: Option<&Value>, rust: Option<&Value>, path: &str, out: &mut Vec<Value>) {
    match (python,rust) {
        (Some(Value::Object(a)),Some(Value::Object(b))) => {
            let keys: std::collections::BTreeSet<_> = a.keys().chain(b.keys()).collect();
            for key in keys { differences(a.get(key),b.get(key),&format!("{path}/{key}"),out); }
        },
        (Some(Value::Array(a)),Some(Value::Array(b))) => {
            for i in 0..a.len().max(b.len()) { differences(a.get(i),b.get(i),&format!("{path}/{i}"),out); }
        },
        _ if python==rust=>{},
        _=>out.push(json!({"path":path,"python":python,"rust":rust,"pythonPresent":python.is_some(),"rustPresent":rust.is_some()})),
    }
}
fn expected_with_deviations(mut expected: Value, case: &Value) -> Value {
    let registry: Value = serde_json::from_str(include_str!("turn_lifecycle/deviations.json"))
        .expect("central deviation registry");
    if let Some(class) = case["deviation"].as_str() {
        assert!(
            registry.get(class).is_some(),
            "deviation must be centrally declared: {class}"
        );
    }
    if let Some(patches) = case["expected_differences"].as_array() {
        assert!(
            case["deviation"].is_string(),
            "differences require a named reason"
        );
        for patch in patches {
            let path = patch["path"].as_str().expect("difference path");
            let class = patch["deviation"]
                .as_str()
                .expect("field patch must name its deviation class");
            assert_eq!(patch["deviation"], case["deviation"]);
            let declaration = registry.get(class).expect("declared deviation");
            let field = path
                .split('/')
                .map(|part| {
                    if part.parse::<usize>().is_ok() {
                        "*"
                    } else {
                        part
                    }
                })
                .collect::<Vec<_>>()
                .join("/");
            assert!(
                declaration["fields"]
                    .as_array()
                    .expect("declared fields")
                    .contains(&json!(field)),
                "undeclared field: {class} {field}"
            );
            assert!(
                patch["pythonPresent"] != patch["rustPresent"] || patch["python"] != patch["rust"],
                "declared difference does not occur: {path}"
            );
            let actual = expected.pointer(path);
            if patch["pythonPresent"] == true {
                assert_eq!(actual, Some(&patch["python"]), "Python changed at {path}");
            } else {
                assert!(actual.is_none(), "Python unexpectedly has {path}");
            }
            let (parent, key) = path.rsplit_once('/').expect("parent");
            let container = expected.pointer_mut(parent).expect("parent exists");
            if let Some(object) = container.as_object_mut() {
                if patch["rustPresent"] == true {
                    object.insert(
                        key.replace("~1", "/").replace("~0", "~"),
                        patch["rust"].clone(),
                    );
                } else {
                    object.remove(key);
                }
            } else {
                let array = container.as_array_mut().expect("array");
                let index = key.parse::<usize>().expect("index");
                if patch["rustPresent"] == true {
                    array[index] = patch["rust"].clone();
                } else {
                    array.remove(index);
                }
            }
        }
        for row in expected["sync_entities"].as_array_mut().expect("sync rows") {
            row["hash"] = json!(studio_runtime_store::sha256_hex(
                studio_runtime_services::event_write::python_json(&row["payload"]).as_bytes()
            ));
        }
    }
    expected
}
#[test]
fn python_written_notification_corpus() {
    if let Some(path) = std::env::var_os("NOTIFICATION_REPLAY_CHILD") {
        let fixture: Value =
            serde_json::from_slice(&fs::read(path).expect("replay fixture")).expect("replay JSON");
        let store = RuntimeStore::open(Some(Path::new(fixture["root"].as_str().expect("root"))))
            .expect("reopen committed database");
        apply(&store, &fixture["case"], &fixture["prepared"])
            .expect("replay after lost acknowledgement");
        return;
    }
    corpus(std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some());
}

fn corpus(full: bool) {
    let corpus_started = std::time::Instant::now();
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("turn_lifecycle/notification_cases.json"))
            .expect("corpus data");
    let coverage: Value =
        serde_json::from_str(include_str!("turn_lifecycle/notification_coverage.json"))
            .expect("coverage manifest");
    assert_eq!(
        coverage["counts"]["total"].as_u64(),
        Some(cases.len() as u64)
    );
    for finding in coverage["review_findings"].as_array().expect("findings") {
        for id in finding["cases"].as_array().expect("witnesses") {
            assert!(
                cases
                    .iter()
                    .any(|case| case["id"] == *id && case["tier"] == "fast"),
                "review witness missing from fast tier: {id}"
            );
        }
    }
    let kinds = |fast: bool| -> std::collections::BTreeSet<String> {
        cases
            .iter()
            .filter(|case| !fast || case["tier"] == "fast")
            .flat_map(|case| case["steps"].as_array().into_iter().flatten())
            .map(|step| step["frame"]["method"].as_str().expect("method").to_owned())
            .collect()
    };
    assert_eq!(
        kinds(true),
        kinds(false),
        "default must exercise every method"
    );
    let root =
        State(std::env::temp_dir().join(format!("notification-corpus-{}", std::process::id())));
    fs::create_dir(&root.0).expect("unique corpus state");
    let mut oracle = Oracle::new(&root.0);
    let mut report = vec![];
    let mut green = 0;
    for case in cases
        .iter()
        .filter(|case| case["unit_test"].is_null() && (full || case["tier"] == "fast"))
    {
        if let Ok(filter) = std::env::var("NOTIFICATION_CASE")
            && !case["id"].as_str().expect("ID").contains(&filter)
        {
            continue;
        }
        let id = case["id"].as_str().expect("case ID");
        let rust = root.0.join(id).join("rust");
        let started = std::time::Instant::now();
        let expected = oracle.call(
            json!({"command":"seed","root":root.0.join(id).join("python"),"rust":rust,"case":case}),
        );
        let seeded = started.elapsed().as_secs_f64();
        let (first, error) = match RuntimeStore::open(Some(&rust)) {
            Ok(store) => {
                let first = apply(&store, case, &expected["before"]["prepared"]);
                let error = first.as_ref().err().cloned();
                drop(store);
                (first, error)
            }
            Err(error) => {
                let error = error.to_string();
                (Err(error.clone()), Some(error))
            }
        };
        if let Ok(value) = &first {
            assert_eq!(value, &case["return"], "{id}: entry-point result");
        }
        if case["crash_replay"] == true {
            let fixture = rust.parent().expect("parent").join("replay.json");
            fs::write(
                &fixture,
                serde_json::to_vec(
                    &json!({"root":rust,"case":case,"prepared":expected["before"]["prepared"]}),
                )
                .expect("fixture"),
            )
            .expect("fixture file");
            let child = Command::new(std::env::current_exe().expect("test executable"))
                .args([
                    "--exact",
                    "python_written_notification_corpus",
                    "--nocapture",
                ])
                .env("NOTIFICATION_REPLAY_CHILD", fixture)
                .output()
                .expect("second process");
            assert!(
                child.status.success(),
                "replay child: {}",
                String::from_utf8_lossy(&child.stderr)
            );
        }
        let applied = started.elapsed().as_secs_f64();
        let actual =
            oracle.call(json!({"command":"snapshot","module":"notification_dispatch","root":rust}));
        let mut diff = vec![];
        let expected_after = expected_with_deviations(expected["after"].clone(), case);
        differences(Some(&expected_after), Some(&actual), "", &mut diff);
        let expected_error = case["expect_refusal"].as_str().map(|e| {
            e.replace(
                "$project",
                expected["pythonRoot"].as_str().expect("Python root"),
            )
        });
        let error_matches = match expected_error {
            Some(message) => error.as_deref().is_some_and(|error| {
                error == message.as_str()
                    && !error.contains("SECRET_STORED_SENTINEL")
                    && !error.contains("CORPUS_PRIVATE_PAYLOAD")
            }),
            None => error.is_none(),
        };
        if diff.is_empty() && error_matches {
            green += 1;
        } else {
            eprintln!("{id}: {} differences; error={error:?}", diff.len());
        }
        report.push(json!({"id":id,"differences":diff,"error":error,"firstResult":first.ok(),"seconds":{"seed":seeded,"apply":applied-seeded,"snapshot":started.elapsed().as_secs_f64()-applied}}));
    }
    assert!(!report.is_empty(), "case filter matched nothing");
    let report_path = std::env::var_os("NOTIFICATION_CORPUS_REPORT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("notification-corpus-report.json"));
    fs::write(
        &report_path,
        serde_json::to_vec_pretty(&report).expect("report JSON"),
    )
    .expect("report written");
    let oracle_cpu = oracle.call(json!({"command":"metrics","module":"notification_dispatch"}));
    // Linux exposes this process's CPU separately from its oracle/replay children.
    // Other platforms still report oracle CPU and per-phase wall durations.
    let rust_cpu = fs::read_to_string("/proc/self/stat").ok().and_then(|stat| {
        let (_, fields) = stat.rsplit_once(')')?;
        let fields: Vec<_> = fields.split_whitespace().collect();
        let hz = oracle_cpu["clockTicks"].as_f64()?;
        Some(
            json!({"userSeconds":fields.get(11)?.parse::<f64>().ok()?/hz,
                    "systemSeconds":fields.get(12)?.parse::<f64>().ok()?/hz}),
        )
    });
    let phase = |key: &str| {
        report
            .iter()
            .map(|row| row["seconds"][key].as_f64().expect("phase duration"))
            .sum::<f64>()
    };
    let timing = json!({"cases":report.len(),"wallSeconds":corpus_started.elapsed().as_secs_f64(),"rustCpu":rust_cpu,"oracleCpu":oracle_cpu,"phases":{"seed":phase("seed"),"apply":phase("apply"),"snapshot":phase("snapshot")}});
    fs::write(
        report_path.with_extension("timing.json"),
        serde_json::to_vec_pretty(&timing).expect("timing JSON"),
    )
    .expect("timing report");
    eprintln!("corpus timing: {timing}");
    assert_eq!(
        green,
        report.len(),
        "{green} green / {} red; field differences: {}",
        report.len() - green,
        report_path.display()
    );
}
