use serde_json::{Value, json};
use studio_runtime_services::{capacity_retry, continuation::Snapshot};
/// The stored-record inputs of the continuation corpus.
fn cases() -> Value {
    serde_json::from_str(include_str!("continuation/cases.json")).expect("corpus JSON")
}
#[test]
fn malformed_records_are_located_and_never_decided() {
    let cases = cases();
    let original = cases
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["family"] == "usage")
        .expect("usage fixture")["input"]
        .clone();
    for (field, value) in [
        ("epoch", json!("1")),
        ("epoch", json!(-1)),
        ("status", Value::Null),
        ("accountKey", json!(true)),
        ("dueAt", json!("later")),
        ("taskClaims", json!([1])),
    ] {
        let mut raw = original.clone();
        raw["record"][field] = value;
        let error = Snapshot::from_records(raw)
            .err()
            .expect("malformed stored data rejected");
        assert!(error.to_string().contains("/record"));
    }
    for key in [
        "id",
        "accountKey",
        "threadId",
        "epoch",
        "turnId",
        "status",
        "failedAt",
    ] {
        for null in [false, true] {
            let mut raw = original.clone();
            if null {
                raw["record"][key] = Value::Null;
            } else {
                raw["record"].as_object_mut().expect("record").remove(key);
            }
            assert!(
                Snapshot::from_records(raw).is_err(),
                "missing/null required {key}"
            );
        }
    }
    let mut raw = original;
    raw["agent"]["autoWake"] = json!("yes");
    assert!(Snapshot::from_records(raw).is_err());
}
#[test]
fn unknown_capacity_never_resumes() {
    let cases = cases();
    let case = cases
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["name"] == "capacity-restart-unknown-True")
        .expect("unknown fixture");
    let s = Snapshot::from_records(case["input"].clone()).expect("snapshot");
    assert!(
        capacity_retry::transition(
            &s,
            capacity_retry::Action::Error {
                reason: "late start outcome",
                unknown: false
            },
            1000.0
        )
        .expect("stale error")
        .record
        .is_none()
    );
    assert!(
        capacity_retry::transition(
            &s,
            capacity_retry::Action::Claim {
                settings: &json!({}),
                automatic: false,
            },
            1000.0
        )
        .expect("claim")
        .effects
        .is_empty()
    );
    assert!(
        capacity_retry::transition(&s, capacity_retry::Action::Wait("local wait"), 1000.0)
            .expect("wait")
            .record
            .is_none()
    );
}

#[test]
fn automatic_capacity_claim_and_late_completion_cannot_resurrect_cancelled_retry() {
    let cases = cases();
    let case = cases
        .as_array()
        .expect("corpus cases")
        .iter()
        .find(|c| c["name"] == "capacity-claim-success")
        .expect("scheduled retry");
    let mut raw = case["input"].clone();
    raw["record"]["status"] = json!("cancelled");
    raw["record"]["dueAt"] = Value::Null;
    raw["agent"]["capacityRetry"]["status"] = json!("cancelled");
    let snapshot = Snapshot::from_records(raw).expect("cancelled retry snapshot");
    for action in [
        capacity_retry::Action::Claim {
            settings: &json!({}),
            automatic: true,
        },
        capacity_retry::Action::Completed {
            turn_id: "stale-turn",
            completed: true,
        },
    ] {
        let decision =
            capacity_retry::transition(&snapshot, action, 1000.0).expect("stale timer/completion");
        assert!(
            decision.record.is_none(),
            "cancelled retry remains untouched"
        );
        assert!(decision.agent.is_none());
        assert!(decision.effects.is_empty());
    }
}

/// A claimed capacity retry follows its turn to a terminal state: the claim queues its follow-up, the
/// provider's positive receipt binds the turn, and that turn's completion finishes it.
#[test]
fn claimed_capacity_retry_ends_finished_after_its_turn_settles() {
    let cases = cases();
    let case = cases
        .as_array()
        .expect("corpus cases")
        .iter()
        .find(|c| c["name"] == "capacity-claim-success")
        .expect("scheduled retry");
    let mut raw = case["input"].clone();
    let settings = raw["record"]["settings"].clone();
    let step = |raw: &mut Value, action: capacity_retry::Action<'_>| {
        let snapshot = Snapshot::from_records(raw.clone()).expect("snapshot");
        let decision = capacity_retry::transition(&snapshot, action, 1000.0).expect("transition");
        raw["record"] = decision.record.clone().expect("record");
        raw["agent"] = decision.agent.clone().expect("agent");
        decision
    };
    step(
        &mut raw,
        capacity_retry::Action::Claim {
            settings: &settings,
            automatic: true,
        },
    );
    assert_eq!(raw["record"]["status"], "starting");
    assert_eq!(raw["agent"]["status"], "queued");
    step(
        &mut raw,
        capacity_retry::Action::Accepted { turn_id: "turn-2" },
    );
    assert_eq!(raw["record"]["acceptedTurnId"], "turn-2");
    assert_eq!(raw["agent"]["capacityRetryCount"], 1);
    // A replayed start receipt counts the attempt once.
    let snapshot = Snapshot::from_records(raw.clone()).expect("accepted snapshot");
    let replay = capacity_retry::transition(
        &snapshot,
        capacity_retry::Action::Accepted { turn_id: "turn-2" },
        1000.0,
    )
    .expect("replay");
    assert!(replay.record.is_none() && replay.agent.is_none());
    step(
        &mut raw,
        capacity_retry::Action::Completed {
            turn_id: "turn-2",
            completed: true,
        },
    );
    assert_eq!(raw["record"]["status"], "finished");
    assert_eq!(raw["agent"]["capacityRetry"]["status"], "finished");
}

/// A rejected or unknown start of the retry's turn leaves the record terminal, not `starting`.
#[test]
fn rejected_capacity_start_ends_failed_and_lost_reply_ends_unknown() {
    let cases = cases();
    let case = cases
        .as_array()
        .expect("corpus cases")
        .iter()
        .find(|c| c["name"] == "capacity-claim-success")
        .expect("scheduled retry");
    for (unknown, status) in [(false, "failed"), (true, "unknown")] {
        let mut raw = case["input"].clone();
        let settings = raw["record"]["settings"].clone();
        let snapshot = Snapshot::from_records(raw.clone()).expect("snapshot");
        let claimed = capacity_retry::transition(
            &snapshot,
            capacity_retry::Action::Claim {
                settings: &settings,
                automatic: true,
            },
            1000.0,
        )
        .expect("claim");
        raw["record"] = claimed.record.expect("record");
        raw["agent"] = claimed.agent.expect("agent");
        let snapshot = Snapshot::from_records(raw).expect("claimed snapshot");
        let failed = capacity_retry::transition(
            &snapshot,
            capacity_retry::Action::Error {
                reason: "start failed",
                unknown,
            },
            1000.0,
        )
        .expect("error");
        assert_eq!(failed.record.expect("record")["status"], status);
    }
}
