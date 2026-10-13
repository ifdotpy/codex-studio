use serde_json::{Value, json};
use std::{path::PathBuf, process::Command};
use studio_runtime_services::{
    capacity_retry, claude_auth_wait,
    continuation::{Decision, Effect, Snapshot},
    usage_resume, wakeups,
};
fn oracle(args: &[&str]) -> Value {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let python = std::env::var("CODEX_AGENTS_PYTHON")
        .expect("set CODEX_AGENTS_PYTHON to an isolated equipped interpreter");
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
            let cause = match args["error"]["codexErrorInfo"].as_str().expect("kind") {
                "usageLimitExceeded" => usage_resume::Cause::UsageLimit,
                "rateLimitExceeded" => usage_resume::Cause::RateLimit,
                _ => usage_resume::Cause::Auth,
            };
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
        "auth" => {
            let mut before = args.get("claimInput").unwrap_or(&case["input"]).clone();
            before["record"]["dueAt"] = json!(980.0);
            let original_pin = before["record"]["accountPin"].clone();
            let before = Snapshot::from_records(before).expect("before probe");
            let (_, ticket) =
                claude_auth_wait::claim_probe(&before, &json!({}), &original_pin, 1000.0)
                    .expect("claim probe");
            claude_auth_wait::apply_probe(
                s,
                &ticket.expect("probe ticket"),
                &json!({}),
                &args["pin"],
                &args["result"],
                1000.0,
            )
            .expect("apply probe")
        }
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
fn unknown_capacity_and_stale_auth_ticket_never_resume() {
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
                settings: &json!({})
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
    let case = cases
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["family"] == "auth")
        .expect("auth fixture");
    let mut raw = case["input"].clone();
    raw["record"]["dueAt"] = json!(980.0);
    let s = Snapshot::from_records(raw.clone()).expect("auth snapshot");
    let (_, ticket) =
        claude_auth_wait::claim_probe(&s, &json!({}), &case["args"]["pin"], 1000.0).expect("claim");
    raw["record"]["dueAt"] = json!(1360.0);
    let newer = Snapshot::from_records(raw).expect("new claim");
    assert!(
        claude_auth_wait::apply_probe(
            &newer,
            &ticket.expect("ticket"),
            &json!({}),
            &case["args"]["pin"],
            &case["args"]["result"],
            1001.0
        )
        .expect("stale receipt")
        .record
        .is_none()
    );
}

#[test]
fn exact_unsent_receipt_is_retained_and_profile_probe_slots_are_bounded() {
    let cases = oracle(&[]);
    let case = cases
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["family"] == "auth")
        .expect("Python-written auth receipt");
    let mut raw = case["input"].clone();
    let pin = &case["args"]["pin"];
    let saved = Snapshot::from_records(raw.clone()).expect("stored receipt");
    assert!(claude_auth_wait::retained(&saved, &json!({}), pin));
    raw["agent"]["startAttempt"]
        .as_object_mut()
        .expect("attempt")
        .remove("executionOutcome");
    let before = Snapshot::from_records(raw.clone()).expect("original unsubmitted attempt");
    let capture = claude_auth_wait::capture(&before, &json!({}), pin).expect("capture exact input");
    raw["agent"]["startAttempt"]["executionOutcome"] = json!("unsent");
    let restored = Snapshot::from_records(raw.clone()).expect("restored original inputs");
    let decision = claude_auth_wait::record_timeout(&restored, &capture, &json!({}), pin, 800.0)
        .expect("write wait");
    let mut expected = case["input"]["record"].clone();
    expected["dueAt"] = json!(980.0);
    assert_eq!(
        decision.record.as_ref(),
        Some(&expected),
        "receipt equals the Python writer output before probe rescheduling"
    );
    raw["record"] = expected;
    let due = Snapshot::from_records(raw).expect("due receipt");
    let settings = std::collections::BTreeMap::from([("a".into(), json!({}))]);
    let pins = std::collections::BTreeMap::from([("default".into(), pin.clone())]);
    let active = std::collections::BTreeSet::from(["one".into(), "two".into()]);
    assert!(
        claude_auth_wait::plan_probes(
            std::slice::from_ref(&due),
            &active,
            &settings,
            &pins,
            1000.0
        )
        .expect("at capacity")
        .jobs
        .is_empty()
    );
    let plan = claude_auth_wait::plan_probes(
        &[due],
        &std::collections::BTreeSet::new(),
        &settings,
        &pins,
        1000.0,
    )
    .expect("free profile slot");
    assert_eq!(plan.jobs.len(), 1);
    assert_eq!(plan.jobs[0].tickets.len(), 1);
    assert_eq!(
        plan.writes[0].1.record.as_ref().expect("deadline")["dueAt"],
        1180.0
    );
}

#[test]
fn probe_plan_selects_only_the_earliest_sixteen_due_rows() {
    let cases = oracle(&[]);
    let raw = cases
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["name"] == "claude-proof-readyaccount")
        .expect("auth fixture")["input"]
        .clone();
    let mut snapshots = Vec::new();
    let mut settings = std::collections::BTreeMap::new();
    let pins =
        std::collections::BTreeMap::from([("default".into(), raw["record"]["accountPin"].clone())]);
    for index in (0..40).rev() {
        let mut row = raw.clone();
        let id = format!("a-{index}");
        row["agent"]["id"] = json!(id);
        row["agent"]["rootId"] = json!(id);
        row["record"]["agent"] = json!(id);
        row["record"]["id"] = json!(format!("receipt-{index}"));
        row["record"]["dueAt"] = json!(900.0 + index as f64);
        // These are valid stored rows whose original source changed; each selected row cancels.
        for event in row["events"].as_array_mut().expect("events") {
            event["agent"] = json!(id);
        }
        settings.insert(id, json!({}));
        snapshots.push(Snapshot::from_records(row).expect("checked input"));
    }
    let plan = claude_auth_wait::plan_probes(
        &snapshots,
        &std::collections::BTreeSet::new(),
        &settings,
        &pins,
        1000.0,
    )
    .expect("plan");
    assert!(plan.jobs.is_empty());
    assert_eq!(plan.writes.len(), 16);
    assert_eq!(
        plan.writes
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>(),
        (0..16).map(|i| format!("a-{i}")).collect::<Vec<_>>()
    );
}
