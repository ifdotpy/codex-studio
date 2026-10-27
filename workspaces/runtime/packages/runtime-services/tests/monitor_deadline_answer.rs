//! A loss decided at the command deadline, and the command's late real answer.
use crate::monitor_fixtures as fixtures;
use serde_json::{Value, json};
use studio_runtime_services::monitor_policy::{self, Effect, MonitorSnapshot, Operation};

/// A command lost with `proof` after its reattach; `rule` makes it a rule check.
fn lost(proof: &str, rule: bool) -> (Value, Value, Operation) {
    let root = fixtures::tempdir();
    let case = fixtures::corpus(root.path()).remove(0);
    let mut record = case["before"].clone();
    record["status"] = json!("lost");
    record["finished"] = json!(3000.0);
    record["error"] = json!("Command outcome unknown; not rerun.");
    record["reattachRecovery"] = json!({"accountKey":record["operation"]["accountKey"],
        "epoch":record["operation"]["epoch"],"status":"not_reattachable","startedAt":2900.0,
        "proof":proof});
    if rule {
        record["ruleId"] = json!("rule");
    }
    let operation = serde_json::from_value(record["operation"].clone()).expect("operation");
    (record, case["actor"].clone(), operation)
}

fn finish(record: &Value, actor: &Value, operation: &Operation, now: f64) -> (Value, Vec<Effect>) {
    let snapshot = MonitorSnapshot::checked(record.clone(), actor.clone()).expect("snapshot");
    let decision =
        monitor_policy::finish(&snapshot, Some(operation), Some(0), None, now).expect("finish");
    (decision.record, decision.effects)
}

#[test]
fn a_late_answer_completes_a_deadline_loss_once() {
    let (record, actor, operation) = lost(monitor_policy::DEADLINE_PASSED, false);
    let (completed, effects) = finish(&record, &actor, &operation, 3100.0);
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(completed["error"], Value::Null);
    assert_eq!(completed["finished"], 3100.0);
    assert_eq!(completed["lateAnswerAt"], 3100.0);
    assert!(completed.get("reattachRecovery").is_none());
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::ExitEvent))
    );
    // A repeated answer finds a finished command and changes nothing.
    let (again, _) = finish(&completed, &actor, &operation, 3200.0);
    assert_eq!(again, completed);
}

#[test]
fn other_losses_and_rule_checks_stay_lost_after_an_answer() {
    for (proof, rule) in [
        ("provider_rejected", false),
        ("reattach_window_expired", false),
        ("retained_exit_unavailable", false),
        (monitor_policy::DEADLINE_PASSED, true),
    ] {
        let (record, actor, operation) = lost(proof, rule);
        let (after, _) = finish(&record, &actor, &operation, 3100.0);
        assert_eq!(after, record, "{proof} rule={rule}");
    }
}

/// Reproduce the exact transaction order of a deadline waiter racing a tick.
/// The tick observes a recovery and obtains its proof before the waiter commits
/// the loss. The proof must not make the next tick fail or reopen the command.
#[tokio::test]
async fn a_reattach_proof_observed_before_deadline_loss_is_superseded() {
    use studio_runtime_services::monitor_runtime;
    use studio_runtime_store::MonitorsRepository;
    let root = fixtures::tempdir();
    let store = fixtures::store(root.path(), "stale-proof").await;
    let case = fixtures::corpus(root.path()).remove(0);
    fixtures::seed(&store, &case).await;
    let id = case["before"]["id"].as_str().expect("id");
    let operation: Operation =
        serde_json::from_value(case["before"]["operation"].clone()).expect("operation");
    monitor_runtime::reconcile_unknown(&store, id, 2900.0)
        .await
        .expect("recovery");
    let mut proof = monitor_policy::ReattachProof {
        operation,
        operation_id: format!("monitor:{id}"),
        native_id: None,
        current_connection: "current".into(),
    };
    monitor_runtime::settle_reattach(&store, id, monitor_policy::DEADLINE_PASSED, 3000.0)
        .await
        .expect("deadline loss");
    let read = || {
        MonitorsRepository::get(&store.test_reader().expect("reader"), id)
            .expect("read")
            .expect("monitor")
            .value()
            .clone()
    };
    let before = read();
    let result = monitor_runtime::reattach(&store, id, &proof, 3001.0).await;
    assert!(
        result.is_ok(),
        "a superseded exact proof must not fail a tick: {:?}",
        result.err()
    );
    assert_eq!(
        read(),
        before,
        "the stale proof must not reopen the command"
    );
    assert_eq!(
        fixtures::events(&store).as_array().expect("events").len(),
        1
    );
    proof.operation_id = "monitor:another-command".into();
    assert!(
        monitor_runtime::reattach(&store, id, &proof, 3002.0)
            .await
            .is_err(),
        "a superseded proof still needs its exact command identity"
    );
}

#[tokio::test]
async fn an_unanswered_command_commits_only_its_loss_and_replays_without_changes() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use studio_runtime_services::monitor_runtime;
    use studio_runtime_store::{MonitorsRepository, ObservedTable};
    let root = fixtures::tempdir();
    let store = fixtures::store(root.path(), "atomic-loss").await;
    let case = fixtures::corpus(root.path()).remove(0);
    fixtures::seed(&store, &case).await;
    let id = case["before"]["id"].as_str().expect("id");
    let operation: Operation =
        serde_json::from_value(case["before"]["operation"].clone()).expect("operation");
    let commits = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&commits);
    store.add_table_change_observer(ObservedTable::Monitors, move || {
        observed.fetch_add(1, Ordering::SeqCst);
    });
    for now in [3000.0, 3001.0] {
        monitor_runtime::settle_unanswered(
            &store,
            id,
            &operation,
            monitor_policy::DEADLINE_PASSED,
            now,
        )
        .await
        .expect("atomic deadline loss");
        let record = MonitorsRepository::get(&store.test_reader().expect("reader"), id)
            .expect("read")
            .expect("monitor")
            .value()
            .clone();
        assert_eq!(record["status"], "lost");
        assert_eq!(record["finished"], 3000.0);
        assert_eq!(
            record["reattachRecovery"]["proof"],
            monitor_policy::DEADLINE_PASSED
        );
        assert_eq!(
            commits.load(Ordering::SeqCst),
            1,
            "one terminal commit, no intermediate recovery"
        );
        assert_eq!(
            fixtures::events(&store).as_array().expect("events").len(),
            1
        );
    }
    let mut wrong_operation = operation.clone();
    wrong_operation.connection_id = "different-command".into();
    assert!(
        monitor_runtime::settle_unanswered(
            &store,
            id,
            &wrong_operation,
            monitor_policy::DEADLINE_PASSED,
            3002.0
        )
        .await
        .is_err()
    );
    assert_eq!(commits.load(Ordering::SeqCst), 1);
}
