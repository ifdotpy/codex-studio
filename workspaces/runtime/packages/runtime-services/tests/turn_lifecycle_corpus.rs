//! Both sides start from a committed Python-written database, never a Rust snapshot.
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use studio_runtime_services::scheduler::{
    claim_intent, lifecycle_transaction, recover_on_startup, settle_terminal_attempt,
};
use studio_runtime_store::{RuntimeStore, SchedulerRepository, TurnAttemptIdentity};

struct Oracle {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Oracle {
    fn new(root: &Path) -> Self {
        let python = std::env::var_os("CODEX_AGENTS_PYTHON")
            .expect("set CODEX_AGENTS_PYTHON to the prepared interpreter (read-only)");
        let mut child = Command::new(python)
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/turn_lifecycle/oracle.py"))
            .arg(root)
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
        let _ignored = writeln!(self.input, "{{\"command\":\"close\"}}");
        let _ignored = self.input.flush();
        self.child.wait().expect("reap oracle");
    }
}
struct State(PathBuf);
impl Drop for State {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove disposable states");
    }
}
fn apply(store: &RuntimeStore, case: &Value) -> Result<(), String> {
    let identity = TurnAttemptIdentity {
        agent_id: "agent".into(),
        epoch: 0,
        attempt_id: "attempt".into(),
    };
    let transition = case["transition"].as_str().expect("transition");
    match transition {
        "claim" => claim_intent(store, "agent", 0, "attempt", 10.0)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        "submission" => lifecycle_transaction(store, |tx| {
            SchedulerRepository::mark_submitted_identity(
                tx,
                "agent",
                0,
                "attempt",
                case["ticket"].as_str().unwrap_or("ticket"),
                "default",
                1,
                "thread",
            )
            .map(|_| ())
        })
        .map_err(|e| e.to_string()),
        "accepted" => lifecycle_transaction(store, |tx| {
            SchedulerRepository::settle_started(
                tx,
                "agent",
                0,
                "attempt",
                case["turnId"].as_str().unwrap_or("turn"),
            )
            .map(|_| ())
        })
        .map_err(|e| e.to_string()),
        "unknown" | "not_sent" => lifecycle_transaction(store, |tx| {
            SchedulerRepository::settle_start_outcome(
                tx,
                &identity,
                if transition == "unknown" {
                    "uncertain"
                } else {
                    "pending"
                },
                None,
            )
        })
        .map_err(|e| e.to_string()),
        "rejected" => lifecycle_transaction(store, |tx| {
            SchedulerRepository::settle_start_rejected_provider(
                tx,
                "agent",
                0,
                "attempt",
                "ticket",
                "default",
                1,
                "thread",
                json!({"message":case["message"].as_str().unwrap_or("rejected")}),
            )
            .map(|_| ())
        })
        .map_err(|e| e.to_string()),
        "completed" | "failed" | "interrupted" => {
            let mut turn = json!({"id":"turn","status":transition});
            if let Some(fields) = case["turn"].as_object() {
                for (k, v) in fields {
                    turn[k] = v.clone();
                }
            }
            settle_terminal_attempt(store, &identity, &json!({"threadId":"thread","turn":turn}))
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        "interrupt_accepted" | "interrupt_rejected" | "interrupt_unknown" => {
            lifecycle_transaction(store, |tx| {
                SchedulerRepository::prepare_interrupt(
                    tx,
                    "agent",
                    "thread",
                    "turn",
                    1,
                    "interrupt-ticket",
                )?;
                SchedulerRepository::settle_interrupt_result(
                    tx,
                    "interrupt-ticket",
                    1,
                    transition.trim_start_matches("interrupt_"),
                    match transition {
                        "interrupt_rejected" => Some("rejected"),
                        "interrupt_unknown" => Some("interrupt response unknown"),
                        _ => None,
                    },
                )
            })
            .map_err(|e| e.to_string())
        }
        "stop" | "pause" => lifecycle_transaction(store, |tx| {
            SchedulerRepository::stop_agent(tx, "agent", "Stopped by user").map(|_| ())
        })
        .map_err(|e| e.to_string()),
        "started" => lifecycle_transaction(store, |tx| {
            SchedulerRepository::observe_started(tx, &identity, "turn").map(|_| ())
        })
        .map_err(|e| e.to_string()),
        "restart" => recover_on_startup(store)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        "disconnect" => lifecycle_transaction(store, |tx| {
            SchedulerRepository::disconnect_account(tx, "default", 1, 20.0, false)
        })
        .map_err(|e| e.to_string()),
        "reattach" => lifecycle_transaction(store, |tx| {
            SchedulerRepository::reconcile_generation(tx, "default", 1, 20.0)
        })
        .map_err(|e| e.to_string()),
        "generation_change" => lifecycle_transaction(store, |tx| {
            SchedulerRepository::reconcile_generation(tx, "default", 2, 20.0)
        })
        .map_err(|e| e.to_string()),
        _ => panic!("unimplemented Rust transition {transition}"),
    }
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
// Named, field-level contracts: a deviation may disappear as its owner is wired,
// but a different value or an additional field never becomes an allowed difference.
fn expected_difference(diff: &Value, case: &Value, python_root: &Path) -> bool {
    case["expectedDifferences"].as_array().is_some_and(|rules| {
        rules.iter().any(|rule| {
            if rule["path"] != diff["path"] {
                return false;
            }
            assert!(matches!(
                rule["deviation"].as_str(),
                Some(
                    "rust_receipts"
                        | "notification_dispatch_unwired"
                        | "durable_reconciliation_hold"
                )
            ));
            if rule["deviation"] == "notification_dispatch_unwired" {
                assert!(matches!(
                    rule["entrypoint"].as_str(),
                    Some("record_user_submission" | "dispatch_notification")
                ));
            }
            if rule["pythonPresent"] != diff["pythonPresent"]
                || rule["rustPresent"] != diff["rustPresent"]
            {
                return false;
            }
            if rule["expectedAbsent"] == true {
                return diff["rustPresent"] == false
                    && diff["pythonPresent"] == true
                    && (rule.get("pythonId").is_none()
                        || rule["pythonId"] == diff["python"]["id"]);
            }
            let expanded = serde_json::to_string(rule)
                .expect("expectation JSON")
                .replace("${PYTHON_ROOT}", &python_root.to_string_lossy());
            let rule: Value = serde_json::from_str(&expanded).expect("expanded expectation");
            rule["python"] == diff["python"] && rule["rust"] == diff["rust"]
        })
    })
}
fn unexpected_differences(diff: &[Value], case: &Value, python_root: &Path) -> Vec<Value> {
    diff.iter()
        .filter(|d| {
            if expected_difference(d, case, python_root) {
                return false;
            }
            let path = d["path"].as_str().expect("difference path");
            if path.starts_with("/sync_entities/") && path.ends_with("/hash") {
                // snapshot validates each raw payload hash and derives this normalized
                // hash. Its only permitted cause is an explicitly declared payload delta.
                let payload = format!("{}/payload/", path.trim_end_matches("/hash"));
                let fields: Vec<_> = diff
                    .iter()
                    .filter(|d| d["path"].as_str().is_some_and(|p| p.starts_with(&payload)))
                    .collect();
                return fields.is_empty()
                    || !fields
                        .iter()
                        .all(|d| expected_difference(d, case, python_root));
            }
            true
        })
        .cloned()
        .collect()
}
#[test]
fn fast_witnesses_cover_transitions_states_and_review_findings() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("turn_lifecycle/cases.json")).expect("corpus");
    for key in ["transition", "state"] {
        let all: std::collections::BTreeSet<_> = cases
            .iter()
            .map(|c| c[key].as_str().expect("coverage key"))
            .collect();
        let fast: std::collections::BTreeSet<_> = cases
            .iter()
            .filter(|c| c["tier"] == "fast")
            .map(|c| c[key].as_str().expect("coverage key"))
            .collect();
        assert_eq!(all, fast, "fast {key} witnesses");
    }
    for finding in (1..=13)
        .map(|n| format!("A{n}"))
        .chain((1..=6).map(|n| format!("B{n}")))
    {
        assert!(
            cases.iter().any(|c| c["tier"] == "fast"
                && c["findings"]
                    .as_array()
                    .is_some_and(|f| f.contains(&json!(finding)))),
            "missing {finding}"
        );
    }
    let coverage: Value =
        serde_json::from_str(include_str!("turn_lifecycle/coverage.json")).expect("coverage data");
    for case in coverage["sameStateComparisons"]
        .as_object()
        .expect("comparison map")
        .values()
    {
        assert!(
            cases.iter().any(|c| c["id"] == *case),
            "missing original comparison {case}"
        );
    }
    for entry in coverage["matrix"].as_array().expect("matrix") {
        if let Some(ids) = entry["cases"].as_array() {
            assert!(!ids.is_empty());
            for id in ids {
                assert!(cases.iter().any(|c| c["id"] == *id
                    && c["transition"] == entry["transition"]
                    && c["state"] == entry["state"]));
            }
        } else {
            assert!(
                entry["reason"]
                    .as_str()
                    .is_some_and(|reason| !reason.is_empty())
            );
        }
    }
}

#[test]
fn python_written_turn_lifecycle_corpus() {
    let mut cases: Vec<Value> =
        serde_json::from_str(include_str!("turn_lifecycle/cases.json")).expect("corpus data");
    if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_none()
        && std::env::var_os("LIFECYCLE_CASE_FILTER").is_none()
    {
        cases.retain(|c| c["tier"] == "fast");
    }
    if let Ok(filter) = std::env::var("LIFECYCLE_CASE_FILTER") {
        cases.retain(|c| c["id"].as_str().is_some_and(|id| id.contains(&filter)));
    }
    let root = State(std::env::temp_dir().join(format!("turn-corpus-{}", std::process::id())));
    fs::create_dir(&root.0).expect("unique corpus state");
    let mut oracle = Oracle::new(&root.0);
    let registry: Value = serde_json::from_str(include_str!("turn_lifecycle/deviations.json"))
        .expect("central deviations");
    let mut report = vec![];
    let mut green = 0;
    for case in &cases {
        let id = case["id"].as_str().expect("case ID");
        for rule in case["expectedDifferences"].as_array().into_iter().flatten() {
            let class = rule["deviation"].as_str().expect("named deviation");
            let declaration = registry
                .get(class)
                .expect("deviation must be centrally declared");
            let path = rule["path"]
                .as_str()
                .expect("deviation field")
                .split('/')
                .map(|p| if p.parse::<usize>().is_ok() { "*" } else { p })
                .collect::<Vec<_>>()
                .join("/");
            assert!(
                declaration["fields"]
                    .as_array()
                    .expect("fixed fields")
                    .contains(&json!(path)),
                "{id}: undeclared deviation field {path}"
            );
            if declaration["approvedClass"] == "a" {
                assert!(
                    declaration["entrypoints"]
                        .as_array()
                        .expect("deviation entrypoints")
                        .contains(&rule["entrypoint"])
                );
            }
        }
        let rust = root.0.join(id).join("rust");
        let expected = oracle.call(
            json!({"command":"seed","root":root.0.join(id).join("python"),"rust":rust,"case":case}),
        );
        let store = RuntimeStore::open(Some(&rust)).expect("open Python-written state");
        if case["roundtripHold"] == true {
            lifecycle_transaction(&store, |tx| {
                SchedulerRepository::hold_account(tx, "default", Some(7), "first account hold")?;
                SchedulerRepository::hold_account(tx, "default", Some(8), "second account hold")?;
                SchedulerRepository::clear_account_hold(tx, "default")
            })
            .expect("nested hold restoration");
        }
        let mut error = apply(&store, case).err();
        if let Some(text) = case["afterEventText"].as_str() {
            store
                .transaction(|tx| {
                    tx.execute("UPDATE runtime_events SET text=?1 WHERE id='event'", [text])?;
                    Ok(())
                })
                .expect("Python writer mutation equivalent");
        }
        if let Some(probe) = case["probe"].as_object() {
            let mut changed = case.clone();
            for (key, value) in probe {
                changed[key] = value.clone();
            }
            let refusal =
                apply(&store, &changed).expect_err("conflicting evidence must be refused");
            assert!(
                refusal.contains("runtime_") && !refusal.contains("CORPUS_PRIVATE_PAYLOAD"),
                "sanitized stored error"
            );
        }
        if case["replay"] == true && error.is_none() {
            let before = store
                .transaction(|tx| Ok(tx.total_changes()))
                .expect("write counter");
            error = apply(&store, case).err();
            let after = store
                .transaction(|tx| Ok(tx.total_changes()))
                .expect("write counter");
            if after != before {
                error = Some(format!("replay made {} writes", after - before));
            }
        }
        drop(store);
        let actual = oracle.call(json!({"command":"snapshot","root":rust}));
        let mut diff = vec![];
        let comparison = if let Some(refusal) = case.get("expectRefusal") {
            let message = error
                .take()
                .expect("malformed or conflicting stored evidence must be refused");
            assert!(
                message.contains(refusal["table"].as_str().expect("refusal table"))
                    && message.contains(refusal["id"].as_str().expect("refusal id"))
                    && !message.contains("CORPUS_PRIVATE_PAYLOAD"),
                "error must locate table and stored id only"
            );
            &expected["before"]
        } else {
            &expected["after"]
        };
        differences(Some(comparison), Some(&actual), "", &mut diff);
        for rule in case["expectedDifferences"].as_array().into_iter().flatten() {
            if !diff.iter().any(|d| d["path"] == rule["path"]) {
                error = Some(format!(
                    "stale deviation: {} {}",
                    rule["deviation"], rule["path"]
                ));
            }
        }
        let unexpected = unexpected_differences(
            &diff,
            case,
            Path::new(expected["pythonRoot"].as_str().expect("oracle Python root")),
        );
        if unexpected.is_empty() && error.is_none() {
            green += 1;
        } else {
            eprintln!(
                "{id}: {} unexpected differences; error={error:?}",
                unexpected.len()
            );
        }
        report.push(json!({"id":id,"differences":diff,"unexpected":unexpected,"error":error}));
    }
    let report_path = std::env::var_os("LIFECYCLE_CORPUS_REPORT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("turn-lifecycle-corpus-report.json"));
    fs::write(
        &report_path,
        serde_json::to_vec_pretty(&report).expect("report JSON"),
    )
    .expect("report written");
    assert_eq!(
        green,
        cases.len(),
        "{green} green / {} red; field differences: {}",
        cases.len() - green,
        report_path.display()
    );
}

#[test]
fn oracle_refuses_real_provider_homes() {
    let python = std::env::var_os("CODEX_AGENTS_PYTHON").expect("prepared interpreter");
    let output = Command::new(python)
        .arg("-B")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/turn_lifecycle/oracle.py"))
        .arg("--guard-test")
        .output()
        .expect("guard subprocess");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "guard refused both real provider homes"
    );
}
