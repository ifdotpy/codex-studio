use serde_json::{Value, json};
use studio_runtime_services::task_recovery::{TaskRecoveryDecision, TaskRecoverySnapshot};
#[test]
fn exact_task_receipts_match_python_and_unknown_local_outcomes_override_native() {
    let result = std::process::Command::new(
        std::env::var_os("CODEX_AGENTS_PYTHON").expect("prepared Python required"),
    )
    .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/recovery/task_oracle.py"))
    .output()
    .expect("valid recovery fixture");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let cases: Vec<Value> = serde_json::from_slice(&result.stdout).expect("valid recovery fixture");
    assert_eq!(cases.len(), 34);
    for case in cases {
        let snapshot = TaskRecoverySnapshot::from_records(
            case["agent"].clone(),
            case["task"].clone(),
            serde_json::from_value(case["aliases"].clone()).expect("valid recovery fixture"),
            vec![("receipt".into(), case["receipt"].clone())],
        )
        .expect("valid recovery fixture");
        assert_eq!(
            snapshot.terminal_receipt().cloned().unwrap_or(Value::Null),
            case["selected"],
            "{}",
            case["name"]
        );
        let proof = snapshot.decide(
            &[json!({"id":"item","type":"dynamicToolCall","status":"completed","success":false})],
            Some(&case["receipt"]),
        );
        let actual = match proof {
            TaskRecoveryDecision::Complete(item) => item,
            TaskRecoveryDecision::Unconfirmed => Value::Null,
        };
        assert_eq!(actual, case["proof"], "{}", case["name"]);
    }
}
#[test]
fn malformed_persisted_recovery_inputs_are_located() {
    use studio_runtime_services::recovery_policy::RecoverySnapshot;
    let base = json!({"id":"agent","rootId":"agent","epoch":0,"status":"running"});
    for (field, value) in [
        ("epoch", json!(-1)),
        ("startAttempt", json!({"created":"not a clock"})),
        (
            "startAttempt",
            json!({"supervisorIdentity":{"generation":true}}),
        ),
        ("inFlight", json!("yes")),
        ("startAttempt", json!([])),
        ("threadId", json!(1)),
        ("autoWake", json!(0)),
        ("id", json!("")),
        ("startAttempt", json!({"events":vec!["event";33]})),
    ] {
        let mut invalid = base.clone();
        invalid[field] = value;
        let error = match RecoverySnapshot::from_record(invalid) {
            Ok(_) => panic!("accepted invalid {field}"),
            Err(e) => e,
        };
        assert!(error.location.starts_with("runtime_agents"));
    }
}

#[test]
fn task_consumer_resolves_local_receipt_without_native_read() {
    use std::time::Instant;
    use studio_runtime_services::{recovery::RecoveryError, recovery_history::*, task_recovery::*};
    struct Reader;
    impl RecoveryReader for Reader {
        fn observation(
            &mut self,
            scope: &ConnectionScope,
        ) -> Result<studio_runtime_services::recovery_absence::RecoveryObservation, HistoryError>
        {
            Ok(
                studio_runtime_services::recovery_absence::RecoveryObservation {
                    scope: scope.clone(),
                    at: 100.0,
                    supervisor: None,
                    quiet: false,
                    journal_drained: false,
                    account_reserved: false,
                    tools_refreshing: false,
                    preparation_pending: false,
                    completed_turn: None,
                },
            )
        }

        fn read(
            &mut self,
            _: &ConnectionScope,
            _: HistoryMethod,
            _: Value,
            _: Instant,
        ) -> Result<HistoryReceipt, HistoryError> {
            panic!("local receipt must win")
        }
        fn barrier(&mut self, _: &ConnectionScope, _: Instant) -> Result<(), HistoryError> {
            panic!("no native read")
        }
    }
    #[derive(Default)]
    struct Effects {
        completed: Vec<Value>,
    }
    impl TaskRecoveryEffects for Effects {
        fn apply_absence(
            &mut self,
            _: &TaskRecoverySnapshot,
            _: &ConnectionScope,
            _: &studio_runtime_services::recovery_absence::RecoveryObservation,
            _: studio_runtime_services::recovery_absence::TaskAbsenceDecision,
        ) -> Result<bool, RecoveryError> {
            Ok(false)
        }

        fn resolve_receipt(&mut self, receipt: &Value) -> Result<Value, RecoveryError> {
            Ok(receipt.clone())
        }
        fn complete(
            &mut self,
            _: &TaskRecoverySnapshot,
            _: &ConnectionScope,
            item: Value,
        ) -> Result<bool, RecoveryError> {
            self.completed.push(item);
            Ok(true)
        }
    }
    let output = std::process::Command::new(
        std::env::var_os("CODEX_AGENTS_PYTHON").expect("valid recovery fixture"),
    )
    .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/recovery/task_oracle.py"))
    .output()
    .expect("valid recovery fixture");
    assert!(output.status.success());
    let cases: Vec<Value> = serde_json::from_slice(&output.stdout).expect("valid recovery fixture");
    for name in ["exact", "running-unknown-True"] {
        let case = cases
            .iter()
            .find(|c| c["name"] == name)
            .expect("valid recovery fixture");
        let snapshot = TaskRecoverySnapshot::from_records(
            case["agent"].clone(),
            case["task"].clone(),
            vec![],
            vec![("receipt".into(), case["receipt"].clone())],
        )
        .expect("valid recovery fixture");
        let mut effects = Effects::default();
        let scope = ConnectionScope {
            account: "default".into(),
            connection: "connection".into(),
            generation: 1,
        };
        assert_eq!(
            recover_task(&snapshot, scope, &mut Reader, &mut effects)
                .expect("valid recovery fixture"),
            name == "exact"
        );
        assert_eq!(effects.completed.len(), usize::from(name == "exact"));
        if name == "exact" {
            assert_eq!(effects.completed[0], case["proof"]);
        }
    }
}

#[test]
fn native_task_consumer_preserves_duplicate_ambiguity() {
    use std::time::Instant;
    use studio_runtime_services::{recovery::RecoveryError, recovery_history::*, task_recovery::*};
    struct Reader {
        entries: Vec<Value>,
        barrier: bool,
    }
    impl RecoveryReader for Reader {
        fn observation(
            &mut self,
            scope: &ConnectionScope,
        ) -> Result<studio_runtime_services::recovery_absence::RecoveryObservation, HistoryError>
        {
            Ok(
                studio_runtime_services::recovery_absence::RecoveryObservation {
                    scope: scope.clone(),
                    at: 100.0,
                    supervisor: None,
                    quiet: false,
                    journal_drained: false,
                    account_reserved: false,
                    tools_refreshing: false,
                    preparation_pending: false,
                    completed_turn: None,
                },
            )
        }

        fn read(
            &mut self,
            scope: &ConnectionScope,
            method: HistoryMethod,
            params: Value,
            _: Instant,
        ) -> Result<HistoryReceipt, HistoryError> {
            assert_eq!(method, HistoryMethod::Items);
            assert_eq!(
                params,
                json!({"threadId":"thread","turnId":"turn","limit":1000,"sortDirection":"asc"})
            );
            Ok(HistoryReceipt {
                scope: scope.clone(),
                body: json!({"data":self.entries}),
            })
        }
        fn barrier(&mut self, _: &ConnectionScope, _: Instant) -> Result<(), HistoryError> {
            self.barrier = true;
            Ok(())
        }
    }
    struct Effects {
        completed: bool,
    }
    impl TaskRecoveryEffects for Effects {
        fn apply_absence(
            &mut self,
            _: &TaskRecoverySnapshot,
            _: &ConnectionScope,
            _: &studio_runtime_services::recovery_absence::RecoveryObservation,
            _: studio_runtime_services::recovery_absence::TaskAbsenceDecision,
        ) -> Result<bool, RecoveryError> {
            Ok(false)
        }

        fn resolve_receipt(&mut self, _: &Value) -> Result<Value, RecoveryError> {
            panic!("no local receipt")
        }
        fn complete(
            &mut self,
            _: &TaskRecoverySnapshot,
            _: &ConnectionScope,
            item: Value,
        ) -> Result<bool, RecoveryError> {
            assert_eq!(item["contentItems"][0]["text"], "exact native text");
            self.completed = true;
            Ok(true)
        }
    }
    let output =
        std::process::Command::new(std::env::var_os("CODEX_AGENTS_PYTHON").expect("Python"))
            .arg(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/recovery/task_oracle.py"),
            )
            .output()
            .expect("oracle");
    assert!(output.status.success());
    let cases: Vec<Value> = serde_json::from_slice(&output.stdout).expect("cases");
    let case = &cases[0];
    let snapshot = TaskRecoverySnapshot::from_records(
        case["agent"].clone(),
        case["task"].clone(),
        vec![],
        vec![],
    )
    .expect("snapshot");
    let entry = json!({"turnId":"turn","item":{"id":"item","type":"dynamicToolCall","status":"completed","contentItems":[{"text":"exact native text"}]}});
    for count in [1, 2] {
        let mut reader = Reader {
            entries: vec![entry.clone(); count],
            barrier: false,
        };
        let mut effects = Effects { completed: false };
        assert_eq!(
            recover_task(
                &snapshot,
                ConnectionScope {
                    account: "default".into(),
                    connection: "connection".into(),
                    generation: 1
                },
                &mut reader,
                &mut effects
            )
            .expect("recovery"),
            count == 1
        );
        assert_eq!(effects.completed, count == 1);
        assert!(reader.barrier);
    }
}

#[test]
fn native_task_proofs_match_real_python_reader() {
    let output =
        std::process::Command::new(std::env::var_os("CODEX_AGENTS_PYTHON").expect("Python"))
            .arg(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/recovery/native_task_oracle.py"),
            )
            .output()
            .expect("oracle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cases: Vec<Value> = serde_json::from_slice(&output.stdout).expect("cases");
    assert_eq!(cases.len(), 32);
    for case in cases {
        let snapshot = TaskRecoverySnapshot::from_records(
            case["agent"].clone(),
            case["task"].clone(),
            vec![],
            vec![],
        )
        .expect("snapshot");
        let items: Vec<Value> = case["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .map(|e| e["item"].clone())
            .collect();
        let actual = match snapshot.decide(&items, None) {
            TaskRecoveryDecision::Unconfirmed => Value::Null,
            TaskRecoveryDecision::Complete(value) => value,
        };
        assert_eq!(actual, case["expected"], "{}", case["task"]);
    }
}
