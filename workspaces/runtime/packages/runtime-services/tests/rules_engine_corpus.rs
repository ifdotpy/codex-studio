use serde::Deserialize;
use serde_json::value::RawValue;
use studio_runtime_services::rules_engine::{
    CheckResult, RuleDecision, RuleSnapshot, TickObservation, active_workers,
};
use studio_runtime_store::RuleRecord;

#[derive(Deserialize)]
struct Case {
    name: String,
    operation: String,
    #[serde(deserialize_with = "legacy_rule")]
    rule: RuleRecord,
    owner: Box<RawValue>,
    now: f64,
    fingerprint: Option<(i64, u64, u64)>,
    workers: Vec<Box<RawValue>>,
    monitors: Vec<Monitor>,
    event: Option<Event>,
    result: Option<ResultFixture>,
    expected: Outcome,
}

// One frozen legacy witness predates the stored file-rule defaults.
fn legacy_rule<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<RuleRecord, D::Error> {
    let raw = Box::<RawValue>::deserialize(deserializer)?;
    let text = raw.get();
    if text.contains("\"fileGeneration\"") {
        serde_json::from_str(text).map_err(serde::de::Error::custom)
    } else {
        let migrated = format!(
            "{{\"fileGeneration\":0,\"stallTimeoutSeconds\":1800,\"stallWakeGeneration\":-1,\"livenessCommand\":\"\",{}",
            &text[1..]
        );
        serde_json::from_str(&migrated).map_err(serde::de::Error::custom)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Monitor {
    id: String,
    agent: String,
    status: String,
    cancel_requested: bool,
}
#[derive(Deserialize)]
struct Event {
    kind: String,
    text: String,
    key: String,
}
#[derive(Deserialize)]
struct ResultFixture {
    code: Option<i64>,
    error: Option<String>,
    output: String,
}
#[derive(Deserialize)]
struct Outcome {
    #[serde(deserialize_with = "legacy_rule")]
    rule: RuleRecord,
    wakes: Vec<Wake>,
    launches: Vec<Launch>,
}
#[derive(Deserialize)]
struct Wake {
    id: String,
    kind: String,
    text: String,
}
#[derive(Deserialize)]
struct Launch {
    agent: String,
    data: LaunchData,
    key: String,
    approved: bool,
    epoch: u64,
    rule: RuleRecord,
}
#[derive(Deserialize, Debug, PartialEq)]
struct LaunchData {
    command: String,
    timeout_ms: u64,
    stall_timeout_seconds: u64,
}

#[derive(Deserialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Payload {
    rule: String,
    name: String,
    instruction: Option<String>,
    output: Option<String>,
    message: Option<String>,
    path: Option<String>,
    quiet_seconds: Option<f64>,
    liveness_result: Option<String>,
    liveness_exit_code: Option<i64>,
    liveness_error: Option<String>,
    due_at: Option<f64>,
    delay_seconds: Option<f64>,
}
#[derive(Deserialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkerOutput {
    active_subagents: u64,
    minimum_subagents: u64,
    below_threshold_since: f64,
    elapsed_minutes: f64,
}

fn oracle() -> Vec<Case> {
    serde_json::from_str(include_str!("rules_engine/expected.json"))
        .expect("frozen rules decisions")
}
macro_rules! owner {
    ($case:expr) => {
        serde_json::from_str(&format!("{{\"compactions\":0,{}", &$case.owner.get()[1..]))
            .expect("owner")
    };
}
fn monitor_record(value: &Monitor) -> studio_runtime_store::MonitorRecord {
    let mut record: studio_runtime_store::MonitorRecord = serde_json::from_str(r#"{"id":"monitor","agent":"owner","epoch":0,"status":"running","requestId":"request","created":0,"activityAt":0,"activityGeneration":0,"bytes":0,"command":"","cwd":"","interactive":false,"livenessCommand":"","log":"watch.log","stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"successExitCodes":[0],"tail":"","timeout_ms":3600000,"wakeOn":"exit"}"#).expect("monitor fixture");
    assert_eq!(value.status, "running");
    record.id = value.id.clone();
    record.agent = value.agent.clone();
    record.cancel_requested = Some(value.cancel_requested);
    record
}

#[test]
fn python_written_records_and_real_rules_functions_match() {
    let corpus = oracle();
    for case in &corpus {
        let name = &case.name;
        let snapshot = RuleSnapshot::from_stored(&case.rule, owner!(case))
            .unwrap_or_else(|error| panic!("{name}: constructor: {error}"));
        let now = case.now;
        let decision = match case.operation.as_str() {
            "tick" => {
                let fingerprint = snapshot.fingerprint_path(now).map(|_| case.fingerprint);
                let mut agents: Vec<_> = case
                    .workers
                    .iter()
                    .map(|worker| {
                        serde_json::from_str(&format!("{{\"compactions\":0,{}", &worker.get()[1..]))
                            .expect("worker")
                    })
                    .collect();
                agents.push(owner!(case));
                let activity = active_workers(
                    &agents,
                    &case.monitors.iter().map(monitor_record).collect::<Vec<_>>(),
                )
                .expect("activity checked");
                snapshot.tick(
                    now,
                    TickObservation {
                        fingerprint,
                        active_workers: Some(activity),
                        held_input: false,
                    },
                )
            }
            "finish" => {
                let result = case.result.as_ref().expect("result");
                snapshot.finish(
                    now,
                    CheckResult {
                        code: result.code,
                        error: result.error.as_deref(),
                        cancelled: false,
                        output: &result.output,
                    },
                )
            }
            "recover" => snapshot.recover(now),
            "event" => {
                let event = case.event.as_ref().expect("event");
                snapshot.event(&event.kind, &event.text, &event.key, now)
            }
            operation => panic!("unexpected operation {operation}"),
        }
        .unwrap_or_else(|error| panic!("{name}: decision: {error}"));
        compare(case, &decision);
    }
    assert!(corpus.len() >= 100);
}

// Only generated JSON is decoded; arbitrary output and event text remain exact.
fn compare_payload(actual: &str, expected: &str, low_workers: bool) {
    let mut actual: Payload = serde_json::from_str(actual).expect("actual payload");
    let mut expected: Payload = serde_json::from_str(expected).expect("expected payload");
    if low_workers
        && actual
            .output
            .as_ref()
            .is_some_and(|output| !output.is_empty())
    {
        let a: WorkerOutput =
            serde_json::from_str(actual.output.take().as_deref().expect("output"))
                .expect("worker output");
        let b: WorkerOutput =
            serde_json::from_str(expected.output.take().as_deref().expect("output"))
                .expect("worker output");
        assert_eq!(a, b);
    }
    assert_eq!(actual, expected);
}
fn compare_record(actual: &RuleRecord, expected: &RuleRecord, low_workers: bool) {
    let mut actual = actual.clone();
    let mut expected = expected.clone();
    if actual.stall_text.is_some() && expected.stall_text.is_some() {
        compare_payload(
            &actual.stall_text.take().expect("stall text"),
            &expected.stall_text.take().expect("stall text"),
            false,
        );
    }
    if low_workers && actual.last_output.is_some() && expected.last_output.is_some() {
        let a = actual.last_output.take().expect("output");
        let b = expected.last_output.take().expect("output");
        let a: WorkerOutput = serde_json::from_str(a.as_str()).expect("worker output");
        let b: WorkerOutput = serde_json::from_str(b.as_str()).expect("worker output");
        assert_eq!(a, b);
    }
    assert_eq!(actual, expected);
}
fn compare(case: &Case, decision: &RuleDecision) {
    let actual = decision.record.as_ref().unwrap_or(&case.rule);
    let expected = &case.expected;
    let low_workers = matches!(
        case.rule.kind,
        studio_runtime_schema::rule_record::RuleKind::LowWorkers { .. }
    );
    compare_record(actual, &expected.rule, low_workers);
    assert_eq!(
        decision.wake.iter().count(),
        expected.wakes.len(),
        "{}",
        case.name
    );
    for (a, b) in decision.wake.iter().zip(&expected.wakes) {
        assert_eq!((&a.id, a.kind), (&b.id, b.kind.as_str()));
        compare_payload(&a.text, &b.text, low_workers);
    }
    assert_eq!(
        decision.check.iter().count(),
        expected.launches.len(),
        "{}",
        case.name
    );
    for (a, b) in decision.check.iter().zip(&expected.launches) {
        assert_eq!(
            (
                &a.agent_id,
                format!("rule:{}:{}", a.rule_id, a.check_number),
                a.epoch
            ),
            (&b.agent, b.key.clone(), b.epoch)
        );
        assert!(b.approved);
        assert_eq!(
            LaunchData {
                command: a.command.clone(),
                timeout_ms: a.timeout_ms,
                stall_timeout_seconds: 0
            },
            b.data
        );
        compare_record(actual, &b.rule, low_workers);
    }
}

#[test]
fn malformed_stored_fields_are_located_and_never_panic() {
    let seed = include_str!("rules_engine/seed.json");
    #[derive(Deserialize)]
    struct Seed {
        rule: Box<RawValue>,
    }
    let seed: Seed = serde_json::from_str(seed).expect("seed");
    let rule = seed.rule.get();
    for (field, original, invalid) in [
        ("checks", "0", "-1"),
        ("checks", "0", "true"),
        ("checks", "0", "9007199254740992"),
        ("intervalSeconds", "60", "0"),
        ("inFlight", "false", "\"false\""),
        ("kind", "\"interval\"", "\"alien\""),
        ("fileGeneration", "0", "-1"),
        ("stallWakeGeneration", "-1", "-2"),
        ("fingerprint", "null", "[1,2]"),
        (
            "name",
            "\"Watch 東京\"",
            &format!("\"{}\"", "x".repeat(121)),
        ),
    ] {
        let input = rule.replacen(
            &format!("\"{field}\": {original}"),
            &format!("\"{field}\": {invalid}"),
            1,
        );
        assert_ne!(input, rule, "fixture field {field}");
        let mut deserializer = serde_json::Deserializer::from_str(&input);
        let error = serde_path_to_error::deserialize::<_, RuleRecord>(&mut deserializer)
            .expect_err("malformed record refused");
        assert!(
            field == "kind" || error.path().to_string().contains(field),
            "{field}: {error}"
        );
    }
    for field in [
        "id",
        "agent",
        "epoch",
        "name",
        "status",
        "kind",
        "intervalSeconds",
        "nextAt",
        "created",
        "command",
        "text",
    ] {
        let line = rule
            .lines()
            .find(|line| line.trim_start().starts_with(&format!("\"{field}\":")))
            .expect("field");
        let input = rule.replacen(line, "", 1);
        assert!(
            serde_json::from_str::<RuleRecord>(&input).is_err(),
            "missing {field}"
        );
    }
}

#[test]
fn sampled_configuration_and_exact_completion_identity_are_checked() {
    let corpus = oracle();
    let find = |name| {
        corpus
            .iter()
            .find(|case| case.name == name)
            .expect("named oracle witness")
    };
    let due = find("interval-boundary-0");
    let snapshot =
        RuleSnapshot::from_stored(&due.rule, owner!(due)).expect("checked Python record");
    assert!(snapshot.current(&due.rule, &owner!(due)));
    let mut unrelated_path = due.rule.clone();
    unrelated_path.kind =
        serde_json::from_str(r#"{"kind":"interval","path":"/disposable/unused"}"#)
            .expect("interval kind");
    let interval = RuleSnapshot::from_stored(&unrelated_path, owner!(due)).expect("open record");
    assert_eq!(interval.fingerprint_path(due.now), None);
    assert!(!snapshot.current(&find("interval-boundary-1").rule, &owner!(due)));
    for name in [
        "interval-stopped-autoWake",
        "interval-stopped-epoch",
        "interval-stopped-deletedAt",
    ] {
        assert!(
            !snapshot.current(&due.rule, &owner!(find(name))),
            "stale owner {name}"
        );
    }
    let decision = snapshot
        .tick(due.now, TickObservation::default())
        .expect("reserve");
    let check = decision.check.expect("command check");
    let active = decision.record.expect("in-flight record");
    let snapshot =
        studio_runtime_services::rules_engine::RuleSnapshot::from_stored(&active, owner!(due))
            .expect("checked reservation");
    let mut foreign = check.clone();
    foreign.monitor_id = "foreign".to_owned();
    assert!(
        snapshot
            .finish_check(
                &foreign,
                CheckResult {
                    code: Some(0),
                    error: None,
                    cancelled: false,
                    output: "must not wake"
                },
                0.0
            )
            .expect("foreign refusal")
            .record
            .is_none()
    );
    let stopped = studio_runtime_services::rules_engine::RuleSnapshot::from_stored(
        &active,
        owner!(find("interval-stopped-epoch")),
    )
    .expect("new owner epoch");
    assert!(
        stopped
            .finish_check(
                &check,
                CheckResult {
                    code: Some(0),
                    error: None,
                    cancelled: false,
                    output: "stale"
                },
                0.0
            )
            .expect("stale refusal")
            .record
            .is_none()
    );
    let finished = snapshot
        .finish_check(
            &check,
            CheckResult {
                code: Some(0),
                error: None,
                cancelled: false,
                output: "ok",
            },
            0.0,
        )
        .expect("finish");
    let snapshot = studio_runtime_services::rules_engine::RuleSnapshot::from_stored(
        &finished.record.expect("finished"),
        owner!(due),
    )
    .expect("checked finished record");
    assert!(
        snapshot
            .finish_check(
                &check,
                CheckResult {
                    code: Some(0),
                    error: None,
                    cancelled: false,
                    output: "duplicate"
                },
                0.0
            )
            .expect("repeat refusal")
            .record
            .is_none()
    );
}
