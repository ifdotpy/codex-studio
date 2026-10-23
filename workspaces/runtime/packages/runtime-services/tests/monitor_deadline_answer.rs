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
