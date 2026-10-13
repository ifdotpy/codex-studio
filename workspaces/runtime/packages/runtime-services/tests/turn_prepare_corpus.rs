//! Real Python decisions over immutable observations; no Runtime or provider is created.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use studio_runtime_services::turn_prepare::{
    ContextManifest, Event, NativeReceipt, PreparationSnapshot, PrepareError, accept, compile,
};

struct Oracle {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Oracle {
    fn new() -> Self {
        let python = std::env::var_os("CODEX_AGENTS_PYTHON")
            .expect("set CODEX_AGENTS_PYTHON to the equipped isolated interpreter (read-only)");
        let mut child = Command::new(python)
            .arg("-B")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/turn_prepare/oracle.py"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start guarded preparation oracle");
        let input = child.stdin.take().expect("oracle input");
        let output = BufReader::new(child.stdout.take().expect("oracle output"));
        Self {
            child,
            input,
            output,
        }
    }
    fn call(&mut self, value: Value) -> Value {
        writeln!(self.input, "{value}").expect("write oracle request");
        self.input.flush().expect("flush oracle request");
        let mut line = String::new();
        self.output
            .read_line(&mut line)
            .expect("read oracle response");
        assert!(!line.is_empty(), "oracle exited before response");
        serde_json::from_str(&line).expect("oracle JSON")
    }
}
impl Drop for Oracle {
    fn drop(&mut self) {
        let _ignored = writeln!(self.input, "{{\"command\":\"close\"}}");
        let _ignored = self.input.flush();
        self.child.wait().expect("reap oracle");
    }
}

fn difference(left: &Value, right: &Value, path: &str) -> Option<String> {
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            for key in a.keys().chain(b.keys()) {
                if !a.contains_key(key) || !b.contains_key(key) {
                    return Some(format!("{path}.{key}: absent versus present"));
                }
                if let Some(diff) = difference(&a[key], &b[key], &format!("{path}.{key}")) {
                    return Some(diff);
                }
            }
            None
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                return Some(format!("{path}: array lengths {} != {}", a.len(), b.len()));
            }
            a.iter()
                .zip(b)
                .enumerate()
                .find_map(|(i, (a, b))| difference(a, b, &format!("{path}[{i}]")))
        }
        _ if left == right => None,
        _ => Some(format!(
            "{path}: Rust {} != Python {}",
            preview(left),
            preview(right)
        )),
    }
}
fn preview(value: &Value) -> String {
    value.to_string().chars().take(300).collect()
}

#[test]
fn python_submission_compiler_corpus() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("turn_prepare/cases.json")).expect("corpus data");
    let full = std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some();
    let filter = std::env::var("TURN_PREPARE_CASE").ok();
    let mut oracle = Oracle::new();
    let mut count = 0;
    for case in cases.iter().filter(|c| {
        (full || c["fast"] == true)
            && filter
                .as_ref()
                .is_none_or(|s| c["id"].as_str().is_some_and(|id| id.contains(s)))
    }) {
        let fixture = oracle.call(json!({"command":"case","case":case}));
        let snapshot: PreparationSnapshot =
            serde_json::from_value(fixture["snapshot"].clone()).expect("typed snapshot");
        let rows: Vec<Event> =
            serde_json::from_value(fixture["events"].clone()).expect("typed events");
        let original = json!(snapshot);
        let mut actual = match compile(&snapshot, &rows) {
            Ok(prepared) => {
                assert!(
                    fixture["expected"]["ok"] == true,
                    "{}: Rust prepared a submission but Python rejected it: {}",
                    case["id"],
                    fixture["expected"]["error"]
                );
                // Replay compilation without artifact observations: it must defer, returning
                // the exact immutable content needed by the adapter, never a ready request.
                if !prepared.artifact_intents.is_empty() {
                    let mut unresolved = snapshot.clone();
                    unresolved.artifact_paths.clear();
                    match compile(&unresolved, &rows) {
                        Err(PrepareError::ArtifactsRequired(intents)) => assert!(
                            json!(intents) == json!(prepared.artifact_intents),
                            "{}: artifact intent mismatch",
                            case["id"]
                        ),
                        _ => panic!("{}: unresolved evidence did not defer", case["id"]),
                    }
                }
                let receipt: NativeReceipt =
                    serde_json::from_value(fixture["receipt"].clone()).expect("receipt");
                let current: Option<ContextManifest> =
                    serde_json::from_value(fixture["current"].clone()).expect("delivered manifest");
                let delivery = accept(&prepared, &receipt, current.as_ref());
                let mut value = json!(prepared);
                value["ok"] = json!(true);
                value["delivery"] = json!(delivery);
                value
            }
            Err(error) => json!({"ok":false,"error":json!(error)["kind"]}),
        };
        assert!(
            json!(snapshot) == original,
            "compiler mutated its input snapshot"
        );
        actual =
            oracle.call(json!({"command":"normalize","tokens":fixture["tokens"],"value":actual}));
        if let Some(diff) = difference(&actual, &fixture["expected"], "$") {
            panic!("{} ({}) {diff}", case["id"], case["family"]);
        }
        count += 1;
    }
    assert!(count > 0, "case selection was empty");
    eprintln!(
        "turn_prepare: {count} differential cases ({})",
        if full { "full" } else { "fast" }
    );
}

#[test]
fn malformed_snapshot_fields_are_refused_at_the_typed_boundary() {
    let mut oracle = Oracle::new();
    let fixture = oracle.call(json!({"command":"case","case":{}}));
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("turn_prepare/invalid_snapshots.json"))
            .expect("invalid fixture data");
    for case in cases {
        let mut snapshot = fixture["snapshot"].clone();
        let path = case["path"].as_str().expect("fixture pointer");
        *snapshot.pointer_mut(path).expect("existing field") = case["value"].clone();
        assert!(
            serde_json::from_value::<PreparationSnapshot>(snapshot).is_err(),
            "accepted invalid field {path}"
        );
    }
    for value in [json!("yesterday"), json!(false), json!([]), json!({})] {
        let mut row = fixture["events"][0].clone();
        row["metadata"]["accepted_at"] = value;
        assert!(
            serde_json::from_value::<Event>(row).is_err(),
            "accepted invalid receipt clock type"
        );
    }
}
