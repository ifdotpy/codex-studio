//! Both sides start from a committed Python-written database, never a Rust snapshot.
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use studio_runtime_services::scheduler::{
    claim_intent, recover_on_startup, settle_terminal_attempt,
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
        self.child.kill().expect("stop oracle");
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
        "submission" => store
            .transaction(|tx| {
                SchedulerRepository::mark_submitted_identity(
                    tx, "agent", 0, "attempt", "ticket", "default", 1, "thread",
                )
                .map(|_| ())
            })
            .map_err(|e| e.to_string()),
        "accepted" => store
            .transaction(|tx| {
                SchedulerRepository::settle_started(tx, "agent", 0, "attempt", "turn").map(|_| ())
            })
            .map_err(|e| e.to_string()),
        "unknown" | "not_sent" => store
            .transaction(|tx| {
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
        "rejected" => store
            .transaction(|tx| {
                SchedulerRepository::settle_start_rejected_provider(
                    tx,
                    "agent",
                    0,
                    "attempt",
                    "ticket",
                    "default",
                    1,
                    "thread",
                    json!({"message":"rejected"}),
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
        "started" => store
            .transaction(|tx| {
                SchedulerRepository::observe_started(tx, &identity, "turn").map(|_| ())
            })
            .map_err(|e| e.to_string()),
        "restart" => recover_on_startup(store)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        "disconnect" => store
            .transaction(|tx| {
                SchedulerRepository::disconnect_account(tx, "default", 1, 20.0, false)
            })
            .map_err(|e| e.to_string()),
        "generation_change" => store
            .transaction(|tx| SchedulerRepository::reconcile_generation(tx, "default", 2, 20.0))
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
#[test]
fn python_written_turn_lifecycle_corpus() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("turn_lifecycle/cases.json")).expect("corpus data");
    let root = State(std::env::temp_dir().join(format!("turn-corpus-{}", std::process::id())));
    fs::create_dir(&root.0).expect("unique corpus state");
    let mut oracle = Oracle::new(&root.0);
    let mut report = vec![];
    let mut green = 0;
    for case in &cases {
        let id = case["id"].as_str().expect("case ID");
        let rust = root.0.join(id).join("rust");
        let expected = oracle.call(
            json!({"command":"seed","root":root.0.join(id).join("python"),"rust":rust,"case":case}),
        );
        let store = RuntimeStore::open(Some(&rust)).expect("open Python-written state");
        let mut error = apply(&store, case).err();
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
        differences(Some(&expected["after"]), Some(&actual), "", &mut diff);
        if diff.is_empty() && error.is_none() {
            green += 1;
        } else {
            eprintln!("{id}: {} differences; error={error:?}", diff.len());
        }
        report.push(json!({"id":id,"differences":diff,"error":error}));
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
