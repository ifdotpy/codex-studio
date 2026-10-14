#[path = "support/prepared_python.rs"]
mod prepared_python;

use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use studio_runtime_services::scheduler::recover_on_startup;
use studio_runtime_store::RuntimeStore;

struct Oracle {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Oracle {
    fn new(scratch: &Path) -> Self {
        let python = prepared_python::prepared_python(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
        );
        let mut child = Command::new(python)
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/turn_lifecycle/oracle.py"))
            .arg(scratch)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Python recovery oracle starts");
        Self {
            input: child.stdin.take().expect("oracle stdin"),
            output: BufReader::new(child.stdout.take().expect("oracle stdout")),
            child,
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
        let _ = writeln!(self.input, "{{\"command\":\"close\"}}");
        let _ = self.input.flush();
        self.child.wait().expect("reap Python oracle");
    }
}
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove recovery scratch state");
    }
}

fn rust_state(store: &RuntimeStore) -> Value {
    let db = store.read().expect("read recovered store");
    let mut agents = Vec::new();
    let mut statement = db
        .prepare("SELECT id,record FROM runtime_agents ORDER BY id")
        .expect("prepare agents");
    for row in statement
        .query_map([], |row| {
            let id: String = row.get(0)?;
            let record: String = row.get(1)?;
            Ok((id, record))
        })
        .expect("query agents")
    {
        let (id, record) = row.expect("agent row");
        agents.push(
            json!({"id":id,"record":serde_json::from_str::<Value>(&record).expect("agent JSON")}),
        );
    }
    let mut events = Vec::new();
    let mut statement = db
        .prepare("SELECT id,agent,kind,text,status,created,epoch,turn_id,error FROM runtime_events ORDER BY id")
        .expect("prepare events");
    for row in statement
        .query_map([], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?, "agent": row.get::<_, String>(1)?,
                "kind": row.get::<_, String>(2)?, "text": row.get::<_, String>(3)?,
                "status": row.get::<_, String>(4)?, "created": row.get::<_, f64>(5)?,
                "epoch": row.get::<_, i64>(6)?, "turn_id": row.get::<_, Option<String>>(7)?,
                "error": row.get::<_, Option<String>>(8)?,
            }))
        })
        .expect("query events")
    {
        events.push(row.expect("event row"));
    }
    json!({"agents":agents,"events":events})
}

fn diff_paths(left: &Value, right: &Value, path: &str, out: &mut Vec<String>) {
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            for key in a
                .keys()
                .chain(b.keys())
                .collect::<std::collections::BTreeSet<_>>()
            {
                diff_paths(
                    &a.get(key).cloned().unwrap_or(Value::Null),
                    &b.get(key).cloned().unwrap_or(Value::Null),
                    &format!("{path}/{key}"),
                    out,
                );
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (index, (a, b)) in a.iter().zip(b).enumerate() {
                diff_paths(a, b, &format!("{path}/{index}"), out);
            }
        }
        _ if left == right => {}
        _ => out.push(path.to_owned()),
    }
}

fn registry_path(path: &str) -> String {
    let parts = path.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        ["", "agents", _index, "record", rest @ ..] => {
            format!("/runtime_agents/*/record/{}", rest.join("/"))
        }
        ["", "events", _index, field, rest @ ..] => {
            let mut tail = vec![*field];
            tail.extend_from_slice(rest);
            format!("/runtime_events/*/{}", tail.join("/"))
        }
        _ => path.to_owned(),
    }
}

#[test]
fn python_written_completed_start_recovery_exclusion_matches_rust_scans() {
    let base =
        std::env::temp_dir().join(format!("restart-recovery-exclusion-{}", std::process::id()));
    fs::create_dir_all(&base).expect("create scratch parent");
    let _scratch = Scratch(base.clone());
    let mut oracle = Oracle::new(&base);
    let cases = [
        ("completed_start", json!({}), Some("completed")),
        ("in_flight", json!({}), Some("written")),
        ("action", json!({"startAttempt.action":"turn/start"}), None),
        (
            "active_reservation",
            json!({"startAttempt.activeAtReservation":true}),
            None,
        ),
        (
            "outcome_hold",
            json!({"startOutcomeHold":{"stage":"account_settlement"}}),
            None,
        ),
        (
            "response_error",
            json!({"startAttempt.responseError":"lost response"}),
            None,
        ),
        (
            "unknown_outcome",
            json!({"startAttempt.executionOutcome":"unknown"}),
            None,
        ),
        (
            "different_turn",
            json!({"startAttempt.turnId":"other-turn"}),
            None,
        ),
        (
            "non_completed_last_status",
            json!({"lastCompletedTurnStatus":"failed"}),
            None,
        ),
        ("outside_idle_status", json!({"status":"queued"}), None),
        ("auth_pin_changed", json!({}), Some("auth_pin_changed")),
    ];
    let registry: Value = serde_json::from_str(include_str!("turn_lifecycle/deviations.json"))
        .expect("central deviations");
    for (index, (id, mutations, flow)) in cases.iter().enumerate() {
        let root = base.join(format!("python-{index}"));
        let rust = base.join(format!("rust-{index}"));
        let result = oracle.call(json!({
            "command":"run", "module":"recovery_exclusion",
            "case":{"id":id,"mutations":mutations,"flow":flow},
            "root":root,"rust":rust
        }));
        if let Some(flow) = flow {
            assert_eq!(
                result["flow"],
                json!(flow),
                "{id}: oracle did not execute the requested fixture flow"
            );
            if *flow != "auth_pin_changed" {
                assert_eq!(
                    result["providerFrameCount"], 1,
                    "{id}: scripted provider did not receive exactly one turn/start frame"
                );
            }
        }
        let store = RuntimeStore::open(Some(&rust)).expect("open Python-written Rust state");
        recover_on_startup(&store).unwrap_or_else(|error| {
            panic!("run both Rust startup recovery scans for {id}: {error}")
        });
        let actual = oracle.call(json!({"command":"normalize","value":rust_state(&store)}));
        let expected = oracle.call(json!({"command":"normalize","value":result["after"]}));
        let mut paths = Vec::new();
        diff_paths(&actual, &expected, "", &mut paths);
        if matches!(*id, "unknown_outcome" | "in_flight") {
            let deviation = &registry["durable_reconciliation_hold"];
            let fields = deviation["fields"].as_array().expect("registered fields");
            assert!(!paths.is_empty(), "{id}: registered difference disappeared");
            for path in &paths {
                let normalized = registry_path(path);
                assert!(
                    fields.iter().any(|field| field == &json!(normalized)),
                    "{id}: unregistered recovery difference at {path} ({normalized})"
                );
            }
        } else {
            assert!(
                paths.is_empty(),
                "{id}: unregistered difference: {paths:?}; actual={actual}; expected={expected}"
            );
        }
        drop(store);
    }
}
