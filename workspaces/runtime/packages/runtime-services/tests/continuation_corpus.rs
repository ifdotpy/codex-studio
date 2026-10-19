#[path = "support/prepared_python.rs"]
mod prepared_python;
use serde_json::{Value, json};
use std::{path::PathBuf, process::Command};
use studio_runtime_services::{
    capacity_retry,
    continuation::{Decision, Effect, Snapshot},
    usage_resume, wakeups,
};
fn oracle(args: &[&str]) -> Value {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let python = prepared_python::prepared_python(&root.join("../../apps/server/src"));
    let result = Command::new(python)
        .arg(root.join("tests/turn_lifecycle/continuation_oracle.py"))
        .args(args)
        .output()
        .expect("isolated oracle starts");
    assert!(
        result.status.success(),
        "oracle failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).expect("oracle JSON")
}
fn decision(case: &Value, s: &Snapshot) -> Decision {
    let args = &case["args"];
    match case["family"].as_str().expect("family") {
        "create" => {
            let limits = usage_resume::LimitsObservation::from_record(args["limits"].clone(), None)
                .expect("limits");
            let cause = studio_runtime_services::provider_adapter::adapter(
                studio_runtime_providers::ProviderKind::of_record(&case["input"]["agent"])
                    .expect("corpus provider"),
            )
            .classify_error(&args["error"])
            .unwrap_or_else(|| panic!("Python classified {} but Rust did not", case["name"]));
            usage_resume::record_failure(
                s,
                usage_resume::FailureObservation {
                    turn_id: "turn",
                    cause,
                    error_reset: None,
                    auth_attempt: 1,
                    limits: &limits,
                },
                1000.0,
            )
            .expect("record failure")
        }
        "usage" | "limits" => {
            let limits = usage_resume::LimitsObservation::from_record(
                args["limits"].clone(),
                args["marker"].as_str().map(str::to_owned),
            )
            .expect("limits validate");
            if case["family"] == "usage" {
                usage_resume::tick(s, &limits, 1000.0)
            } else {
                usage_resume::limits_changed(s, &limits, 1000.0)
            }
            .expect("usage decision")
        }
        "enable" => {
            usage_resume::set_enabled(s, args["enabled"].as_bool().expect("enabled"), 1000.0)
                .expect("toggle")
        }
        "capacity-create" => {
            let kind = if args["turn"]["error"]["codexErrorInfo"] == "serverOverloaded" {
                capacity_retry::Failure::ServerOverloaded
            } else {
                capacity_retry::Failure::HttpConnectionFailed
            };
            capacity_retry::failed(s, kind, "turn", &json!({}), 1000.0).expect("failure")
        }
        "capacity" => {
            let action = match args["action"].as_str().expect("action") {
                "claim" => capacity_retry::Action::Claim {
                    settings: &args["settings"],
                    automatic: false,
                },
                "error" => capacity_retry::Action::Error {
                    attempt: &args["attempt"],
                    reason: args["reason"].as_str().expect("reason"),
                    unknown: args["unknown"].as_bool().expect("unknown"),
                },
                "restart" => capacity_retry::Action::Restart,
                "reset" => capacity_retry::Action::Reset(args["reason"].as_str().expect("reason")),
                "wait" => capacity_retry::Action::Wait(args["reason"].as_str().expect("reason")),
                "accepted" => capacity_retry::Action::Accepted {
                    attempt: &args["attempt"],
                    turn_id: args["turn"].as_str().expect("turn"),
                },
                _ => panic!("unknown capacity action"),
            };
            capacity_retry::transition(s, action, 1000.0).expect("capacity decision")
        }
        "wake" => wakeups::pending_batch(s),
        "reconcile" => wakeups::reconcile_start(
            s,
            &serde_json::from_value::<Vec<String>>(args["ids"].clone()).expect("ids"),
        ),
        other => panic!("unknown family {other}"),
    }
}
fn apply(input: &Value, d: Decision) -> Value {
    let mut result = json!({"agent":input["agent"],"record":input["record"],"events":input["events"],"metadata":input["metadata"],"holds":[],"batch":d.batch,"starts":[]});
    if let Some(a) = d.agent {
        result["agent"] = a;
    }
    if let Some(r) = d.record {
        result["record"] = r;
    }
    for e in d.events {
        let target = result["events"]
            .as_array_mut()
            .expect("events")
            .iter_mut()
            .find(|r| r["id"] == e["id"])
            .expect("stored event");
        *target = e;
    }
    for (id, m) in d.metadata {
        result["metadata"][id] = m;
    }
    for effect in d.effects {
        match effect{
        Effect::Followup{id,text}=>result["events"].as_array_mut().expect("events").push(json!({"id":id,"agent":input["agent"]["id"],"epoch":input["agent"]["epoch"],"kind":"followup","text":text,"status":"pending","created":1000.0,"turn_id":null,"error":null})),
        Effect::WorkerHold{operation,transition,reason}=>result["holds"].as_array_mut().expect("holds").push(json!([operation,transition,reason])),
        Effect::CapacityStart{attempt}=>result["starts"].as_array_mut().expect("starts").push(json!([input["agent"]["id"],attempt])),
    }
    }
    result
}
fn normalize_projection(value: &mut Value) {
    // Only pending_batch's newly compiled projection has JSON object key-order freedom.
    for e in value["batch"].as_array_mut().expect("batch") {
        if let Some(text) = e["text"].as_str()
            && let Ok(payload) = serde_json::from_str::<Value>(text)
            && payload.get("earlierProgressUpdates").is_some()
        {
            e["text"] = payload;
        }
    }
}
#[test]
fn complete_outputs_match_real_python_functions() {
    let cases = oracle(&[]);
    let cases = cases.as_array().expect("cases");
    assert!(cases.len() >= 30, "systematic corpus unexpectedly shrank");
    for case in cases {
        let s = Snapshot::from_records(case["input"].clone())
            .unwrap_or_else(|e| panic!("{}: {e}", case["name"]));
        if case["family"] == "usage"
            && case["args"]["limits"]["accountKey"] != case["input"]["record"]["accountKey"]
        {
            let limits = usage_resume::LimitsObservation::from_record(
                case["args"]["limits"].clone(),
                case["args"]["marker"].as_str().map(str::to_owned),
            )
            .expect("foreign limits observation shape");
            assert!(matches!(
                usage_resume::tick(&s, &limits, 1000.0),
                Err(
                    studio_runtime_services::continuation::ContinuationError::Unavailable(
                        "usage-policy/limits-account-mismatch"
                    )
                )
            ));
            continue;
        }
        let mut actual = apply(&case["input"], decision(case, &s));
        let mut expected = case["expected"].clone();
        normalize_projection(&mut actual);
        normalize_projection(&mut expected);
        assert_eq!(actual, expected, "case {}", case["name"]);
    }
}
#[test]
fn malformed_records_are_located_and_never_decided() {
    let cases = oracle(&[]);
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
    let cases = oracle(&[]);
    let case = cases
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["name"] == "capacity-restart-unknown-True")
        .expect("unknown fixture");
    let s = Snapshot::from_records(case["input"].clone()).expect("snapshot");
    let foreign = json!({"id":"foreign","action":"capacity","capacityRetryId":"retry","epoch":1,"accountKey":"default"});
    assert!(
        capacity_retry::transition(
            &s,
            capacity_retry::Action::Error {
                attempt: &foreign,
                reason: "foreign receipt",
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
    let cases = oracle(&[]);
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
