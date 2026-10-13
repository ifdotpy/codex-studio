//! Pure gates versus real Python functions, with an isolated, guarded oracle.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};
use studio_runtime_services::submit_gates::{
    AuthorizationRecords, AuthorizationSnapshot, CatalogIdentity, CatalogObservation,
    ExecutionSettings, ExecutionSnapshot, authorize_events, budget_admission, select_execution,
    validate_execution,
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
fn profile(case: &Value, record: &Value, key: &str) -> CatalogIdentity {
    let p = case.get(key).unwrap_or(&case["profile"]);
    CatalogIdentity {
        account_key: record["accountKey"].as_str().unwrap_or("default").into(),
        home: p["home"].as_str().expect("profile home").into(),
        provider: p["provider"].as_str().expect("provider").into(),
        generation: p["generation"].as_u64().expect("generation"),
    }
}
fn catalog(case: &Value, now: Instant) -> Option<CatalogObservation> {
    (!case["catalog"].is_null()).then(|| CatalogObservation {
        identity: CatalogIdentity {
            account_key: case["catalog"]["account_key"]
                .as_str()
                .expect("catalog account")
                .into(),
            home: case["catalog"]["home"].as_str().expect("home").into(),
            provider: case["catalog"]["provider"]
                .as_str()
                .expect("provider")
                .into(),
            generation: case["catalog"]["generation"].as_u64().expect("generation"),
        },
        expires_at: now
            + Duration::from_secs(case["catalog"]["lifetime"].as_u64().expect("expiry")),
        models: serde_json::from_value(case["catalog"]["models"].clone())
            .expect("model capabilities"),
    })
}
fn rejection(error: impl serde::Serialize) -> Value {
    json!({"allowed":false,"denial":error})
}
fn rust(case: &Value, records: Option<&Value>) -> Value {
    match case["gate"].as_str().expect("gate") {
        "authorization" => {
            let records: AuthorizationRecords =
                serde_json::from_value(records.expect("Python-written rows").clone())
                    .expect("rows");
            let snapshot = match AuthorizationSnapshot::from_records(&records) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return json!({"allowed":false,"denial":"invalid-stored-input","location":error.location});
                }
            };
            match authorize_events(&snapshot) {
                Ok(()) => json!({"allowed":true}),
                Err(error) => rejection(error),
            }
        }
        "execution" => {
            let current = match ExecutionSnapshot::from_record(
                &case["current"],
                profile(case, &case["current"], "profile"),
            ) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return json!({"allowed":false,"denial":"invalid-stored-input","location":error.location});
                }
            };
            let before = case.get("observed").unwrap_or(&case["current"]);
            let observed =
                ExecutionSnapshot::from_record(before, profile(case, before, "observed_profile"))
                    .expect("observed snapshot");
            let now = Instant::now();
            let catalog = catalog(case, now);
            let selected = select_execution(&current, &observed, catalog.as_ref(), now);
            if let Some(expected) = case.get("selection_denial") {
                assert_eq!(
                    selected.as_ref().err().map(|error| json!(error)),
                    Some(expected.clone()),
                    "selection must refuse before producing a freeze patch"
                );
            }
            match selected {
                Ok(selection) => {
                    let record = case.get("commit_record").unwrap_or(&case["current"]);
                    let account = profile(case, record, "commit_profile");
                    match selection.freeze_record(
                        record,
                        &account,
                        now + Duration::from_secs(case["commit_delay"].as_u64().unwrap_or(0)),
                    ) {
                        Ok(after) => {
                            json!({"allowed":true,"record_after":after,"provider_settings":selection.provider_settings(),
                            "freeze":selection.freeze.is_some(),"invalidated_loaded":selection.freeze.is_some() && selection.expected.pending.is_some()})
                        }
                        Err(error) => rejection(error),
                    }
                }
                Err(error) => rejection(error),
            }
        }
        "validation" => {
            let settings: ExecutionSettings =
                serde_json::from_value(case["settings"].clone()).expect("typed settings");
            match validate_execution(
                &settings,
                &catalog(case, Instant::now()).expect("catalog"),
                case["fallback_effort"].as_bool().unwrap_or(false),
            ) {
                Ok(efforts) => json!({"allowed":true,"efforts":efforts}),
                Err(error) => rejection(error),
            }
        }
        "budget" => match budget_admission(case.get("budget")) {
            Ok(()) => json!({"allowed":true}),
            Err(error) => rejection(error.to_string()),
        },
        _ => panic!("unknown gate"),
    }
}
fn compare(oracle: &mut Oracle, name: &str, case: &Value, deviation: Option<&str>) {
    let mut python = oracle.call(case);
    let actual = rust(case, python.get("records"));
    python.as_object_mut().expect("result").remove("records");
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
        let keys: Vec<_> = patch["path"]
            .as_str()
            .expect("path")
            .trim_start_matches('/')
            .split('/')
            .map(|s| s.replace("~1", "/").replace("~0", "~"))
            .collect();
        let mut parent = &mut case;
        for key in &keys[..keys.len() - 1] {
            parent = if parent.is_array() {
                &mut parent[key.parse::<usize>().expect("index")]
            } else {
                &mut parent[key]
            };
        }
        let key = keys.last().expect("key");
        if patch["remove"] == true {
            parent.as_object_mut().expect("object").remove(key);
        } else if parent.is_array() {
            parent[key.parse::<usize>().expect("index")] = patch["value"].clone();
        } else {
            parent[key] = patch["value"].clone();
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
                        case["records"]["agents"]["sender"]["rootId"] = root.clone();
                        case["records"]["agents"]["sender"]["deletedAt"] = json!(deleted);
                        let mut payload: Value =
                            serde_json::from_str(case["events"][0]["text"].as_str().expect("text"))
                                .expect("payload");
                        payload["sender"] = sender.clone();
                        case["events"][0]["text"] = json!(payload.to_string());
                        if broadcast {
                            case["records"]["rooms"]["room"] =
                                json!({"kind":"broadcast","rootId":root});
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
                                    case["current"]["executionSettingsAccountKey"] = json!("old");
                                    case["current"]["startAttempt"]["settingsFixed"] = json!(fixed);
                                    case["current"]["model"] = model.clone();
                                    case["current"]["effort"] = effort.clone();
                                    case["current"]["fastMode"] = json!(fast);
                                    case["catalog"]["models"][0]["priority"] = json!(priority);
                                    case["catalog"]["models"][0]["hidden"] = json!(hidden);
                                    if pending {
                                        case["current"]["pendingSettings"] = json!({"model":model,"effort":effort,"nativeEffort":"medium","fastMode":fast,"daybreakEnabled":false});
                                        case["current"]["pendingSettingsAccountKey"] =
                                            json!("account");
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
fn malformed_settings_fail_closed_at_typed_boundary() {
    let fixtures: Value =
        serde_json::from_str(include_str!("submit_gates/cases.json")).expect("fixtures");
    for value in [json!("yes"), json!(0), json!({})] {
        let mut case = fixtures["seeds"]["execution"].clone();
        case["current"]["fastMode"] = value;
        assert_eq!(rust(&case, None)["allowed"], false);
    }
}

#[test]
fn oracle_boundaries_and_separate_preparation_default() {
    let mut oracle = Oracle::new();
    assert_eq!(
        oracle.call(&json!({"gate":"boundary-guard"})),
        json!({"allowed":true})
    );
    let fixtures: Value =
        serde_json::from_str(include_str!("submit_gates/cases.json")).expect("fixtures");
    let mut case = fixtures["seeds"]["execution"].clone();
    case["gate"] = json!("preparation-effort");
    case["current"]
        .as_object_mut()
        .expect("record")
        .remove("nativeEffort");
    let prepared = oracle.call(&case);
    assert_eq!(prepared["record_after"]["nativeEffort"], "medium");
    assert_eq!(
        prepared["provider_settings"],
        json!({"serviceTier":"default","effort":"medium"})
    );
    case["current"]["nativeEffort"] = Value::Null;
    assert_eq!(
        oracle.call(&case)["provider_settings"],
        json!({"serviceTier":"default"})
    );
}
