use serde_json::{Value, json};
use studio_runtime_services::{capacity_retry, continuation::Snapshot};

#[test]
fn capacity_wait_accepts_usage_record_with_matching_embedded_id() {
    let retry = json!({"id":"same","accountKey":"default","threadId":"thread","epoch":1,"turnId":"turn","status":"scheduled","cause":"serverOverloaded","cwd":"","settings":{},"attempt":1,"maxAttempts":4});
    let usage = json!({"id":"same","accountKey":"default","threadId":"thread","epoch":1,"turnId":"turn","status":"scheduled","cause":"auth","failedAt":10});
    let snapshot = Snapshot::from_records(json!({"agent":{"id":"a","rootId":"a","epoch":1,"status":"failed","provider":"codex","capacityRetry":retry},"record":usage})).unwrap();
    let decision =
        capacity_retry::transition(&snapshot, capacity_retry::Action::Wait("wait"), 20.0).unwrap();
    let record = decision.record.unwrap();
    assert_eq!(record["cause"], "auth");
    assert_eq!(record["status"], "scheduled");
    assert_eq!(record["waits"], 1);
}

#[test]
fn typed_continuation_snapshot_matches_schema_shapes() {
    let schema: Value = serde_json::from_str(include_str!(
        "../../runtime-schema/src/schemas/persisted-continuation-snapshot.schema.json"
    ))
    .expect("schema");
    let validator = jsonschema::validator_for(&schema).expect("validator");
    let base =
        json!({"agent":{"id":"a","rootId":"a","epoch":1,"status":"running","provider":"codex"}});
    for (key, value) in [
        ("ancestors", json!(null)),
        ("claims", json!(["a"])),
        ("claims", json!([""])),
        ("agent", json!(["a", "a", 1, "running", "codex"])),
        ("record", json!([])),
        ("record", json!(null)),
        ("limits", json!({"accountKey":"a","unknown":true})),
    ] {
        let mut input = base.clone();
        input[key] = value;
        let typed = <studio_runtime_schema::continuation_snapshot::ContinuationSnapshotRecord as serde::Deserialize>::deserialize(&input)
        .is_ok();
        assert_eq!(typed, validator.is_valid(&input), "{input}");
    }
}

#[test]
fn typed_continuation_snapshot_enforces_schema_bounds() {
    use studio_runtime_schema::continuation_snapshot::ContinuationSnapshotRecord;
    let base =
        json!({"agent":{"id":"a","rootId":"a","epoch":1,"status":"running","provider":"codex"}});
    for (key, accepted, rejected) in [
        ("claims", json!(vec!["a"; 4096]), json!(vec!["a"; 4097])),
        (
            "ancestors",
            json!(vec![base["agent"].clone(); 64]),
            json!(vec![base["agent"].clone(); 65]),
        ),
    ] {
        let mut input = base.clone();
        input[key] = accepted;
        assert!(<ContinuationSnapshotRecord as serde::Deserialize>::deserialize(&input).is_ok());
        input[key] = rejected;
        assert!(<ContinuationSnapshotRecord as serde::Deserialize>::deserialize(input).is_err());
    }
    let mut input = base;
    input["agent"]["id"] = json!("é".repeat(4096));
    assert!(<ContinuationSnapshotRecord as serde::Deserialize>::deserialize(&input).is_ok());
    input["agent"]["id"] = json!("é".repeat(4097));
    assert!(<ContinuationSnapshotRecord as serde::Deserialize>::deserialize(input).is_err());
}

#[test]
fn continuation_event_identity_keeps_exact_epoch_evidence() {
    let mut input = json!({
        "agent":{"id":"a","rootId":"a","epoch":1,"status":"running","provider":"codex"},
        "events":[{"id":"e","agent":"a","epoch":1,"kind":"input","text":"x","status":"pending","created":1}]
    });
    assert!(Snapshot::from_records(input.clone()).is_ok());
    input["events"][0]["epoch"] = json!(1.0);
    assert!(
        Snapshot::from_records(input)
            .err()
            .expect("different numeric spelling must not match")
            .to_string()
            .contains("/events/identity")
    );
}

#[test]
fn usage_identity_keeps_exact_epoch_evidence() {
    let records = cases();
    let mut input = records
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "usage-auth-proof-899.0")
        .unwrap()["input"]
        .clone();
    input["record"]["epoch"] = json!(1.0);
    let snapshot = Snapshot::from_records(input).unwrap();
    let decision =
        studio_runtime_services::usage_resume::tick_without_observation(&snapshot, 1000.0).unwrap();
    assert_eq!(decision.record.unwrap()["status"], "cancelled");
}

#[test]
fn snapshot_rejects_object_encoded_wire_enums() {
    let records = cases();
    for family in ["usage", "capacity"] {
        let original = records
            .as_array()
            .expect("cases")
            .iter()
            .find(|case| case["family"] == family && case["input"]["record"]["status"].is_string())
            .expect("family case")["input"]
            .clone();
        assert!(Snapshot::from_records(original.clone()).is_ok());
        for key in ["status", "cause"] {
            if let Some(spelling) = original["record"][key].as_str() {
                let mut input = original.clone();
                input["record"][key] = json!({spelling:null});
                assert!(Snapshot::from_records(input).is_err(), "{family} {key}");
            }
        }
        let mut input = original;
        input["agent"]["provider"] = json!({"codex":null});
        assert!(Snapshot::from_records(input).is_err());
    }
}

#[test]
fn capacity_counts_preserve_integer_and_decimal_alias_behavior() {
    let cases = cases();
    let create = cases
        .as_array()
        .expect("cases")
        .iter()
        .find(|case| case["name"] == "capacity-create-server-0")
        .expect("fixture")["input"]
        .clone();
    for (number, expected) in [(json!(1), 2), (json!(1.0), 1)] {
        let mut input = create.clone();
        input["agent"]["capacityRetryCount"] = number;
        let snapshot = Snapshot::from_records(input).expect("snapshot");
        let decision = capacity_retry::failed(
            &snapshot,
            capacity_retry::Failure::ServerOverloaded,
            "turn",
            &json!({}),
            1000.0,
        )
        .expect("failure");
        assert_eq!(decision.record.expect("record")["attempt"], expected);
    }
    let original = cases
        .as_array()
        .expect("cases")
        .iter()
        .find(|case| case["name"] == "capacity-claim-success")
        .expect("fixture")["input"]
        .clone();
    for (number, expected) in [(json!(1), 2), (json!(1.0), 1)] {
        let mut input = original.clone();
        input["record"]["waits"] = number;
        let snapshot = Snapshot::from_records(input).expect("snapshot");
        let decision =
            capacity_retry::transition(&snapshot, capacity_retry::Action::Wait("wait"), 1000.0)
                .expect("wait");
        assert_eq!(decision.record.expect("record")["waits"], expected);
    }
}

#[test]
fn auth_backoff_preserves_integer_and_decimal_alias_behavior() {
    let cases = cases();
    let original = cases
        .as_array()
        .expect("cases")
        .iter()
        .find(|case| case["name"] == "usage-auth-proof-899.0")
        .expect("auth fixture")["input"]
        .clone();
    for (number, due) in [(json!(1), 1600.0), (json!(1.0), 1180.0)] {
        let mut input = original.clone();
        input["record"]["authAttempt"] = number;
        let snapshot = Snapshot::from_records(input).expect("snapshot");
        let decision =
            studio_runtime_services::usage_resume::tick_without_observation(&snapshot, 1000.0)
                .expect("tick");
        assert_eq!(decision.record.expect("rescheduled")["dueAt"], due);
    }
}
/// The stored-record inputs of the continuation corpus.
fn cases() -> Value {
    let mut cases: Value =
        serde_json::from_str(include_str!("continuation/cases.json")).expect("corpus JSON");
    // The frozen corpus predates explicit identity for these Codex continuations.
    for case in cases.as_array_mut().expect("corpus cases") {
        case["input"]["agent"]["provider"] = json!(studio_runtime_schema::ProviderKind::Codex);
        for ancestor in case["input"]["ancestors"]
            .as_array_mut()
            .expect("corpus ancestors")
        {
            ancestor["provider"] = json!(studio_runtime_schema::ProviderKind::Codex);
        }
    }
    cases
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
