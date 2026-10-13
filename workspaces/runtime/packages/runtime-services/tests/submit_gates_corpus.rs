//! Pure gates versus real Python functions, with an isolated, guarded oracle.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};
use studio_runtime_services::submit_gates::{
    AuthorizationSnapshot, CatalogObservation, EventSnapshot, ExecutionSnapshot, authorize_events,
    budget_admission, select_execution,
};

struct Oracle {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Oracle {
    fn new() -> Self {
        let python = std::env::var_os("CODEX_AGENTS_PYTHON")
            .expect("set CODEX_AGENTS_PYTHON to the prepared isolated interpreter");
        for key in ["HOME", "CODEX_HOME", "CLAUDE_CONFIG_DIR"] {
            assert!(
                std::env::var(key)
                    .expect("disposable environment")
                    .starts_with("/var/tmp/")
            );
        }
        let mut child = Command::new(python)
            .arg("-B")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/submit_gates/oracle.py"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("guarded Python oracle");
        Self {
            input: child.stdin.take().expect("stdin"),
            output: BufReader::new(child.stdout.take().expect("stdout")),
            child,
        }
    }
    fn call(&mut self, value: &Value) -> Value {
        writeln!(self.input, "{value}").expect("write oracle case");
        self.input.flush().expect("flush");
        let mut line = String::new();
        self.output.read_line(&mut line).expect("read oracle");
        serde_json::from_str(&line).expect("oracle JSON response")
    }
}
impl Drop for Oracle {
    fn drop(&mut self) {
        let _ignored = writeln!(self.input, "{{\"close\":true}}");
        let _ignored = self.input.flush();
        assert!(self.child.wait().expect("reap oracle").success());
    }
}
fn rust(case: &Value) -> Value {
    match case["gate"].as_str().expect("gate") {
        "authorization" => {
            let snapshot: AuthorizationSnapshot =
                serde_json::from_value(case["snapshot"].clone()).expect("typed authority");
            let events: Vec<EventSnapshot> =
                serde_json::from_value(case["events"].clone()).expect("typed events");
            match authorize_events(&snapshot, &events) {
                Ok(()) => json!({"allowed":true}),
                Err(error) => {
                    json!({"allowed":false,"denial":serde_json::to_value(error).expect("denial")})
                }
            }
        }
        "execution" => {
            let current: ExecutionSnapshot =
                serde_json::from_value(case["current"].clone()).expect("typed current");
            let observed: ExecutionSnapshot =
                serde_json::from_value(case.get("observed").unwrap_or(&case["current"]).clone())
                    .expect("typed observed");
            let now = Instant::now();
            let catalog = (!case["catalog"].is_null()).then(|| CatalogObservation {
                account_key: case["catalog"]["account_key"]
                    .as_str()
                    .expect("account")
                    .into(),
                generation: case["catalog"]["generation"].as_u64().expect("generation"),
                expires_at: now
                    + Duration::from_secs(case["catalog"]["lifetime"].as_u64().expect("expiry")),
                models: serde_json::from_value(case["catalog"]["models"].clone())
                    .expect("typed models"),
            });
            match select_execution(&current, &observed, catalog.as_ref(), now) {
                Ok(selection) => json!({"allowed":true,"selection":selection}),
                Err(error) => {
                    json!({"allowed":false,"denial":serde_json::to_value(error).expect("denial")})
                }
            }
        }
        "budget" => match budget_admission(case["budget"].as_u64()) {
            Ok(()) => json!({"allowed":true}),
            Err(error) => json!({"allowed":false,"denial":error.to_string()}),
        },
        _ => panic!("unknown gate"),
    }
}
fn compare(oracle: &mut Oracle, name: &str, case: &Value, deviation: Option<&str>) {
    let python = oracle.call(case);
    let actual = rust(case);
    // This assertion is deliberately independent of the deviation list: no
    // Python denial can ever be transformed into a Rust allow.
    assert!(
        !(python["allowed"] == false && actual["allowed"] == true),
        "{name}: Python denial became Rust allow: {case}\nPython={python}\nRust={actual}"
    );
    if let Some(denial) = deviation {
        assert_eq!(
            actual["allowed"], false,
            "{name}: required refusal disappeared"
        );
        assert_eq!(actual["denial"], denial, "{name}: wrong named refusal");
    }
    if python["allowed"] == true && actual["allowed"] == false {
        assert_eq!(
            actual["denial"].as_str(),
            deviation,
            "{name}: unapproved refusal: {case}\nPython={python}\nRust={actual}"
        );
    } else if actual["allowed"] == true {
        assert_eq!(actual, python, "{name}: selection/freeze differs: {case}");
    }
}
fn patched(seed: &Value, witness: &Value) -> Value {
    let mut case = seed.clone();
    for patch in witness["patches"].as_array().expect("patches") {
        let path = patch["path"].as_str().expect("path");
        if path == "/observed" {
            case["observed"] = patch["value"].clone();
        } else {
            *case.pointer_mut(path).expect("existing fixture path") = patch["value"].clone();
        }
    }
    case
}
#[test]
fn submit_gates_python_corpus() {
    let fixtures: Value =
        serde_json::from_str(include_str!("submit_gates/cases.json")).expect("corpus");
    let mut oracle = Oracle::new();
    let mut count = 0;
    for witness in fixtures["witnesses"].as_array().expect("witnesses") {
        let name = witness["name"].as_str().expect("name");
        let case = patched(
            &fixtures["seeds"][witness["gate"].as_str().expect("gate")],
            witness,
        );
        compare(&mut oracle, name, &case, witness["deviation"].as_str());
        count += 1;
    }
    if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some() {
        let matrix = &fixtures["matrix"];
        for root in matrix["roots"].as_array().expect("roots") {
            for sender in matrix["senders"].as_array().expect("senders") {
                for broadcast in [false, true] {
                    for deleted in [false, true] {
                        let mut case = fixtures["seeds"]["authorization"].clone();
                        case["snapshot"]["agents"]["sender"]["root_id"] = root.clone();
                        case["snapshot"]["agents"]["sender"]["deleted"] = json!(deleted);
                        case["events"][0]["sender"] = sender.clone();
                        if broadcast {
                            case["snapshot"]["rooms"]["room"] =
                                json!({"kind":"broadcast","root_id":root});
                        }
                        compare(&mut oracle, "authorization-matrix", &case, None);
                        count += 1;
                    }
                }
            }
        }
        for model in matrix["models"].as_array().expect("models") {
            for effort in matrix["efforts"].as_array().expect("efforts") {
                for fixed in [false, true] {
                    for pending in [false, true] {
                        for fast in [false, true] {
                            for priority in [false, true] {
                                for hidden in [false, true] {
                                    let mut case = fixtures["seeds"]["execution"].clone();
                                    case["current"]["execution_settings_account_key"] =
                                        json!("old");
                                    case["current"]["settings_fixed"] = json!(fixed);
                                    case["current"]["settings"]["model"] = model.clone();
                                    case["current"]["settings"]["effort"] = effort.clone();
                                    case["current"]["settings"]["fast_mode"] = json!(fast);
                                    case["catalog"]["models"][0]["priority"] = json!(priority);
                                    case["catalog"]["models"][0]["hidden"] = json!(hidden);
                                    if pending {
                                        case["current"]["pending"] = json!({"account_key":"account","settings":case["current"]["settings"]});
                                    }
                                    compare(&mut oracle, "execution-matrix", &case, None);
                                    count += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        for budget in matrix["budgets"].as_array().expect("budgets") {
            for spent in [0, 1, 10, 100] {
                let case = json!({"gate":"budget","budget":budget,"spent":spent});
                compare(
                    &mut oracle,
                    "budget-matrix",
                    &case,
                    (!budget.is_null()).then_some("budget-policy/coverage-unavailable"),
                );
                count += 1;
            }
        }
    }
    eprintln!("submit-gates differential cases: {count}");
}

#[test]
fn malformed_snapshots_fail_closed_at_typed_boundary() {
    let fixtures: Value =
        serde_json::from_str(include_str!("submit_gates/cases.json")).expect("fixtures");
    for value in [json!(null), json!("yes"), json!(0), json!({})] {
        let mut current = fixtures["seeds"]["execution"]["current"].clone();
        current["settings"]["fast_mode"] = value.clone();
        assert!(serde_json::from_value::<ExecutionSnapshot>(current).is_err());
        let mut snapshot = fixtures["seeds"]["authorization"]["snapshot"].clone();
        snapshot["rooms"]["room"]["members"] = value;
        assert!(serde_json::from_value::<AuthorizationSnapshot>(snapshot).is_err());
    }
}
