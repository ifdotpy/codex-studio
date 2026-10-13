#[path = "monitors/fixtures.rs"]
mod fixtures;
#[path = "support/prepared_python.rs"]
mod prepared_python_support;
use studio_runtime_services::monitor_policy::{self as policy, MonitorSnapshot, Operation};

#[test]
fn python_monitor_writers_drive_checked_snapshots_and_complete_decisions() {
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    assert!(cases.len() >= 70);
    for case in cases {
        let snapshot = MonitorSnapshot::checked(case["before"].clone(), case["actor"].clone())
            .expect("monitor fixture");
        let op: Option<Operation> =
            serde_json::from_value(case["before"]["operation"].clone()).expect("monitor fixture");
        let actual = match case["action"].as_str().expect("monitor fixture") {
            "create" => policy::Decision {
                record: policy::create_record(
                    &case["actor"],
                    &case["argument"],
                    "create-case",
                    root.path().join("monitor-logs").to_str().expect("log root"),
                    2000.0,
                )
                .expect("creation"),
                effects: vec![],
                response: serde_json::Value::Null,
            },
            "cancel" => policy::cancel(&snapshot, None, 2000.0).expect("monitor fixture"),
            "finish" | "legacy" => policy::finish(
                &snapshot,
                op.as_ref(),
                case["argument"][0].as_i64(),
                case["argument"][1].as_str(),
                2000.0,
            )
            .expect("monitor fixture"),
            "output" => {
                let bytes: Vec<u8> =
                    serde_json::from_value(case["argument"].clone()).expect("monitor fixture");
                policy::output(
                    &snapshot,
                    op.as_ref().expect("monitor fixture"),
                    &bytes,
                    2000.0,
                )
                .expect("monitor fixture")
            }
            "tick" => policy::tick(&snapshot, 2000.0).expect("monitor fixture"),
            "probe" => policy::probe_result(
                &snapshot,
                case["argument"].as_i64().expect("generation"),
                case["probeResults"][0].clone(),
                2000.0,
            )
            .expect("probe"),
            "input" => {
                let input = serde_json::from_value(case["argument"]["input"].clone())
                    .expect("input fixture");
                let begun = policy::begin_input(
                    &snapshot,
                    op.as_ref().expect("operation"),
                    "ticket-1",
                    &input,
                    2000.0,
                )
                .expect("input admission");
                assert_eq!(
                    begun.record, case["submissions"][0]["record"],
                    "{}",
                    case["name"]
                );
                if let policy::Effect::Native {
                    identity,
                    method,
                    params,
                } = &begun.effects[0]
                {
                    assert_eq!(
                        identity,
                        case["submissions"][0]["identity"]
                            .as_str()
                            .expect("identity")
                    );
                    assert_eq!(
                        *method,
                        case["submissions"][0]["method"].as_str().expect("method")
                    );
                    assert_eq!(params, &case["submissions"][0]["params"]);
                } else {
                    panic!("input native effect missing");
                }
                let next = MonitorSnapshot::checked(begun.record, case["actor"].clone())
                    .expect("submitted snapshot");
                let outcome = match case["argument"]["outcome"].as_str().expect("outcome") {
                    "acknowledged" => policy::InputOutcome::Acknowledged,
                    "failed" => policy::InputOutcome::Rejected("native rejected".into()),
                    _ => policy::InputOutcome::Unknown("transport disconnected".into()),
                };
                policy::settle_input(
                    &next,
                    op.as_ref().expect("operation"),
                    "ticket-1",
                    outcome,
                    2000.0,
                )
                .expect("settled input")
            }
            _ => unreachable!(),
        };
        assert_eq!(actual.record, case["after"], "{}", case["name"]);
        let probes: Vec<_> = actual
            .effects
            .iter()
            .filter_map(|effect| match effect {
                policy::Effect::Probe {
                    generation,
                    command,
                } => Some(serde_json::json!([generation, command])),
                _ => None,
            })
            .collect();
        let expected_probes: Vec<_> = case["probes"]
            .as_array()
            .expect("recorded probes")
            .iter()
            .map(|probe| serde_json::json!([probe[1], probe[3]]))
            .collect();
        assert_eq!(probes, expected_probes, "{} probes", case["name"]);
        if case["action"] == "cancel" {
            assert_eq!(actual.response, case["response"], "{}", case["name"]);
        }
    }
}

#[test]
fn transaction_adapter_matches_python_event_payloads_and_expiry() {
    use studio_runtime_services::monitor_runtime;
    let root = fixtures::tempdir();
    let cases = fixtures::corpus(root.path());
    let store = fixtures::store(root.path(), "events");
    for case in &cases {
        if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_none()
            && ![
                "cancel-approval-False",
                "finish-0-None-False",
                "finish-1-failed-True",
                "tick-10-0--1-",
                "tick-10-0--1-printf alive",
                "tick-owner-epoch",
                "legacy-True",
                "legacy-False",
            ]
            .contains(&case["name"].as_str().expect("case name"))
        {
            continue;
        }
        if !["cancel", "finish", "legacy", "tick", "probe"]
            .contains(&case["action"].as_str().expect("monitor fixture"))
        {
            continue;
        }
        fixtures::seed(&store, case);
        let id = case["before"]["id"].as_str().expect("monitor fixture");
        let result: Result<(), studio_runtime_services::monitor_policy::MonitorError> = store
            .transaction_with(|tx| {
                let (snapshot, actor) = monitor_runtime::snapshot(tx, id)?;
                let decision = match case["action"].as_str().expect("monitor fixture") {
                    "cancel" => policy::cancel(&snapshot, None, 2000.0)?,
                    "finish" | "legacy" => policy::finish(
                        &snapshot,
                        snapshot.operation(),
                        case["argument"][0].as_i64(),
                        case["argument"][1].as_str(),
                        2000.0,
                    )?,
                    "probe" => policy::probe_result(
                        &snapshot,
                        case["argument"].as_i64().expect("generation"),
                        case["probeResults"][0].clone(),
                        2000.0,
                    )?,
                    "tick" => policy::tick(&snapshot, 2000.0)?,
                    _ => unreachable!(),
                };
                monitor_runtime::apply(tx, &snapshot, &actor, &decision, 2000.0)
            });
        result.unwrap_or_else(|e| panic!("{}: {e}", case["name"]));
        assert_eq!(fixtures::events(&store), case["events"], "{}", case["name"]);
    }
}
