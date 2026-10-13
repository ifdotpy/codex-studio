//! Both sides start from a committed Python-written database, never a Rust snapshot.
#[path = "support/prepared_python.rs"]
mod prepared_python;
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
        let python = prepared_python::prepared_python(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
        );
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
fn apply(store: &RuntimeStore, case: &Value) -> Result<Value, String> {
    let identity = TurnAttemptIdentity {
        agent_id: "agent".into(),
        epoch: 0,
        attempt_id: "attempt".into(),
    };
    let transition = case["transition"].as_str().expect("transition");
    match transition {
        "reconcile" => lifecycle_transaction(store, |tx| SchedulerRepository::reconcile_obsolete_before_submit(tx,"agent",0,"attempt").map(|proceed| json!(!proceed))).map_err(|e|e.to_string()),
        "claim" => claim_intent(store, "agent", 0, "attempt", 10.0)
            .map(|value| value.map(|v| json!({"agent_id":v.agent_id,"epoch":v.epoch,"attempt_id":v.attempt_id,"account_key":v.account_key,"provider":v.provider,"thread_id":v.thread_id,"cwd":v.cwd,"model":v.model,"events":v.events.iter().map(|e|json!({"id":e.id,"kind":e.kind,"text":e.text,"created":e.created,"epoch":e.epoch,"error":e.error,"metadata":e.metadata})).collect::<Vec<_>>()})).unwrap_or(Value::Null))
            .map_err(|e| e.to_string()),
        "submission" | "reconcile_submission" => lifecycle_transaction(store, |tx| {
            use studio_runtime_services::{account_worker::{commit_prepared_turn,preparation::{prepare_turn,PreparationInput,PreparationResources,persist_prepared_compilation}},scheduler::{TurnIntent,TurnEvent}};
            use studio_runtime_store::{AgentRepository,AgentId,QueueEventRepository,QueueEventId};
            let agent = AgentRepository::get(tx,&AgentId::from("agent"))?.expect("seed agent");
            let event = QueueEventRepository::get(tx,&QueueEventId::from("event"))?.expect("seed input");
            let intent = TurnIntent {agent_id:"agent".into(),epoch:0,attempt_id:"attempt".into(),account_key:"default".into(),provider:"codex".into(),thread_id:Some("thread".into()),cwd:Some("/fixture".into()),model:Some("fixture-model".into()),events:vec![TurnEvent {id:event.id.0,kind:event.kind,text:event.text,created:event.created,epoch:event.epoch,error:event.error,metadata:json!({})}]};
            let resources=PreparationResources::bundled(String::new()).map_err(|e|studio_runtime_store::StoreError::SchemaMismatch(e.to_string()))?;
            let state_root=case["preparationRoot"].as_str().map(Path::new).unwrap_or(store.root());
            let prepared=prepare_turn(PreparationInput {intent:&intent,agent:&agent,transaction:tx,resources:&resources,state_root,submitted_at:10.0}).map_err(studio_runtime_store::StoreError::SchemaMismatch)?;
            commit_prepared_turn(tx,&intent,case["ticket"].as_str().unwrap_or("ticket"),1,&prepared.submission).and_then(|v|{if v {persist_prepared_compilation(tx,&prepared.compiled)?;} Ok(json!(v))}).map_err(|e|match e {studio_runtime_services::account_worker::WorkerError::Store(e)=>e,_=>studio_runtime_store::StoreError::SchemaMismatch(e.to_string())})
        }).map_err(|e|e.to_string()),
        "accepted" => lifecycle_transaction(store, |tx| {
            SchedulerRepository::settle_started(
                tx,
                "agent",
                0,
                "attempt",
                case["turnId"].as_str().unwrap_or("turn"),
            )
            .map(|value| json!(value))
        })
        .map(|value| json!(value))
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
        .map(|value| json!(value))
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
            .map(|value| json!(value))
        })
        .map(|value| json!(value))
        .map_err(|e| e.to_string()),
        "completed" | "failed" | "interrupted" => {
            let mut turn = json!({"id":"turn","status":transition});
            if let Some(fields) = case["turn"].as_object() {
                for (k, v) in fields {
                    turn[k] = v.clone();
                }
            }
            settle_terminal_attempt(store, &identity, &json!({"threadId":"thread","turn":turn}))
                .map(|value| json!(value))
                .map_err(|e| e.to_string())
        }
        "interrupt_accepted" | "interrupt_rejected" | "interrupt_unknown" => {
            lifecycle_transaction(store, |tx| {
                let prepared = SchedulerRepository::prepare_interrupt(
                    tx,
                    "agent",
                    "thread",
                    "turn",
                    1,
                    "interrupt-ticket",
                )?;
                let settled = SchedulerRepository::settle_interrupt_result(
                    tx,
                    "interrupt-ticket",
                    1,
                    transition.trim_start_matches("interrupt_"),
                    match transition {
                        "interrupt_rejected" => Some("rejected"),
                        "interrupt_unknown" => Some("interrupt response unknown"),
                        _ => None,
                    },
                ).map(|value| json!(value))?;
                Ok(json!({"prepared": prepared, "settled": settled}))
            })
            .map(|value| json!(value))
            .map_err(|e| e.to_string())
        }
        "stop" | "pause" => lifecycle_transaction(store, |tx| {
            SchedulerRepository::stop_agent(tx, "agent", "Stopped by user").map(|value| json!(value))
        })
        .map(|value| json!(value))
        .map_err(|e| e.to_string()),
        "started" => lifecycle_transaction(store, |tx| {
            SchedulerRepository::observe_started(tx, &identity, "turn").map(|value| json!(value))
        })
        .map(|value| json!(value))
        .map_err(|e| e.to_string()),
        "restart" => recover_on_startup(store)
            .map(|value| json!(value))
            .map_err(|e| e.to_string()),
        "disconnect" => lifecycle_transaction(store, |tx| {
            SchedulerRepository::disconnect_account(tx, "default", 1, 20.0, false)
        })
        .map(|value| json!(value))
        .map_err(|e| e.to_string()),
        "reattach" => lifecycle_transaction(store, |tx| {
            SchedulerRepository::reconcile_generation(tx, "default", 1, 20.0)
        })
        .map(|value| json!(value))
        .map_err(|e| e.to_string()),
        "generation_change" => lifecycle_transaction(store, |tx| {
            SchedulerRepository::reconcile_generation(tx, "default", 2, 20.0)
        })
        .map(|value| json!(value))
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
    let notifications: Vec<Value> =
        serde_json::from_str(include_str!("turn_lifecycle/notification_cases.json"))
            .expect("notification cases");
    let registry: Value = serde_json::from_str(include_str!("turn_lifecycle/deviations.json"))
        .expect("central registry");
    let registry = registry.as_object().expect("class declarations");
    assert_eq!(
        registry.len(),
        4,
        "the remaining lifecycle and notification classes are closed"
    );
    let owners: std::collections::BTreeSet<_> = cases
        .iter()
        .flat_map(|c| c["expectedDifferences"].as_array().into_iter().flatten())
        .map(|rule| rule["deviation"].as_str().expect("class"))
        .chain(notifications.iter().filter_map(|c| c["deviation"].as_str()))
        .collect();
    assert_eq!(
        owners,
        registry.keys().map(String::as_str).collect(),
        "every case deviation is declared and every declaration has an owner, including unit witnesses"
    );
    let mutations: Vec<Value> = serde_json::from_str(include_str!("turn_lifecycle/mutations.json"))
        .expect("mutation inventory");
    for mutation in mutations {
        for witness in mutation["workerWitnesses"].as_array().into_iter().flatten() {
            let name = witness.as_str().expect("worker witness name");
            assert!(
                include_str!("../src/account_worker/tests.rs")
                    .contains(&format!("async fn {name}("))
                    || include_str!("../src/account_worker/tests/crash_matrix.rs")
                        .contains(&format!("async fn {name}(")),
                "missing worker witness {name}"
            );
        }
        for id in mutation["witnesses"]
            .as_array()
            .expect("mutation witnesses")
        {
            assert!(
                cases
                    .iter()
                    .chain(&notifications)
                    .any(|c| c["id"] == *id && c["tier"] == "fast"),
                "{}: missing fast mutation witness {id}",
                mutation["id"]
            );
        }
        for combination in mutation["lifecycleCombinations"]
            .as_array()
            .expect("transition/state combinations")
        {
            assert!(
                cases.iter().any(|c| c["tier"] == "fast"
                    && combination
                        .as_object()
                        .expect("combination")
                        .iter()
                        .all(|(key, value)| c[key] == *value)),
                "missing mutation transition/state combination: {combination}"
            );
        }
    }
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
    assert!(!cases.is_empty(), "case filter matched nothing");
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
        let store = match RuntimeStore::open(Some(&rust)) {
            Ok(store) => store,
            Err(error) => {
                let refusal = case
                    .get("expectRefusal")
                    .unwrap_or_else(|| panic!("{id}: open Python-written state: {error}"));
                let location = format!(
                    "{}/{}",
                    refusal["table"].as_str().expect("table"),
                    refusal["id"].as_str().expect("id")
                );
                let message = error.to_string();
                assert!(
                    message.contains(&location),
                    "{id}: startup refusal must locate {location}: {message}"
                );
                assert!(
                    !message.contains("CORPUS_PRIVATE_PAYLOAD")
                        && !message.contains("SECRET_STORED_SENTINEL")
                );
                let actual = oracle.call(json!({"command":"snapshot","root":rust}));
                assert_eq!(
                    actual, expected["before"],
                    "{id}: startup refusal must not write"
                );
                report.push(json!({"id":id,"differences":[],"unexpected":[],"error":null,"firstResult":null,"pythonResult":expected["entryResult"]}));
                green += 1;
                continue;
            }
        };
        if case["roundtripHold"] == true {
            lifecycle_transaction(&store, |tx| {
                SchedulerRepository::hold_account(tx, "default", Some(7), "first account hold")?;
                SchedulerRepository::hold_account(tx, "default", Some(8), "second account hold")?;
                SchedulerRepository::clear_account_hold(tx, "default")
            })
            .expect("nested hold restoration");
        }
        let mut prepared_case = case.clone();
        prepared_case["preparationRoot"] = expected["pythonRoot"].clone();
        let case = &prepared_case;
        let first = apply(&store, case);
        let mut error = first.as_ref().err().cloned();
        if let Ok(value) = &first {
            let normalized = oracle.call(json!({"command":"normalize","value":value}));
            assert_eq!(normalized, case["return"], "{id}: entry-point return value");
        }
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
            let before = store
                .transaction(|tx| Ok(tx.total_changes()))
                .expect("probe write counter");
            let refusal =
                apply(&store, &changed).expect_err("conflicting evidence must be refused");
            assert_eq!(
                refusal,
                case["probeRefusal"].as_str().expect("exact conflict error"),
                "{id}: exact conflict location"
            );
            let after = store
                .transaction(|tx| Ok(tx.total_changes()))
                .expect("probe write counter");
            assert_eq!(before, after, "conflicting replay wrote SQLite");
        }
        if let Some(outcome) = case["transition"]
            .as_str()
            .and_then(|v| v.strip_prefix("interrupt_"))
            && error.is_none()
        {
            let response_error = match outcome {
                "rejected" => Some("rejected"),
                "unknown" => Some("interrupt response unknown"),
                _ => None,
            };
            let before = store
                .transaction(|tx| Ok(tx.total_changes()))
                .expect("interrupt counter");
            let replay = lifecycle_transaction(&store, |tx| {
                SchedulerRepository::settle_interrupt_result(
                    tx,
                    "interrupt-ticket",
                    1,
                    outcome,
                    response_error,
                )
                .map(|value| json!(value))
            })
            .expect("interrupt settlement replay");
            assert_eq!(
                json!(replay),
                first.as_ref().expect("first result")["settled"],
                "{id}: saved interrupt result"
            );
            let refusal = lifecycle_transaction(&store, |tx| {
                SchedulerRepository::settle_interrupt_result(
                    tx,
                    "interrupt-ticket",
                    1,
                    outcome,
                    Some("CORPUS_PRIVATE_PAYLOAD"),
                )
            })
            .expect_err("changed interrupt result must refuse");
            assert_eq!(
                refusal.to_string(),
                "invalid stored sync object runtime_agents/agent"
            );
            let after = store
                .transaction(|tx| Ok(tx.total_changes()))
                .expect("interrupt counter");
            assert_eq!(
                before, after,
                "{id}: interrupt replay or refusal wrote SQLite"
            );
        }
        if case["replay"] == true && error.is_none() {
            let before = store
                .transaction(|tx| Ok(tx.total_changes()))
                .expect("write counter");
            let replay = apply(&store, case);
            assert_eq!(
                replay, first,
                "{id}: replay must return the saved first result"
            );
            error = replay.err();
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
                message
                    == refusal["message"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!(
                            "invalid stored sync object {}/{}",
                            refusal["table"].as_str().expect("refusal table"),
                            refusal["id"].as_str().expect("refusal id")
                        )),
                "{id}: error must locate table and stored id only: {message}"
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
        report.push(json!({"id":id,"differences":diff,"unexpected":unexpected,"error":error,"firstResult":first.ok(),"pythonResult":expected["entryResult"]}));
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
fn oracle_guard_refuses_synthetic_provider_home_access() {
    let python = prepared_python::prepared_python(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
    );
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
        "guard refused synthetic provider-home content and metadata access"
    );
}
