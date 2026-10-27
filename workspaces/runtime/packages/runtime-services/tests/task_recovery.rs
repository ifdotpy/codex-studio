use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_services::task_recovery::{TaskRecoveryDecision, TaskRecoverySnapshot};

#[test]
fn cancelled_receipt_requires_exact_attempt_and_negative_finished_result() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("recovery/task_oracle.json"))
        .expect("frozen recovery cases");
    let case = cases
        .iter()
        .find(|case| case["name"] == "completed-not_applied-False")
        .expect("negative receipt");
    let mut cancelled = case["receipt"].clone();
    cancelled["stage"] = json!("cancelled");
    cancelled["epoch"] = case["agent"]["epoch"].clone();
    let decide = |receipt: Value| {
        TaskRecoverySnapshot::from_records(
            case["agent"].clone(),
            case["task"].clone(),
            vec![],
            vec![("receipt".into(), receipt.clone())],
        )
        .expect("valid receipt shape")
        .decide(&[], Some(&receipt))
    };
    assert_eq!(
        decide(cancelled.clone()),
        TaskRecoveryDecision::Complete(case["proof"].clone())
    );
    for (field, value) in [
        ("id", json!("other")),
        ("agent", json!("other")),
        ("accountKey", json!("other")),
        ("threadId", json!("other")),
        ("turnId", json!("other")),
        ("callId", json!("other")),
        ("epoch", json!(1)),
        ("epoch", Value::Null),
        ("finished", Value::Null),
        ("outcome", json!("unknown")),
        ("stage", json!("queued")),
        ("result", json!({"success":true})),
    ] {
        let mut invalid = cancelled.clone();
        invalid[field] = value;
        assert_eq!(
            decide(invalid),
            TaskRecoveryDecision::Unconfirmed,
            "{field}"
        );
    }
    let mut missing_epoch = cancelled.clone();
    missing_epoch
        .as_object_mut()
        .expect("receipt")
        .remove("epoch");
    assert_eq!(decide(missing_epoch), TaskRecoveryDecision::Unconfirmed);
    let snapshot = TaskRecoverySnapshot::from_records(
        case["agent"].clone(),
        case["task"].clone(),
        vec![],
        vec![("receipt".into(), cancelled.clone())],
    )
    .expect("cancelled receipt");
    for field in ["epoch", "supervisor", "connectionId", "signature"] {
        let mut resolved = cancelled.clone();
        resolved[field] = json!("changed by payload resolution");
        assert_eq!(
            snapshot.decide(&[], Some(&resolved)),
            TaskRecoveryDecision::Unconfirmed,
            "{field}"
        );
    }
}

#[tokio::test]
async fn historical_negative_receipt_resolves_without_rewriting_or_native_evidence() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("recovery/task_oracle.json"))
        .expect("frozen recovery cases");
    let case = cases
        .iter()
        .find(|case| case["name"] == "completed-not_applied-False")
        .expect("historical receipt");
    let receipt = case["receipt"].clone();
    let snapshot = TaskRecoverySnapshot::from_records(
        case["agent"].clone(),
        case["task"].clone(),
        vec![],
        vec![("receipt".into(), receipt.clone())],
    )
    .expect("historical receipt is readable");
    assert_eq!(snapshot.terminal_receipt(), Some(&receipt));
    assert_eq!(
        snapshot.decide(&[], Some(&receipt)),
        TaskRecoveryDecision::Complete(case["proof"].clone())
    );
    assert_eq!(
        snapshot.terminal_receipt().expect("retained receipt")["stage"],
        "completed"
    );
    let root = tempfile::tempdir().expect("state root");
    let store = studio_runtime_store::RuntimeStore::open(Some(root.path()))
        .await
        .expect("store");
    let owner = case["agent"].to_string();
    let raw = receipt.to_string();
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_agents(id,record) VALUES('agent',?1)",
                [owner],
            )?;
            tx.execute(
                "INSERT INTO runtime_tool_requests(id,record) VALUES('receipt',?1)",
                [raw],
            )?;
            Ok(())
        })
        .await
        .expect("seed historical receipt");
    for _ in 0..2 {
        let recovered = store
            .transaction_with(|tx| {
                studio_runtime_services::tool_requests::execute(
                    tx,
                    "agent",
                    studio_runtime_services::tool_requests::Action::Get("receipt"),
                    3.0,
                )
            })
            .await
            .expect("read retained terminal receipt");
        assert_eq!(recovered, receipt);
    }
}

#[test]
fn exact_task_receipts_match_python_and_unknown_local_outcomes_override_native() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("recovery/task_oracle.json"))
        .expect("frozen recovery cases");
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
    let base = json!({"provider":studio_runtime_schema::ProviderKind::Codex,"id":"agent","rootId":"agent","epoch":0,"status":"running"});
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
        ("startAttempt", json!({"events":["event", 1]})),
    ] {
        let mut invalid = base.clone();
        invalid[field] = value;
        let error = match RecoverySnapshot::from_record(invalid) {
            Ok(_) => panic!("accepted invalid {field}"),
            Err(e) => e,
        };
        assert!(matches!(
            error,
            studio_runtime_services::recovery_policy::InvalidRecoveryRecord::Schema {
                location: "runtime_agents.recovery"
            }
        ));
    }
}

#[test]
fn large_recovery_attempt_retains_every_persisted_event_identity() {
    use studio_runtime_services::recovery_policy::RecoverySnapshot;
    let events: Vec<_> = (0..1000).map(|index| format!("event-{index}")).collect();
    let agent = json!({"provider":studio_runtime_schema::ProviderKind::Codex,"id":"agent","rootId":"agent","epoch":0,"status":"running",
        "startAttempt":{"id":"attempt","events":events}});
    let snapshot = RecoverySnapshot::from_record(agent.clone()).expect("valid large attempt");
    assert_eq!(snapshot.record(), &agent);
    assert_eq!(snapshot.start_attempt()["events"], json!(events));
}

#[tokio::test]
async fn task_consumer_resolves_local_receipt_without_native_read() {
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

        fn read<'a>(
            &'a mut self,
            _: &ConnectionScope,
            _: HistoryMethod,
            _: Value,
            _: Instant,
        ) -> BoxFuture<'a, Result<HistoryReceipt, HistoryError>> {
            Box::pin(async { panic!("local receipt must win") })
        }
        fn barrier<'a>(
            &'a mut self,
            _: &ConnectionScope,
            _: Instant,
        ) -> BoxFuture<'a, Result<(), HistoryError>> {
            Box::pin(async { panic!("no native read") })
        }
    }
    #[derive(Default)]
    struct Effects {
        completed: Vec<Value>,
    }
    impl TaskRecoveryEffects for Effects {
        fn apply_absence<'a>(
            &'a mut self,
            _: &'a TaskRecoverySnapshot,
            _: &'a ConnectionScope,
            _: &'a studio_runtime_services::recovery_absence::RecoveryObservation,
            _: studio_runtime_services::recovery_absence::TaskAbsenceDecision,
        ) -> BoxFuture<'a, Result<bool, RecoveryError>> {
            Box::pin(async { Ok(false) })
        }

        fn resolve_receipt(
            &mut self,
            receipt: Arc<Value>,
        ) -> BoxFuture<'_, Result<Value, RecoveryError>> {
            Box::pin(async move { Ok(receipt.as_ref().clone()) })
        }
        fn complete<'a>(
            &'a mut self,
            _: &'a TaskRecoverySnapshot,
            _: &'a ConnectionScope,
            item: Value,
        ) -> BoxFuture<'a, Result<bool, RecoveryError>> {
            Box::pin(async move {
                self.completed.push(item);
                Ok(true)
            })
        }
    }
    let mut cases: Vec<Value> = serde_json::from_str(include_str!("recovery/task_oracle.json"))
        .expect("frozen receipt cases");
    let mut cancelled = cases
        .iter()
        .find(|case| case["name"] == "completed-not_applied-False")
        .expect("negative receipt")
        .clone();
    cancelled["name"] = json!("cancelled");
    cancelled["receipt"]["stage"] = json!("cancelled");
    cancelled["receipt"]["epoch"] = cancelled["agent"]["epoch"].clone();
    cases.push(cancelled);
    for name in [
        "exact",
        "cancelled",
        "running-unknown-True",
        "queued-applied-False",
    ] {
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
                .await
                .expect("valid recovery fixture"),
            name == "exact" || name == "cancelled"
        );
        assert_eq!(
            effects.completed.len(),
            usize::from(name == "exact" || name == "cancelled")
        );
        if name == "exact" || name == "cancelled" {
            assert_eq!(effects.completed[0], case["proof"]);
        }
    }
}

#[tokio::test]
async fn native_task_consumer_preserves_duplicate_ambiguity() {
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

        fn read<'a>(
            &'a mut self,
            scope: &ConnectionScope,
            method: HistoryMethod,
            params: Value,
            _: Instant,
        ) -> BoxFuture<'a, Result<HistoryReceipt, HistoryError>> {
            let scope = scope.clone();
            Box::pin(async move {
                assert_eq!(method, HistoryMethod::Items);
                assert_eq!(
                    params,
                    json!({"threadId":"thread","turnId":"turn","limit":1000,"sortDirection":"asc"})
                );
                Ok(HistoryReceipt {
                    scope,
                    body: json!({"data":self.entries.clone()}),
                })
            })
        }
        fn barrier<'a>(
            &'a mut self,
            _: &ConnectionScope,
            _: Instant,
        ) -> BoxFuture<'a, Result<(), HistoryError>> {
            Box::pin(async move {
                self.barrier = true;
                Ok(())
            })
        }
    }
    struct Effects {
        completed: bool,
    }
    impl TaskRecoveryEffects for Effects {
        fn apply_absence<'a>(
            &'a mut self,
            _: &'a TaskRecoverySnapshot,
            _: &'a ConnectionScope,
            _: &'a studio_runtime_services::recovery_absence::RecoveryObservation,
            _: studio_runtime_services::recovery_absence::TaskAbsenceDecision,
        ) -> BoxFuture<'a, Result<bool, RecoveryError>> {
            Box::pin(async { Ok(false) })
        }

        fn resolve_receipt(
            &mut self,
            _: Arc<Value>,
        ) -> BoxFuture<'_, Result<Value, RecoveryError>> {
            Box::pin(async { panic!("no local receipt") })
        }
        fn complete<'a>(
            &'a mut self,
            _: &'a TaskRecoverySnapshot,
            _: &'a ConnectionScope,
            item: Value,
        ) -> BoxFuture<'a, Result<bool, RecoveryError>> {
            Box::pin(async move {
                assert_eq!(item["contentItems"][0]["text"], "exact native text");
                self.completed = true;
                Ok(true)
            })
        }
    }
    let cases: Vec<Value> = serde_json::from_str(include_str!("recovery/task_oracle.json"))
        .expect("frozen receipt cases");
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
            .await
            .expect("recovery"),
            count == 1
        );
        assert_eq!(effects.completed, count == 1);
        assert!(reader.barrier);
    }
}

#[test]
fn native_task_proofs_match_real_python_reader() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("recovery/native_task_oracle.json"))
        .expect("frozen recovery cases");
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
